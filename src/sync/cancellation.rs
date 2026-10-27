//! A token that signals a cancellation request to one or more tasks, with
//! child tokens that follow their parent.
//!
//! [`CancellationToken::cancel`] marks the token and every token derived
//! from it by [`child_token`](CancellationToken::child_token), and wakes
//! every task waiting in [`cancelled`](CancellationToken::cancelled). A child
//! can be cancelled alone; cancelling a child never cancels its parent. The
//! API is the established async cancellation-token API; nothing here needs a
//! runtime.
//!
//! ```
//! use nuclei::sync::CancellationToken;
//!
//! let parent = CancellationToken::new();
//! let child = parent.child_token();
//! assert!(!child.is_cancelled());
//! parent.cancel();
//! assert!(child.is_cancelled());
//! ```
//!
//! The tree, what `cancel` promises and the progress guarantee of each
//! operation are documented on [`CancellationToken`], where a reader of the
//! public API finds them. This module's docs keep the argument for the
//! memory ordering of the tree.
//!
//! # Memory ordering of the tree
//!
//! `child_token` and `cancel` race: a child added while the parent is being
//! cancelled must end up cancelled. The two sides use the same pattern, a
//! store followed by a `SeqCst` fence followed by a read of the other's store
//! (Dekker):
//!
//! - `child_token` publishes the child in the parent's registry, fences, and
//!   then reads the parent's flag.
//! - `cancel` sets the flag (unless an earlier call did), fences, and then
//!   iterates the registry.
//!
//! The fences are ordered one way or the other in the single `SeqCst` order.
//! If the child's fence comes first, everything before it (the registry
//! insert) is visible to the reads after the canceller's fence, so the
//! iteration, which starts after that fence, yields the child: the registry
//! guarantees that an entry present from the start of an iteration to its end
//! is yielded exactly once. If the canceller's fence comes first, the flag
//! set before it is visible to the read after the child's fence, so the child
//! sees its parent cancelled and cancels itself. Either way the child is
//! cancelled. The same argument covers the registry itself being created
//! lazily, since its publication is a store before the child's fence.
//!
//! A `cancel` that finds the flag already set stores nothing, but it fences
//! and iterates all the same, and that is what its own guarantee rests on:
//! a child whose `child_token` had completed before the call is in the
//! registry for the whole walk, so the registry yields it. A child inserted
//! while the walk runs is the race above, and one of the two sides covers
//! it: the call that stored the flag, or the child, which reads the flag
//! after its own fence. So a child being created meanwhile is cancelled once
//! both the `cancel` and its own `child_token` have returned.
//!
//! Under `--cfg loom` the registry is a loom `Mutex` around a `BTreeMap` (the
//! map in the real build is a lock-free kovan map that loom cannot
//! instrument; the ordered map keeps the model deterministic), and
//! [`Arc`]/[`Weak`] are the small models in `rc`, since loom has neither a
//! weak reference nor `Arc::into_inner`. So the loom models check the
//! protocol around the registry (the flag, the wakeups, the order of insert,
//! visit and removal, the ownership of nodes) with a registry that is
//! atomic by itself; the argument above is what covers the real registry.

use core::fmt;
use core::future::Future;
use core::hash::{Hash, Hasher};

mod guard;
mod node;
#[cfg(all(loom, test))]
mod rc;
mod wait;

pub use guard::{DropGuard, DropGuardRef};
pub use wait::{WaitForCancellationFuture, WaitForCancellationFutureOwned};

use node::Node;
use wait::RunUntilCancelled;

#[cfg(not(all(loom, test)))]
use std::sync::{Arc, Weak};

#[cfg(all(loom, test))]
use self::rc::{Arc, Weak};

