//! IO buffers: each shard's pool, recycled, and charged before they exist.
//!
//! A shard reads into buffers of its own pool when the bytes cannot go
//! straight into the caller's slice (the read had to wait), and copies
//! writes that cannot go out at once into them. Buffers come in four size
//! classes; a request takes the smallest class that holds it. A freed buffer
//! goes back to its class's free list for the next read, so a pool that has
//! grown to what its connections need allocates nothing more. A datagram
//! larger than the largest class gets a buffer made for it, charged like any
//! other and freed when it comes back.
//!
//! # Accounting
//!
//! The pool counts every byte it took from the allocator, in use or free,
//! and takes none past its cap. At the cap it frees idle buffers of other
//! classes to make room; when every byte is in use, [`Pool::get`] answers
//! `None` and the side that asked waits ([`Pool::wait_for_return`]) until a
//! buffer comes back. So a shard's buffer memory is bounded by its cap,
//! whatever its connections do.
//!
//! # Where a buffer goes back
//!
//! A buffer dropped on its pool's own thread goes straight to the free list.
//! Dropped anywhere else (its read-ahead moved with a handle to another
//! thread, its write-behind written by a blocking-pool job), it goes onto
//! the pool's return list, linked through the returned buffers themselves:
//! pushed with a compare-and-swap, taken whole by the pool when its free
//! list runs dry, so nothing is allocated for it and nothing waits for
//! reclamation (the push and the take of proofs/tla/remote, without its
//! close: the home outlives every buffer). The shared part of a pool
//! ([`Home`]) lives as long as any of its buffers, and frees what comes back
//! after the pool itself is gone. Either way a buffer coming back wakes
//! whoever waits for one; nothing that drops a buffer may hold a source's
//! guard (see `io::source`), since the wake reaches into sources.

#[cfg(any(ring, unix))]
pub(crate) mod spare;

use core::cell::{Cell, RefCell};
use core::ptr::NonNull;
use core::task::Waker;
use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering, fence};

use kovan_queue::seg_queue::SegQueue;

/// The size classes, smallest first.
pub(crate) const CLASSES: [usize; 4] = [512, 4 * 1024, 16 * 1024, 64 * 1024];
/// The largest class.
pub(crate) const LARGEST: usize = CLASSES[CLASSES.len() - 1];
/// The class tag of a buffer made for one request larger than every class.
const WHOLE: u8 = u8::MAX;
/// What a shard's pool may hold unless its runtime says otherwise.
pub(crate) const DEFAULT_CAP: usize = 64 << 20;

/// The class a request of `len` bytes takes: the smallest that holds it, or
/// the largest.
fn class_for(len: usize) -> usize {
    CLASSES
        .iter()
        .position(|&size| size >= len)
        .unwrap_or(CLASSES.len() - 1)
}

/// How a class's buffers are laid out: page-aligned from 4 KiB, so the
/// larger ones can serve direct IO; cache-line aligned below.
fn layout(class: usize) -> Layout {
    let size = CLASSES[class];
    let align = if size >= 4096 { 4096 } else { 64 };
    // SAFETY: every class size is a non-zero multiple of its alignment, and
    // the alignment is a power of two.
    unsafe { Layout::from_size_align_unchecked(size, align) }
}

/// How a whole buffer of `size` bytes (a multiple of 4096) is laid out.
fn layout_whole(size: usize) -> Layout {
    // SAFETY: `size` is a non-zero multiple of the page alignment, and was
    // checked to fit a `u32` when the buffer was made.
    unsafe { Layout::from_size_align_unchecked(size, 4096) }
}

/// A buffer's memory, without its length.
struct Raw(NonNull<u8>);

// SAFETY: plain memory nobody else points at.
unsafe impl Send for Raw {}

/// A buffer on its way back to its pool.
struct Returned {
    raw: Raw,
    class: u8,
    cap: u32,
}

/// A returned buffer as it lies on the return list, written into its first
/// bytes: every buffer is at least 512 bytes, aligned to 64. No padding, so
/// every byte of the buffer stays initialized (a slice over it is sound).
#[repr(C)]
struct Back {
    next: *mut Back,
    cap: u32,
    class: u32,
}

/// The part of a pool its buffers go back to from any thread.
pub(crate) struct Home {
    /// The buffers given back, last first.
    returned: AtomicPtr<Back>,
    /// Someone waits for a buffer to come back.
    waiting: AtomicBool,
    /// Who: woken, all of them, by the next buffer that comes back.
    waiters: SegQueue<Waker>,
}

