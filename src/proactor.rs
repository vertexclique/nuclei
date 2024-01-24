use std::task::{Context, Poll};
use std::time::Duration;
use std::{future::Future, io};

use once_cell::sync::Lazy;

use super::syscore::*;
use super::waker::*;
use crate::spawn_blocking;
use crate::sys::IoBackend;

pub use super::handle::*;

///
/// Concrete proactor instance
pub struct Proactor(SysProactor);
unsafe impl Send for Proactor {}
unsafe impl Sync for Proactor {}

impl Proactor {
    /// Returns a reference to the proactor.
    pub fn get() -> &'static Proactor {
        static PROACTOR: Lazy<Proactor> =
            Lazy::new(|| Proactor(SysProactor::new().expect("cannot initialize poll backend")));

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

    /// Get the IO backend that is used with Nuclei's proactor.
    pub(crate) fn backend() -> IoBackend {
        BACKEND
    }
}

///
/// IO driver that drives event systems
pub fn drive<T>(future: impl Future<Output = T>) -> T {
    let p = Proactor::get();
    let waker = waker_fn(move || {
        p.wake();
    });

    let cx = &mut Context::from_waker(&waker);
    futures::pin_mut!(future);

    let driver = spawn_blocking(move || loop {
        let _ = p.wait(1, None);
    });

    futures::pin_mut!(driver);

    loop {
        if let Poll::Ready(val) = future.as_mut().poll(cx) {
            return val;
        }

        cx.waker().wake_by_ref();

        // TODO: (vcq): we don't need this.
        // let _duration = Duration::from_millis(1);
        let _ = driver.as_mut().poll(cx);
    }
}
