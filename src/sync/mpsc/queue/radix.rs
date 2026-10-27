//! The unbounded channel's queue (and a bounded one's past the ring's size):
//! leaves of cells found through a radix directory, wait-free for the senders
//! and the receiver both, with kovan reclaiming what the receiver passed.
//!
//! Position `p` is cell `p % CELLS` of leaf `p / CELLS`. The leaves hang under
//! a tree of inner nodes of `FAN` children each; the root's level says how
//! many leaves it spans (`FAN.pow(level)`). A send claims its position with one
//! fetch-add, grows the root by a level while the position lies past it (a new
//! root whose first child is the old one, swapped in once), then descends one
//! level at a time to its leaf, making a missing node with one
//! compare-and-swap: a swap it loses was won by another send, whose node it
//! takes. So a send takes at most `MAX_LEVEL` growths and `MAX_LEVEL` levels,
//! whatever other threads do. It writes its value and marks the cell ready.
//!
//! The receiver reads its cell's ready mark and nothing else another thread
//! writes. Done with a leaf, it finds the next one the same way (without
//! making anything: a leaf no send made yet means nothing is there), then
//! retires the leaf it leaves and every inner node whose span ended with it.
//! No send ever descends into a span the receiver finished: its own position
//! is in no such span, and a root that span is no longer the root. So every
//! node a send loads was reachable when it loaded it, which is what kovan's
//! guard protects (`kovan::Atomic::load`). A retired node holds no value and
//! is freed through a type-erased header that knows only its layout, so the
//! queue needs no `T: 'static`.
//!
//! The ready mark's store and the receiver's loads are sequentially
//! consistent: this queue's half of the channel's wake handshake (see `chan`).
//! proofs/tla/radix checks these steps, the root's growth and the
//! reclamation.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ptr;

use kovan::{
    Atomic, CachePadded, Guard, Reclaimable, RetiredNode, Shared as Ptr, alloc_node_uninit,
    free_node, pin, retire_reclaimable,
};

use crate::sync::internal::{AtomicBool, AtomicU64, Ordering};

/// Cells per leaf.
pub(super) const CELLS: u64 = 32;
/// Children per inner node, as bits of a leaf's number.
const FAN_BITS: u32 = 6;
const FAN: usize = 1 << FAN_BITS;
/// The highest level a root reaches: `FAN.pow(MAX_LEVEL)` leaves span every
/// 64-bit position.
const MAX_LEVEL: u32 = 10;

/// Leaves spanned by a node of `level`.
const fn span(level: u32) -> u64 {
    if level >= MAX_LEVEL {
        u64::MAX
    } else {
        1 << (FAN_BITS * level)
    }
}

struct Cell<T> {
    /// The value is written: stored once by its send.
    ready: AtomicBool,
    value: UnsafeCell<MaybeUninit<T>>,
}

/// The type-erased head of every node, the one kovan retires: kovan's
/// [`RetiredNode`] and the function, made for the node's type, that frees
/// its memory and touches nothing else (see the module).
#[repr(C)]
struct Header {
    retired: RetiredNode,
    free: unsafe fn(*mut Header),
}

// SAFETY: `RetiredNode` is the first field of the `#[repr(C)]` `Header`, and
// `Header` the first of `Leaf<T>` and `Inner`, both `#[repr(C)]`, so a node's
// address is its retired node's. `dealloc` only frees memory and cannot panic.
unsafe impl Reclaimable for Header {
    fn retired_node(&self) -> &RetiredNode {
        &self.retired
    }

    fn retired_node_mut(&mut self) -> &mut RetiredNode {
        &mut self.retired
    }

    unsafe fn dealloc(node: *mut Self) {
        // SAFETY: kovan calls this once, when no thread can reach the node;
        // `free` was set for its type when it was made.
        unsafe { ((*node).free)(node) }
    }
}

#[repr(C)]
struct Leaf<T> {
    header: Header,
    cells: [Cell<T>; CELLS as usize],
}

