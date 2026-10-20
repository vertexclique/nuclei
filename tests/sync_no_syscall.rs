//! `nuclei::sync` makes no system call except `sched_yield`.
//!
//! The test re-runs itself in a child process. The child warms every
//! primitive up (so pools, thread registrations and allocator arenas
//! exist), then each of its working threads installs a seccomp filter on
//! itself that allows `sched_yield` (kovan's queues yield under
//! contention) and `exit_group` (to end the child) and kills the process
//! on anything else. The hot loops then run: every primitive uncontended,
//! every primitive contended between futures on one thread, and the locks
//! and the semaphore contended across threads. The parent passes only if
//! the child exits cleanly; a forbidden call shows up as death by SIGSYS.
//!
//! The `blocking_*` methods are left out on purpose: they park the calling
//! thread, which is a system call by design.
//!
//! Waiting threads spin on `sched_yield` and the wakers set a flag, so the
//! harness itself stays inside the filter. The allocator is taken out of
//! the measurement: a primitive reaches the allocator when its node pool
//! is empty (always, for a fresh one-shot `Event`), and glibc's `malloc`
//! may then grow its arena with `mprotect`, `brk` or `mmap`. That is the
//! allocator's behaviour, not this module's, so this binary allocates
//! from a bump region mapped once at startup and never freed. A failure
//! names the process, not the call; `strace -f` on the child shows which.

#![cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use std::future::Future;
use std::os::unix::process::ExitStatusExt;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use nuclei::sync::{
    Barrier, Event, Latch, Lazy, MappedMutexGuard, Mutex, MutexGuard, Notify, OnceCell, Owner,
    ReentrantMutex, ReentrantRwLock, RwLock, RwLockWriteGuard, Semaphore, SetOnce,
};

const CHILD: &str = "NUCLEI_SYNC_SECCOMP_CHILD";
const TEST: &str = "hot_loops_make_no_system_call_but_sched_yield";
const ROUNDS: usize = 20_000;
const THREADS: usize = 3;

/// A lock-free bump allocator over one region mapped by the first
/// allocation, which happens at startup before any filter exists.
/// Nothing is ever returned; the region is reserved, not committed, so
/// only what is used costs memory.
struct Bump {
    base: AtomicUsize,
    used: AtomicUsize,
}

const REGION: usize = 4 << 30;

#[global_allocator]
static ALLOCATOR: Bump = Bump {
    base: AtomicUsize::new(0),
    used: AtomicUsize::new(0),
};

impl Bump {
    fn base(&self) -> usize {
        let base = self.base.load(Ordering::Acquire);
        if base != 0 {
            return base;
        }
        // SAFETY: an anonymous private mapping with no address hint.
        let fresh = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                REGION,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        assert_ne!(fresh, libc::MAP_FAILED, "reserve the bump region");
        match self
            .base
            .compare_exchange(0, fresh as usize, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => fresh as usize,
            Err(winner) => {
                // SAFETY: `fresh` lost the race and was never handed out.
                unsafe { libc::munmap(fresh, REGION) };
                winner
            }
        }
    }
}

// SAFETY: every block is a distinct, suitably aligned range of the
// region, and blocks are never reused.
unsafe impl std::alloc::GlobalAlloc for Bump {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        let base = self.base();
        let mut used = self.used.load(Ordering::Relaxed);
        loop {
            let start = (base + used).next_multiple_of(layout.align()) - base;
            let end = start + layout.size();
            if end > REGION {
                return std::ptr::null_mut();
            }
            match self
                .used
                .compare_exchange_weak(used, end, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return (base + start) as *mut u8,
                Err(actual) => used = actual,
            }
        }
    }

    unsafe fn dealloc(&self, _: *mut u8, _: std::alloc::Layout) {}
}

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;

