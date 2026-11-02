//! User code that panics where nobody polls it: a task's future as it is
//! dropped, a task hook, a `shard_local!` value as its worker stops. The
//! panic goes to the task's handle where one awaits it and is logged at error
//! where none does; the worker, the runtime and every other task go on.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use nuclei::runtime::Runtime;
use nuclei::task::Builder as TaskBuilder;

const SAFETY: Duration = Duration::from_secs(10);

static LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Keeps every record logged at error.
struct Capture;

impl log::Log for Capture {
    fn enabled(&self, meta: &log::Metadata<'_>) -> bool {
        meta.level() <= log::Level::Error
    }

    fn log(&self, record: &log::Record<'_>) {
        if record.level() == log::Level::Error {
            LOGGED.lock().unwrap().push(record.args().to_string());
        }
    }

    fn flush(&self) {}
}

fn capture() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        log::set_logger(&Capture).expect("no other logger in this test binary");
        log::set_max_level(log::LevelFilter::Error);
    });
}

/// Waits until a record logged at error holds `marker`, or fails at the
/// deadline.
fn logged(marker: &str) -> String {
    let deadline = Instant::now() + SAFETY;
    let mut pause = Duration::from_micros(50);
    loop {
        if let Some(line) = LOGGED.lock().unwrap().iter().find(|l| l.contains(marker)) {
            return line.clone();
        }
        assert!(
            Instant::now() < deadline,
            "nothing logged at error holds {marker:?}"
        );
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(20));
    }
}

fn runtime(configure: impl FnOnce(nuclei::Builder) -> nuclei::Builder) -> Runtime {
    configure(Runtime::builder().worker_threads(1).pin_workers(false))
        .build()
        .expect("the runtime starts")
}

/// Panics with its message as it is dropped.
struct Bomb(&'static str);

impl Drop for Bomb {
    fn drop(&mut self) {
        std::panic::panic_any(self.0);
    }
}

fn message(panic: &(dyn std::any::Any + Send)) -> &str {
    panic
        .downcast_ref::<&str>()
        .copied()
        .unwrap_or("not a string")
}

#[test]
fn a_detached_task_whose_future_panics_as_it_is_dropped_is_logged_and_the_runtime_runs_on() {
    capture();
    let rt = runtime(|b| b);
    let h = rt.handle();
    rt.block_on(async {
        // Pending with no waker kept: nothing can wake it or await it, so
        // its future is dropped where it ran.
        let bomb = Bomb("detached future dropped");
        h.spawn(async move {
            let _bomb = bomb;
            core::future::pending::<()>().await
        })
        .detach();
        logged("detached future dropped");
        assert_eq!(h.spawn(async { 7 }).await, 7, "another task runs on");
    });
}

#[test]
fn a_task_whose_future_panics_as_it_is_cancelled_hands_the_panic_to_its_handle() {
    capture();
    let rt = runtime(|b| b);
    let h = rt.handle();
    rt.block_on(async {
        let bomb = Bomb("cancelled future dropped");
        let task = h.spawn(async move {
            let _bomb = bomb;
            core::future::pending::<()>().await
        });
        task.abort();
        let err = task.join().await.expect_err("cancelled");
        assert!(
            err.is_panic(),
            "the drop's panic is the task's outcome: {err}"
        );
        assert_eq!(message(&*err.into_panic()), "cancelled future dropped");
        assert_eq!(h.spawn(async { 7 }).await, 7, "another task runs on");
    });
}

/// A runtime whose task hook `which` panics for the task named "bomb".
fn hooked(which: &'static str) -> Runtime {
    let hook = move |meta: &nuclei::runtime::TaskMeta<'_>| {
        if meta.name() == Some("bomb") {
            std::panic::panic_any(which);
        }
    };
    runtime(|b| match which {
        "before" => b.on_before_task_poll(hook),
        "after" => b.on_after_task_poll(hook),
        "terminate" => b.on_task_terminate(hook),
        _ => b.on_task_spawn(hook),
    })
}

