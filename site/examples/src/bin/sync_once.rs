use std::time::Duration;

use nuclei::sync::{Lazy, OnceCell, SetOnce};

static TABLE: Lazy<Vec<u32>> = Lazy::new(|| (0..10).map(|n| n * n).collect());
static SETTINGS: OnceCell<String> = OnceCell::const_new();

async fn load_settings() -> String {
    nuclei::time::sleep(Duration::from_millis(5)).await;
    "port=7000".to_string()
}

#[nuclei::main]
async fn main() {
    // Lazy: computed on first use, the same value after.
    assert_eq!(TABLE[3], 9);

    // OnceCell: the first caller runs the async initializer; others wait for it.
    let (a, b) = nuclei::join!(
        SETTINGS.get_or_init(load_settings),
        SETTINGS.get_or_init(load_settings),
    );
    assert!(std::ptr::eq(a, b));

    // SetOnce: set once from anywhere; wait until someone has.
    let shutdown_reason = SetOnce::new();
    shutdown_reason.set("maintenance").unwrap();
    assert_eq!(*shutdown_reason.wait().await, "maintenance");
    assert!(shutdown_reason.set("again").is_err());
}
