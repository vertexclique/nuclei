use std::task::{Context, Poll};
use std::time::Duration;
use std::{future::Future, io, pin::Pin};

use lever::prelude::*;
use once_cell::sync::Lazy;

use super::syscore::*;
use super::waker::*;
use crate::spawn_blocking;

#[cfg(target_os = "windows")]
use crate::syscore::iocp::SysProactor;

pub use super::handle::*;

///
/// Concrete proactor instance
pub struct Proactor(SysProactor);

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
}

///
/// IO driver that drives event systems
pub fn drive<T>(future: impl Future<Output = T>) -> T {
    let p = Proactor::get();
    let waker = waker_fn(move || {
        p.wake();
    });

    let cx = &mut Context::from_waker(&waker);
    futures_util::pin_mut!(future);

    let driver = spawn_blocking(move || loop {
        let _ = p.wait(1, None);
    });

    futures_util::pin_mut!(driver);

    loop {
        if let Poll::Ready(val) = future.as_mut().poll(cx) {
            return val;
        }

        cx.waker().wake_by_ref();

        let duration = Duration::from_millis(1);
        driver.as_mut().poll(cx);
    }
}