impl Home {
    /// Takes back a buffer dropped on a thread that is not its pool's.
    fn give_back(&self, returned: Returned) {
        let back = returned.raw.0.cast::<Back>().as_ptr();
        let mut head = self.returned.load(Ordering::Relaxed);
        loop {
            // SAFETY: the buffer is no one's but this call's until the
            // exchange publishes it, and holds a `Back`.
            unsafe {
                back.write(Back {
                    next: head,
                    cap: returned.cap,
                    class: u32::from(returned.class),
                });
            }
            match self.returned.compare_exchange_weak(
                head,
                back,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(now) => head = now,
            }
        }
        // Pairs with the fence of `wait_for_return`: either this sees the
        // waiter's flag, or the waiter sees this buffer in the queue.
        fence(Ordering::SeqCst);
        if self.waiting.load(Ordering::Relaxed) {
            self.wake_waiters();
        }
    }

    /// Every buffer given back so far, the last first.
    fn take_all(&self) -> Taken {
        if self.returned.load(Ordering::Relaxed).is_null() {
            return Taken(core::ptr::null_mut());
        }
        Taken(self.returned.swap(core::ptr::null_mut(), Ordering::Acquire))
    }

    fn wake_waiters(&self) {
        if self.waiting.swap(false, Ordering::AcqRel) {
            while let Ok(waker) = self.waiters.try_pop() {
                waker.wake();
            }
        }
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        // The pool is gone: what came back after it goes to the allocator.
        for r in self.take_all() {
            // SAFETY: allocated with this layout, owned by nobody else.
            unsafe { dealloc(r.raw.0.as_ptr(), returned_layout(r.class, r.cap)) };
        }
    }
}

/// Buffers taken off a return list, theirs alone now.
struct Taken(*mut Back);

impl Iterator for Taken {
    type Item = Returned;

    fn next(&mut self) -> Option<Returned> {
        let back = NonNull::new(self.0)?;
        // SAFETY: a buffer given back, this list's since the swap that took
        // it (Acquire, against the push's Release).
        let Back { next, cap, class } = unsafe { back.as_ptr().read() };
        self.0 = next;
        Some(Returned {
            raw: Raw(back.cast()),
            // Written from a `u8` by `give_back`.
            class: class as u8,
            cap,
        })
    }
}

fn returned_layout(class: u8, cap: u32) -> Layout {
    if class == WHOLE {
        layout_whole(cap as usize)
    } else {
        layout(usize::from(class))
    }
}

/// A shard's buffer pool.
pub(crate) struct Pool {
    home: Arc<Home>,
    free: [RefCell<Vec<NonNull<u8>>>; CLASSES.len()],
    /// Bytes the pool may hold, in use and free.
    cap: usize,
    /// The largest class that fits under the cap, if any does.
    top: Option<usize>,
    /// Bytes it holds.
    charged: Cell<usize>,
    /// Times a buffer was wanted with every byte under the cap in use.
    exhausted: Cell<u64>,
}

// SAFETY: the free buffers are memory the pool owns outright and the rest
// is shared through `Arc<Home>`; a pool moved before it is entered (a shard
// built on one thread, run on its worker) leaves nothing behind. While
// entered it is borrowed, so it cannot move.
unsafe impl Send for Pool {}

impl Pool {
    /// A pool holding at most `cap` bytes, and at least one buffer of the
    /// smallest class whatever `cap` says: IO through a pool that could hold
    /// none would never move.
    pub(crate) fn new(cap: usize) -> Self {
        let cap = cap.max(CLASSES[0]);
        Self {
            home: Arc::new(Home {
                returned: AtomicPtr::new(core::ptr::null_mut()),
                waiting: AtomicBool::new(false),
                waiters: SegQueue::new(),
            }),
            free: Default::default(),
            cap,
            top: CLASSES.iter().rposition(|&size| size <= cap),
            charged: Cell::new(0),
            exhausted: Cell::new(0),
        }
    }

    /// Bytes the pool holds, in use and free.
    pub(crate) fn charged(&self) -> usize {
        self.charged.get()
    }

    /// Times a buffer was wanted while every byte under the cap was in use.
    pub(crate) fn exhausted(&self) -> u64 {
        self.exhausted.get()
    }

    /// Whether `home` is this pool's.
    fn is_home(&self, home: &Arc<Home>) -> bool {
        Arc::ptr_eq(&self.home, home)
    }

