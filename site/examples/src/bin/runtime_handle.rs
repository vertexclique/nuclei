use nuclei::runtime::{Runtime, RuntimeHandle};

fn main() -> std::io::Result<()> {
    let runtime = Runtime::new()?;
    let handle = runtime.handle();

    // A plain thread hands work to the runtime through its handle.
    let thread = std::thread::spawn(move || handle.block_on(handle.spawn(async { 2 + 2 })));
    assert_eq!(thread.join().unwrap(), 4);

    // Outside block_on this thread is inside no runtime...
    assert!(RuntimeHandle::try_current().is_err());
    {
        // ...until enter sets one: the free functions use it while the guard lives.
        let _guard = runtime.enter();
        assert_eq!(RuntimeHandle::try_current().unwrap().id(), runtime.id());
        let task = nuclei::spawn(async { "on the entered runtime" });
        assert_eq!(runtime.block_on(task), "on the entered runtime");
    }
    Ok(())
}
