// The runtime's settings, from a function of the program's own.
fn runtime() -> nuclei::Builder {
    nuclei::Builder::new()
        .worker_threads(2)
        .thread_name("server")
}

#[nuclei::main(builder = "runtime")]
async fn main() {
    let thread = nuclei::spawn(async { std::thread::current().name().map(str::to_owned) });
    assert_eq!(thread.await.as_deref(), Some("server"));
}
