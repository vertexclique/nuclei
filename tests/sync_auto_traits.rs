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
//! too. So must the guards, locks and cells that carry a value: each is `Send`
//! and `Sync` for exactly the values the established API's same-named types accept,
//! asserted in both directions below. The handles of `nuclei::sync` with
//! nothing to bound are asserted beside their definitions, in
//! `src/sync/mod.rs`.

const fn assert_send<T: Send>() {}

const fn assert_send_sync<T: Send + Sync>() {}

/// The futures of the `nuclei::sync` waits: a task that holds one across an
/// await can move between threads.
#[test]
fn the_sync_wait_futures_are_send_and_sync() {
    use nuclei::sync::{
        Acquire, AcquireOwned, BarrierWait, EventWait, LatchWait, Lock, LockOwned, Notified,
        OwnedNotified, Read, ReadOwned, UpgradableRead, Upgrade, Write, WriteOwned,
    };
    assert_send_sync::<Lock<'static, u8>>();
    assert_send_sync::<LockOwned<u8>>();
    assert_send_sync::<Read<'static, u8>>();
    assert_send_sync::<ReadOwned<u8>>();
    assert_send_sync::<OwnedNotified>();
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

/// Compiles only while `$ty` does not implement `$tr`. Asked for without
/// saying which impl, `AmbiguousIfImpl` is ambiguous, a compile error, for a
/// type that implements the trait.
macro_rules! assert_not {
    ($tr:path: $($ty:ty),* $(,)?) => {$({
        trait AmbiguousIfImpl<A> {
            fn check() {}
        }
        impl<T: ?Sized> AmbiguousIfImpl<()> for T {}
        struct Is;
        impl<T: ?Sized + $tr> AmbiguousIfImpl<Is> for T {}
        let _ = <$ty as AmbiguousIfImpl<_>>::check;
    })*};
}

/// A value that moves between threads but cannot be shared.
type SendOnly = std::cell::Cell<u8>;

/// A value that can be shared but cannot move: a held `std` mutex guard.
type SyncOnly = std::sync::MutexGuard<'static, ()>;

/// A value that does neither.
type Neither = std::rc::Rc<u8>;

/// A lock moves with its value and is shared when the value moves and
/// shares.
#[test]
fn the_locks_and_their_guards_follow_their_values() {
    use nuclei::sync::{
        MappedMutexGuard, Mutex, MutexGuard, OwnedMappedMutexGuard, OwnedMutexGuard,
        OwnedRwLockMappedWriteGuard, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock,
        RwLockMappedWriteGuard, RwLockReadGuard, RwLockWriteGuard,
    };
    // Mutex: `Send` and `Sync` for a `Send` value.
    assert_send_sync::<Mutex<SendOnly>>();
    assert_not!(Send: Mutex<SyncOnly>, Mutex<Neither>);
    assert_not!(Sync: Mutex<SyncOnly>, Mutex<Neither>);
    // Its guards: `Send` for a `Send` value, `Sync` for a `Send + Sync` one.
    assert_send::<MutexGuard<'static, SendOnly>>();
    assert_send::<OwnedMutexGuard<SendOnly>>();
    assert_send::<MappedMutexGuard<'static, SendOnly>>();
    assert_send::<OwnedMappedMutexGuard<SendOnly, SendOnly>>();
    assert_not!(Sync: MutexGuard<'static, SendOnly>, OwnedMutexGuard<SendOnly>);
    assert_not!(Sync: MappedMutexGuard<'static, SendOnly>);
    assert_not!(Sync: OwnedMappedMutexGuard<SendOnly, SendOnly>);
    assert_not!(Send: MutexGuard<'static, SyncOnly>, OwnedMutexGuard<SyncOnly>);
    assert_not!(Send: MappedMutexGuard<'static, SyncOnly>);
    assert_not!(Send: OwnedMappedMutexGuard<SyncOnly, SyncOnly>);
    // A mapped guard shares for a `Sync` component alone.
    const fn assert_sync<T: Sync>() {}
    assert_sync::<MappedMutexGuard<'static, SyncOnly>>();

    // RwLock: `Send` for a `Send` value, `Sync` for a `Send + Sync` one.
    assert_send::<RwLock<SendOnly>>();
    assert_not!(Sync: RwLock<SendOnly>);
    assert_not!(Send: RwLock<SyncOnly>, RwLock<Neither>);
    assert_not!(Sync: RwLock<SyncOnly>, RwLock<Neither>);
    assert_send_sync::<RwLock<u8>>();
    // A read guard moves for a `Sync` value and is shared for a `Send +
    // Sync` one.
    const fn assert_send_only<T: Send>() {}
    assert_send_only::<RwLockReadGuard<'static, SyncOnly>>();
    assert_send_only::<OwnedRwLockReadGuard<u8, SyncOnly>>();
    assert_not!(Send: RwLockReadGuard<'static, SendOnly>);
    assert_not!(Send: OwnedRwLockReadGuard<SendOnly, SendOnly>);
    assert_not!(Sync: RwLockReadGuard<'static, SendOnly>, RwLockReadGuard<'static, SyncOnly>);
    assert_not!(Sync: OwnedRwLockReadGuard<SendOnly, SendOnly>);
    // A write guard hands out `&mut T` and moves and shares for a `Send +
    // Sync` value alone.
    assert_send_sync::<RwLockWriteGuard<'static, u8>>();
    assert_send_sync::<RwLockMappedWriteGuard<'static, u8>>();
    assert_send_sync::<OwnedRwLockWriteGuard<u8>>();
    assert_send_sync::<OwnedRwLockMappedWriteGuard<u8, u8>>();
    assert_not!(Send: RwLockWriteGuard<'static, SendOnly>, RwLockWriteGuard<'static, SyncOnly>);
    assert_not!(Sync: RwLockWriteGuard<'static, SendOnly>, RwLockWriteGuard<'static, SyncOnly>);
    assert_not!(Send: RwLockMappedWriteGuard<'static, SendOnly>, OwnedRwLockWriteGuard<SendOnly>);
    assert_not!(Sync: RwLockMappedWriteGuard<'static, SendOnly>, OwnedRwLockWriteGuard<SendOnly>);
    assert_not!(Send: OwnedRwLockMappedWriteGuard<SendOnly, SendOnly>);
    assert_not!(Sync: OwnedRwLockMappedWriteGuard<SendOnly, SendOnly>);
}

/// The cells follow their values the same way, and the handles with nothing
/// to bound are `Send` and `Sync` whatever a future holds.
#[test]
fn the_cells_and_semaphore_handles_follow_their_values() {
    use nuclei::sync::{
        AcquireError, OnceCell, OwnedSemaphorePermit, Semaphore, SemaphorePermit, SetError,
        SetOnce, SetOnceError, TryAcquireError, TryLockError,
    };
    assert_send::<OnceCell<SendOnly>>();
    assert_not!(Sync: OnceCell<SendOnly>);
    assert_not!(Send: OnceCell<SyncOnly>, OnceCell<Neither>);
    assert_not!(Sync: OnceCell<SyncOnly>, OnceCell<Neither>);
    assert_send::<SetOnce<SendOnly>>();
    assert_not!(Sync: SetOnce<SendOnly>);
    assert_not!(Send: SetOnce<SyncOnly>, SetOnce<Neither>);
    assert_send_sync::<OnceCell<u8>>();
    assert_send_sync::<SetOnce<u8>>();
    assert_send_sync::<Semaphore>();
    assert_send_sync::<SemaphorePermit<'static>>();
    assert_send_sync::<OwnedSemaphorePermit>();
    assert_send_sync::<AcquireError>();
    assert_send_sync::<TryAcquireError>();
    assert_send_sync::<TryLockError>();
    assert_send_sync::<SetError<u8>>();
    assert_send_sync::<SetOnceError<u8>>();
}

/// A future that is not `Unpin` would need pinning to be dropped or moved by
/// callers that do neither; the waits of `nuclei::sync` are all `Unpin`, so
/// `select!`-style loops can poll them by reference.
#[test]
fn the_waits_and_guards_are_unpin() {
    use nuclei::sync::{
        Acquire, Lock, MutexGuard, Notified, OwnedRwLockReadGuard, Read, RwLockReadGuard,
        RwLockWriteGuard, Write,
    };
    const fn assert_unpin<T: Unpin>() {}
    assert_unpin::<Lock<'static, u8>>();
    assert_unpin::<Read<'static, u8>>();
    assert_unpin::<Write<'static, u8>>();
    assert_unpin::<Acquire<'static>>();
    assert_unpin::<Notified<'static>>();
    assert_unpin::<MutexGuard<'static, u8>>();
    assert_unpin::<RwLockReadGuard<'static, u8>>();
    assert_unpin::<RwLockWriteGuard<'static, u8>>();
    assert_unpin::<OwnedRwLockReadGuard<u8, u8>>();
}
