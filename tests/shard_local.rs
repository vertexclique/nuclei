//! `shard_local!` values: one per worker of each runtime, made on first use,
//! out of reach off a worker, dropped with the runtime, moving with a shard at
//! a handoff, and keeping the shard where it is while a `with` runs.

use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use nuclei::runtime::{Runtime, worker_index};

const SAFETY: Duration = Duration::from_secs(10);

nuclei::shard_local! {
    static COUNT: Cell<u64> = Cell::new(0);
    static NAMES: RefCell<Vec<String>> = RefCell::new(Vec::new());
    static OTHER: Cell<u8> = Cell::new(9);
}

nuclei::shard_local! { static SOLO: Cell<u8> = Cell::new(3) }

static MADE: AtomicUsize = AtomicUsize::new(0);
nuclei::shard_local! {
    static COUNTED_MAKING: Cell<usize> = Cell::new(MADE.fetch_add(1, Ordering::SeqCst));
}

nuclei::shard_local! {
    static RECURSIVE: Cell<u8> = Cell::new(RECURSIVE.try_with(|c| c.get()).map_or_else(
        |err| if err.is_initializing() { 1 } else { 2 },
        |_| 3,
    ));
}

static ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
nuclei::shard_local! {
    static FLAKY: u32 = {
        if ATTEMPTS.fetch_add(1, Ordering::SeqCst) == 0 {
            panic!("the first making fails");
        }
        7
    };
}

static DROPPED: AtomicUsize = AtomicUsize::new(0);

struct Dropper;

impl Drop for Dropper {
    fn drop(&mut self) {
        DROPPED.fetch_add(1, Ordering::SeqCst);
    }
}

nuclei::shard_local! { static HELD: Dropper = Dropper }

nuclei::shard_local! { static SURVIVOR: Cell<u64> = Cell::new(0) }
nuclei::shard_local! { static PINNED: Cell<u64> = Cell::new(0) }

fn runtime(workers: usize) -> Runtime {
    Runtime::builder()
        .worker_threads(workers)
        .pin_workers(false)
        .build()
        .expect("the runtime starts")
}

