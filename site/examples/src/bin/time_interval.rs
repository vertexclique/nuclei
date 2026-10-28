use std::time::Duration;

use nuclei::time::{self, MissedTickBehavior};

#[nuclei::main]
async fn main() {
    let mut every = time::interval(Duration::from_millis(10));
    // When polled late, skip the ticks that were missed instead of bursting them.
    every.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let first = every.tick().await; // the first tick is at once
    for _ in 0..3 {
        every.tick().await;
    }
    assert!(first.elapsed() >= Duration::from_millis(30));

    // reset: the next tick is one period from now.
    every.reset();
    every.tick().await;
    assert_eq!(every.period(), Duration::from_millis(10));
}
