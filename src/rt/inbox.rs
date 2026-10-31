//! A shard's inbox: the runnables other threads hand the shard, in a queue
//! linked through the tasks themselves, so a push allocates nothing. The
//! queue holds any [`Linked`] item the same way (a `JoinSet`'s woken
//! members, linked through their wakers).
//!
//! Each task has one [`Link`] in its header, and a task is in a run queue at
//! most once (its scheduled bit), so its link is free for this queue
//! whenever it is pushed. Any thread pushes: it clears the task's link,
//! swaps the task in as the queue's head, then links the old head to it.
//! One thread at a time takes, the holder of the taker's role: the shard,
//! which holds it from its making; once the shard stopped, whoever drains it
//! (see [`Inbox::try_take`]). The taker walks from its own end, past a stub
//! node of the queue's own, and hands a task out only once it read the
//! task's link; the last task goes out only after the stub is pushed behind
//! it, so the taker's end never points at a task it gave away.
//!
//! Between a push's swap and its link the queue is busy: not empty, but with
//! nothing to take yet ([`Pop::Busy`]). A head on the stub says the queue is
//! empty ([`Inbox::is_empty`]) to any thread but while the taker puts the
//! stub back: then the stub is the head while a push that swapped in just
//! before it may still link the last task to its own, both behind the
//! taker's end. So the taker looks at both ends ([`Inbox::is_drained`]): that
//! is the look a shard makes once more after it says it parks. The swap and
//! the look at the head are both sequentially consistent, so either the look
//! sees the push or the pusher sees the park. proofs/tla/inbox checks the
//! queue and its park at the grain of these steps, and proofs/tla/park
//! (`ShardStop`) the taker's role once the shard stopped.

use core::marker::PhantomData;
use core::ptr::{self, NonNull};

use super::task::Runnable;
use crate::sync::internal::{AtomicBool, AtomicPtr, Ordering, UnsafeCell};

/// An item an inbox holds through the [`Link`] it carries: owned, in the
/// queue at most once, its link free while it is out of it.
///
/// # Safety
///
/// `into_link` gives up the item as a pointer to its link, valid until
/// `from_link` takes the item back from it; an item may be taken on another
/// thread than the one that pushed it.
pub(crate) unsafe trait Linked {
    /// The item, given up as its link.
    fn into_link(self) -> NonNull<Link>;

    /// The item back from its link.
    ///
    /// # Safety
    ///
    /// `link` came from `into_link` and was not taken back since.
    unsafe fn from_link(link: NonNull<Link>) -> Self;
}

// SAFETY: a runnable is its task's reference, its link the header's; the
// tasks an inbox holds are `Send` ones, or a shard's bound tasks, which only
// its own thread takes.
unsafe impl Linked for Runnable {
    fn into_link(self) -> NonNull<Link> {
        Runnable::into_link(self)
    }

    unsafe fn from_link(link: NonNull<Link>) -> Self {
        // SAFETY: the caller's contract.
        unsafe { Runnable::from_link(link) }
    }
}

/// An item's place in an inbox.
pub(crate) struct Link {
    next: AtomicPtr<Link>,
}

impl Link {
    pub(crate) fn new() -> Self {
        Self {
            next: AtomicPtr::new(ptr::null_mut()),
        }
    }
}

/// What a take found.
pub(crate) enum Pop<T = Runnable> {
    Item(T),
    /// Nothing in the queue.
    Empty,
    /// A push is between its swap and its link: something is coming.
    Busy,
}

/// The queue.
pub(crate) struct Inbox<T: Linked = Runnable> {
    /// The pushers' end: the task pushed last, or the stub.
    head: AtomicPtr<Link>,
    /// The taker's end; only the role's holder reaches it.
    tail: UnsafeCell<*mut Link>,
    /// The queue's own node, boxed so it stays put as the inbox moves.
    stub: Box<Link>,
    /// The taker's role is held.
    taken: AtomicBool,
    items: PhantomData<T>,
}

