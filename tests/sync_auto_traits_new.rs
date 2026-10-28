//! Auto traits of the one-shot channel, the broadcast channel, the watch
//! channel and the cancellation token, asserted at compile time.
//!
//! They match the channels and the token they follow:
//!
//! - A one-shot half is `Send + Sync` when the value is `Send`, and always
//!   `Unpin`.
//! - A broadcast half is `Send + Sync` when the value is.
//! - A watch half is `Send + Sync` when the value is; a watch `Ref` is not
//!   `Send` (its reclamation guard belongs to the thread that took it) but
//!   is `Sync`.
//! - The cancellation token, its futures and its guards are `Send + Sync`.
//! - Every `async fn` future is `Send + Sync` when what it borrows is, and is
//!   not `Unpin`.
//!
//! A bound lost, or gained where it must not be, fails to compile this file
//! instead of passing quietly.

use std::cell::Cell;
use std::future::Future;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::rc::Rc;

use nuclei::sync::broadcast;
use nuclei::sync::oneshot::{self, error as oneshot_error};
use nuclei::sync::watch::{self, error as watch_error};
use nuclei::sync::{
    CancellationToken, DropGuard, DropGuardRef, WaitForCancellationFuture,
    WaitForCancellationFutureOwned,
};

const fn assert_send<T: Send>() {}
const fn assert_sync<T: Sync>() {}
const fn assert_unpin<T: Unpin>() {}
const fn assert_send_sync<T: Send + Sync>() {}
const fn assert_unwind_safe<T: UnwindSafe + RefUnwindSafe>() {}

fn send_sync_future<F: Future + Send + Sync>(_: &F) {}

/// Compiles only while `$ty` is neither `Send` nor `Sync`. `OnlyOne` is
/// implemented once for every type, once more for every `Send` type and once
/// more for every `Sync` type, so asking for it without saying which is
/// ambiguous, a compile error, for a type that is either.
macro_rules! assert_neither_send_nor_sync {
    ($($ty:ty),* $(,)?) => {$({
        trait OnlyOne<Which> {
            fn check() {}
        }
        struct IsSend;
        struct IsSync;
        impl<T: ?Sized> OnlyOne<()> for T {}
        impl<T: ?Sized + Send> OnlyOne<IsSend> for T {}
        impl<T: ?Sized + Sync> OnlyOne<IsSync> for T {}
        let _ = <$ty as OnlyOne<_>>::check;
    })*};
}

/// Compiles only while the type of the value is not `Send`.
macro_rules! assert_not_send {
    ($($ty:ty),* $(,)?) => {$({
        trait OnlyOne<Which> {
            fn check() {}
        }
        struct IsSend;
        impl<T: ?Sized> OnlyOne<()> for T {}
        impl<T: ?Sized + Send> OnlyOne<IsSend> for T {}
        let _ = <$ty as OnlyOne<_>>::check;
    })*};
}

/// Compiles only while the type of the value is not `Unpin`.
macro_rules! assert_not_unpin_value {
    ($value:expr) => {{
        trait OnlyOne<Which> {
            fn check(&self) {}
        }
        struct IsUnpin;
        impl<T: ?Sized> OnlyOne<()> for T {}
        impl<T: ?Sized + Unpin> OnlyOne<IsUnpin> for T {}
        let value = &$value;
        OnlyOne::<_>::check(value);
    }};
}

#[test]
fn the_oneshot_halves_follow_the_value() {
    // `Cell<u8>` is `Send` and not `Sync`: the halves are both, as the value
    // is only ever moved.
    assert_send_sync::<oneshot::Sender<u8>>();
    assert_send_sync::<oneshot::Receiver<u8>>();
    assert_send_sync::<oneshot::Sender<Cell<u8>>>();
    assert_send_sync::<oneshot::Receiver<Cell<u8>>>();
    assert_unpin::<oneshot::Sender<Rc<u8>>>();
    assert_unpin::<oneshot::Receiver<Rc<u8>>>();
    assert_neither_send_nor_sync!(oneshot::Sender<Rc<u8>>, oneshot::Receiver<Rc<u8>>);
}

