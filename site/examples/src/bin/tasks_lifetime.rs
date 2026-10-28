use std::time::Duration;

use nuclei::time::sleep;

#[nuclei::main]
async fn main() {
    // Dropping a Task cancels it: this one never prints.
    let dropped = nuclei::spawn(async {
        sleep(Duration::from_millis(50)).await;
        println!("never printed");
    });
    drop(dropped);

    // detach lets a task run to its end with no handle.
    nuclei::spawn(async { println!("detached, and it runs") }).detach();

    // cancel stops a task, and gives its output back if it had finished.
    let slow = nuclei::spawn(async {
        sleep(Duration::from_secs(10)).await;
        1
    });
    assert_eq!(slow.cancel().await, None);

    // abort cancels without waiting; an AbortHandle does it from anywhere.
    let stuck = nuclei::spawn(std::future::pending::<()>());
    let abort = stuck.abort_handle();
    abort.abort();
    assert!(stuck.join().await.unwrap_err().is_cancelled());

    sleep(Duration::from_millis(100)).await;
}
