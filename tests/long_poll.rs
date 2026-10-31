//! `Builder::on_long_poll` and the names tasks carry: a poll that runs past
//! `handoff_after` is reported once with its task's id and name, short polls
//! are not, a hook that panics stops nothing, `on_long_poll` needs a handoff,
//! and the task hooks see the name `task::Builder` gave.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nuclei::runtime::{Runtime, RuntimeHandle};
use nuclei::task::{Builder as TaskBuilder, Id};

const SAFETY: Duration = Duration::from_secs(10);
/// Far above any short poll of a test, so only a deliberate block passes it.
const THRESHOLD: Duration = Duration::from_millis(100);

type Seen = Arc<Mutex<Vec<(Id, Option<String>)>>>;

fn seen() -> Seen {
    Arc::default()
}

fn runtime(
    handoff: Duration,
    configure: impl FnOnce(nuclei::Builder) -> nuclei::Builder,
) -> Runtime {
    configure(
        Runtime::builder()
            .worker_threads(1)
            .pin_workers(false)
            .handoff_after(Some(handoff)),
    )
    .build()
    .expect("the runtime starts")
}

fn reporting(into: &Seen) -> Runtime {
    let into = Arc::clone(into);
    runtime(THRESHOLD, move |builder| {
        builder.on_long_poll(move |meta| {
            into.lock()
                .unwrap()
                .push((meta.id(), meta.name().map(str::to_owned)));
        })
    })
}

#[derive(Default)]
struct Gate {
    open: AtomicBool,
    gave_up: AtomicBool,
}