    /// A buffer of at least `len` bytes (or of the largest class that fits
    /// the cap), empty, or `None` when every byte under the cap is in use.
    pub(crate) fn get(&self, len: usize) -> Option<PoolBuf> {
        self.get_class(class_for(len).min(self.top?))
    }

    fn get_class(&self, class: usize) -> Option<PoolBuf> {
        // The free list is let go before the fallbacks, which reach it.
        let free = self.free[class].borrow_mut().pop();
        let ptr = free
            .or_else(|| self.take_returned(class))
            .or_else(|| self.allocate(class))?;
        Some(self.buf(ptr, class as u8, CLASSES[class]))
    }

    /// Whether a buffer holding all of `len` bytes could ever come from the
    /// pool, every other buffer back.
    pub(crate) fn could_hold_whole(&self, len: usize) -> bool {
        let size = if len <= LARGEST {
            CLASSES[class_for(len)]
        } else {
            len.next_multiple_of(4096)
        };
        size <= self.cap
    }

    /// The bytes the pool may hold.
    pub(crate) fn cap(&self) -> usize {
        self.cap
    }

    /// A buffer that holds all of `len` bytes, whatever their number: a class
    /// buffer when one is large enough, else one made for them.
    pub(crate) fn get_whole(&self, len: usize) -> Option<PoolBuf> {
        if len <= LARGEST {
            return self.get_class(class_for(len));
        }
        let size = len.checked_next_multiple_of(4096)?;
        u32::try_from(size).ok()?;
        // Wholes that came back are freed here, which may make the room.
        let _ = self.take_returned(usize::MAX);
        if !self.make_room(size) {
            return None;
        }
        // SAFETY: the layout has a non-zero size.
        let ptr = NonNull::new(unsafe { alloc_zeroed(layout_whole(size)) })?;
        self.charged.set(self.charged.get() + size);
        Some(self.buf(ptr, WHOLE, size))
    }

    fn buf(&self, ptr: NonNull<u8>, class: u8, cap: usize) -> PoolBuf {
        PoolBuf {
            ptr,
            class,
            cap: cap as u32,
            start: 0,
            end: 0,
            home: Arc::clone(&self.home),
        }
    }

    /// A buffer of `class` that came back from another thread; others found
    /// on the way go to their free lists, and wholes to the allocator.
    fn take_returned(&self, class: usize) -> Option<NonNull<u8>> {
        let mut found = None;
        for r in self.home.take_all() {
            if r.class == WHOLE {
                self.free_whole(r.raw.0, r.cap);
            } else if found.is_none() && usize::from(r.class) == class {
                found = Some(r.raw.0);
            } else {
                self.free[usize::from(r.class)].borrow_mut().push(r.raw.0);
            }
        }
        found
    }

    /// A new buffer of `class`, charged first; none past the cap.
    fn allocate(&self, class: usize) -> Option<NonNull<u8>> {
        let size = CLASSES[class];
        if !self.make_room(size) {
            return None;
        }
        // Zeroed once, when the pool grows: every byte of a buffer is
        // initialized from then on, so a slice over it is always sound, and
        // a recycled buffer costs nothing.
        // SAFETY: the layout has a non-zero size.
        let ptr = NonNull::new(unsafe { alloc_zeroed(layout(class)) })?;
        self.charged.set(self.charged.get() + size);
        Some(ptr)
    }

    /// Frees idle buffers, the largest first, until `size` more bytes fit
    /// under the cap. False when even all of them would not make the room.
    fn make_room(&self, size: usize) -> bool {
        let fits = |charged: usize| charged.saturating_add(size) <= self.cap;
        if fits(self.charged.get()) {
            return true;
        }
        for class in (0..CLASSES.len()).rev() {
            let mut free = self.free[class].borrow_mut();
            while let Some(ptr) = free.pop() {
                // SAFETY: allocated with this class's layout; free, so owned
                // by the pool alone.
                unsafe { dealloc(ptr.as_ptr(), layout(class)) };
                self.charged.set(self.charged.get() - CLASSES[class]);
                if fits(self.charged.get()) {
                    return true;
                }
            }
        }
        self.exhausted.set(self.exhausted.get() + 1);
        false
    }

    fn free_whole(&self, ptr: NonNull<u8>, cap: u32) {
        // SAFETY: made with this layout, and owned by nobody else.
        unsafe { dealloc(ptr.as_ptr(), layout_whole(cap as usize)) };
        self.charged.set(self.charged.get() - cap as usize);
    }

