//! `shard_local!` values: one per worker of each runtime, made on first use,
//! out of reach off a worker, dropped with the runtime, moving with a shard at
//! a handoff, keeping the shard where it is while a `with` runs, and lent back
//! to a `with` whose poll lost the shard. A poll that stalls past the watch
//! (a preemption, a long loop) may lose its shard before any call in it: every
//! test here passes whether or not that happens.

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
nuclei::shard_local! {
    static LENT: Cell<u64> = Cell::new(0);
    static LENT_INNER: Cell<u64> = Cell::new(0);
    static NESTED: Cell<u64> = Cell::new(0);
    static BOUND: Cell<u64> = Cell::new(0);
    static PANICKY: Cell<u64> = Cell::new(0);
    static CLOSED: Cell<u64> = Cell::new(0);
    static CLOSED_PIN: Cell<u64> = Cell::new(0);
    static LENT_LOOP: Cell<u64> = Cell::new(0);
}

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
        let mut pause = Duration::from_micros(50);
        while !self.open.load(Ordering::SeqCst) {
            if Instant::now() >= deadline {
                self.gave_up.store(true, Ordering::SeqCst);
                return;
            }
            std::thread::sleep(pause);
            pause = (pause * 2).min(Duration::from_millis(1));
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
    rt.block_on(
        h.spawn_on_worker(0, async { SURVIVOR.with(|v| v.set(41)) })
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
    let blocked_on: ThreadId = rt.block_on(blocked);
    assert_eq!(seen, 42, "the shard's value did not go with it");
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
    // A stall before the `with` may have let the shard go, and the `with` then ran on the
    // shard lent back: either way nothing takes it while the block holds the value.
    let pinned_at = handoffs();
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(
        handoffs(),
        pinned_at,
        "the shard was handed off under a with"
    );
    gate.release_after("the pinned block");
    rt.block_on(pinned);

    let free_at = handoffs();
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
    wait_for("the handoff", || handoffs() > free_at);
    gate.release_after("the handoff");
    rt.block_on(free);
}

/// A task on worker 0 of `rt` that says it started, then holds its thread in `gate`'s block.
fn stall_then<F, T>(
    rt: &Runtime,
    gate: &Arc<Gate>,
    then: F,
) -> (nuclei::task::Task<T>, Arc<AtomicBool>)
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let (held, started) = (Arc::clone(gate), Arc::new(AtomicBool::new(false)));
    let up = Arc::clone(&started);
    let task = rt
        .handle()
        .spawn_on_worker(0, async move {
            up.store(true, Ordering::SeqCst);
            held.hold();
            then()
        })
        .unwrap();
    (task, started)
}

/// Waits until `started` and then until worker 0's shard was taken once more.
fn wait_for_the_take(rt: &Runtime, started: &AtomicBool, before: u64) {
    wait_for("the stalled task", || started.load(Ordering::SeqCst));
    wait_for("the take", || rt.stats().shards[0].handoffs > before);
}

#[test]
fn a_with_after_its_poll_lost_the_shard_runs_on_the_shard_lent_back() {
    let rt = on();
    let h = rt.handle();
    rt.block_on(
        h.spawn_on_worker(0, async { LENT.with(|v| v.set(41)) })
            .unwrap(),
    );
    let before = rt.stats().shards[0].handoffs;
    let gate = Arc::new(Gate::default());
    let (stalled, started) = stall_then(&rt, &gate, || {
        // The same poll, after its shard was taken: each call asks for it back.
        let tried = LENT.try_with(|v| {
            v.set(v.get() * 10);
            v.get()
        });
        let nested = LENT.with(|v| {
            LENT_INNER.with(|inner| inner.set(v.get() + 1));
            v.set(v.get() + 1);
            v.get()
        });
        (tried, nested, LENT_INNER.with(Cell::get))
    });
    wait_for_the_take(&rt, &started, before);
    // The shard runs on its new thread meanwhile, with its value.
    let probe = h
        .spawn_on_worker(0, async {
            LENT.with(|v| {
                v.set(v.get() + 1);
                v.get()
            })
        })
        .unwrap();
    assert_eq!(
        rt.block_on(probe),
        42,
        "the value did not go with the shard"
    );
    gate.release_after("the take");
    let (tried, nested, inner) = rt.block_on(stalled);
    assert_eq!(tried, Ok(420), "try_with after the take");
    assert_eq!(nested, 421, "with after the take");
    assert_eq!(inner, 421, "a with nested in the lent call");
    let after = h
        .spawn_on_worker(0, async {
            (LENT.with(Cell::get), LENT_INNER.with(Cell::get))
        })
        .unwrap();
    assert_eq!(
        rt.block_on(after),
        (421, 421),
        "the calls' writes stayed with the shard"
    );
}

