//! The lists of live tasks: a shard keeps its tasks whose poll returned
//! pending, a home its bound tasks from their spawn. Intrusive and circular,
//! the nodes in the tasks' headers, touched only on the owner's thread (a
//! shard's runner or its last holder, a home's thread), so no atomics
//! (proofs/tla/teardown/Teardown.tla).

use core::cell::{Cell, UnsafeCell};
use core::sync::atomic::{AtomicPtr, Ordering};

/// A link of a node. Only the list's owner writes it, and any thread may read a node's `next`
/// (`is_listed` off the owner). So the owner's writes to a shared list, and the reads of other
/// threads, are relaxed atomic ones through [`AtomicPtr::from_ptr`] (the same plain load and
/// store on the targets that matter, and defined where they meet); the owner's own reads are plain
/// ones, which fold into the instructions around them, and so are a home's list's accesses, which
/// no other thread meets.
struct Ptr(UnsafeCell<*const Links>);

// SAFETY: see the type's docs: a write another thread may read is atomic.
unsafe impl Send for Ptr {}
// SAFETY: as above.
unsafe impl Sync for Ptr {}

impl Ptr {
    const fn null() -> Self {
        Self(UnsafeCell::new(core::ptr::null()))
    }

    #[inline(always)]
    fn atomic(&self) -> &AtomicPtr<Links> {
        // SAFETY: the cell holds a pointer, aligned as an atomic one, and is only accessed
        // atomically once shared.
        unsafe { AtomicPtr::from_ptr(self.0.get().cast()) }
    }

    #[inline(always)]
    fn get(&self) -> *const Links {
        self.atomic().load(Ordering::Relaxed)
    }

    /// The list owner's read: plain, which folds into a compare of memory where an atomic one
    /// cannot. Only the owner writes a link, so its own reads meet no write.
    #[inline(always)]
    fn get_owner(&self) -> *const Links {
        // SAFETY: the owner's thread writes the link, and it is the one reading.
        unsafe { *self.0.get() }
    }

    /// A plain write.
    ///
    /// # Safety
    ///
    /// No other thread reads or writes the link meanwhile.
    #[inline(always)]
    unsafe fn set_plain(&self, to: *const Links) {
        // SAFETY: the caller's contract.
        unsafe { *self.0.get() = to };
    }

    #[inline(always)]
    fn set(&self, to: *const Links) {
        self.atomic().store(to.cast_mut(), Ordering::Relaxed);
    }
}

/// A task's place in a list; not on any while `next` is null.
pub(crate) struct Links {
    prev: Cell<*const Links>,
    next: Ptr,
}

impl Links {
    pub(crate) const fn new() -> Self {
        Self {
            prev: Cell::new(core::ptr::null()),
            next: Ptr::null(),
        }
    }

    #[inline(always)]
    pub(crate) fn is_listed(&self) -> bool {
        !self.next.get().is_null()
    }

    /// [`is_listed`](Self::is_listed) for the thread that polls the task, which no other thread
    /// lists or unlists meanwhile (a task is polled by one thread at a time, and a list edit of
    /// it is made by that thread or posted after its poll), as a plain read: the per-poll check.
    ///
    /// # Safety
    ///
    /// The caller runs the task's poll, and has posted no edit of it yet.
    #[inline(always)]
    pub(crate) unsafe fn is_listed_in_poll(&self) -> bool {
        !self.next.get_owner().is_null()
    }

    /// [`unlink`](Self::unlink) for a list no other thread reads: a home's, whose tasks are
    /// bound to its thread and are never looked at from another. Plain writes, the per-task cost
    /// of a bound task.
    ///
    /// # Safety
    ///
    /// As `unlink`, and the list is a home's.
    #[inline(always)]
    pub(crate) unsafe fn unlink_home(&self) {
        // SAFETY: the caller's contract: both neighbours are live nodes of the same list, which
        // only this thread touches.
        unsafe {
            let (prev, next) = (self.prev.get(), self.next.get_owner());
            (*prev).next.set_plain(next);
            (*next).prev.set(prev);
            self.next.set_plain(core::ptr::null());
        }
    }