    /// Takes back a buffer dropped on this pool's own thread.
    fn take_back(&self, ptr: NonNull<u8>, class: u8, cap: u32) {
        if class == WHOLE {
            self.free_whole(ptr, cap);
        } else {
            self.free[usize::from(class)].borrow_mut().push(ptr);
        }
        // Only this thread raises the flag, so a plain load sees it.
        if self.home.waiting.load(Ordering::Relaxed) {
            self.home.wake_waiters();
        }
    }

    /// After a [`get`](Self::get) found nothing: `waker` is woken when a
    /// buffer comes back to this pool, from any thread; at once if one came
    /// back since, or if none is out to come back (the request is larger
    /// than the cap allows).
    pub(crate) fn wait_for_return(&self, waker: Waker) {
        if self.charged.get() == 0 {
            waker.wake();
            return;
        }
        self.home.waiters.push(waker);
        self.home.waiting.store(true, Ordering::SeqCst);
        // Pairs with the fence of `Home::give_back`.
        fence(Ordering::SeqCst);
        let idle = self.free.iter().any(|f| !f.borrow().is_empty());
        if idle || !self.home.returned.load(Ordering::SeqCst).is_null() {
            self.home.wake_waiters();
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        for (class, free) in self.free.iter_mut().enumerate() {
            for ptr in free.get_mut().drain(..) {
                // SAFETY: allocated with this class's layout; free, so owned
                // by the pool alone.
                unsafe { dealloc(ptr.as_ptr(), layout(class)) };
            }
        }
        // Buffers still out come back to `Home`, which frees them; whoever
        // waits for one is woken, to find the pool gone.
        self.home.wake_waiters();
    }
}

thread_local! {
    /// The pool of a thread that runs no shard (a task of another executor
    /// writing to a handle), made on first need.
    static FALLBACK: Pool = Pool::new(DEFAULT_CAP);
}

/// Runs `f` with this thread's pool: the pool of the shard it runs (a call
/// from a task's code comes back into the shard first, see `rt::context`),
/// or its own if it runs none. `None` only while the thread exits, after its
/// pool is gone.
pub(crate) fn with_pool<R>(f: impl FnOnce(Option<&Pool>) -> R) -> R {
    crate::rt::context::current(|shard| match shard {
        Some(shard) => f(Some(shard.pool())),
        None => {
            let fallback = FALLBACK
                .try_with(core::ptr::from_ref)
                .unwrap_or(core::ptr::null());
            // SAFETY: the fallback lives until the thread's destructors run,
            // which `try_with` would have reported.
            f(unsafe { fallback.as_ref() })
        }
    })
}

/// [`with_pool`], unless the caller already holds this thread's pool.
pub(crate) fn with_pool_or<R>(known: Option<&Pool>, f: impl FnOnce(Option<&Pool>) -> R) -> R {
    match known {
        Some(pool) => f(Some(pool)),
        None => with_pool(f),
    }
}

/// Copies the front of `src` into `bufs`, in order; returns how many bytes.
pub(crate) fn copy_vectored(src: &[u8], bufs: &mut [std::io::IoSliceMut<'_>]) -> usize {
    let mut n = 0;
    for buf in bufs.iter_mut() {
        let rest = &src[n..];
        if rest.is_empty() {
            break;
        }
        let k = rest.len().min(buf.len());
        buf[..k].copy_from_slice(&rest[..k]);
        n += k;
    }
    n
}

/// Copies the front of `src` onto the end of `buf`, as much as its spare
/// capacity holds; returns how many bytes.
pub(crate) fn append_spare(src: &[u8], buf: &mut Vec<u8>) -> usize {
    let n = src.len().min(buf.capacity() - buf.len());
    buf.extend_from_slice(&src[..n]);
    n
}

/// A buffer of a shard's pool, with the bytes `start..end` filled and not
/// read yet.
pub(crate) struct PoolBuf {
    ptr: NonNull<u8>,
    class: u8,
    cap: u32,
    start: u32,
    end: u32,
    home: Arc<Home>,
}

// SAFETY: the buffer owns its memory; going back to its pool is either a
// thread-local free list (on the pool's own thread) or the lock-free return
// queue.
unsafe impl Send for PoolBuf {}
// SAFETY: `&PoolBuf` only reads the filled bytes.
unsafe impl Sync for PoolBuf {}

impl PoolBuf {
    /// The buffer's size.
    pub(crate) fn capacity(&self) -> usize {
        self.cap as usize
    }

    /// The filled bytes not read yet.
    pub(crate) fn filled(&self) -> &[u8] {
        // SAFETY: `start..end` is within the buffer and was written.
        unsafe {
            core::slice::from_raw_parts(
                self.ptr.as_ptr().add(self.start as usize),
                (self.end - self.start) as usize,
            )
        }
    }

    /// Whether every filled byte was read.
    pub(crate) fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// The whole buffer to fill from its start, discarding what it held; then
    /// [`set_filled`](Self::set_filled) says how much was written.
    pub(crate) fn spare(&mut self) -> &mut [u8] {
        self.start = 0;
        self.end = 0;
        // SAFETY: the buffer is ours, and every byte of it is initialized (it
        // was zeroed when it was allocated).
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.capacity()) }
    }