#[test]
fn a_with_in_a_nested_block_on_that_lost_the_shard_and_after_it_runs_on_the_shard() {
    let rt = on();
    let h = rt.handle();
    rt.block_on(
        h.spawn_on_worker(0, async { NESTED.with(|v| v.set(1)) })
            .unwrap(),
    );
    let before = rt.stats().shards[0].handoffs;
    let gate = Arc::new(Gate::default());
    let (held, started) = (Arc::clone(&gate), Arc::new(AtomicBool::new(false)));
    let up = Arc::clone(&started);
    let task = h
        .spawn_on_worker(0, async move {
            let inner = nuclei::block_on(async move {
                up.store(true, Ordering::SeqCst);
                held.hold();
                // In the nested poll that lost the shard.
                let lost_in = NESTED.with(|v| {
                    v.set(v.get() * 10);
                    v.get()
                });
                // On the thread's own shard, where the nested future goes on.
                nuclei::task::yield_now().await;
                let moved = NESTED.with(|v| {
                    v.set(v.get() + 1);
                    v.get()
                });
                (lost_in, moved)
            });
            // Back in the outer poll, which lost the shard too.
            let outer = NESTED.with(|v| {
                v.set(v.get() + 1);
                v.get()
            });
            (inner, outer)
        })
        .unwrap();
    wait_for_the_take(&rt, &started, before);
    gate.release_after("the take");
    assert_eq!(rt.block_on(task), ((10, 11), 12));
    let after = h
        .spawn_on_worker(0, async { NESTED.with(Cell::get) })
        .unwrap();
    assert_eq!(rt.block_on(after), 12);
}

#[test]
fn a_block_on_inside_a_with_on_the_lent_shard_runs_on_that_shard() {
    let rt = on();
    let h = rt.handle();
    let before = rt.stats().shards[0].handoffs;
    let gate = Arc::new(Gate::default());
    let (tx, rx) = std::sync::mpsc::channel();
    let (stalled, started) = stall_then(&rt, &gate, move || {
        let got = LENT_LOOP.with(|v| {
            v.set(v.get() + 1);
            // A nested loop on the lent shard: its polls are its own, not the ones that lost it.
            nuclei::block_on(async {
                nuclei::task::yield_now().await;
                LENT_LOOP.with(|inner| {
                    inner.set(inner.get() + 1);
                    inner.get()
                })
            })
        });
        let _ = tx.send(got);
    });
    stalled.detach();
    wait_for_the_take(&rt, &started, before);
    gate.release_after("the take");
    let got = rx
        .recv_timeout(SAFETY)
        .expect("the block_on on the lent shard never ended");
    assert_eq!(got, 2, "the nested future's with");
    let after = h
        .spawn_on_worker(0, async { LENT_LOOP.with(Cell::get) })
        .unwrap();
    assert_eq!(rt.block_on(after), 2);
}