impl Gate {
    fn hold(&self) {
        let deadline = Instant::now() + SAFETY;
        while !self.open.load(Ordering::SeqCst) {
            if Instant::now() >= deadline {
                self.gave_up.store(true, Ordering::SeqCst);
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn release_after(&self, what: &str) {
        assert!(
            !self.gave_up.load(Ordering::SeqCst),
            "{what} waited out the block"
        );
        self.open.store(true, Ordering::SeqCst);
    }
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + SAFETY;
    let mut pause = Duration::from_micros(50);
    while !done() {
        assert!(Instant::now() < deadline, "{what} never happened");
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(20));
    }
}

/// A task, named if asked, that holds its thread at the gate in one poll.
fn blocker(handle: &RuntimeHandle, name: Option<&str>) -> (nuclei::Task<()>, Arc<Gate>) {
    let gate = Arc::new(Gate::default());
    let held = Arc::clone(&gate);
    let started = Arc::new(AtomicBool::new(false));
    let up = Arc::clone(&started);
    let builder = name.map_or_else(TaskBuilder::new, |name| TaskBuilder::new().name(name));
    let task = builder
        .spawn_on(
            async move {
                up.store(true, Ordering::SeqCst);
                held.hold();
            },
            handle,
        )
        .unwrap();
    wait_for("the blocking task", || started.load(Ordering::SeqCst));
    (task, gate)
}

fn reports(into: &Seen) -> Vec<(Id, Option<String>)> {
    into.lock().unwrap().clone()
}

#[test]
fn a_named_task_blocking_its_worker_is_reported_once_with_its_id_and_name() {
    let into = seen();
    let rt = reporting(&into);
    let h = rt.handle();

    let shorts: Vec<_> = (0..50)
        .map(|_| {
            rt.spawn(async {
                for _ in 0..5 {
                    nuclei::task::yield_now().await;
                }
            })
        })
        .collect();
    for task in shorts {
        rt.block_on(task);
    }
    assert!(reports(&into).is_empty(), "a short poll was reported");

    let (task, gate) = blocker(&h, Some("slow"));
    let id = task.id();
    wait_for("the report", || !reports(&into).is_empty());
    // Several handoff thresholds more in the same poll: still one report.
    std::thread::sleep(THRESHOLD * 4);
    gate.release_after("the report");
    rt.block_on(task);
    assert_eq!(reports(&into), [(id, Some("slow".to_owned()))]);
}

#[test]
fn an_unnamed_task_is_reported_without_a_name() {
    let into = seen();
    let rt = reporting(&into);
    let (task, gate) = blocker(&rt.handle(), None);
    let id = task.id();
    wait_for("the report", || !reports(&into).is_empty());
    gate.release_after("the report");
    rt.block_on(task);
    assert_eq!(reports(&into), [(id, None)]);
}

#[test]
fn each_blocked_poll_is_reported_on_its_own() {
    let into = seen();
    let rt = reporting(&into);
    let h = rt.handle();
    let mut ids = Vec::new();
    for name in ["first", "second", "third"] {
        let (task, gate) = blocker(&h, Some(name));
        ids.push((task.id(), Some(name.to_owned())));
        wait_for("the report", || reports(&into).len() == ids.len());
        gate.release_after("the report");
        rt.block_on(task);
    }
    assert_eq!(reports(&into), ids);
}

#[test]
fn a_hook_that_panics_does_not_stop_later_reports() {
    let ids = Arc::new(Mutex::new(Vec::new()));
    let rec = Arc::clone(&ids);
    let rt = runtime(THRESHOLD, move |builder| {
        builder.on_long_poll(move |meta| {
            rec.lock().unwrap().push(meta.id());
            panic!("the hook fails");
        })
    });
    let h = rt.handle();
    let mut expected = Vec::new();
    for _ in 0..2 {
        let (task, gate) = blocker(&h, Some("hooked"));
        expected.push(task.id());
        wait_for("the report", || ids.lock().unwrap().len() == expected.len());
        gate.release_after("the report");
        rt.block_on(task);
    }
    assert_eq!(*ids.lock().unwrap(), expected);
    assert_eq!(
        rt.block_on(rt.spawn(async { 5 })),
        5,
        "the runtime still runs"
    );
}

#[test]
fn a_long_poll_hook_needs_a_handoff_to_report_for() {
    let err = Runtime::builder()
        .worker_threads(1)
        .handoff_after(None)
        .on_long_poll(|_| {})
        .build()
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{err}");
    assert!(err.to_string().contains("on_long_poll"), "{err}");

    let default_handoff = Runtime::builder()
        .worker_threads(1)
        .pin_workers(false)
        .on_long_poll(|_| {})
        .build()
        .expect("the default handoff takes a long poll hook");
    assert_eq!(default_handoff.block_on(async { 1 }), 1);
}

#[test]
fn the_task_hooks_see_the_name_the_builder_gave() {
    let (spawned, polled) = (seen(), seen());
    let (on_spawn, on_poll) = (Arc::clone(&spawned), Arc::clone(&polled));
    let rt = Runtime::builder()
        .worker_threads(1)
        .pin_workers(false)
        .on_task_spawn(move |meta| {
            on_spawn
                .lock()
                .unwrap()
                .push((meta.id(), meta.name().map(str::to_owned)));
        })
        .on_before_task_poll(move |meta| {
            on_poll
                .lock()
                .unwrap()
                .push((meta.id(), meta.name().map(str::to_owned)));
        })
        .build()
        .unwrap();
    let h = rt.handle();

    let named = TaskBuilder::new()
        .name("alpha")
        .spawn_on(
            async {
                nuclei::task::yield_now().await;
            },
            &h,
        )
        .unwrap();
    let unnamed = TaskBuilder::new().spawn_on(async {}, &h).unwrap();
    let plain = rt.spawn(async {});
    let (named_id, unnamed_id, plain_id) = (named.id(), unnamed.id(), plain.id());
    rt.block_on(async {
        named.await;
        unnamed.await;
        plain.await;
    });

    for (hook, into) in [("spawn", &spawned), ("poll", &polled)] {
        let all = reports(into);
        let of = |id: Id| -> Vec<Option<String>> {
            all.iter()
                .filter(|(got, _)| *got == id)
                .map(|(_, name)| name.clone())
                .collect()
        };
        let alpha = of(named_id);
        assert!(
            !alpha.is_empty(),
            "the {hook} hook never saw the named task"
        );
        assert!(
            alpha.iter().all(|n| n.as_deref() == Some("alpha")),
            "{hook}: {alpha:?}"
        );
        for id in [unnamed_id, plain_id] {
            let names = of(id);
            assert!(!names.is_empty(), "the {hook} hook never saw task {id}");
            assert!(names.iter().all(Option::is_none), "{hook}: {names:?}");
        }
    }
    assert!(
        reports(&polled)
            .iter()
            .filter(|(id, _)| *id == named_id)
            .count()
            >= 2
    );
}

#[test]
fn the_poll_hooks_and_latencies_go_on_working_beside_a_long_poll_hook() {
    let (before, after) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (b, a) = (Arc::clone(&before), Arc::clone(&after));
    let into = seen();
    let report = Arc::clone(&into);
    let rt = runtime(THRESHOLD, move |builder| {
        builder
            .record_latencies(true)
            .on_before_task_poll(move |_| {
                b.fetch_add(1, Ordering::SeqCst);
            })
            .on_after_task_poll(move |_| {
                a.fetch_add(1, Ordering::SeqCst);
            })
            .on_long_poll(move |meta| {
                report
                    .lock()
                    .unwrap()
                    .push((meta.id(), meta.name().map(str::to_owned)));
            })
    });
    let task = rt.spawn(async {
        for _ in 0..10 {
            nuclei::task::yield_now().await;
        }
    });
    rt.block_on(task);
    wait_for("the poll hooks", || after.load(Ordering::SeqCst) >= 11);
    let ended = after.load(Ordering::SeqCst);
    assert!(before.load(Ordering::SeqCst) >= ended);

    let (task, gate) = blocker(&rt.handle(), Some("beside"));
    let id = task.id();
    wait_for("the report", || !reports(&into).is_empty());
    gate.release_after("the report");
    rt.block_on(task);
    assert_eq!(reports(&into), [(id, Some("beside".to_owned()))]);
}