/// A token which can be used to signal a cancellation request to one or more
/// tasks.
///
/// Tasks call [`cancelled`](Self::cancelled) to get a future that completes
/// when cancellation is requested, and [`cancel`](Self::cancel) requests it.
/// A clone is another handle to the same token: cancelling one cancels all.
/// A [`child_token`](Self::child_token) is cancelled with its parent, and can
/// be cancelled alone.
///
/// # The tree
///
/// A token is an `Arc` of a node. A node holds its own cancelled flag (an
/// [`Event`](crate::sync::Event), which also wakes the waiters), a *strong* handle to
/// its parent node, and, once it has a child, a registry of its children
/// that holds a *weak* handle to each of them.
///
/// - A parent whose tokens are all gone stays alive as long as any child is,
///   as a relay: cancelling a grandparent still reaches the grandchildren. A
///   node nobody can cancel or wait on any more is freed.
/// - Memory is bounded by the live tokens. A node removes itself from its
///   parent's registry when it is freed, so the registry holds only the
///   children that exist.
/// - Freeing a node walks up the chain of ancestors it was the last owner of,
///   in a loop and not by recursion, so a chain of a million nested tokens
///   can be dropped without overflowing the stack.
///
/// # Cancelling
///
/// `cancel` sets the node's flag, which wakes its waiters, and then walks the
/// whole subtree with an explicit stack: it sets the flag of every descendant
/// that is not set yet, and descends into every descendant, those some other
/// call already set included. So:
///
/// - When any `cancel` call returns, the token it was called on and every
///   descendant that existed when the call began (its `child_token` had
///   completed) are cancelled, and their waiters woken, whether this call or
///   another one set the flag first. A call on an already cancelled token
///   still walks its subtree, so it too returns with the subtree cancelled.
/// - A descendant being created at the same moment is cancelled by the time
///   both calls have returned: either the new child sees its parent
///   cancelled and cancels itself, or the walk sees the child.
/// - The price is the walk: every call visits every descendant once, so
///   concurrent or repeated calls over one tree each pay for it. Cancel a
///   large tree once.
/// - Cancellation is not atomic over the tree: while a call runs, another
///   thread can see a child cancelled and then a sibling not yet cancelled.
///
/// # Progress
///
/// - Wait-free: [`is_cancelled`](CancellationToken::is_cancelled) (one load),
///   [`Clone`], dropping a token that is not the last owner of its node,
///   [`cancelled`](CancellationToken::cancelled) and
///   [`cancelled_owned`](CancellationToken::cancelled_owned) themselves
///   (they only build the future), and a poll of the future once the token is
///   cancelled (one load).
/// - Lock-free: [`cancel`](CancellationToken::cancel) and the first poll of a
///   future of an uncancelled token. Setting a flag is a lock-free queue
///   operation, and the visit of the descendants reads each registry with
///   the map's iterator, which restarts a bucket whose walk was disturbed
///   by a concurrent update. A `cancel` never waits for another `cancel`:
///   each walks the subtree on its own. The time it takes grows with the
///   number of descendants, whether or not they were cancelled already.
/// - Neither wait-free nor lock-free:
///   [`child_token`](CancellationToken::child_token) and dropping the last
///   owner of a node. They insert into or remove from the parent's registry
///   (a CAS that retries when another thread changed the same bucket), and
///   the registry is a map that grows past 48 children: a write that meets
///   the resize in progress waits for the thread resizing it.
///
/// Waking a task calls its [`Waker`](std::task::Waker), which is the
/// executor's and is not counted.
///
/// ```
/// use nuclei::sync::CancellationToken;
/// use std::future::Future;
/// use std::pin::pin;
/// use std::task::{Context, Poll, Waker};
///
/// let token = CancellationToken::new();
/// let clone = token.clone();
/// let mut cx = Context::from_waker(Waker::noop());
/// let mut cancelled = pin!(clone.cancelled());
/// assert_eq!(cancelled.as_mut().poll(&mut cx), Poll::Pending);
/// token.cancel();
/// assert_eq!(cancelled.as_mut().poll(&mut cx), Poll::Ready(()));
/// ```
pub struct CancellationToken {
    node: Arc<Node>,
}

impl std::panic::UnwindSafe for CancellationToken {}
impl std::panic::RefUnwindSafe for CancellationToken {}

impl fmt::Debug for CancellationToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancellationToken")
            .field("is_cancelled", &self.is_cancelled())
            .finish()
    }
}

impl Clone for CancellationToken {
    /// Creates a clone of the token which gets cancelled whenever the
    /// current token gets cancelled, and vice versa.
    fn clone(&self) -> Self {
        CancellationToken {
            node: Arc::clone(&self.node),
        }
    }
}

impl PartialEq for CancellationToken {
    /// Whether two tokens are equal in terms of their cancellation: cancelling
    /// one always cancels the other and vice versa. That holds for clones
    /// only, not for a parent and its child.
    fn eq(&self, other: &CancellationToken) -> bool {
        Arc::ptr_eq(&self.node, &other.node)
    }
}

impl Eq for CancellationToken {}

impl Hash for CancellationToken {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.node).hash(state);
    }
}

impl Default for CancellationToken {
    fn default() -> CancellationToken {
        CancellationToken::new()
    }
}

impl CancellationToken {
    /// Creates a new token in the non-cancelled state.
    pub fn new() -> CancellationToken {
        CancellationToken {
            node: Node::new(None),
        }
    }

    /// Creates a token which is cancelled whenever this token is. Unlike a
    /// clone, cancelling the child does not cancel this token.
    ///
    /// If this token is already cancelled, the child is returned cancelled.
    ///
    /// Neither wait-free nor lock-free: it inserts into this token's
    /// registry of children; see the [type docs](CancellationToken).
    ///
    /// ```
    /// use nuclei::sync::CancellationToken;
    ///
    /// let token = CancellationToken::new();
    /// let child = token.child_token();
    /// child.cancel();
    /// assert!(child.is_cancelled());
    /// assert!(!token.is_cancelled());
    ///
    /// token.cancel();
    /// assert!(token.child_token().is_cancelled());
    /// ```
    pub fn child_token(&self) -> CancellationToken {
        CancellationToken {
            node: Node::child(&self.node),
        }
    }