// SAFETY: its items may move between threads (`Linked`), and the taker's
// end is reached only by the role's holder.
unsafe impl<T: Linked> Send for Inbox<T> {}
// SAFETY: as for `Send`.
unsafe impl<T: Linked> Sync for Inbox<T> {}

impl<T: Linked> Inbox<T> {
    /// An empty inbox, its taker's role held by its maker.
    pub(crate) fn new() -> Self {
        let stub = Box::new(Link::new());
        let at = ptr::from_ref::<Link>(&stub).cast_mut();
        Self {
            head: AtomicPtr::new(at),
            tail: UnsafeCell::new(at),
            stub,
            taken: AtomicBool::new(true),
            items: PhantomData,
        }
    }

    fn stub(&self) -> *mut Link {
        ptr::from_ref::<Link>(&self.stub).cast_mut()
    }

    /// Hands `item` to the queue.
    pub(crate) fn push(&self, item: T) {
        self.push_link(item.into_link());
    }

    fn push_link(&self, link: NonNull<Link>) {
        // SAFETY: the link is an item's (or the stub) and is in no queue now,
        // so nothing else writes it.
        unsafe { link.as_ref() }
            .next
            .store(ptr::null_mut(), Ordering::Relaxed);
        // SeqCst: against the shard's look at the head after it says it
        // parks (see the module).
        let prev = self.head.swap(link.as_ptr(), Ordering::SeqCst);
        // SAFETY: `prev` stays valid until the taker passes it, which it does
        // only once this link is stored. Release: the taker that reads it
        // sees the task's state as the pusher left it.
        unsafe { (*prev).next.store(link.as_ptr(), Ordering::Release) };
    }

    /// Whether the queue holds nothing, a push under way counting as
    /// something. Any thread may look; while the taker puts the stub back,
    /// this says empty with tasks still behind the taker's end, which only
    /// [`is_drained`](Self::is_drained) sees.
    pub(crate) fn is_empty(&self) -> bool {
        self.head.load(Ordering::SeqCst) == self.stub()
    }

    /// Whether the queue holds nothing, as its taker sees it: both ends on
    /// the stub. A push under way counts as something, and so do the tasks a
    /// pop left behind the taker's end as it put the stub back.
    ///
    /// # Safety
    ///
    /// The caller holds the taker's role.
    pub(crate) unsafe fn is_drained(&self) -> bool {
        // SAFETY: the role's holder alone reaches the taker's end.
        let tail = self.tail.with(|tail| unsafe { *tail });
        tail == self.stub() && self.is_empty()
    }

    /// Takes the next item.
    ///
    /// # Safety
    ///
    /// The caller holds the taker's role.
    pub(crate) unsafe fn pop(&self) -> Pop<T> {
        let stub = self.stub();
        // SAFETY: the role's holder alone reaches the taker's end.
        self.tail.with_mut(|tail| unsafe {
            let mut cur = *tail;
            let mut next = (*cur).next.load(Ordering::Acquire);
            if cur == stub {
                if next.is_null() {
                    return if self.is_empty() {
                        Pop::Empty
                    } else {
                        Pop::Busy
                    };
                }
                *tail = next;
                cur = next;
                next = (*cur).next.load(Ordering::Acquire);
            }
            if !next.is_null() {
                *tail = next;
                return Pop::Item(T::from_link(NonNull::new_unchecked(cur)));
            }
            if cur != self.head.load(Ordering::Acquire) {
                return Pop::Busy;
            }
            // `cur` is the last task: the stub goes behind it first, so the
            // taker's end moves off it before it is handed out.
            self.push_link(NonNull::new_unchecked(stub));
            next = (*cur).next.load(Ordering::Acquire);
            if next.is_null() {
                return Pop::Busy;
            }
            *tail = next;
            Pop::Item(T::from_link(NonNull::new_unchecked(cur)))
        })
    }

