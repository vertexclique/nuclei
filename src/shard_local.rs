//! Shard-local values: [`shard_local!`](crate::shard_local) declares a
//! [`ShardLocal`], which holds one value per worker shard, made on its first
//! use there. The value lives in the shard, not in the thread: when a
//! worker's shard is handed to another thread (its runner blocked in one
//! poll, see [`Builder::handoff_after`](crate::Builder::handoff_after)), the
//! value goes with it, and the shard's tasks keep seeing it there.

use core::fmt;

use crate::rt::context;
use crate::rt::shard::locals::Missing;
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
/// [`shard_local!`](crate::shard_local).
///
/// Each worker of each runtime has its own value, made by the declaration's
/// expression the first time code running on that worker uses it, and
/// dropped with the worker's shard, once the shard's IO is settled. A value
/// that panics as it is dropped is logged at error, and the shard's other
/// values and the rest of its going go on. Only code running on a worker (its
/// tasks, and `block_on` called from them) reaches a value: a thread's own
/// shard in `block_on`, the blocking pool and any other thread have none.
///
/// A task spawned on a worker stays on that worker's shard, and a handoff
/// moves the shard with its values, so the task always sees one value. A
/// task kept on a thread by `spawn_local` whose worker's shard was handed
/// on runs on that thread's own shard, off the worker: it gets
/// [`ShardAccessError`] there.
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
    /// # Panics
    ///
    /// Off a worker, or when the value's own initializer uses it;
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
    /// answers a [`ShardAccessError`] off a worker or while the value's own
    /// initializer runs.
    pub fn try_with<R>(&'static self, f: impl FnOnce(&T) -> R) -> Result<R, ShardAccessError> {
        let key = self.key();
        context::current_pinned(|shard| {
            let locals = shard
                .and_then(|shard| shard.locals())
                .ok_or(ShardAccessError { making: false })?;
            match locals.get_or_init(key, self.init) {
                Ok(value) => Ok(f(value)),
                Err(Missing::Making) => Err(ShardAccessError { making: true }),
            }
        })
    }
}

impl<T: Send + 'static> fmt::Debug for ShardLocal<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShardLocal").finish_non_exhaustive()
    }
}

/// A [`ShardLocal`] was used where it has no value: off a worker, or by its
/// own initializer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShardAccessError {
    making: bool,
}

impl ShardAccessError {
    /// Whether the value was being made: its initializer used it.
    pub fn is_initializing(&self) -> bool {
        self.making
    }
}

impl fmt::Display for ShardAccessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.making {
            "the shard-local value was used by its own initializer"
        } else {
            "this code runs on no worker, so it has no shard-local value: use it in a task"
        })
    }
}

impl std::error::Error for ShardAccessError {}
