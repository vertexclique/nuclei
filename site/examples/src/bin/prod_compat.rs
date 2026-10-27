use nuclei::GlobalExecutorConfig;

fn main() {
    // The default runtime, set up the way earlier versions of nuclei did it.
    nuclei::init_with_config(
        GlobalExecutorConfig::default()
            .with_env_var("APP_WORKERS") // read the worker count from here first
            .with_min_threads(2)
            .with_max_threads(4),
    );
    nuclei::block_on(async {
        let started = nuclei::spawn_more_threads(2).await.expect("workers start");
        println!("started {started} more workers");
        let stopped = nuclei::stop_thread().await;
        println!("stopped one: {stopped}");
    });
}