#[repr(C)]
struct Inner {
    header: Header,
    /// 1: the children are leaves; above, inner nodes of the level below.
    level: u32,
    children: [Atomic<Header>; FAN],
}

/// Frees a leaf of `T`s without touching any `T`.
///
/// # Safety
///
/// `header` heads a `Leaf<T>` from [`Leaf::alloc`] that no thread can reach
/// any more and whose cells hold no value.
unsafe fn free_leaf<T>(header: *mut Header) {
    // SAFETY: the caller's contract; the values sit in `MaybeUninit`s, so the
    // leaf has no drop glue of its own, and `free_node` drops nothing.
    unsafe { free_node(header.cast::<Leaf<T>>()) }
}

/// Frees an inner node, its children left alone.
///
/// # Safety
///
/// `header` heads an `Inner` from [`Inner::alloc`] that no thread can reach.
unsafe fn free_inner(header: *mut Header) {
    // SAFETY: the caller's contract; the children are pointers, not owned.
    unsafe { free_node(header.cast::<Inner>()) }
}

impl<T> Leaf<T> {
    fn alloc() -> *mut Header {
        let p = alloc_node_uninit::<Self>().cast::<Self>();
        // SAFETY: a fresh allocation; every field but the values is written,
        // and a value is a `MaybeUninit`, valid uninitialised.
        unsafe {
            (&raw mut (*p).header).write(Header {
                retired: RetiredNode::new(),
                free: free_leaf::<T>,
            });
            let cells = (&raw mut (*p).cells).cast::<Cell<T>>();
            for i in 0..CELLS as usize {
                (&raw mut (*cells.add(i)).ready).write(AtomicBool::new(false));
            }
        }
        p.cast()
    }
}

impl Inner {
    fn alloc(level: u32) -> *mut Header {
        let p = alloc_node_uninit::<Self>().cast::<Self>();
        // SAFETY: a fresh allocation, every field written.
        unsafe {
            (&raw mut (*p).header).write(Header {
                retired: RetiredNode::new(),
                free: free_inner,
            });
            (&raw mut (*p).level).write(level);
            let children = (&raw mut (*p).children).cast::<Atomic<Header>>();
            for i in 0..FAN {
                children.add(i).write(Atomic::null());
            }
        }
        p.cast()
    }
}

/// The receiver's end.
struct Head<T> {
    /// The next position to take: written by the receiver only, read by
    /// [`Radix::len`].
    pos: AtomicU64,
    /// The leaf of that position and its number; the receiver's alone.
    leaf: UnsafeCell<(*mut Leaf<T>, u64)>,
}

/// The queue.
pub(in crate::sync::mpsc) struct Radix<T> {
    /// The next position a send claims.
    tail: CachePadded<AtomicU64>,
    /// The root: an inner node, replaced only by a higher one.
    root: CachePadded<Atomic<Header>>,
    head: CachePadded<Head<T>>,
    /// The queue owns the values in its cells.
    _values: PhantomData<T>,
}

// SAFETY: values move between threads through the queue, which is all
// `T: Send` allows; the receiver's end is reached by one thread at a time
// (`try_pop`'s contract), and every shared word is an atomic.
unsafe impl<T: Send> Send for Radix<T> {}
// SAFETY: as above.
unsafe impl<T: Send> Sync for Radix<T> {}

impl<T> Radix<T> {
    pub(in crate::sync::mpsc) fn new() -> Self {
        Self::starting_at(0)
    }