    /// Cancels this token and every token derived from it, waking all tasks
    /// waiting for cancellation.
    ///
    /// When the call returns, this token and every descendant that existed
    /// when it began are cancelled, even if another call cancelled some of
    /// them first: every call walks the whole subtree. Cancellation is not
    /// atomic over the tree, though: a thread running alongside the call can
    /// see `true` from [`is_cancelled`](Self::is_cancelled) on one child and
    /// then `false` on another. See the [type docs](CancellationToken).
    ///
    /// Lock-free.
    ///
    /// ```
    /// use nuclei::sync::CancellationToken;
    ///
    /// let token = CancellationToken::new();
    /// let child = token.child_token();
    /// let grandchild = child.child_token();
    /// token.cancel();
    /// assert!(child.is_cancelled() && grandchild.is_cancelled());
    /// ```
    pub fn cancel(&self) {
        Node::cancel(&self.node);
    }

    /// Whether the token is cancelled. One atomic load.
    ///
    /// ```
    /// use nuclei::sync::CancellationToken;
    ///
    /// let token = CancellationToken::new();
    /// assert!(!token.is_cancelled());
    /// token.cancel();
    /// assert!(token.is_cancelled());
    /// ```
    pub fn is_cancelled(&self) -> bool {
        self.node.cancelled.is_set()
    }

    /// A future that completes when cancellation is requested.
    ///
    /// Equivalent to `async fn cancelled(&self)`. It completes at once if the
    /// token is already cancelled.
    ///
    /// # Cancel safety
    ///
    /// Cancel safe: dropping the future withdraws its wait and nothing else.
    ///
    /// ```
    /// use nuclei::sync::CancellationToken;
    /// use std::future::Future;
    /// use std::pin::pin;
    /// use std::task::{Context, Poll, Waker};
    ///
    /// let token = CancellationToken::new();
    /// token.cancel();
    /// let mut cx = Context::from_waker(Waker::noop());
    /// assert_eq!(pin!(token.cancelled()).poll(&mut cx), Poll::Ready(()));
    /// ```
    pub fn cancelled(&self) -> WaitForCancellationFuture<'_> {
        WaitForCancellationFuture {
            wait: self.node.cancelled.wait(),
        }
    }

    /// A future that completes when cancellation is requested, and owns the
    /// token.
    ///
    /// Equivalent to `async fn cancelled_owned(self)`. It completes at once
    /// if the token is already cancelled.
    ///
    /// # Cancel safety
    ///
    /// Cancel safe.
    pub fn cancelled_owned(self) -> WaitForCancellationFutureOwned {
        WaitForCancellationFutureOwned::new(self)
    }

    /// Creates a [`DropGuard`] for this token: it cancels the token (and its
    /// children) when dropped, unless disarmed.
    ///
    /// ```
    /// use nuclei::sync::CancellationToken;
    ///
    /// let token = CancellationToken::new();
    /// drop(token.clone().drop_guard());
    /// assert!(token.is_cancelled());
    /// ```
    pub fn drop_guard(self) -> DropGuard {
        DropGuard {
            token: self,
            armed: true,
        }
    }

    /// Creates a [`DropGuardRef`] for this token: the borrowed counterpart of
    /// [`drop_guard`](Self::drop_guard).
    pub fn drop_guard_ref(&self) -> DropGuardRef<'_> {
        DropGuardRef {
            token: self,
            armed: true,
        }
    }

    /// Runs `fut` to completion and returns its output, unless the token is
    /// cancelled first: then it returns `None` and drops `fut`.
    ///
    /// # Fairness
    ///
    /// On an already cancelled token this returns `None` without polling
    /// `fut`. After that, when `fut` completes and the token is cancelled
    /// together, the output of `fut` wins.
    ///
    /// # Cancel safety
    ///
    /// Cancel safe only if `fut` is.
    pub async fn run_until_cancelled<F>(&self, fut: F) -> Option<F::Output>
    where
        F: Future,
    {
        if self.is_cancelled() {
            None
        } else {
            RunUntilCancelled {
                cancellation: self.cancelled(),
                future: fut,
            }
            .await
        }
    }

    /// Like [`run_until_cancelled`](Self::run_until_cancelled), taking the
    /// token by value: the returned future owns it.
    pub async fn run_until_cancelled_owned<F>(self, fut: F) -> Option<F::Output>
    where
        F: Future,
    {
        self.run_until_cancelled(fut).await
    }
}

#[cfg(all(test, not(loom)))]
mod tests;
