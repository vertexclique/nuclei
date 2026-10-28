//! Console control events as streams: [`ctrl_c`], [`ctrl_break`],
//! [`ctrl_close`], [`ctrl_logoff`] and [`ctrl_shutdown`].
//!
//! The first listener installs the process's console control handler,
//! which stays for good. The system runs it on a thread of its own, so it
//! counts the event and dispatches itself. An event with a listener alive
//! is taken (the handler says so); one with none goes on to the next
//! handler, which for Ctrl-C by default ends the process. For the close,
//! logoff and shutdown events the system ends the process once the
//! handlers return, whatever they say.

use core::pin::Pin;
use core::task::{Context, Poll};
use std::io;
use std::sync::OnceLock;

use super::Listener;
use crate::syscore::windows::{BOOL, SetConsoleCtrlHandler};

const CTRL_C_EVENT: u32 = 0;
const CTRL_BREAK_EVENT: u32 = 1;
const CTRL_CLOSE_EVENT: u32 = 2;
const CTRL_LOGOFF_EVENT: u32 = 5;
const CTRL_SHUTDOWN_EVENT: u32 = 6;

/// The deliveries of one console control event, as a stream: each item
/// stands for one event or more since the item before.
#[derive(Debug)]
pub struct CtrlEvent {
    listener: Listener,
}

impl CtrlEvent {
    /// Waits for the next event. Always `Some`: the stream never ends.
    pub async fn recv(&mut self) -> Option<()> {
        core::future::poll_fn(|cx| self.poll_recv(cx)).await
    }

    /// Ready with `Some` once the event came since the last item.
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<()>> {
        self.listener.poll_recv(cx, || {})
    }
}

impl futures_core::Stream for CtrlEvent {
    type Item = ();

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<()>> {
        self.get_mut().poll_recv(cx)
    }
}

/// Listens to Ctrl-C (`CTRL_C_EVENT`).
///
/// # Errors
///
/// When the console control handler cannot be installed.
pub fn ctrl_c() -> io::Result<CtrlEvent> {
    listen(CTRL_C_EVENT)
}

/// Listens to Ctrl-Break (`CTRL_BREAK_EVENT`).
///
/// # Errors
///
/// As for [`ctrl_c`].
pub fn ctrl_break() -> io::Result<CtrlEvent> {
    listen(CTRL_BREAK_EVENT)
}

/// Listens to the console's closing (`CTRL_CLOSE_EVENT`).
///
/// # Errors
///
/// As for [`ctrl_c`].
pub fn ctrl_close() -> io::Result<CtrlEvent> {
    listen(CTRL_CLOSE_EVENT)
}

/// Listens to the user's logging off (`CTRL_LOGOFF_EVENT`; services only).
///
/// # Errors
///
/// As for [`ctrl_c`].
pub fn ctrl_logoff() -> io::Result<CtrlEvent> {
    listen(CTRL_LOGOFF_EVENT)
}

/// Listens to the system's shutting down (`CTRL_SHUTDOWN_EVENT`; services
/// only).
///
/// # Errors
///
/// As for [`ctrl_c`].
pub fn ctrl_shutdown() -> io::Result<CtrlEvent> {
    listen(CTRL_SHUTDOWN_EVENT)
}

fn listen(event: u32) -> io::Result<CtrlEvent> {
    install()?;
    Ok(CtrlEvent {
        listener: Listener::new(event as usize),
    })
}

/// Installs the console control handler, once.
fn install() -> io::Result<()> {
    static INSTALLED: OnceLock<Result<(), i32>> = OnceLock::new();
    INSTALLED
        .get_or_init(|| {
            // SAFETY: the handler is a function that lives for the process.
            if unsafe { SetConsoleCtrlHandler(Some(on_event), 1) } != 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error().raw_os_error().unwrap_or(0))
            }
        })
        .map_err(io::Error::from_raw_os_error)
}

/// The console control handler: takes an event that has a listener.
unsafe extern "system" fn on_event(event: u32) -> BOOL {
    let signum = event as usize;
    if !super::listened(signum) {
        return 0;
    }
    super::delivered(signum);
    super::dispatch();
    1
}

#[cfg(test)]
pub(super) fn deliver_for_test(event: u32) -> bool {
    // SAFETY: the handler takes any event number.
    unsafe { on_event(event) != 0 }
}