    /// A queue whose first position is `start` (tests reach the root's
    /// growth this way without sending billions of values).
    pub(super) fn starting_at(start: u64) -> Self {
        let queue = Self {
            tail: CachePadded::new(AtomicU64::new(start)),
            root: CachePadded::new(Atomic::new(Inner::alloc(1))),
            head: CachePadded::new(Head {
                pos: AtomicU64::new(start),
                leaf: UnsafeCell::new((ptr::null_mut(), 0)),
            }),
            _values: PhantomData,
        };
        let guard = pin();
        let first = queue.leaf(start / CELLS, true, &guard);
        // SAFETY: nobody else reaches the queue yet.
        unsafe { *queue.head.leaf.get() = (first.cast_mut(), start / CELLS) };
        drop(guard);
        queue
    }

    /// Appends `value`. Wait-free: see the module.
    pub(in crate::sync::mpsc) fn push(&self, value: T) {
        let guard = pin();
        let pos = self.tail.fetch_add(1, Ordering::AcqRel);
        let leaf = self.leaf(pos / CELLS, true, &guard);
        // SAFETY: `leaf` was made or found on this position's path, which the
        // receiver has not passed (this position is not written yet), so it
        // was reachable when loaded and the guard keeps it.
        let cell = unsafe { &(*leaf).cells[(pos % CELLS) as usize] };
        // SAFETY: the position is this send's alone, and the receiver reads
        // the value only after it reads the mark stored below.
        unsafe { cell.value.get().write(MaybeUninit::new(value)) };
        // SeqCst: the first half of the channel's wake handshake.
        cell.ready.store(true, Ordering::SeqCst);
    }

    /// The leaf of number `n`, made where missing if `make`, else `null` when
    /// a node on its path is not there yet.
    fn leaf(&self, n: u64, make: bool, guard: &Guard) -> *const Leaf<T> {
        let Some(mut node) = self.root_for(n, make, guard) else {
            return ptr::null();
        };
        loop {
            // SAFETY: an inner node loaded while reachable (the root, or a
            // child on the path of a leaf the receiver has not passed).
            let inner = unsafe { &*node.cast::<Inner>() };
            let level = inner.level;
            let index = ((n >> (FAN_BITS * (level - 1))) as usize) & (FAN - 1);
            let child = Self::child(&inner.children[index], level - 1, make, guard);
            if child.is_null() || level == 1 {
                return child.cast();
            }
            node = child;
        }
    }

    /// The root, grown (if `make`) until it spans leaf `n`; `None` when it
    /// does not and `make` is false. At most `MAX_LEVEL` turns: each one that
    /// does not return saw the root's level rise.
    fn root_for(&self, n: u64, make: bool, guard: &Guard) -> Option<*const Header> {
        loop {
            let root = self.root.load(Ordering::SeqCst, guard);
            // SAFETY: the root is never retired while it is the root, and the
            // guard keeps one it loaded as the root.
            let level = unsafe { (*root.as_raw().cast::<Inner>()).level };
            if n < span(level) {
                return Some(root.as_raw());
            }
            if !make {
                return None;
            }
            let higher = Inner::alloc(level + 1);
            // SAFETY: unpublished; its first child is the root it would
            // replace, which spans leaves 0 to `span(level)`.
            unsafe { (*higher.cast::<Inner>()).children[0].store(root, Ordering::Relaxed) };
            // SAFETY: a fresh node, swapped in once.
            let higher_ptr = unsafe { Ptr::from_raw(higher) };
            if self
                .root
                .compare_exchange(root, higher_ptr, Ordering::SeqCst, Ordering::SeqCst, guard)
                .is_err()
            {
                // Another send grew it: this one was never seen.
                // SAFETY: unpublished; its child is not its own.
                unsafe { free_inner(higher) };
            }
        }
    }

