use std::time::Duration;

use nuclei::sync::CancellationToken;
use nuclei::time::sleep;

#[nuclei::main]
async fn main() {
    let shutdown = CancellationToken::new();

    // A child token is cancelled with its parent, and alone too.
    let worker_token = shutdown.child_token();
    let worker = nuclei::spawn(async move {
        // The work, or None once the token is cancelled first.
        worker_token
            .run_until_cancelled(sleep(Duration::from_secs(60)))
            .await
    });

    shutdown.cancel();
    assert_eq!(worker.await, None);
    assert!(shutdown.is_cancelled());

    // A drop guard cancels its token when it drops, on every way out of a scope.
    let token = CancellationToken::new();
    {
        let _guard = token.clone().drop_guard();
    }
    token.cancelled().await;
}
