//! Short names for the platform configurations the sources are written
//! against, so a gate says what it is about:
//!
//! - `ring`: the io_uring driver is built (Linux or Android with the
//!   `iouring` feature);
//! - `port`: the I/O completion port driver (Windows);
//! - `completion`: either of the two, the drivers whose IO is operations
//!   completed by the kernel;
//! - `readiness`: a readiness poller the driver makes calls on (epoll,
//!   kqueue: every unix);
//! - `precise_timer`: a kqueue whose `EVFILT_TIMER` counts nanoseconds
//!   (macOS, iOS, FreeBSD, NetBSD), which ends a wait at a precise deadline.

fn main() {
    for name in ["ring", "port", "completion", "readiness", "precise_timer"] {
        println!("cargo::rustc-check-cfg=cfg({name})");
    }
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let unix = std::env::var("CARGO_CFG_UNIX").is_ok();
    let iouring = std::env::var("CARGO_FEATURE_IOURING").is_ok();
    let ring = iouring && (os == "linux" || os == "android");
    let port = os == "windows";
    let set = |name: &str, on: bool| {
        if on {
            println!("cargo::rustc-cfg={name}");
        }
    };
    set("ring", ring);
    set("port", port);
    set("completion", ring || port);
    set("readiness", unix);
    set(
        "precise_timer",
        matches!(os.as_str(), "macos" | "ios" | "freebsd" | "netbsd"),
    );
}