#[test]
fn the_oneshot_errors_are_plain_values() {
    assert_send_sync::<oneshot_error::RecvError>();
    assert_send_sync::<oneshot_error::TryRecvError>();
    assert_unpin::<oneshot_error::RecvError>();
}

#[test]
fn the_watch_halves_need_a_value_that_is_send_and_sync() {
    assert_send_sync::<watch::Sender<u8>>();
    assert_send_sync::<watch::Receiver<u8>>();
    assert_send_sync::<watch::Sender<String>>();
    assert_unpin::<watch::Sender<u8>>();
    assert_unpin::<watch::Receiver<u8>>();
}

#[test]
fn the_broadcast_halves_need_a_value_that_is_send_and_sync() {
    assert_send_sync::<broadcast::Sender<u8>>();
    assert_send_sync::<broadcast::Receiver<u8>>();
    assert_send_sync::<broadcast::Sender<String>>();
    assert_unpin::<broadcast::Sender<u8>>();
    assert_unpin::<broadcast::Receiver<u8>>();
    assert_send_sync::<broadcast::RecvError>();
    assert_send_sync::<broadcast::TryRecvError>();
    assert_send_sync::<broadcast::SendError<u8>>();
    assert_neither_send_nor_sync!(broadcast::SendError<Rc<u8>>);
}

#[test]
fn a_watch_ref_stays_on_its_thread_and_can_be_shared() {
    assert_not_send!(watch::Ref<'static, u8>);
    assert_sync::<watch::Ref<'static, u8>>();
    assert_unpin::<watch::Ref<'static, u8>>();
}

#[test]
fn the_watch_errors_follow_the_value() {
    assert_send_sync::<watch_error::RecvError>();
    assert_send_sync::<watch_error::SendError<u8>>();
    assert_neither_send_nor_sync!(watch_error::SendError<Rc<u8>>);
}

#[test]
fn the_cancellation_types_are_send_sync() {
    assert_send_sync::<CancellationToken>();
    assert_unpin::<CancellationToken>();
    assert_unwind_safe::<CancellationToken>();
    assert_send_sync::<WaitForCancellationFuture<'static>>();
    assert_send_sync::<WaitForCancellationFutureOwned>();
    assert_send_sync::<DropGuard>();
    assert_send_sync::<DropGuardRef<'static>>();
    assert_send::<CancellationToken>();
    assert_sync::<CancellationToken>();
}

#[test]
fn the_async_fn_futures_are_send_sync_and_not_unpin() {
    let (mut otx, mut orx) = oneshot::channel::<u8>();
    send_sync_future(&otx.closed());
    assert_not_unpin_value!(otx.closed());
    // The receiver is itself the future.
    send_sync_future(&orx);
    assert_unpin::<oneshot::Receiver<u8>>();
    let _ = orx.try_recv();

    let (btx, mut brx) = broadcast::channel::<u8>(1);
    send_sync_future(&btx.closed());
    assert_not_unpin_value!(btx.closed());
    send_sync_future(&brx.recv());
    assert_not_unpin_value!(brx.recv());

    let (wtx, mut wrx) = watch::channel(0u8);
    send_sync_future(&wtx.closed());
    assert_not_unpin_value!(wtx.closed());
    send_sync_future(&wrx.changed());
    assert_not_unpin_value!(wrx.changed());
    send_sync_future(&wrx.wait_for(|v| *v == 1));
    assert_not_unpin_value!(wrx.wait_for(|v| *v == 1));

    let token = CancellationToken::new();
    send_sync_future(&token.run_until_cancelled(std::future::ready(1u8)));
    assert_not_unpin_value!(token.run_until_cancelled(std::future::ready(1u8)));
    send_sync_future(
        &token
            .clone()
            .run_until_cancelled_owned(std::future::ready(1u8)),
    );
    send_sync_future(&token.cancelled());
    send_sync_future(&token.clone().cancelled_owned());
}