#[test]
fn a_bound_task_finished_after_its_thread_lost_the_shard_runs_its_with_on_the_shard() {
    let rt = on();
    let h = rt.handle();
    rt.block_on(
        h.spawn_on_worker(0, async { BOUND.with(|v| v.set(5)) })
            .unwrap(),
    );
    let before = rt.stats().shards[0].handoffs;
    let gate = Arc::new(Gate::default());
    let (held, started) = (Arc::clone(&gate), Arc::new(AtomicBool::new(false)));
    let up = Arc::clone(&started);
    let (tx, rx) = std::sync::mpsc::channel();
    let task = h
        .spawn_on_worker(0, async move {
            let (go, wait) = nuclei::sync::oneshot::channel::<()>();
            nuclei::spawn_local(async move {
                let _ = wait.await;
                let _ = tx.send(BOUND.try_with(|v| {
                    v.set(v.get() + 1);
                    v.get()
                }));
            })
            .detach();
            up.store(true, Ordering::SeqCst);
            held.hold();
            let _ = go.send(());
        })
        .unwrap();
    wait_for_the_take(&rt, &started, before);
    gate.release_after("the take");
    rt.block_on(task);
    let seen = rx.recv_timeout(SAFETY).expect("the bound task never ran");
    assert_eq!(seen, Ok(6), "the bound task's with");
    let after = h
        .spawn_on_worker(0, async { BOUND.with(Cell::get) })
        .unwrap();
    assert_eq!(rt.block_on(after), 6);
}

#[test]
fn a_panic_in_a_with_on_the_lent_shard_gives_it_back_and_reaches_the_caller() {
    let rt = on();
    let h = rt.handle();
    let before = rt.stats().shards[0].handoffs;
    let gate = Arc::new(Gate::default());
    let (stalled, started) = stall_then(&rt, &gate, || {
        let caught = std::panic::catch_unwind(|| {
            PANICKY.with(|v| {
                v.set(7);
                panic!("in the lent call");
            })
        });
        let text = caught
            .expect_err("the panic did not reach the caller")
            .downcast_ref::<&str>()
            .copied()
            .unwrap_or_default();
        (text, PANICKY.with(Cell::get))
    });
    wait_for_the_take(&rt, &started, before);
    gate.release_after("the take");
    assert_eq!(rt.block_on(stalled), ("in the lent call", 7));
    let after = h
        .spawn_on_worker(0, async { PANICKY.with(|v| v.get() + 1) })
        .unwrap();
    assert_eq!(rt.block_on(after), 8, "the shard was not given back");
}

#[test]
fn a_with_still_waiting_for_its_shard_as_the_runtime_shuts_down_is_told_so() {
    let rt = on();
    let h = rt.handle();
    let before = rt.stats().shards[0].handoffs;
    let gate = Arc::new(Gate::default());
    let (tx, rx) = std::sync::mpsc::channel();
    let (stalled, started) = stall_then(&rt, &gate, move || {
        let _ = tx.send(CLOSED.try_with(|_| ()));
    });
    stalled.detach();
    wait_for_the_take(&rt, &started, before);
    // The shard's new runner holds a value across a block: it lends nothing until that ends.
    let pin = Arc::new(Gate::default());
    let (held, pinned) = (Arc::clone(&pin), Arc::new(AtomicBool::new(false)));
    let up = Arc::clone(&pinned);
    h.spawn_on_worker(0, async move {
        CLOSED_PIN.with(|_| {
            nuclei::block_on(async {
                up.store(true, Ordering::SeqCst);
                held.hold();
            });
        });
    })
    .unwrap()
    .detach();
    wait_for("the pinned block", || pinned.load(Ordering::SeqCst));
    gate.release_after("the take");
    drop(h);
    let dropper = std::thread::spawn(move || drop(rt));
    let told = rx
        .recv_timeout(SAFETY)
        .expect("the waiting with never ended");
    let err = told.expect_err("a with was lent a shard while its runner held a value");
    assert!(!err.is_initializing());
    assert!(err.to_string().contains("shut down"), "{err}");
    pin.release_after("the shutdown");
    dropper.join().expect("the runtime's drop");
}