/// Allows `sched_yield` and `exit_group` on the calling thread; any other
/// system call kills the process.
fn forbid_system_calls() {
    use libc::{
        BPF_ABS, BPF_JEQ, BPF_JMP, BPF_K, BPF_LD, BPF_RET, BPF_W, SECCOMP_RET_ALLOW,
        SECCOMP_RET_KILL_PROCESS, sock_filter,
    };
    let jump = |k: u32, jt: u8, jf: u8| sock_filter {
        code: (BPF_JMP | BPF_JEQ | BPF_K) as u16,
        jt,
        jf,
        k,
    };
    let load = |offset: u32| sock_filter {
        code: (BPF_LD | BPF_W | BPF_ABS) as u16,
        jt: 0,
        jf: 0,
        k: offset,
    };
    let ret = |action: u32| sock_filter {
        code: (BPF_RET | BPF_K) as u16,
        jt: 0,
        jf: 0,
        k: action,
    };
    // `seccomp_data` holds the call number at offset 0 and the
    // architecture at offset 4.
    let mut program = [
        load(4),
        jump(AUDIT_ARCH, 1, 0),
        ret(SECCOMP_RET_KILL_PROCESS),
        load(0),
        jump(libc::SYS_sched_yield as u32, 2, 0),
        jump(libc::SYS_exit_group as u32, 1, 0),
        ret(SECCOMP_RET_KILL_PROCESS),
        ret(SECCOMP_RET_ALLOW),
    ];
    let filter = libc::sock_fprog {
        len: program.len() as u16,
        filter: program.as_mut_ptr(),
    };
    // SAFETY: plain system calls; `filter` outlives them and the kernel
    // copies the program.
    unsafe {
        // A killed child need not leave a core dump behind.
        assert_eq!(
            libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0),
            0,
            "dumpable"
        );
        assert_eq!(
            libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0),
            0,
            "no_new_privs"
        );
        assert_eq!(
            libc::syscall(
                libc::SYS_seccomp,
                libc::SECCOMP_SET_MODE_FILTER,
                0,
                &filter as *const libc::sock_fprog
            ),
            0,
            "seccomp filter"
        );
    }
}

struct Flag(AtomicBool);

impl Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::Release);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.store(true, Ordering::Release);
    }
}

/// Polls `future` to completion, yielding while it waits; allocates
/// nothing.
fn spin_on<F: Future>(future: F, waker: &Waker, flag: &Flag) -> F::Output {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(waker);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        while !flag.0.swap(false, Ordering::Acquire) {
            std::thread::yield_now();
        }
    }
}

fn poll_once<F: Future>(future: &mut std::pin::Pin<&mut F>, waker: &Waker) -> Poll<F::Output> {
    future.as_mut().poll(&mut Context::from_waker(waker))
}

struct Shared {
    mutex: Mutex<u64>,
    rwlock: RwLock<u64>,
    semaphore: Semaphore,
}

/// One thread's share of the cross-thread contention.
fn contend(shared: &Shared, rounds: usize, waker: &Waker, flag: &Flag) {
    for i in 0..rounds {
        *spin_on(shared.mutex.lock(), waker, flag) += 1;
        match i % 4 {
            0 => *spin_on(shared.rwlock.write(), waker, flag) += 1,
            1 => {
                let upgradable = spin_on(shared.rwlock.upgradable_read(), waker, flag);
                *spin_on(upgradable.upgrade(), waker, flag) += 1;
            }
            _ => {
                let _ = *spin_on(shared.rwlock.read(), waker, flag);
            }
        }
        let _permit = spin_on(
            shared.semaphore.acquire_many(1 + (i % 2) as u32),
            waker,
            flag,
        )
        .expect("the semaphore is open");
    }
}

