//! A maximum of zero threads on a process of its own: every task runs on the
//! thread inside `block_on`.

use nuclei::{GlobalExecutorConfig, block_on, spawn};

#[test]
fn max_threads_zero_runs_every_task_on_the_block_on_thread() {
    nuclei::init_with_config(GlobalExecutorConfig::default().with_max_threads(0));
    let me = std::thread::current().id();
    let ran_on = block_on(async {
        let a = spawn(async { std::thread::current().id() });
        let b = spawn(async {
            nuclei::sleep(std::time::Duration::from_millis(1)).await;
            std::thread::current().id()
        });
        (a.await, b.await)
    });
    assert_eq!(ran_on, (me, me));
}
