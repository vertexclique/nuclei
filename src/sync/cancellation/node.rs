//! The private tree behind a cancellation token: a node holds the cancelled
//! flag, a strong handle to its parent and a registry of weak handles to its
//! children, and freeing a node walks up its ancestors in a loop.
//!
//! The ordering argument for `Node::child` and `Node::cancel` is in the
//! [module docs](super).

use super::{Arc, Weak};
use crate::sync::OnceCell;
use crate::sync::event::Event;
use crate::sync::internal::{Ordering, fence};

/// Ids key a parent's registry. They only need to differ among the live
/// children of one parent, and a global counter does that without a lock.
/// (Under loom this is a plain counter outside the model: it orders nothing.)
fn next_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    // Relaxed: the id is a name; nothing is published through it.
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The children of one node: id to weak handle.
///
/// The real registry is a lock-free map; loom cannot instrument it, so under
/// `--cfg loom` it is an ordered map behind a loom `Mutex` with the same
/// three operations.
pub(super) struct Children {
    #[cfg(not(loom))]
    map: kovan_map::HashMap<u64, Weak<Node>>,
    #[cfg(loom)]
    map: loom::sync::Mutex<std::collections::BTreeMap<u64, Weak<Node>>>,
}

impl Children {
    /// An empty registry. The real map starts small: most nodes never get a
    /// child, and the map grows when one gets many.
    fn new() -> Self {
        #[cfg(not(loom))]
        {
            // 64 is the smallest table the map accepts.
            Self {
                map: kovan_map::HashMap::with_capacity(64),
            }
        }
        #[cfg(loom)]
        {
            Self {
                map: loom::sync::Mutex::new(std::collections::BTreeMap::new()),
            }
        }
    }

    fn insert(&self, id: u64, child: Weak<Node>) {
        #[cfg(not(loom))]
        {
            self.map.insert(id, child);
        }
        #[cfg(loom)]
        {
            let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
            map.insert(id, child);
        }
    }

    fn remove(&self, id: u64) {
        #[cfg(not(loom))]
        {
            self.map.remove(&id);
        }
        #[cfg(loom)]
        {
            let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
            map.remove(&id);
        }
    }

    /// Appends every child that is still alive to `out`.
    ///
    /// Upgrading is all this does while the map is walked: a child that
    /// dies meanwhile removes itself from the map, which is not done from
    /// inside the walk because the caller owns the handles it gets.
    fn collect(&self, out: &mut Vec<Arc<Node>>) {
        #[cfg(not(loom))]
        {
            out.extend(self.map.iter().filter_map(|(_, child)| child.upgrade()));
        }
        #[cfg(loom)]
        {
            let weak: Vec<Weak<Node>> = {
                let map = self.map.lock().unwrap_or_else(|e| e.into_inner());
                map.values().cloned().collect()
            };
            out.extend(weak.iter().filter_map(Weak::upgrade));
        }
    }

    #[cfg(all(test, not(loom)))]
    pub(super) fn len(&self) -> usize {
        self.map.len()
    }
}

/// One node of the tree; a [`CancellationToken`](super::CancellationToken) is
/// an `Arc` of it.
pub(super) struct Node {
    /// Set when the node is cancelled; wakes the waiters.
    pub(super) cancelled: Event,
    /// Strong, so the chain of ancestors outlives the descendants.
    parent: Option<Arc<Node>>,
    id: u64,
    /// Weak handles to the children, created with the first child.
    pub(super) children: OnceCell<Children>,
}

impl Node {
    pub(super) fn new(parent: Option<Arc<Node>>) -> Arc<Node> {
        Arc::new(Node {
            cancelled: Event::new(),
            parent,
            id: next_id(),
            children: OnceCell::new(),
        })
    }

    /// A new child of `parent`, cancelled if `parent` is.
    pub(super) fn child(parent: &Arc<Node>) -> Arc<Node> {
        let child = Node::new(Some(Arc::clone(parent)));
        parent
            .children
            .get_or_init_racy(Children::new)
            .insert(child.id, Arc::downgrade(&child));
        // The child's half of the handshake in the module docs: the insert
        // above (and the registry's creation) is before this fence, the read
        // of the parent's flag after it.
        fence(Ordering::SeqCst);
        if parent.cancelled.is_set() {
            Node::cancel(&child);
        }
        child
    }

    /// Cancels `this` and every descendant, and returns with all of them
    /// cancelled.
    ///
    /// The walk covers the whole subtree on every call: a node some other
    /// call already marked is not marked again but is still descended into,
    /// because the call that marked it may still be on its way down, and this
    /// call must not return before the subtree is cancelled.
    pub(super) fn cancel(this: &Arc<Node>) {
        if !this.cancelled.is_set() {
            this.cancelled.set();
        }
        // Neither vector allocates until a node has a child: cancelling a
        // token that has none costs the flag and one fence.
        let mut stack = Vec::new();
        // Children found at the node being visited; the handles are dropped
        // only after the registry walk has ended.
        let mut found = Vec::new();
        this.mark_children(&mut stack, &mut found);
        while let Some(node) = stack.pop() {
            node.mark_children(&mut stack, &mut found);
        }
    }

    /// Visits a node whose flag is set: marks each of its children that is
    /// not marked yet, and pushes every child on `stack` to be visited in
    /// turn, marked before or not.
    fn mark_children(&self, stack: &mut Vec<Arc<Node>>, found: &mut Vec<Arc<Node>>) {
        // The canceller's half of the handshake in the module docs: the flag
        // of this node is set before this fence (by this call or by an
        // earlier one), the registry is read after it.
        fence(Ordering::SeqCst);
        let Some(children) = self.children.get() else {
            return;
        };
        children.collect(found);
        for child in found.drain(..) {
            if !child.cancelled.is_set() {
                child.cancelled.set();
            }
            stack.push(child);
        }
    }

    /// Removes this node from its parent's registry and hands the parent over.
    fn detach(&mut self) -> Option<Arc<Node>> {
        let parent = self.parent.take()?;
        if let Some(children) = parent.children.get() {
            children.remove(self.id);
        }
        Some(parent)
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        // Iterative: a node that held the last handle of its parent frees the
        // parent here and moves on to the grandparent, so the depth of the
        // tree never becomes the depth of the stack.
        let mut parent = self.detach();
        while let Some(handle) = parent {
            parent = match Arc::into_inner(handle) {
                // The node is dropped at the end of the arm with no parent
                // left, so its own drop only frees its fields.
                Some(mut node) => node.detach(),
                None => None,
            };
        }
    }
}