    /// The child in `slot`, a node of `level` (0: a leaf), made if missing
    /// and `make`; null if missing and not.
    fn child(slot: &Atomic<Header>, level: u32, make: bool, guard: &Guard) -> *const Header {
        let found = slot.load(Ordering::SeqCst, guard);
        if !found.is_null() || !make {
            return found.as_raw();
        }
        let fresh = if level == 0 {
            Leaf::<T>::alloc()
        } else {
            Inner::alloc(level)
        };
        // SAFETY: a fresh node, and the null pointer the swap expects.
        let (fresh_ptr, none) = unsafe { (Ptr::from_raw(fresh), Ptr::from_raw(ptr::null_mut())) };
        if slot
            .compare_exchange(none, fresh_ptr, Ordering::SeqCst, Ordering::SeqCst, guard)
            .is_err()
        {
            // SAFETY: unpublished, and empty.
            unsafe { ((*fresh).free)(fresh) };
        }
        // Loaded, not taken from the swap, so the guard protects it.
        slot.load(Ordering::SeqCst, guard).as_raw()
    }

    /// Takes the oldest value, without waiting: `None` when there is none
    /// yet, or when the next one is still being written.
    ///
    /// # Safety
    ///
    /// The caller is the queue's one receiver: no two calls of this function
    /// on one queue run at once.
    pub(in crate::sync::mpsc) unsafe fn try_pop(&self) -> Option<T> {
        let pos = self.head.pos.load(Ordering::Relaxed);
        // SAFETY: the caller's contract: the head's leaf is ours.
        let (mut leaf, mut number) = unsafe { *self.head.leaf.get() };
        if pos / CELLS != number {
            // Used up: on to the next leaf, once a send made it.
            let guard = pin();
            let next = self.leaf(number + 1, false, &guard);
            if next.is_null() {
                return None;
            }
            // SAFETY: the caller's contract; `number` is the leaf left.
            unsafe { self.leave(number, &guard) };
            (leaf, number) = (next.cast_mut(), number + 1);
            // SAFETY: the caller's contract.
            unsafe { *self.head.leaf.get() = (leaf, number) };
        }
        // SAFETY: the head's leaf is retired by this end alone, and not yet.
        let cell = unsafe { &(*leaf).cells[(pos % CELLS) as usize] };
        // SeqCst: the receiver's look in the channel's wake handshake.
        if !cell.ready.load(Ordering::SeqCst) {
            return None;
        }
        // SAFETY: ready: its send wrote the value before the store this load
        // read; the position is passed below, so it is moved out once.
        let value = unsafe { cell.value.get().read().assume_init() };
        self.head.pos.store(pos + 1, Ordering::Relaxed);
        Some(value)
    }

    /// The receiver is done with leaf `number`: it retires that leaf and every
    /// inner node whose span ends with it, found on the leaf's path (still
    /// linked: the next leaf is on another, and a root it ends is a root no
    /// more, since that next leaf lies past it).
    ///
    /// # Safety
    ///
    /// The caller is the receiver, every value of leaf `number` is taken, and
    /// leaf `number + 1` exists.
    unsafe fn leave(&self, number: u64, guard: &Guard) {
        let root = self.root.load(Ordering::SeqCst, guard).as_raw();
        let mut node = root;
        let mut ended: [*mut Header; MAX_LEVEL as usize + 1] =
            [ptr::null_mut(); MAX_LEVEL as usize + 1];
        let mut count = 0;
        loop {
            // SAFETY: on the path of a leaf not retired yet: linked, and
            // retired by this end alone.
            let inner = unsafe { &*node.cast::<Inner>() };
            let level = inner.level;
            let index = ((number >> (FAN_BITS * (level - 1))) as usize) & (FAN - 1);
            // SAFETY: as above; this end alone retires these nodes.
            let child = unsafe { inner.children[index].load_unprotected(Ordering::Acquire, guard) }
                .as_raw();
            if level == 1 {
                ended[count] = child;
                count += 1;
                break;
            }
            // The child of `level - 1` ends with this leaf when the leaf is the
            // last of its span.
            if (number + 1).is_multiple_of(span(level - 1)) {
                ended[count] = child;
                count += 1;
            }
            node = child;
        }
        for &node in &ended[..count] {
            // SAFETY: every value under it is taken and every send into it
            // returned; no send descends into its span from now on. Retired
            // once: a span ends once.
            unsafe { retire_reclaimable(node) };
        }
    }

