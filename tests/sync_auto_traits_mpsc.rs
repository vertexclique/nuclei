//! Auto traits of the multi-producer, single-consumer channels, asserted at
//! compile time, for a message type that is neither `Send` nor `Sync` (`NN`),
//! only `Send` (`YN`) and both (`YY`).
//!
//! They match the established channels they follow:
//!
//! - Every handle (sender, weak sender, receiver, permit) is `Send + Sync`
//!   when the message is `Send`, and `Unpin` always.
//! - An error that carries the message is `Send` when the message is, and
//!   `Sync` only when the message is: `SendError<YN>` is `Send` but not `Sync`.
//! - Every `async fn` future is `!Unpin`, and `Send + Sync` when what it holds
//!   is. The future of `send` holds the message, so it is `Sync` only for a
//!   `Sync` message.
//!
//! A bound lost, or gained where it must not be, fails to compile this file
//! instead of passing quietly.
#![allow(clippy::type_complexity, clippy::diverging_sub_expression)]

use std::cell::Cell;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::rc::Rc;

use nuclei::sync::mpsc;

#[derive(Clone)]
#[allow(unused)]
struct YY {}

#[derive(Clone)]
#[allow(unused)]
struct YN {
    _value: Cell<u8>,
}

#[derive(Clone)]
#[allow(unused)]
struct NN {
    _value: Rc<u8>,
}

#[allow(dead_code)]
fn require_send<T: Send>(_t: &T) {}
#[allow(dead_code)]
fn require_sync<T: Sync>(_t: &T) {}
#[allow(dead_code)]
fn require_unpin<T: Unpin>(_t: &T) {}

#[allow(dead_code)]
struct Invalid;

#[allow(unused)]
trait AmbiguousIfSend<A> {
    fn some_item(&self) {}
}
impl<T: ?Sized> AmbiguousIfSend<()> for T {}
impl<T: ?Sized + Send> AmbiguousIfSend<Invalid> for T {}

#[allow(unused)]
trait AmbiguousIfSync<A> {
    fn some_item(&self) {}
}
impl<T: ?Sized> AmbiguousIfSync<()> for T {}
impl<T: ?Sized + Sync> AmbiguousIfSync<Invalid> for T {}

#[allow(unused)]
trait AmbiguousIfUnpin<A> {
    fn some_item(&self) {}
}
impl<T: ?Sized> AmbiguousIfUnpin<()> for T {}
impl<T: ?Sized + Unpin> AmbiguousIfUnpin<Invalid> for T {}

macro_rules! into_todo {
    ($typ:ty) => {{
        let x: $typ = todo!();
        x
    }};
}

macro_rules! assert_send {
    (Send & $(!)?Sync & $(!)?Unpin, $value:expr) => {
        require_send(&$value);
    };
    (!Send & $(!)?Sync & $(!)?Unpin, $value:expr) => {
        AmbiguousIfSend::some_item(&$value);
    };
}
macro_rules! assert_sync {
    ($(!)?Send & Sync & $(!)?Unpin, $value:expr) => {
        require_sync(&$value);
    };
    ($(!)?Send & !Sync & $(!)?Unpin, $value:expr) => {
        AmbiguousIfSync::some_item(&$value);
    };
}
macro_rules! assert_unpin {
    ($(!)?Send & $(!)?Sync & Unpin, $value:expr) => {
        require_unpin(&$value);
    };
    ($(!)?Send & $(!)?Sync & !Unpin, $value:expr) => {
        AmbiguousIfUnpin::some_item(&$value);
    };
}

/// The future an `async fn` returns, built from placeholder arguments that are
/// never evaluated.
macro_rules! async_assert_fn {
    ($($f:ident $(< $($generic:ty),* > )? )::+($($arg:ty),*): $($tok:tt)*) => {
        #[allow(unreachable_code)]
        #[allow(unused_variables)]
        const _: fn() = || {
            let f = $($f $(::<$($generic),*>)? )::+( $( into_todo!($arg) ),* );
            assert_send!($($tok)*, f);
            assert_sync!($($tok)*, f);
            assert_unpin!($($tok)*, f);
        };
    };
}