fn on() -> Runtime {
    Runtime::builder()
        .worker_threads(1)
        .pin_workers(false)
        .handoff_after(Some(Duration::from_millis(10)))
        .build()
        .expect("the runtime starts")
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

#[test]
fn each_worker_has_a_value_of_its_own_that_its_tasks_share() {
    let rt = runtime(3);
    let h = rt.handle();
    for k in 0..3usize {
        for _ in 0..=k {
            let task = h
                .spawn_on_worker(k, async { COUNT.with(|c| c.set(c.get() + 1)) })
                .unwrap();
            rt.block_on(task);
        }
    }
    for k in 0..3usize {
        let read = h
            .spawn_on_worker(k, async { COUNT.with(Cell::get) })
            .unwrap();
        assert_eq!(rt.block_on(read), k as u64 + 1, "worker {k}");
    }
}

#[test]
fn tasks_of_one_worker_see_each_others_writes_across_awaits() {
    let rt = runtime(2);
    let h = rt.handle();
    let writer = h
        .spawn_on_worker(1, async {
            for n in 0..5 {
                NAMES.with(|names| names.borrow_mut().push(format!("n{n}")));
                nuclei::task::yield_now().await;
            }
        })
        .unwrap();
    let reader = h
        .spawn_on_worker(1, async {
            loop {
                if NAMES.with(|names| names.borrow().len()) == 5 {
                    return NAMES.with(|names| names.borrow().clone());
                }
                nuclei::task::yield_now().await;
            }
        })
        .unwrap();
    rt.block_on(writer);
    assert_eq!(rt.block_on(reader), ["n0", "n1", "n2", "n3", "n4"]);
    let none = h
        .spawn_on_worker(0, async { NAMES.with(|names| names.borrow().len()) })
        .unwrap();
    assert_eq!(rt.block_on(none), 0, "the other worker's value is its own");
}

#[test]
fn separate_statics_hold_separate_values_and_nest() {
    let rt = runtime(1);
    let task = rt.spawn(async {
        COUNT.with(|c| {
            c.set(5);
            OTHER.with(|o| o.set(c.get() as u8 + 1));
            COUNT.with(|again| again.set(again.get() + 1));
        });
        (
            COUNT.with(Cell::get),
            OTHER.with(Cell::get),
            SOLO.with(Cell::get),
        )
    });
    assert_eq!(rt.block_on(task), (6, 6, 3));
}

#[test]
fn the_runtimes_do_not_share_values() {
    let (a, b) = (runtime(1), runtime(1));
    rt_set(&a, 11);
    rt_set(&b, 22);
    assert_eq!(rt_read(&a), 11);
    assert_eq!(rt_read(&b), 22);
}

fn rt_set(rt: &Runtime, value: u64) {
    rt.block_on(rt.spawn(async move { COUNT.with(|c| c.set(value)) }));
}

fn rt_read(rt: &Runtime) -> u64 {
    rt.block_on(rt.spawn(async { COUNT.with(Cell::get) }))
}

#[test]
fn the_value_is_made_once_per_worker() {
    let rt = runtime(2);
    let h = rt.handle();
    let before = MADE.load(Ordering::SeqCst);
    let mut seen = Vec::new();
    for round in 0..4 {
        for k in 0..2 {
            let task = h
                .spawn_on_worker(k, async { COUNTED_MAKING.with(Cell::get) })
                .unwrap();
            let made = rt.block_on(task);
            if round == 0 {
                seen.push(made);
            } else {
                assert_eq!(made, seen[k], "worker {k} made its value again");
            }
        }
    }
    assert_eq!(MADE.load(Ordering::SeqCst) - before, 2);
    assert_ne!(seen[0], seen[1]);
}

#[test]
fn off_a_worker_try_with_answers_an_error_that_is_not_initializing() {
    let rt = runtime(2);
    let expect_off = |result: Result<(), nuclei::runtime::ShardAccessError>, what: &str| {
        let err = result.expect_err(what);
        assert!(!err.is_initializing(), "{what}");
        assert!(err.to_string().contains("no worker"), "{what}: {err}");
    };
    expect_off(COUNT.try_with(|_| ()), "the main thread");
    expect_off(
        rt.block_on(async { COUNT.try_with(|_| ()) }),
        "block_on's own future",
    );
    expect_off(
        rt.block_on(rt.spawn_blocking(|| COUNT.try_with(|_| ()))),
        "the blocking pool",
    );
    expect_off(
        std::thread::spawn(|| COUNT.try_with(|_| ()))
            .join()
            .unwrap(),
        "a plain thread",
    );
    let on_worker = rt.block_on(rt.spawn(async { COUNT.try_with(|_| ()) }));
    assert!(on_worker.is_ok(), "a task of a worker has the value");
}

#[test]
fn a_task_run_by_block_on_itself_has_no_value() {
    let rt = runtime(0);
    let ran_on_worker = rt.block_on(async {
        nuclei::spawn(async { (worker_index(), COUNT.try_with(|_| ()).is_ok()) }).await
    });
    assert_eq!(ran_on_worker, (None, false));
}

#[test]
fn with_panics_off_a_worker_naming_the_reason() {
    let payload = std::panic::catch_unwind(|| COUNT.with(|_| ())).unwrap_err();
    let text = payload
        .downcast_ref::<String>()
        .cloned()
        .unwrap_or_default();
    assert!(text.contains("no worker"), "{text:?}");
}

#[test]
fn an_initializer_that_uses_its_own_value_is_told_so() {
    let rt = runtime(1);
    let task = rt.spawn(async { RECURSIVE.with(Cell::get) });
    assert_eq!(
        rt.block_on(task),
        1,
        "the inner use was not told it was initializing"
    );
}

#[test]
fn an_initializer_that_panics_is_tried_again_at_the_next_use() {
    let rt = runtime(1);
    let first = rt.block_on(rt.spawn(async { FLAKY.with(|v| *v) }).join());
    assert!(first.unwrap_err().is_panic());
    assert_eq!(rt.block_on(rt.spawn(async { FLAKY.with(|v| *v) })), 7);
    assert_eq!(rt.block_on(rt.spawn(async { FLAKY.with(|v| *v) })), 7);
    assert_eq!(ATTEMPTS.load(Ordering::SeqCst), 2);
}

#[test]
fn values_are_dropped_when_the_runtime_is() {
    let rt = runtime(2);
    let h = rt.handle();
    for k in 0..2 {
        rt.block_on(h.spawn_on_worker(k, async { HELD.with(|_| ()) }).unwrap());
    }
    assert_eq!(DROPPED.load(Ordering::SeqCst), 0, "dropped while it ran");
    drop(h);
    drop(rt);
    wait_for("the values' drops", || DROPPED.load(Ordering::SeqCst) == 2);
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(DROPPED.load(Ordering::SeqCst), 2, "dropped more than once");
}

#[test]
fn a_value_survives_a_handoff_of_its_shard() {
    let rt = on();
    let h = rt.handle();
    let first: ThreadId = rt.block_on(
        h.spawn_on_worker(0, async {
            SURVIVOR.with(|v| v.set(41));
            std::thread::current().id()
        })
        .unwrap(),
    );

    let gate = Arc::new(Gate::default());
    let (held, started) = (Arc::clone(&gate), Arc::new(AtomicBool::new(false)));
    let up = Arc::clone(&started);
    let blocked = h
        .spawn_on_worker(0, async move {
            up.store(true, Ordering::SeqCst);
            held.hold();
            std::thread::current().id()
        })
        .unwrap();
    wait_for("the blocking task", || started.load(Ordering::SeqCst));

    let probe = h
        .spawn_on_worker(0, async {
            let seen = SURVIVOR.with(|v| {
                v.set(v.get() + 1);
                v.get()
            });
            (seen, std::thread::current().id())
        })
        .unwrap();
    let (seen, probe_on) = rt.block_on(probe);
    gate.release_after("the task placed on the worker");
    let blocked_on = rt.block_on(blocked);
    assert_eq!(seen, 42, "the shard's value did not go with it");
    assert_eq!(
        blocked_on, first,
        "the worker's first thread was the blocked one"
    );
    assert_ne!(probe_on, blocked_on, "the shard did not move");
    let after = rt.block_on(
        h.spawn_on_worker(0, async { SURVIVOR.with(Cell::get) })
            .unwrap(),
    );
    assert_eq!(after, 42);
}

#[test]
fn a_with_that_blocks_in_a_nested_block_on_keeps_its_shard_and_without_it_the_shard_goes() {
    let rt = on();
    let h = rt.handle();
    let handoffs = || rt.stats().shards[0].handoffs;

    let gate = Arc::new(Gate::default());
    let (held, started) = (Arc::clone(&gate), Arc::new(AtomicBool::new(false)));
    let up = Arc::clone(&started);
    let pinned = h
        .spawn_on_worker(0, async move {
            PINNED.with(|v| {
                v.set(1);
                nuclei::block_on(async {
                    up.store(true, Ordering::SeqCst);
                    held.hold();
                });
            });
        })
        .unwrap();
    wait_for("the pinned block", || started.load(Ordering::SeqCst));
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(handoffs(), 0, "the shard was handed off under a with");
    gate.release_after("the pinned block");
    rt.block_on(pinned);
    assert_eq!(handoffs(), 0);

    let gate = Arc::new(Gate::default());
    let (held, started) = (Arc::clone(&gate), Arc::new(AtomicBool::new(false)));
    let up = Arc::clone(&started);
    let free = h
        .spawn_on_worker(0, async move {
            nuclei::block_on(async {
                up.store(true, Ordering::SeqCst);
                held.hold();
            });
        })
        .unwrap();
    wait_for("the free block", || started.load(Ordering::SeqCst));
    wait_for("the handoff", || handoffs() >= 1);
    gate.release_after("the handoff");
    rt.block_on(free);
    assert!(handoffs() >= 1);
}
