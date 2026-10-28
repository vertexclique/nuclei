nuclei::task_local! {
    static REQUEST: u64;
}

async fn handle() -> String {
    // Any code the task runs reads the value its scope set.
    format!("request {}", REQUEST.get())
}

#[nuclei::main]
async fn main() {
    let first = nuclei::spawn(REQUEST.scope(1, handle()));
    let second = nuclei::spawn(REQUEST.scope(2, handle()));
    assert_eq!(first.await, "request 1");
    assert_eq!(second.await, "request 2");

    // Read where no scope set it, try_with answers an error instead of panicking.
    assert!(REQUEST.try_with(|id| *id).is_err());
}