    /// Takes the task off its list.
    ///
    /// # Safety
    ///
    /// The task is listed, and this is its list owner's thread.
    #[inline(always)]
    pub(crate) unsafe fn unlink(&self) {
        let (prev, next) = (self.prev.get(), self.next.get_owner());
        // SAFETY: the caller's contract: both neighbours are live nodes of the same list.
        unsafe {
            (*prev).next.set(next);
            (*next).prev.set(prev);
        }
        self.next.set(core::ptr::null());
    }
}

/// A list of tasks; its head is a sentinel that points at itself once the list is first used.
pub(crate) struct TaskList {
    head: Links,
}

// SAFETY: the list's links are written only on its owner's thread (see the module docs) and are
// atomic where another thread may read them, so those reads are defined.
unsafe impl Send for TaskList {}
// SAFETY: as above.
unsafe impl Sync for TaskList {}

impl TaskList {
    pub(crate) const fn new() -> Self {
        Self { head: Links::new() }
    }

    /// [`push`](Self::push) for a home's list (see [`Links::unlink_home`]).
    ///
    /// # Safety
    ///
    /// As `push`, and the list is a home's.
    #[inline(always)]
    pub(crate) unsafe fn push_home(&self, task: &Links) {
        let head: *const Links = &self.head;
        // SAFETY: a home's list is touched by its thread only.
        unsafe {
            let mut first = self.head.next.get_owner();
            if first.is_null() {
                first = head;
            }
            task.prev.set(head);
            task.next.set_plain(first);
            (*first).prev.set(task);
            self.head.next.set_plain(task);
        }
    }

    /// Puts `task`, on no list, at the front.
    ///
    /// # Safety
    ///
    /// The owner's thread; the list does not move once used (it lives in an `Arc`).
    #[inline(always)]
    pub(crate) unsafe fn push(&self, task: &Links) {
        let head: *const Links = &self.head;
        let mut first = self.head.next.get_owner();
        if first.is_null() {
            first = head;
        }
        task.prev.set(head);
        task.next.set(first);
        // SAFETY: `first` is the head or a live node of this list.
        unsafe { (*first).prev.set(task) };
        self.head.next.set(task);
    }

    /// Cancels every task on the list, on the owner's thread: an idle one's future is dropped here,
    /// at once; one whose runnable is out (woken, its push on its way, or queued where its owner
    /// drops it) is closed and stays listed until that runnable is dropped. Returns how many it
    /// dropped. Dropping a future runs user code, which may cancel, wake or end other tasks of the
    /// list: the walk takes the first task each time, and moves the ones it only closed to a list
    /// of its own, put back before it returns.
    ///
    /// # Safety
    ///
    /// The owner's thread.
    pub(crate) unsafe fn cancel_all(&self) -> usize {
        let waiting = TaskList::new();
        let _back = Back {
            from: &waiting,
            to: self,
        };
        let mut dropped = 0;
        while let Some(links) = self.first() {
            // SAFETY: a listed task is live, and the caller is the list's owner.
            match unsafe { super::close_listed(links) } {
                Some(runnable) => {
                    // Its future is dropped here, and the task leaves the list with it.
                    drop(runnable);
                    dropped += 1;
                }
                None => {
                    // SAFETY: the task is listed here; `waiting` outlives `_back`'s move.
                    unsafe {
                        links.unlink();
                        waiting.push(links);
                    }
                }
            }
        }
        dropped
    }

    /// Takes every task off the list without touching the tasks' neighbours' fates: for a shard that
    /// goes, so the drops of runnables still on their way, which may come on any thread once the
    /// shard let go of its inbox, find their tasks on no list.
    ///
    /// # Safety
    ///
    /// The owner's thread.
    pub(crate) unsafe fn forget_all(&self) {
        while let Some(links) = self.first() {
            // SAFETY: listed, on the owner's thread.
            unsafe { links.unlink() };
        }
    }

    /// How many tasks are on it.
    ///
    /// # Safety
    ///
    /// The owner's thread.
    pub(crate) unsafe fn len(&self) -> usize {
        let head: *const Links = &self.head;
        let mut count = 0;
        let mut at = self.head.next.get_owner();
        while !at.is_null() && !core::ptr::eq(at, head) {
            count += 1;
            // SAFETY: a node of the list is live.
            at = unsafe { (*at).next.get_owner() };
        }
        count
    }

