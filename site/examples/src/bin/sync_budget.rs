use std::cell::Cell;
use std::rc::Rc;

use nuclei::sync::mpsc;

#[nuclei::main]
async fn main() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    for value in 0..1000u32 {
        tx.send(value).unwrap();
    }
    drop(tx);

    let ran = Rc::new(Cell::new(false));
    let other = nuclei::spawn_local({
        let ran = Rc::clone(&ran);
        async move { ran.set(true) }
    });

    // Every receive is ready at once, yet the loop yields when its budget is
    // spent, so the other task runs while values are still waiting.
    let (mut received, mut ran_after) = (0, None);
    while rx.recv().await.is_some() {
        received += 1;
        if ran.get() && ran_after.is_none() {
            ran_after = Some(received);
        }
    }
    assert!(ran_after.is_some_and(|n| n < 1000));
    other.await;
}
