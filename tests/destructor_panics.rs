//! User code that panics where nobody polls it: a task's future as it is
//! dropped, a task hook. The panic goes to the task's handle where one
//! awaits it and is logged at error where none does; the runtime and every
//! other task go on.

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