/// Every primitive on one thread, uncontended and contended between
/// futures.
fn single_thread_loops(rounds: usize, waker: &Waker, flag: &Flag) {
    let mutex = Mutex::new(0u64);
    let rwlock = RwLock::new(0u64);
    let semaphore = Semaphore::new(2);
    let owned_mutex = Arc::new(Mutex::new(0u64));
    let owned_rwlock = Arc::new(RwLock::new(0u64));
    let owned_semaphore = Arc::new(Semaphore::new(2));
    let reentrant = ReentrantMutex::new(core::cell::Cell::new(0u64));
    let reentrant_rw = ReentrantRwLock::new(AtomicUsize::new(0));
    let notify = Notify::new();
    let barrier = Barrier::new(2);
    let cell = OnceCell::new();
    let lazy: Lazy<u64> = Lazy::new(|| 7);
    let owner = Owner::new();
    for i in 0..rounds as u64 {
        drop(mutex.try_lock());
        let held = spin_on(mutex.lock(), waker, flag);
        {
            let mut queued = pin!(mutex.lock());
            assert!(poll_once(&mut queued, waker).is_pending());
            drop(held);
            assert!(poll_once(&mut queued, waker).is_ready());
        }
        {
            let read = rwlock.try_read();
            let mut write = pin!(rwlock.write());
            assert!(poll_once(&mut write, waker).is_pending());
            drop(read);
            assert!(poll_once(&mut write, waker).is_ready());
        }
        drop(semaphore.try_acquire());
        {
            let all = spin_on(semaphore.acquire_many(2), waker, flag).expect("open");
            let mut one = pin!(semaphore.acquire());
            assert!(poll_once(&mut one, waker).is_pending());
            drop(all);
            assert!(poll_once(&mut one, waker).is_ready());
        }
        {
            // The new fast paths: the `try_*` forms of every primitive, the
            // owned and mapped guards, a downgrade, and the permit calls.
            let guard = Arc::clone(&owned_mutex).try_lock_owned().expect("free");
            assert!(owned_mutex.try_lock().is_err());
            drop(guard);
            let mapped = MutexGuard::map(mutex.try_lock().expect("free"), |count| count);
            let mapped = MappedMutexGuard::map(mapped, |count| count);
            assert!(mutex.try_lock().is_err());
            drop(mapped);
            let read = Arc::clone(&owned_rwlock).try_read_owned().expect("free");
            assert!(Arc::clone(&owned_rwlock).try_write_owned().is_err());
            drop(read);
            let write = rwlock.try_write().expect("free");
            let read = RwLockWriteGuard::downgrade(write);
            assert!(rwlock.try_read().is_ok() && rwlock.try_write().is_err());
            drop(read);
            let write = rwlock.try_write().expect("free");
            let mapped = RwLockWriteGuard::map(write, |value| value);
            drop(mapped);
            let owned = Arc::clone(&owned_rwlock).try_write_owned().expect("free");
            drop(owned.downgrade());
            let permit = Arc::clone(&owned_semaphore)
                .try_acquire_many_owned(2)
                .expect("free");
            assert!(owned_semaphore.try_acquire().is_err());
            drop(permit);
            assert_eq!(semaphore.forget_permits(1), 1);
            semaphore.add_permits(1);
        }
        {
            let closing = Semaphore::new(1);
            let held = closing.try_acquire().expect("free");
            let mut waiting = pin!(closing.acquire());
            assert!(poll_once(&mut waiting, waker).is_pending());
            closing.close();
            assert!(matches!(
                poll_once(&mut waiting, waker),
                Poll::Ready(Err(_))
            ));
            assert!(closing.try_acquire().is_err() && closing.is_closed());
            drop(held);
            assert_eq!(closing.available_permits(), 1);
        }
        {
            notify.notify_last();
            spin_on(notify.notified(), waker, flag);
            let mut enabled = pin!(notify.notified());
            assert!(!enabled.as_mut().enable());
            notify.notify_one();
            assert!(enabled.as_mut().enable());
        }
        {
            let once = SetOnce::new();
            assert!(once.set(i).is_ok() && once.set(i).is_err());
            assert_eq!(*spin_on(once.wait(), waker, flag), i);
            let racy = OnceCell::new();
            assert_eq!(*racy.get_or_init_racy(|| i), i);
            let mut filled = OnceCell::new();
            assert_eq!(*spin_on(filled.get_or_init(|| async { i }), waker, flag), i);
            assert_eq!(filled.take(), Some(i));
        }
        {
            let outer = spin_on(reentrant.lock(&owner), waker, flag);
            let inner = reentrant.try_lock(&owner).expect("re-entered");
            inner.set(outer.get() + 1);
        }
        {
            let write = spin_on(reentrant_rw.write(&owner), waker, flag).expect("no read held");
            let read = reentrant_rw.try_read(&owner).expect("a writer may read");
            write.fetch_add(1, Ordering::Relaxed);
            drop((write, read));
        }
        {
            let upgradable = spin_on(rwlock.upgradable_read(), waker, flag);
            let read = rwlock.try_read().expect("shares with the upgradable read");
            let mut upgrade = pin!(upgradable.upgrade());
            assert!(poll_once(&mut upgrade, waker).is_pending());
            drop(read);
            match poll_once(&mut upgrade, waker) {
                std::task::Poll::Ready(mut write) => *write += 1,
                std::task::Poll::Pending => panic!("the reader left"),
            }
        }
        {
            notify.notify_one();
            spin_on(notify.notified(), waker, flag);
            let mut waiting = pin!(notify.notified());
            assert!(poll_once(&mut waiting, waker).is_pending());
            notify.notify_waiters();
            assert!(poll_once(&mut waiting, waker).is_ready());
        }
        {
            let event = Event::new();
            let mut waiting = pin!(event.wait());
            assert!(poll_once(&mut waiting, waker).is_pending());
            event.set();
            assert!(poll_once(&mut waiting, waker).is_ready());
            let latch = Latch::new(1);
            latch.count_down();
            spin_on(latch.wait(), waker, flag);
        }
        {
            let mut first = pin!(barrier.wait());
            assert!(poll_once(&mut first, waker).is_pending());
            assert!(spin_on(barrier.wait(), waker, flag).is_leader());
            assert!(poll_once(&mut first, waker).is_ready());
        }
        let _ = cell.set(i);
        assert!(cell.get().is_some());
        assert_eq!(*lazy, 7);
    }
}

