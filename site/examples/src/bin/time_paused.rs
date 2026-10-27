use std::time::Duration;

use nuclei::time::{self, Instant};

#[nuclei::main(start_paused = true)]
async fn main() {
    let start = Instant::now();

    // Paused, the clock jumps to the next timer once every task waits:
    // an hour's sleep ends at once, and the clock says an hour passed.
    time::sleep(Duration::from_secs(3600)).await;
    assert_eq!(start.elapsed(), Duration::from_secs(3600));

    // advance moves the clock by hand, firing the timers it passes.
    let timer = nuclei::spawn(time::sleep(Duration::from_secs(10)));
    time::advance(Duration::from_secs(10)).await;
    timer.await;
    assert_eq!(start.elapsed(), Duration::from_secs(3610));

    // resume lets it run with the system's clock again.
    time::resume();
    time::sleep(Duration::from_millis(1)).await;
    assert!(start.elapsed() > Duration::from_secs(3610));
}
