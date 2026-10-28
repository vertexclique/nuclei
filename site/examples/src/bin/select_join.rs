use std::time::Duration;

use nuclei::time::sleep;

async fn fetch(name: &'static str, ms: u64) -> &'static str {
    sleep(Duration::from_millis(ms)).await;
    name
}

async fn parse(text: &str) -> Result<u32, std::num::ParseIntError> {
    text.parse()
}

#[nuclei::main]
async fn main() {
    // join! runs both at once in this task: about 30 ms, not 50.
    let (user, orders) = nuclei::join!(fetch("user", 30), fetch("orders", 20));
    assert_eq!((user, orders), ("user", "orders"));

    // try_join! gives every output, or the first error.
    let both = nuclei::try_join!(parse("4"), parse("2"));
    assert_eq!(both, Ok((4, 2)));
    assert!(nuclei::try_join!(parse("4"), parse("x")).is_err());
}
