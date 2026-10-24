//! The executor API kept from earlier versions, on the default runtime:
//! every call here shares one process, so each test asks only what holds
//! whatever ran before it.

use std::time::Duration;

use nuclei::{
    block_on, spawn, spawn_blocking, spawn_local, spawn_more_threads, stop_current_thread,
    stop_thread,
};

#[test]
fn init_starts_the_default_runtime_and_the_free_functions_run_on_it() {
    nuclei::init();
    nuclei::init();
    let task = spawn(async { 21 * 2 });
    assert_eq!(block_on(task), 42);
    assert_eq!(block_on(spawn_blocking(|| 7)), 7);
    let local = block_on(async { spawn_local(async { std::rc::Rc::new(5) }).await });
    assert_eq!(*local, 5);
}

#[test]
fn workers_grow_within_the_maximum_and_stop_down_to_the_minimum() {
    nuclei::init();
    // The default runtime starts one worker per core and may run four
    // times as many.
    let added = block_on(spawn_more_threads(2)).unwrap();
    assert!(added <= 2);
    let stopped = block_on(stop_thread());
    assert_eq!(stopped, added > 0, "a stop takes back only what was added");
    // A task stopping its own worker: allowed only above the minimum.
    let on_worker = block_on(spawn(async { stop_current_thread().await }));
    let _ = on_worker;
    // Off a worker there is no worker to stop.
    assert!(!block_on(stop_current_thread()));
    // Tasks still run.
    assert_eq!(
        block_on(spawn(async {
            nuclei::sleep(Duration::from_millis(1)).await;
            1
        })),
        1
    );
}
