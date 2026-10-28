use std::time::Duration;

use nuclei::time::sleep;

#[nuclei::main]
async fn main() {
    nuclei::select! {
        _ = nuclei::signal::ctrl_c() => println!("stopping on Ctrl-C"),
        _ = sleep(Duration::from_millis(100)) => println!("the work finished first"),
    }
}