#[test]
fn a_poll_or_terminate_hook_that_panics_panics_the_task_and_the_runtime_runs_on() {
    capture();
    for which in ["before", "after", "terminate"] {
        let rt = hooked(which);
        let h = rt.handle();
        rt.block_on(async {
            let task = TaskBuilder::new()
                .name("bomb")
                .spawn_on(async { 7 }, &h)
                .expect("spawned");
            let err = task.join().await.expect_err("the hook panicked");
            assert!(err.is_panic(), "{which}: {err}");
            assert_eq!(message(&*err.into_panic()), which);
            assert_eq!(
                h.spawn(async { 8 }).await,
                8,
                "{which}: another task runs on"
            );
        });
    }
}

#[test]
fn a_spawn_hook_that_panics_unwinds_out_of_the_spawn_and_the_task_never_runs() {
    capture();
    let rt = hooked("spawn");
    let h = rt.handle();
    let ran = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicUsize::new(0));
    struct Counted(Arc<AtomicUsize>);
    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let (r, d) = (Arc::clone(&ran), Counted(Arc::clone(&dropped)));
    let spawned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        TaskBuilder::new().name("bomb").spawn_on(
            async move {
                let _d = d;
                r.store(true, Ordering::SeqCst);
            },
            &h,
        )
    }));
    let panic = spawned.expect_err("the spawn hook's panic reaches the spawner");
    assert_eq!(message(&*panic), "spawn");
    assert_eq!(dropped.load(Ordering::SeqCst), 1, "the future was dropped");
    rt.block_on(async { assert_eq!(h.spawn(async { 8 }).await, 8, "the runtime runs on") });
    assert!(!ran.load(Ordering::SeqCst), "the task never ran");
}

static SURVIVED: AtomicUsize = AtomicUsize::new(0);

/// Counts its drop.
struct Survivor;

impl Drop for Survivor {
    fn drop(&mut self) {
        SURVIVED.fetch_add(1, Ordering::SeqCst);
    }
}

nuclei::shard_local! { static BRITTLE: Bomb = Bomb("shard-local value dropped") }
nuclei::shard_local! { static STURDY: Survivor = Survivor }

#[test]
fn a_shard_local_value_that_panics_as_its_worker_stops_is_logged_and_the_worker_goes_on() {
    capture();
    let unwound = Arc::new(AtomicUsize::new(0));
    let u = Arc::clone(&unwound);
    let rt = runtime(move |b| {
        b.on_thread_stop(move || {
            if std::thread::panicking() {
                u.fetch_add(1, Ordering::SeqCst);
            }
        })
    });
    let h = rt.handle();
    rt.block_on(async {
        // A receive out as the worker stops: the shard's driver holds it,
        // and lets go of it only as the shard goes.
        let socket = h
            .spawn(async { nuclei::net::UdpSocket::bind("127.0.0.1:0").await })
            .await
            .expect("bound");
        let (out_tx, out_rx) = nuclei::sync::oneshot::channel::<()>();
        h.spawn(async move {
            BRITTLE.with(|_| ());
            STURDY.with(|_| ());
            let mut buf = [0u8; 64];
            let mut receive = std::pin::pin!(socket.recv(&mut buf));
            core::future::poll_fn(|cx| {
                let _ = receive.as_mut().poll(cx);
                core::task::Poll::Ready(())
            })
            .await;
            let _ = out_tx.send(());
            let _ = receive.await;
        })
        .detach();
        out_rx.await.expect("the receive is out");
    });
    drop(h);
    drop(rt);
    assert_eq!(
        unwound.load(Ordering::SeqCst),
        0,
        "the panic unwound out of the worker"
    );
    assert_eq!(
        SURVIVED.load(Ordering::SeqCst),
        1,
        "the other value was dropped too"
    );
    let line = logged("shard-local value dropped");
    assert!(line.contains("shard-local"), "{line}");
}