    /// Whether the queue is empty and nobody holds the taker's role: once a
    /// shard stopped, every item pushed so far was taken and handed out.
    pub(crate) fn is_idle(&self) -> bool {
        !self.taken.load(Ordering::SeqCst) && self.is_empty()
    }

    /// The taker's role, if nobody holds it.
    pub(crate) fn try_take(&self) -> bool {
        self.taken
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// Lets go of the taker's role.
    ///
    /// # Safety
    ///
    /// The caller holds it.
    pub(crate) unsafe fn release(&self) {
        self.taken.store(false, Ordering::SeqCst);
    }

    /// Hands every item in the queue to `each`, waiting out pushes under
    /// way.
    ///
    /// # Safety
    ///
    /// The caller holds the taker's role.
    pub(crate) unsafe fn drain(&self, mut each: impl FnMut(T)) {
        let mut spins = 0u32;
        loop {
            // SAFETY: the caller's contract.
            match unsafe { self.pop() } {
                Pop::Item(item) => each(item),
                Pop::Empty => return,
                // A pusher between two of its steps: it links next.
                Pop::Busy => {
                    spins += 1;
                    if spins.is_multiple_of(64) {
                        std::thread::yield_now();
                    } else {
                        core::hint::spin_loop();
                    }
                }
            }
        }
    }
}

impl<T: Linked> Drop for Inbox<T> {
    fn drop(&mut self) {
        // Nothing else reaches the inbox now: the role is as good as held.
        // What is left is dropped (a stopped shard drained it already).
        // SAFETY: exclusive access.
        unsafe { self.drain(drop) };
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::rt::task::{Schedule, ScheduleInfo};

    /// The schedule of a test's tasks: a wake drops the task.
    struct Ignore;

    // SAFETY: the runnable is not touched after the drop.
    unsafe impl Schedule for Ignore {
        fn schedule(&self, runnable: Runnable, _info: ScheduleInfo) {
            drop(runnable);
        }
    }

    type Log = Arc<Mutex<Vec<u32>>>;

    /// A task that, run, writes `n` in `log`.
    fn task(log: &Log, n: u32) -> Runnable {
        let log = Arc::clone(log);
        let (runnable, task) =
            crate::rt::task::spawn(async move { log.lock().unwrap().push(n) }, Ignore);
        task.detach();
        runnable
    }

    fn take(inbox: &Inbox) -> Option<Runnable> {
        // SAFETY: the test holds the role (the inbox's maker).
        match unsafe { inbox.pop() } {
            Pop::Item(r) => Some(r),
            Pop::Empty => None,
            Pop::Busy => panic!("no push is under way"),
        }
    }

    #[test]
    fn tasks_come_out_in_the_order_they_went_in() {
        let log = Log::default();
        let inbox: Inbox = Inbox::new();
        assert!(inbox.is_empty());
        inbox.push(task(&log, 1));
        assert!(!inbox.is_empty());
        inbox.push(task(&log, 2));
        inbox.push(task(&log, 3));
        while let Some(r) = take(&inbox) {
            r.run();
        }
        assert_eq!(*log.lock().unwrap(), [1, 2, 3]);
        assert!(inbox.is_empty());
        // A task taken goes in again at once, its link reused, again and again.
        let mut again = task(&log, 4);
        for _ in 0..3 {
            inbox.push(again);
            again = take(&inbox).expect("the task came back");
            assert!(take(&inbox).is_none());
        }
        inbox.push(task(&log, 5));
        inbox.push(again);
        while let Some(r) = take(&inbox) {
            r.run();
        }
        assert_eq!(*log.lock().unwrap(), [1, 2, 3, 5, 4]);
    }

    /// The state that lost a shard's wake: the taker put the stub back behind
    /// the last task while a push that swapped in just before it had not
    /// linked yet. The head is the stub, so the queue looks empty from any
    /// thread, with both tasks still in it; its taker sees them.
    #[test]
    fn a_push_racing_the_stub_put_back_is_seen_by_the_taker() {
        let log = Log::default();
        let inbox: Inbox = Inbox::new();
        let stub = inbox.stub();
        inbox.push(task(&log, 1));
        // SAFETY: the steps of a pop and of a push, one at a time, by the
        // only thread; the test holds the role.
        unsafe {
            // The pop: past the stub, onto the last task, the head on it.
            let last = inbox.head.load(Ordering::SeqCst);
            inbox.tail.with_mut(|tail| *tail = last);
            // The push swaps in before the taker puts the stub back.
            let pushed = task(&log, 2).into_link();
            pushed
                .as_ref()
                .next
                .store(ptr::null_mut(), Ordering::Relaxed);
            let prev = inbox.head.swap(pushed.as_ptr(), Ordering::SeqCst);
            assert_eq!(prev, last);
            // The taker puts the stub back, then finds the last task's link
            // still empty: busy.
            inbox.push_link(NonNull::new_unchecked(stub));
            assert!((*last).next.load(Ordering::Acquire).is_null());
            let looks = (inbox.is_empty(), inbox.is_drained());
            // The push links (before the checks, so a failed one leaves a
            // whole queue to drop).
            (*prev).next.store(pushed.as_ptr(), Ordering::Release);
            assert!(looks.0, "the head is the stub");
            assert!(!looks.1, "the taker sees the two tasks behind its end");
        }
        // Both come out, in order, and then nothing.
        let mut taken = 0;
        while let Some(r) = take(&inbox) {
            r.run();
            taken += 1;
        }
        assert_eq!(taken, 2);
        assert_eq!(*log.lock().unwrap(), [1, 2]);
        // SAFETY: the test holds the role.
        assert!(unsafe { inbox.is_drained() });
    }

    #[test]
    fn the_role_goes_to_one_taker_at_a_time() {
        let inbox: Inbox = Inbox::new();
        assert!(!inbox.try_take(), "its maker holds it");
        // SAFETY: the maker holds it.
        unsafe { inbox.release() };
        assert!(inbox.try_take());
        assert!(!inbox.try_take());
        // SAFETY: just taken.
        unsafe { inbox.release() };
    }

    #[test]
    fn a_dropped_inbox_drops_what_it_holds() {
        let log = Log::default();
        let inbox: Inbox = Inbox::new();
        inbox.push(task(&log, 1));
        inbox.push(task(&log, 2));
        assert_eq!(Arc::strong_count(&log), 3);
        drop(inbox);
        assert_eq!(
            Arc::strong_count(&log),
            1,
            "both tasks dropped their futures"
        );
    }

    #[test]
    fn pushes_from_threads_all_come_out_in_each_threads_order() {
        const THREADS: u32 = 4;
        const EACH: u32 = 2_000;
        let log = Log::default();
        let inbox: Arc<Inbox> = Arc::new(Inbox::new());
        let pushers: Vec<_> = (0..THREADS)
            .map(|t| {
                let (inbox, log) = (Arc::clone(&inbox), Arc::clone(&log));
                std::thread::spawn(move || {
                    for i in 0..EACH {
                        inbox.push(task(&log, t * EACH + i));
                    }
                })
            })
            .collect();
        let mut got = 0;
        while got < THREADS * EACH {
            // SAFETY: this thread holds the role.
            match unsafe { inbox.pop() } {
                Pop::Item(r) => {
                    r.run();
                    got += 1;
                }
                Pop::Empty | Pop::Busy => std::thread::yield_now(),
            }
        }
        for p in pushers {
            p.join().unwrap();
        }
        assert!(inbox.is_empty());
        let log = log.lock().unwrap();
        for t in 0..THREADS {
            let mine: Vec<u32> = log.iter().copied().filter(|n| n / EACH == t).collect();
            assert!(
                mine.windows(2).all(|w| w[0] < w[1]),
                "thread {t}'s tasks came out of order"
            );
            assert_eq!(mine.len(), EACH as usize);
        }
    }
}
