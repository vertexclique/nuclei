use std::time::Duration;
use std::task::{Poll, Context};
use std::{pin::Pin, future::Future, io};

use lever::prelude::*;
use once_cell::sync::Lazy;

use super::syscore::*;
use super::waker::*;

pub use super::handle::*;

///
/// Concrete proactor instance
pub struct Proactor(SysProactor);

impl Proactor {
    /// Returns a reference to the proactor.
    pub fn get() -> &'static Proactor {
        static PROACTOR: Lazy<Proactor> = Lazy::new(|| Proactor(
            SysProactor::new().expect("cannot initialize poll backend")
        ));

        &PROACTOR
    }

    /// Wakes the thread waiting on proactor.
    pub fn wake(&self) {
        self.0.wake().expect("failed to wake thread");
    }

    /// Wait for completion of IO object
    pub fn wait(&self, max_event_size: usize, duration: Option<Duration>) -> io::Result<usize> {
        self.0.wait(max_event_size, duration)
    }

    /// Get underlying proactor instance.
    pub(crate) fn inner(&self) -> &SysProactor {
        &self.0
    }
}


pub fn run<T>(future: impl Future<Output = T>) -> T {
    let p = Proactor::get();
    let waker = waker_fn(move || {
        // let _ = p.wait(1, None);
        p.wake();
    });
    let cx = &mut Context::from_waker(&waker);
    futures_util::pin_mut!(future);

    loop {
        if let Poll::Ready(val) = future.as_mut().poll(cx) {
            return val;
        }
        // p.wake();

        cx.waker().wake_by_ref();

        let duration = Some(Duration::from_millis(100));
        // std::thread::sleep(duration.unwrap());
        // let a = p.wait(1, None).unwrap();
        let a = p.wait(1, None).unwrap();
        // dbg!(a);
        // dbg!("AFTER WAIT");
    }
}