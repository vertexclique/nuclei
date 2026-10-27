use nuclei::{Builder, IoBackend, Proactor};

fn main() -> std::io::Result<()> {
    // What the process picked: NUCLEI_BACKEND if set, else io_uring where it may, else the poller.
    println!("default: {:?}", Proactor::backend());

    // One runtime on the platform's poller, whatever the default is.
    let poller = if cfg!(target_os = "linux") {
        IoBackend::Epoll
    } else if cfg!(windows) {
        IoBackend::Iocp
    } else {
        IoBackend::Kqueue
    };
    let runtime = Builder::new().worker_threads(1).backend(poller).build()?;
    let used = runtime.block_on(runtime.spawn(async { Proactor::backend() }));
    // It runs the poller, unless NUCLEI_BACKEND names another: the variable wins.
    if std::env::var_os("NUCLEI_BACKEND").is_none_or(|v| v.is_empty()) {
        assert_eq!(used, poller);
    }
    Ok(())
}