/// A waker whose wake, clone (on its Nth call) or drop panics as a test
/// chooses; `woken` says a wake reached it.
struct Probe {
    wake_panics: Option<&'static str>,
    drop_panics: Option<&'static str>,
    clone_panics_on: usize,
    clones: AtomicUsize,
    woken: AtomicBool,
}

impl Probe {
    fn new() -> Self {
        Self {
            wake_panics: None,
            drop_panics: None,
            clone_panics_on: 0,
            clones: AtomicUsize::new(0),
            woken: AtomicBool::new(false),
        }
    }

    fn waker(self: &Arc<Self>) -> std::task::Waker {
        let data = Arc::into_raw(Arc::clone(self)).cast::<()>();
        // SAFETY: the vtable's functions take `data` as the `Arc<Probe>` just
        // leaked, one count per waker.
        unsafe { std::task::Waker::from_raw(std::task::RawWaker::new(data, &PROBE_VTABLE)) }
    }
}

static PROBE_VTABLE: std::task::RawWakerVTable =
    std::task::RawWakerVTable::new(probe_clone, probe_wake, probe_wake_by_ref, probe_drop);

unsafe fn probe_clone(data: *const ()) -> std::task::RawWaker {
    // SAFETY: `data` is a live `Probe` kept by one count of the waker cloned.
    let probe = unsafe { &*data.cast::<Probe>() };
    if probe.clones.fetch_add(1, Ordering::SeqCst) + 1 == probe.clone_panics_on {
        std::panic::panic_any("probe waker cloned");
    }
    // SAFETY: as above; the new waker takes the count added here.
    unsafe { Arc::increment_strong_count(data.cast::<Probe>()) };
    std::task::RawWaker::new(data, &PROBE_VTABLE)
}

unsafe fn probe_wake(data: *const ()) {
    // SAFETY: the waker is consumed, and its count with it.
    let probe = unsafe { Arc::from_raw(data.cast::<Probe>()) };
    probe.woken.store(true, Ordering::SeqCst);
    if let Some(message) = probe.wake_panics {
        std::panic::panic_any(message);
    }
}

unsafe fn probe_wake_by_ref(data: *const ()) {
    // SAFETY: `data` is a live `Probe` kept by the waker.
    unsafe { &*data.cast::<Probe>() }
        .woken
        .store(true, Ordering::SeqCst);
}

unsafe fn probe_drop(data: *const ()) {
    // SAFETY: the waker is consumed, and its count with it.
    let probe = unsafe { Arc::from_raw(data.cast::<Probe>()) };
    let message = probe.drop_panics;
    drop(probe);
    if let Some(message) = message {
        std::panic::panic_any(message);
    }
}

fn logged_times(marker: &str) -> usize {
    LOGGED
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.contains(marker))
        .count()
}

fn poll_once<F: Future + Unpin>(
    future: &mut F,
    waker: &std::task::Waker,
) -> std::task::Poll<F::Output> {
    Pin::new(future).poll(&mut std::task::Context::from_waker(waker))
}

/// Waits until `flag` is set, or fails at the deadline.
fn wait_for(flag: &AtomicBool, what: &str) {
    let deadline = Instant::now() + SAFETY;
    let mut pause = Duration::from_micros(50);
    while !flag.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(20));
    }
}