fn child() -> ! {
    let shared = Arc::new(Shared {
        mutex: Mutex::new(0),
        rwlock: RwLock::new(0),
        semaphore: Semaphore::new(2),
    });
    let ready = Arc::new(AtomicUsize::new(0));
    let go = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicUsize::new(0));
    for _ in 0..THREADS {
        let (shared, ready, go, done) = (
            Arc::clone(&shared),
            Arc::clone(&ready),
            Arc::clone(&go),
            Arc::clone(&done),
        );
        std::thread::spawn(move || {
            let flag = Arc::new(Flag(AtomicBool::new(false)));
            let waker = Waker::from(Arc::clone(&flag));
            contend(&shared, 200, &waker, &flag);
            forbid_system_calls();
            ready.fetch_add(1, Ordering::SeqCst);
            while !go.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            contend(&shared, ROUNDS, &waker, &flag);
            done.fetch_add(1, Ordering::SeqCst);
            loop {
                std::thread::yield_now();
            }
        });
    }
    let flag = Arc::new(Flag(AtomicBool::new(false)));
    let waker = Waker::from(Arc::clone(&flag));
    single_thread_loops(200, &waker, &flag);
    while ready.load(Ordering::SeqCst) < THREADS {
        std::thread::yield_now();
    }
    forbid_system_calls();
    go.store(true, Ordering::Release);
    single_thread_loops(ROUNDS, &waker, &flag);
    contend(&shared, ROUNDS, &waker, &flag);
    while done.load(Ordering::SeqCst) < THREADS {
        std::thread::yield_now();
    }
    let expected = (200 + ROUNDS as u64) * THREADS as u64 + ROUNDS as u64;
    let code = if shared.mutex.try_lock().map(|count| *count).ok() == Some(expected) {
        0
    } else {
        3
    };
    // SAFETY: ends the process with `exit_group`, the one exit the filter
    // allows; nothing here needs unwinding.
    unsafe { libc::_exit(code) }
}

/// Re-runs the test `name` in a child process with `CHILD` set and
/// returns how it ended.
fn run_child(name: &str) -> std::process::ExitStatus {
    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", name, "--test-threads=1", "--nocapture"])
        .env(CHILD, "1")
        .spawn()
        .expect("spawn the filtered child");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    let mut backoff = std::time::Duration::from_millis(1);
    loop {
        if let Some(status) = child.try_wait().expect("wait for the child") {
            return status;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("the filtered child did not finish within 300 s; a wait is stuck");
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(std::time::Duration::from_millis(200));
    }
}

/// Proves the filter bites: a sleep inside it must kill the child.
#[test]
fn calibration_a_sleep_inside_the_filter_kills_the_child() {
    if std::env::var_os(CHILD).is_some() {
        forbid_system_calls();
        std::thread::sleep(std::time::Duration::from_millis(1));
        // SAFETY: as in `child`; not reached when the filter works.
        unsafe { libc::_exit(0) }
    }
    let status = run_child("calibration_a_sleep_inside_the_filter_kills_the_child");
    assert_eq!(status.signal(), Some(libc::SIGSYS), "{status:?}");
}

#[test]
fn hot_loops_make_no_system_call_but_sched_yield() {
    if std::env::var_os(CHILD).is_some() {
        child();
    }
    let status = run_child(TEST);
    // An assertion failing inside the filter also dies by SIGSYS, since
    // printing the panic is a forbidden write.
    assert_eq!(
        status.signal(),
        None,
        "the child died by signal {:?}; SIGSYS ({}) means a forbidden system call",
        status.signal(),
        libc::SIGSYS
    );
    assert_eq!(
        status.code(),
        Some(0),
        "the child finished with the wrong count or failed an assertion"
    );
}