    /// How many values are queued, as a snapshot: a push in progress counts,
    /// a pop in progress does not.
    pub(in crate::sync::mpsc) fn len(&self) -> usize {
        let front = self.head.pos.load(Ordering::Acquire);
        let back = self.tail.load(Ordering::Acquire);
        usize::try_from(back.saturating_sub(front)).unwrap_or(usize::MAX)
    }
}

impl<T> Drop for Radix<T> {
    fn drop(&mut self) {
        // Nothing else reaches the queue: every push returned, so every
        // position claimed holds its value. The receiver retired every node
        // whose span ended before its leaf; what is left is the rest.
        let head = *self.head.pos.get_mut();
        let tail = *self.tail.get_mut();
        let (_, from) = *self.head.leaf.get_mut();
        let guard = pin();
        // SAFETY: as above, nothing else reaches the tree.
        let root = unsafe { self.root.load_unprotected(Ordering::Relaxed, &guard) }.as_raw();
        drop(guard);
        // SAFETY: the root and the nodes below it whose span reaches the
        // receiver's leaf are this queue's, retired by nobody.
        unsafe { free_tree::<T>(root, 0, from, head, tail) };
    }
}

/// Frees `node`, whose span starts at leaf `base`, and the nodes below it
/// that reach leaf `from` (the earlier ones were retired), dropping the values
/// of positions `head..tail` it holds.
///
/// # Safety
///
/// Nothing else reaches the tree, and every position `head..tail` holds its
/// value.
unsafe fn free_tree<T>(node: *mut Header, base: u64, from: u64, head: u64, tail: u64) {
    // SAFETY: the caller's contract; an inner node.
    let inner = unsafe { &*node.cast::<Inner>() };
    let level = inner.level;
    let child_span = span(level - 1);
    let guard = pin();
    for (i, slot) in inner.children.iter().enumerate() {
        let child_base = base.saturating_add(child_span.saturating_mul(i as u64));
        if child_base.saturating_add(child_span) <= from {
            continue;
        }
        // SAFETY: the caller's contract.
        let child = unsafe { slot.load_unprotected(Ordering::Relaxed, &guard) }.as_raw();
        if child.is_null() {
            continue;
        }
        if level == 1 {
            // SAFETY: a leaf of `T`s nothing else reaches.
            let leaf = unsafe { &mut *child.cast::<Leaf<T>>() };
            let start = child_base * CELLS;
            for pos in head.max(start)..tail.min(start + CELLS) {
                let cell = &mut leaf.cells[(pos - start) as usize];
                if *cell.ready.get_mut() {
                    // SAFETY: written and not taken: dropped once, here.
                    unsafe { cell.value.get_mut().assume_init_drop() };
                }
            }
            // SAFETY: no value is left in it and nothing reaches it.
            unsafe { free_leaf::<T>(child) };
        } else {
            // SAFETY: the caller's contract, passed on.
            unsafe { free_tree::<T>(child, child_base, from, head, tail) };
        }
    }
    drop(guard);
    // SAFETY: nothing reaches it, and its children are freed or retired.
    unsafe { free_inner(node) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    fn pop<T>(queue: &Radix<T>) -> Option<T> {
        // SAFETY: each test pops from one thread only.
        unsafe { queue.try_pop() }
    }

    #[test]
    fn values_come_out_in_order_across_leaves() {
        let queue = Radix::new();
        for i in 0..CELLS * 5 {
            queue.push(i);
        }
        assert_eq!(queue.len(), (CELLS * 5) as usize);
        for i in 0..CELLS * 5 {
            assert_eq!(pop(&queue), Some(i));
        }
        assert_eq!(pop(&queue), None);
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn a_queue_drained_at_each_leaf_end_goes_on_into_the_next() {
        let queue = Radix::new();
        for i in 0..CELLS * 3 {
            queue.push(i);
            assert_eq!(pop(&queue), Some(i));
            assert_eq!(pop(&queue), None, "taken once");
        }
    }

    /// Past the root's span (64 leaves at level 1), and again past the
    /// next one's, values still come out in order and the nodes the receiver
    /// passed are retired.
    #[test]
    fn the_root_grows_as_positions_pass_its_span() {
        let leaves_of_level_1 = span(1);
        let start = (leaves_of_level_1 - 2) * CELLS;
        let queue = Radix::starting_at(start);
        let n = CELLS * 6;
        for i in 0..n {
            queue.push(start + i);
        }
        for i in 0..n {
            assert_eq!(pop(&queue), Some(start + i));
        }
        let guard = pin();
        let root = queue.root.load(Ordering::SeqCst, &guard).as_raw();
        // SAFETY: the root is live.
        assert_eq!(unsafe { (*root.cast::<Inner>()).level }, 2);
        drop(guard);
        // From high up: a level well above the first.
        let far = span(4) * CELLS - 3;
        let queue = Radix::starting_at(far);
        for i in 0..8 {
            queue.push(far + i);
        }
        for i in 0..8 {
            assert_eq!(pop(&queue), Some(far + i));
        }
    }

    #[test]
    fn a_value_left_in_the_queue_is_dropped_once_with_it() {
        struct Counted(Arc<AtomicUsize>);
        impl Drop for Counted {
            fn drop(&mut self) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let start = (span(1) - 1) * CELLS;
        let queue = Radix::starting_at(start);
        for _ in 0..CELLS * 2 + 6 {
            queue.push(Counted(Arc::clone(&drops)));
        }
        for _ in 0..CELLS + 2 {
            assert!(pop(&queue).is_some());
        }
        assert_eq!(
            drops.load(std::sync::atomic::Ordering::SeqCst),
            (CELLS + 2) as usize
        );
        drop(queue);
        assert_eq!(
            drops.load(std::sync::atomic::Ordering::SeqCst),
            (CELLS * 2 + 6) as usize
        );
    }

    /// Values that borrow from the test's frame: no `T: 'static`.
    #[test]
    fn values_that_borrow_go_through() {
        let words = [String::from("a"), String::from("b")];
        let queue = Radix::new();
        for _ in 0..CELLS * 2 + 1 {
            queue.push(&words[0]);
            queue.push(&words[1]);
        }
        let mut got = 0;
        while let Some(w) = pop(&queue) {
            assert!(w == "a" || w == "b");
            got += 1;
        }
        assert_eq!(got, CELLS * 4 + 2);
        drop(queue);
        kovan::flush();
    }

    /// Pushes from several threads while the receiver takes, across leaves
    /// and a growth of the root: every value once, each pusher's in order.
    /// Under Miri few enough to finish, still crossing leaves and the growth.
    #[test]
    fn concurrent_pushes_each_arrive_once_and_in_order() {
        const PUSHERS: u64 = 4;
        let each: u64 = if cfg!(miri) { 3 * CELLS } else { 20_000 };
        let start = (span(1) - 1) * CELLS;
        let queue = Arc::new(Radix::<(u64, u64)>::starting_at(start));
        let pushers: Vec<_> = (0..PUSHERS)
            .map(|p| {
                let queue = Arc::clone(&queue);
                std::thread::spawn(move || {
                    for i in 0..each {
                        queue.push((p, i));
                    }
                })
            })
            .collect();
        let mut next = [0u64; PUSHERS as usize];
        let mut got = 0;
        while got < PUSHERS * each {
            match pop(&queue) {
                Some((p, i)) => {
                    assert_eq!(i, next[p as usize], "pusher {p} out of order");
                    next[p as usize] += 1;
                    got += 1;
                }
                None => std::hint::spin_loop(),
            }
        }
        for p in pushers {
            p.join().unwrap();
        }
        assert_eq!(pop(&queue), None);
    }
}
