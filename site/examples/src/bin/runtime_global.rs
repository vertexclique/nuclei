fn main() -> std::io::Result<()> {
    // Configure the default runtime before anything starts it.
    nuclei::Builder::new().worker_threads(2).build_global()?;

    // From here on the free functions use it.
    let task = nuclei::spawn(async { "on the default runtime" });
    assert_eq!(nuclei::block_on(task), "on the default runtime");
    Ok(())
}