    /// The first task, if any.
    fn first(&self) -> Option<&Links> {
        let first = self.head.next.get_owner();
        // SAFETY: a node of the list is live while it is listed.
        (!first.is_null() && !core::ptr::eq(first, &self.head)).then(|| unsafe { &*first })
    }

    /// Whether no task is on the list.
    ///
    /// # Safety
    ///
    /// The owner's thread.
    #[inline(always)]
    pub(crate) unsafe fn is_empty(&self) -> bool {
        let first = self.head.next.get_owner();
        first.is_null() || core::ptr::eq(first, &self.head)
    }
}

/// Moves every task of `from` back to `to`, also when the walk unwinds.
struct Back<'a> {
    from: &'a TaskList,
    to: &'a TaskList,
}

impl Drop for Back<'_> {
    fn drop(&mut self) {
        while let Some(links) = self.from.first() {
            // SAFETY: listed on `from`, and both lists belong to this thread.
            unsafe {
                links.unlink();
                self.to.push(links);
            }
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::{Links, TaskList};

    fn order(list: &TaskList) -> Vec<*const Links> {
        let head: *const Links = &list.head;
        let mut out = Vec::new();
        let mut at = list.head.next.get_owner();
        while !at.is_null() && at != head {
            out.push(at);
            // SAFETY: every node walked is live in the test.
            at = unsafe { (*at).next.get_owner() };
        }
        out
    }

    fn ptr(links: &Links) -> *const Links {
        links
    }

    #[test]
    fn push_unlink_and_repush() {
        let list = Box::new(TaskList::new());
        let nodes: Vec<Box<Links>> = (0..3).map(|_| Box::new(Links::new())).collect();
        // SAFETY: the test is the only thread, and the boxes do not move.
        unsafe {
            assert!(list.is_empty());
            for node in &nodes {
                assert!(!node.is_listed());
                list.push(node);
                assert!(node.is_listed());
            }
            assert!(!list.is_empty());
            let (a, b, c) = (ptr(&nodes[0]), ptr(&nodes[1]), ptr(&nodes[2]));
            assert_eq!(order(&list), [c, b, a]);

            nodes[1].unlink();
            assert!(!nodes[1].is_listed());
            assert_eq!(order(&list), [c, a]);

            nodes[2].unlink();
            assert!(!nodes[2].is_listed());
            assert_eq!(order(&list), [a]);

            nodes[0].unlink();
            assert!(!nodes[0].is_listed());
            assert!(list.is_empty());
            assert!(order(&list).is_empty());

            list.push(&nodes[1]);
            assert_eq!(order(&list), [ptr(&nodes[1])]);
            nodes[1].unlink();
            assert!(list.is_empty());
        }
    }

    #[test]
    fn unlink_middle_last_then_first() {
        let list = Box::new(TaskList::new());
        let nodes: Vec<Box<Links>> = (0..3).map(|_| Box::new(Links::new())).collect();
        // SAFETY: as above.
        unsafe {
            for node in &nodes {
                list.push(node);
            }
            let (a, b, c) = (ptr(&nodes[0]), ptr(&nodes[1]), ptr(&nodes[2]));
            nodes[0].unlink();
            assert_eq!(order(&list), [c, b]);
            nodes[2].unlink();
            assert_eq!(order(&list), [b]);
            assert!(!nodes[0].is_listed() && !nodes[2].is_listed() && nodes[1].is_listed());
            nodes[1].unlink();
            assert!(list.is_empty());
            let _ = a;
        }
    }

    #[test]
    fn len_and_forget_all() {
        let list = Box::new(TaskList::new());
        let nodes: Vec<Box<Links>> = (0..4).map(|_| Box::new(Links::new())).collect();
        // SAFETY: as above.
        unsafe {
            assert_eq!(list.len(), 0);
            list.forget_all();
            for node in &nodes {
                list.push(node);
            }
            assert_eq!(list.len(), 4);
            nodes[1].unlink();
            assert_eq!(list.len(), 3);
            list.forget_all();
            assert_eq!(list.len(), 0);
            assert!(list.is_empty());
            assert!(nodes.iter().all(|node| !node.is_listed()));
            list.push(&nodes[2]);
            assert_eq!(list.len(), 1);
        }
    }
}