/// A value of a type, never evaluated.
macro_rules! assert_value {
    ($type:ty: $($tok:tt)*) => {
        #[allow(unreachable_code)]
        #[allow(unused_variables)]
        const _: fn() = || {
            let f: $type = todo!();
            assert_send!($($tok)*, f);
            assert_sync!($($tok)*, f);
            assert_unpin!($($tok)*, f);
        };
    };
}

// Handles.
assert_value!(mpsc::OwnedPermit<NN>: !Send & !Sync & Unpin);
assert_value!(mpsc::OwnedPermit<YN>: Send & Sync & Unpin);
assert_value!(mpsc::OwnedPermit<YY>: Send & Sync & Unpin);
assert_value!(mpsc::Permit<'_, NN>: !Send & !Sync & Unpin);
assert_value!(mpsc::Permit<'_, YN>: Send & Sync & Unpin);
assert_value!(mpsc::Permit<'_, YY>: Send & Sync & Unpin);
assert_value!(mpsc::PermitIterator<'_, NN>: !Send & !Sync & Unpin);
assert_value!(mpsc::PermitIterator<'_, YN>: Send & Sync & Unpin);
assert_value!(mpsc::PermitIterator<'_, YY>: Send & Sync & Unpin);
assert_value!(mpsc::Receiver<NN>: !Send & !Sync & Unpin);
assert_value!(mpsc::Receiver<YN>: Send & Sync & Unpin);
assert_value!(mpsc::Receiver<YY>: Send & Sync & Unpin);
assert_value!(mpsc::Sender<NN>: !Send & !Sync & Unpin);
assert_value!(mpsc::Sender<YN>: Send & Sync & Unpin);
assert_value!(mpsc::Sender<YY>: Send & Sync & Unpin);
assert_value!(mpsc::UnboundedReceiver<NN>: !Send & !Sync & Unpin);
assert_value!(mpsc::UnboundedReceiver<YN>: Send & Sync & Unpin);
assert_value!(mpsc::UnboundedReceiver<YY>: Send & Sync & Unpin);
assert_value!(mpsc::UnboundedSender<NN>: !Send & !Sync & Unpin);
assert_value!(mpsc::UnboundedSender<YN>: Send & Sync & Unpin);
assert_value!(mpsc::UnboundedSender<YY>: Send & Sync & Unpin);
assert_value!(mpsc::WeakSender<NN>: !Send & !Sync & Unpin);
assert_value!(mpsc::WeakSender<YN>: Send & Sync & Unpin);
assert_value!(mpsc::WeakSender<YY>: Send & Sync & Unpin);
assert_value!(mpsc::WeakUnboundedSender<NN>: !Send & !Sync & Unpin);
assert_value!(mpsc::WeakUnboundedSender<YN>: Send & Sync & Unpin);
assert_value!(mpsc::WeakUnboundedSender<YY>: Send & Sync & Unpin);

// Errors that carry the message.
assert_value!(mpsc::error::SendError<NN>: !Send & !Sync & Unpin);
assert_value!(mpsc::error::SendError<YN>: Send & !Sync & Unpin);
assert_value!(mpsc::error::SendError<YY>: Send & Sync & Unpin);
assert_value!(mpsc::error::TrySendError<NN>: !Send & !Sync & Unpin);
assert_value!(mpsc::error::TrySendError<YN>: Send & !Sync & Unpin);
assert_value!(mpsc::error::TrySendError<YY>: Send & Sync & Unpin);
assert_value!(mpsc::error::TryRecvError: Send & Sync & Unpin);

// The futures of the bounded channel.
async_assert_fn!(mpsc::Receiver<NN>::recv(_): !Send & !Sync & !Unpin);
async_assert_fn!(mpsc::Receiver<YN>::recv(_): Send & Sync & !Unpin);
async_assert_fn!(mpsc::Receiver<YY>::recv(_): Send & Sync & !Unpin);
async_assert_fn!(mpsc::Receiver<NN>::recv_many(_, &mut Vec<NN>, usize): !Send & !Sync & !Unpin);
async_assert_fn!(mpsc::Receiver<YN>::recv_many(_, &mut Vec<YN>, usize): Send & !Sync & !Unpin);
async_assert_fn!(mpsc::Receiver<YY>::recv_many(_, &mut Vec<YY>, usize): Send & Sync & !Unpin);
async_assert_fn!(mpsc::Sender<NN>::closed(_): !Send & !Sync & !Unpin);
async_assert_fn!(mpsc::Sender<NN>::reserve(_): !Send & !Sync & !Unpin);
async_assert_fn!(mpsc::Sender<NN>::reserve_many(_, usize): !Send & !Sync & !Unpin);
async_assert_fn!(mpsc::Sender<NN>::reserve_owned(_): !Send & !Sync & !Unpin);
async_assert_fn!(mpsc::Sender<NN>::send(_, NN): !Send & !Sync & !Unpin);
async_assert_fn!(mpsc::Sender<YN>::closed(_): Send & Sync & !Unpin);
async_assert_fn!(mpsc::Sender<YN>::reserve(_): Send & Sync & !Unpin);
async_assert_fn!(mpsc::Sender<YN>::reserve_many(_, usize): Send & Sync & !Unpin);
async_assert_fn!(mpsc::Sender<YN>::reserve_owned(_): Send & Sync & !Unpin);
async_assert_fn!(mpsc::Sender<YN>::send(_, YN): Send & !Sync & !Unpin);
async_assert_fn!(mpsc::Sender<YY>::closed(_): Send & Sync & !Unpin);
async_assert_fn!(mpsc::Sender<YY>::reserve(_): Send & Sync & !Unpin);
async_assert_fn!(mpsc::Sender<YY>::reserve_many(_, usize): Send & Sync & !Unpin);
async_assert_fn!(mpsc::Sender<YY>::reserve_owned(_): Send & Sync & !Unpin);
async_assert_fn!(mpsc::Sender<YY>::send(_, YY): Send & Sync & !Unpin);

// The futures of the unbounded channel.
async_assert_fn!(mpsc::UnboundedReceiver<NN>::recv(_): !Send & !Sync & !Unpin);
async_assert_fn!(mpsc::UnboundedReceiver<YN>::recv(_): Send & Sync & !Unpin);
async_assert_fn!(mpsc::UnboundedReceiver<YY>::recv(_): Send & Sync & !Unpin);
async_assert_fn!(mpsc::UnboundedReceiver<NN>::recv_many(_, &mut Vec<NN>, usize): !Send & !Sync & !Unpin);
async_assert_fn!(mpsc::UnboundedReceiver<YN>::recv_many(_, &mut Vec<YN>, usize): Send & !Sync & !Unpin);
async_assert_fn!(mpsc::UnboundedReceiver<YY>::recv_many(_, &mut Vec<YY>, usize): Send & Sync & !Unpin);
async_assert_fn!(mpsc::UnboundedSender<NN>::closed(_): !Send & !Sync & !Unpin);
async_assert_fn!(mpsc::UnboundedSender<YN>::closed(_): Send & Sync & !Unpin);
async_assert_fn!(mpsc::UnboundedSender<YY>::closed(_): Send & Sync & !Unpin);

/// The panic-safety markers hold for every handle whatever the message.
#[allow(dead_code)]
fn unwind_safety_holds<T: 'static>() {
    fn both<U: UnwindSafe + RefUnwindSafe>() {}
    both::<mpsc::Sender<T>>();
    both::<mpsc::WeakSender<T>>();
    both::<mpsc::Receiver<T>>();
    both::<mpsc::UnboundedSender<T>>();
    both::<mpsc::WeakUnboundedSender<T>>();
    both::<mpsc::UnboundedReceiver<T>>();
    both::<mpsc::OwnedPermit<T>>();
    both::<mpsc::Permit<'static, T>>();
    both::<mpsc::PermitIterator<'static, T>>();
}

#[test]
fn the_bounds_are_checked_when_this_file_compiles() {
    // Nothing to run: a wrong bound is a compile error above.
}
