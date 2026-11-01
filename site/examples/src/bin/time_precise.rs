use std::time::Duration;

use nuclei::time::{self, Instant};

#[nuclei::main]
async fn main() {
    // To the microsecond, where sleep would round up to the next millisecond.
    let start = Instant::now();
    time::sleep_precise(Duration::from_micros(250)).await;
    assert!(start.elapsed() >= Duration::from_micros(250));

    // A pacer: ten steps 100 microseconds apart, each deadline counted from
    // the last one, so a late wake does not push the rest back.
    let begin = Instant::now();
    let mut next = begin;
    for _ in 0..10 {
        next += Duration::from_micros(100);
        time::sleep_until_precise(next).await;
    }
    assert!(begin.elapsed() >= Duration::from_millis(1));
}
