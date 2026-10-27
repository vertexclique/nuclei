use std::cell::RefCell;
use std::rc::Rc;

#[nuclei::main]
async fn main() {
    // Rc is not Send: these tasks stay on the thread that spawned them.
    let log = Rc::new(RefCell::new(Vec::new()));
    let tasks: Vec<_> = (0..3)
        .map(|n| {
            let log = Rc::clone(&log);
            nuclei::spawn_local(async move { log.borrow_mut().push(n) })
        })
        .collect();
    for task in tasks {
        task.await;
    }
    assert_eq!(log.borrow().len(), 3);
}
