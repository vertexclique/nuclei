use std::time::Duration;

use nuclei::time::{self, Instant};

#[nuclei::main]
async fn main() {
    let start = Instant::now();
    time::sleep(Duration::from_millis(20)).await;
    assert!(start.elapsed() >= Duration::from_millis(20));

    // A future that takes too long gives Elapsed.
    let slow = time::sleep(Duration::from_secs(5));
    assert!(
        time::timeout(Duration::from_millis(10), slow)
            .await
            .is_err()
    );

    // One that is ready in time gives its output.
    let quick = async { "in time" };
    assert_eq!(
        time::timeout(Duration::from_secs(1), quick).await,
        Ok("in time")
    );

    // The same against a deadline.
    let deadline = Instant::now() + Duration::from_millis(10);
    let never = std::future::pending::<()>();
    assert!(time::timeout_at(deadline, never).await.is_err());
}
