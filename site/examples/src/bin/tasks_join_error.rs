#[nuclei::main]
async fn main() {
    // join gives the output, or a JoinError that says why there is none.
    let ok = nuclei::spawn(async { 7 }).join().await;
    assert_eq!(ok.unwrap(), 7);

    let failed = nuclei::spawn(async {
        if true {
            panic!("the task gave up");
        }
    })
    .join()
    .await;
    let error = failed.unwrap_err();
    assert!(error.is_panic());
    let message = error.into_panic();
    assert_eq!(message.downcast_ref::<&str>(), Some(&"the task gave up"));

    // fallible: None where awaiting the Task would panic.
    let done = nuclei::spawn(async { 1 }).fallible();
    assert_eq!(done.await, Some(1));
}