#[test]
fn a_handle_waker_that_panics_as_it_is_woken_is_logged_and_the_handle_resolves() {
    capture();
    let rt = runtime(|b| b);
    let h = rt.handle();
    rt.block_on(async {
        let (gate, open) = nuclei::sync::oneshot::channel::<()>();
        let mut task = h.spawn(async move {
            let _ = open.await;
            41
        });
        let probe = Arc::new(Probe {
            wake_panics: Some("handle waker woken"),
            ..Probe::new()
        });
        let waker = probe.waker();
        assert!(poll_once(&mut task, &waker).is_pending());
        let _ = gate.send(());
        let line = logged("handle waker woken");
        assert!(line.contains("panicked as it was woken"), "{line}");
        wait_for(&probe.woken, "the waker was never woken");
        let mut result = None;
        let noop = std::task::Waker::noop();
        let deadline = Instant::now() + SAFETY;
        while result.is_none() {
            assert!(Instant::now() < deadline, "the handle never resolved");
            if let std::task::Poll::Ready(v) = poll_once(&mut task, noop) {
                result = Some(v);
            }
        }
        assert_eq!(result, Some(41));
        assert_eq!(logged_times("handle waker woken"), 1);
    });
}

#[test]
fn a_handle_waker_that_panics_as_it_is_dropped_is_logged_once_and_the_handle_resolves() {
    capture();
    let rt = runtime(|b| b);
    let h = rt.handle();
    rt.block_on(async {
        let (gate, open) = nuclei::sync::oneshot::channel::<()>();
        let mut task = h.spawn(async move {
            let _ = open.await;
            42
        });
        let a = Arc::new(Probe {
            drop_panics: Some("handle waker dropped"),
            ..Probe::new()
        });
        let a_waker = a.waker();
        assert!(poll_once(&mut task, &a_waker).is_pending());
        let b = Arc::new(Probe::new());
        let b_waker = b.waker();
        assert!(poll_once(&mut task, &b_waker).is_pending());
        let line = logged("handle waker dropped");
        assert!(line.contains("panicked as it was dropped"), "{line}");
        let own = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(a_waker)));
        assert!(own.is_err(), "the caller's own waker drops as it chose");
        let _ = gate.send(());
        wait_for(&b.woken, "the stored waker was never woken");
        assert_eq!(poll_once(&mut task, &b_waker), std::task::Poll::Ready(42));
        assert_eq!(logged_times("handle waker dropped"), 1);
    });
}

#[test]
fn a_handle_waker_whose_clone_panics_unwinds_the_poll_and_leaves_the_first_waker_stored() {
    capture();
    let rt = runtime(|b| b);
    let h = rt.handle();
    rt.block_on(async {
        let (gate, open) = nuclei::sync::oneshot::channel::<()>();
        let mut task = h.spawn(async move {
            let _ = open.await;
            43
        });
        let probe = Arc::new(Probe {
            clone_panics_on: 2,
            ..Probe::new()
        });
        let waker = probe.waker();
        assert!(poll_once(&mut task, &waker).is_pending());
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            poll_once(&mut task, &waker)
        }))
        .expect_err("the clone panics inside the poll");
        assert_eq!(message(&*unwound), "probe waker cloned");
        let _ = gate.send(());
        wait_for(&probe.woken, "the first waker was never woken");
        assert_eq!(
            poll_once(&mut task, std::task::Waker::noop()),
            std::task::Poll::Ready(43)
        );
        assert_eq!(
            logged_times("probe waker cloned"),
            0,
            "the poller's panic is not logged"
        );
    });
}

#[test]
fn an_output_whose_drop_panics_is_logged_when_its_handle_is_dropped_unawaited() {
    capture();
    let rt = runtime(|b| b);
    let h = rt.handle();
    rt.block_on(async {
        let mut task = h.spawn(async { Bomb("unclaimed output dropped") });
        let probe = Arc::new(Probe::new());
        let waker = probe.waker();
        if poll_once(&mut task, &waker).is_pending() {
            wait_for(&probe.woken, "the task never completed");
        }
        // Completed and not taken: the handle's drop takes the output.
        drop(task);
        let line = logged("unclaimed output dropped");
        assert!(line.contains("panicked as it was dropped"), "{line}");
        assert_eq!(logged_times("unclaimed output dropped"), 1);
    });
}
