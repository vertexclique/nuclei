use nuclei::sync::{RwLock, RwLockUpgradableReadGuard};

#[nuclei::main]
async fn main() {
    let config = RwLock::new(String::from("v1"));

    // Readers share the lock.
    let a = config.read().await;
    let b = config.read().await;
    assert_eq!((a.as_str(), b.as_str()), ("v1", "v1"));
    drop((a, b));

    // An upgradable read sits beside plain readers, and becomes a write
    // without letting another writer in between.
    let check = config.upgradable_read().await;
    if check.as_str() == "v1" {
        let mut write = RwLockUpgradableReadGuard::upgrade(check).await;
        write.push_str("+patch");
    }
    assert_eq!(*config.read().await, "v1+patch");
}