    /// The first `n` bytes of the buffer were written.
    pub(crate) fn set_filled(&mut self, n: usize) {
        debug_assert!(n <= self.capacity());
        self.start = 0;
        self.end = n.min(self.capacity()) as u32;
    }

    /// Fills the buffer with `bytes` (as many as fit); returns how many.
    pub(crate) fn fill_from(&mut self, bytes: &[u8]) -> usize {
        let n = bytes.len().min(self.capacity());
        self.spare()[..n].copy_from_slice(&bytes[..n]);
        self.set_filled(n);
        n
    }

    /// Adds `bytes` after the filled ones, as many as fit, moving the unread
    /// bytes to the front first when that makes room; returns how many.
    pub(crate) fn append(&mut self, bytes: &[u8]) -> usize {
        let cap = self.capacity();
        let (start, end) = (self.start as usize, self.end as usize);
        if start > 0 && end + bytes.len() > cap {
            // SAFETY: both ranges lie within the buffer; `copy` allows the
            // overlap.
            unsafe {
                core::ptr::copy(self.ptr.as_ptr().add(start), self.ptr.as_ptr(), end - start)
            };
            self.start = 0;
            self.end = (end - start) as u32;
        }
        let end = self.end as usize;
        let n = (cap - end).min(bytes.len());
        // SAFETY: `end..end + n` lies within the buffer, which `bytes` (a
        // shared borrow) cannot overlap.
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr.as_ptr().add(end), n) };
        self.end += n as u32;
        n
    }

    /// `n` filled bytes were read.
    pub(crate) fn consume(&mut self, n: usize) {
        let n = n.min((self.end - self.start) as usize);
        self.start += n as u32;
    }

    /// Copies filled bytes into `out` and consumes them; returns how many.
    pub(crate) fn read_into(&mut self, out: &mut [u8]) -> usize {
        let n = self.peek_into(out);
        self.consume(n);
        n
    }

    /// Copies filled bytes into `bufs`, in order, and consumes them;
    /// returns how many.
    pub(crate) fn read_into_vectored(&mut self, bufs: &mut [std::io::IoSliceMut<'_>]) -> usize {
        let n = copy_vectored(self.filled(), bufs);
        self.consume(n);
        n
    }

    /// Adds the bytes of `bufs` after the filled ones, in order, as many as
    /// fit; returns how many.
    pub(crate) fn append_vectored(&mut self, bufs: &[std::io::IoSlice<'_>]) -> usize {
        let mut n = 0;
        for buf in bufs {
            let taken = self.append(buf);
            n += taken;
            if taken < buf.len() {
                break;
            }
        }
        n
    }

    /// Copies filled bytes into `out` without consuming them; returns how
    /// many.
    pub(crate) fn peek_into(&self, out: &mut [u8]) -> usize {
        let src = self.filled();
        let n = src.len().min(out.len());
        out[..n].copy_from_slice(&src[..n]);
        n
    }
}

impl Drop for PoolBuf {
    fn drop(&mut self) {
        let returned = crate::rt::context::current(|shard| match shard {
            Some(shard) if shard.pool().is_home(&self.home) => {
                shard.pool().take_back(self.ptr, self.class, self.cap);
                false
            }
            _ => true,
        });
        if returned {
            self.home.give_back(Returned {
                raw: Raw(self.ptr),
                class: self.class,
                cap: self.cap,
            });
        }
    }
}

impl core::fmt::Debug for PoolBuf {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PoolBuf")
            .field("capacity", &self.capacity())
            .field("filled", &(self.end - self.start))
            .finish()
    }
}

#[cfg(all(test, not(loom)))]
mod tests;
