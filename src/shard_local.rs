//! Shard-local values: [`shard_local!`](crate::shard_local!) declares a
//! [`ShardLocal`], which holds one value per worker shard, made on its first
//! use there. The value lives in the shard, not in the thread: when a
//! worker's shard is handed to another thread (its runner blocked in one
//! poll, see [`Builder::handoff_after`](crate::Builder::handoff_after)), the
//! value goes with it, and the shard's tasks keep seeing it there. Code that
//! still runs on the old thread (the rest of the poll that lost the shard)
//! gets the shard lent back for each call (`rt::handoff::loan`).

use core::fmt;

use crate::rt::context::{self, Unlent};
use crate::rt::shard::Shard;
use crate::rt::shard::locals::{Locals, Missing};
use core::sync::atomic::{AtomicUsize, Ordering};

/// Declares shard-local statics, each a [`ShardLocal`] made with the
/// expression given on its first use on each worker.
///
/// ```
/// use std::cell::Cell;
///
/// nuclei::shard_local! {
///     static REQUESTS: Cell<u64> = Cell::new(0);
/// }
///
/// let rt = nuclei::Runtime::builder().worker_threads(1).build().unwrap();
/// let count = rt.block_on(rt.spawn(async {
///     REQUESTS.with(|n| n.set(n.get() + 1));
///     REQUESTS.with(Cell::get)
/// }));
/// assert_eq!(count, 1);
/// ```
#[macro_export]
macro_rules! shard_local {
    () => {};
    ($(#[$attr:meta])* $vis:vis static $name:ident: $t:ty = $init:expr; $($rest:tt)*) => {
        $(#[$attr])*
        $vis static $name: $crate::runtime::ShardLocal<$t> =
            $crate::runtime::ShardLocal::__new(|| $init);
        $crate::shard_local!($($rest)*);
    };
    ($(#[$attr:meta])* $vis:vis static $name:ident: $t:ty = $init:expr) => {
        $(#[$attr])*
        $vis static $name: $crate::runtime::ShardLocal<$t> =
            $crate::runtime::ShardLocal::__new(|| $init);
    };
}

/// A value per worker shard, declared with
/// [`shard_local!`](crate::shard_local!).
///
/// Each worker of each runtime has its own value, made by the declaration's
/// expression the first time code running on that worker uses it, and
/// dropped with the worker's shard. Only code running on a worker (its
/// tasks, and `block_on` called from them) reaches a value: a thread's own
/// shard in `block_on`, the blocking pool and any other thread have none.
///
/// A task spawned on a worker stays on that worker's shard, and a handoff
/// moves the shard with its values, so the task always sees one value. The
/// rest of a poll that lost its worker's shard to a handoff (a nested
/// `block_on` in it too), and the tasks `spawn_local` kept on its thread,
/// run on that thread, off the worker: each [`with`](Self::with) there
/// borrows the worker's shard back from the thread that runs it now, for
/// the length of the call, and so sees the same value.
pub struct ShardLocal<T: Send + 'static> {
    init: fn() -> T,
    /// The static's place among every shard's values, plus one; 0 until its
    /// first use.
    key: AtomicUsize,
}

/// Keys handed out so far.
static KEYS: AtomicUsize = AtomicUsize::new(0);

impl<T: Send + 'static> ShardLocal<T> {
    #[doc(hidden)]
    pub const fn __new(init: fn() -> T) -> Self {
        Self {
            init,
            key: AtomicUsize::new(0),
        }
    }

    /// The static's key, taken on its first use.
    fn key(&self) -> usize {
        let key = self.key.load(Ordering::Acquire);
        if key != 0 {
            return key - 1;
        }
        let mine = KEYS.fetch_add(1, Ordering::Relaxed) + 1;
        match self
            .key
            .compare_exchange(0, mine, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => mine - 1,
            Err(won) => won - 1,
        }
    }

    /// Runs `f` on this worker's value, made now if this is its first use
    /// here.
    ///
    /// While `f` runs the shard stays on this thread, even across a
    /// `block_on` inside `f`: keep `f` short, as a blocking `f` holds the
    /// whole worker up.
    ///
    /// Called from code whose poll lost its worker's shard to a handoff, it
    /// waits, its thread parked, until the thread that runs the shard now
    /// lends it between two of its polls (after the `with` that thread's code
    /// may hold), runs `f` on the shard's value, and gives the shard back as
    /// `f` returns or unwinds: a round trip between the two threads for each
    /// call, and the lender's thread waits while `f` runs.
    ///
    /// # Panics
    ///
    /// Off a worker, when the value's own initializer uses it, or from code
    /// whose poll lost the shard once the runtime shut down;
    /// [`try_with`](Self::try_with) answers the error instead.
    #[expect(
        clippy::panic,
        reason = "documented under `# Panics`; `try_with` answers the error instead"
    )]
    pub fn with<R>(&'static self, f: impl FnOnce(&T) -> R) -> R {
        match self.try_with(f) {
            Ok(r) => r,
            Err(err) => panic!("{err}"),
        }
    }

    /// Runs `f` on this worker's value, as [`with`](Self::with) does, or
    /// answers a [`ShardAccessError`] off a worker, while the value's own
    /// initializer runs, or from code whose poll lost the shard once the
    /// runtime shut down.
    pub fn try_with<R>(&'static self, f: impl FnOnce(&T) -> R) -> Result<R, ShardAccessError> {
        let key = self.key();
        context::current_pinned(|shard| match shard.and_then(Shard::locals) {
            Some(locals) => self.lend(locals, key, f),
            // The shard this code's poll lost, lent back for the call; else none.
            None => context::lent_back(|lent| match lent.map(Shard::locals) {
                Ok(Some(locals)) => self.lend(locals, key, f),
                Ok(None) | Err(Unlent::Off) => Err(ShardAccessError { why: Why::Off }),
                Err(Unlent::Gone) => Err(ShardAccessError { why: Why::Gone }),
            }),
        })
    }

    /// Runs `f` on the value of the key `key` among `locals`, made now if it has none.
    #[inline(always)]
    fn lend<R>(
        &self,
        locals: &Locals,
        key: usize,
        f: impl FnOnce(&T) -> R,
    ) -> Result<R, ShardAccessError> {
        match locals.get_or_init(key, self.init) {
            Ok(value) => Ok(f(value)),
            Err(Missing::Making) => Err(ShardAccessError { why: Why::Making }),
        }
    }
}

impl<T: Send + 'static> fmt::Debug for ShardLocal<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShardLocal").finish_non_exhaustive()
    }
}

/// A [`ShardLocal`] was used where it has no value: off a worker, by its own
/// initializer, or from a task whose poll lost its worker's shard once the
/// runtime had shut down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShardAccessError {
    why: Why,
}

/// Why a [`ShardAccessError`] has no value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Why {
    Off,
    Making,
    Gone,
}

impl ShardAccessError {
    /// Whether the value was being made: its initializer used it.
    pub fn is_initializing(&self) -> bool {
        self.why == Why::Making
    }
}

impl fmt::Display for ShardAccessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.why {
            Why::Off => {
                "this code runs on no worker, so it has no shard-local value: use it in a task"
            }
            Why::Making => "the shard-local value was used by its own initializer",
            Why::Gone => "the runtime shut down, so this worker's shard-local value is gone",
        })
    }
}

impl std::error::Error for ShardAccessError {}
