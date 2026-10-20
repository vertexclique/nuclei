//! Auto traits of the `nuclei::sync` wait futures, asserted at compile time.
//!
//! A future that silently stops being `Send` or `Sync` breaks every caller
//! that holds it across an await on a multi-threaded executor, and the break
//! shows up in the caller's build, not here. Each assertion below is a bound
//! on a `const fn`, so losing a bound fails to compile this file instead of
//! passing quietly.
//!
//! Three futures must stay the other way: those of the reentrant locks are
//! neither `Send` nor `Sync`, and gaining either fails to compile this file
//! too. The locks, guards and cells of `nuclei::sync` are asserted beside
//! their definitions, in `src/sync/mod.rs`.

const fn assert_send<T: Send>() {}

const fn assert_send_sync<T: Send + Sync>() {}

/// The futures of the `nuclei::sync` waits: a task that holds one across an
/// await can move between threads.
#[test]
fn the_sync_wait_futures_are_send_and_sync() {
    use nuclei::sync::{
        Acquire, AcquireOwned, BarrierWait, EventWait, LatchWait, Lock, LockOwned, Notified, Read,
        UpgradableRead, Upgrade, Write, WriteOwned,
    };
    assert_send_sync::<Lock<'static, u8>>();
    assert_send_sync::<LockOwned<u8>>();
    assert_send_sync::<Read<'static, u8>>();
    assert_send_sync::<Write<'static, u8>>();
    assert_send_sync::<WriteOwned<u8>>();
    assert_send_sync::<UpgradableRead<'static, u8>>();
    assert_send_sync::<Upgrade<'static, u8>>();
    assert_send_sync::<Acquire<'static>>();
    assert_send_sync::<AcquireOwned>();
    assert_send_sync::<Notified<'static>>();
    assert_send_sync::<EventWait<'static>>();
    assert_send_sync::<LatchWait<'static>>();
    assert_send_sync::<BarrierWait<'static>>();
}

/// Compiles only while each type is neither `Send` nor `Sync`. `OnlyOne` is
/// implemented once for every type, once more for every `Send` type and once
/// more for every `Sync` type, so asking for it without saying which is
/// ambiguous, a compile error, for a type that is either.
macro_rules! assert_neither_send_nor_sync {
    ($($handle:ty),* $(,)?) => {$({
        trait OnlyOne<Which> {
            fn check() {}
        }
        struct IsSend;
        struct IsSync;
        impl<T: ?Sized> OnlyOne<()> for T {}
        impl<T: ?Sized + Send> OnlyOne<IsSend> for T {}
        impl<T: ?Sized + Sync> OnlyOne<IsSync> for T {}
        let _ = <$handle as OnlyOne<_>>::check;
    })*};
}

/// The three futures of the reentrant locks borrow the `Owner` they were
/// taken with, and an `Owner` is never shared between threads. So each stays
/// with its owner: neither `Send` nor `Sync`. Were one to become either, two
/// threads could hold guards of one owner, and so `&T` to a `T` that is not
/// `Sync`.
#[test]
fn the_reentrant_lock_futures_stay_with_their_owner() {
    use nuclei::sync::{Owner, ReentrantMutexLock, ReentrantRead, ReentrantWrite};
    assert_neither_send_nor_sync!(
        ReentrantMutexLock<'static, u8>,
        ReentrantRead<'static, u8>,
        ReentrantWrite<'static, u8>,
    );
    // The owner itself moves with its task.
    assert_send::<Owner>();
}
