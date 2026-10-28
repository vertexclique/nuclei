//! The fair semaphore's handshake under the bounded channel: its waiters are
//! served in order, and a waiter that leaves passes its share on (see the
//! parent file's docs).

use crate::sync::loom_model::fair_handoff_counts;

use super::*;

/// The places the fair semaphore hands a leaving waiter's share on, in the
/// order `loom_model::fair_handoff_counts` reports them.
const GRANTED_THEN_DROPPED: usize = 0;
const WITHDREW_AT_GRANT: usize = 1;
const RETURNED_TO_WORD: usize = 2;

/// [`explore`], then fails the model unless its search handed a share on at
/// every place in `places` at least once.
fn explore_handoffs(
    name: &str,
    preemptions: usize,
    min_interleavings: usize,
    places: &[usize],
    model: impl Fn() + Sync + Send + 'static,
) {
    fair_handoff_counts();
    explore(name, preemptions, min_interleavings, model);
    let counts = fair_handoff_counts();
    println!(
        "loom model {name}: shares handed on {} times by a wait granted then dropped, {} by a \
         waiter that withdrew at its grant, {} back to the free count",
        counts[GRANTED_THEN_DROPPED], counts[WITHDREW_AT_GRANT], counts[RETURNED_TO_WORD]
    );
    for &place in places {
        assert!(
            counts[place] > 0,
            "{name}: the search never handed a share on at place {place}"
        );
    }
}

/// Polls `future` once with a waker that does nothing.
pub(super) fn poll_once<F: Future + Unpin>(future: &mut F) -> Poll<F::Output> {
    std::pin::Pin::new(future).poll(&mut Context::from_waker(std::task::Waker::noop()))
}

/// Two reservations wait on a full channel of one slot; the slot's permit
/// drops on another thread while the first reservation is dropped. Whichever
/// comes first (the grant, the withdrawal, or the withdrawal between the
/// pass picking the waiter and granting it), the slot reaches the second
/// reservation, whose waker is called, and the capacity ends whole.
#[test]
fn a_reservation_dropped_racing_its_grant_passes_the_slot_on() {
    explore_handoffs(
        "a_reservation_dropped_racing_its_grant_passes_the_slot_on",
        3,
        200,
        &[GRANTED_THEN_DROPPED, WITHDREW_AT_GRANT],
        || {
            let (tx, rx) = mpsc::channel::<u8>(1);
            let permit = tx.clone().try_reserve_owned().expect("the slot is free");
            let mut first = Box::pin(tx.reserve());
            let mut second = Box::pin(tx.reserve());
            assert!(poll_once(&mut first).is_pending());
            assert!(poll_once(&mut second).is_pending());
            let releaser = thread::spawn(move || drop(permit));
            drop(first);
            let served = block_on(second).expect("the receiver is alive");
            releaser.join().expect("releaser");
            drop(served);
            assert_eq!(tx.capacity(), 1, "a slot was lost or made up");
            drop(rx);
        },
    );
}

/// As above with a share: a request for two slots holds one set aside and
/// waits for the other, with a single reservation behind it. The second
/// slot's permit drops on another thread while the request for two is
/// dropped. The reservation behind is served, and both slots end free.
#[test]
fn a_waiting_reserve_many_dropped_racing_its_grant_passes_its_share_on() {
    explore_handoffs(
        "a_waiting_reserve_many_dropped_racing_its_grant_passes_its_share_on",
        3,
        200,
        &[GRANTED_THEN_DROPPED, WITHDREW_AT_GRANT, RETURNED_TO_WORD],
        || {
            let (tx, rx) = mpsc::channel::<u8>(2);
            let held = tx
                .clone()
                .try_reserve_owned()
                .expect("the first slot is free");
            let last = tx
                .clone()
                .try_reserve_owned()
                .expect("the second slot is free");
            let mut many = Box::pin(tx.reserve_many(2));
            let mut one = Box::pin(tx.reserve());
            assert!(poll_once(&mut many).is_pending());
            assert!(poll_once(&mut one).is_pending());
            drop(held);
            assert_eq!(
                tx.capacity(),
                0,
                "the first slot is set aside for the request"
            );
            let releaser = thread::spawn(move || drop(last));
            drop(many);
            let served = block_on(one).expect("the receiver is alive");
            releaser.join().expect("releaser");
            drop(served);
            assert_eq!(tx.capacity(), 2, "a slot was lost or made up");
            drop(rx);
        },
    );
}

/// A release on another thread races two waiting reservations of one slot:
/// the one that began to wait first gets the slot, and the later one waits
/// until that slot comes back.
#[test]
fn a_release_racing_two_waiting_reservations_serves_the_older_one() {
    explore(
        "a_release_racing_two_waiting_reservations_serves_the_older_one",
        3,
        100,
        || {
            let (tx, rx) = mpsc::channel::<u8>(1);
            let permit = tx.clone().try_reserve_owned().expect("the slot is free");
            let mut older = Box::pin(tx.reserve());
            let mut later = Box::pin(tx.reserve());
            assert!(poll_once(&mut older).is_pending());
            assert!(poll_once(&mut later).is_pending());
            let releaser = thread::spawn(move || drop(permit));
            let first = block_on(older).expect("the receiver is alive");
            assert!(
                poll_once(&mut later).is_pending(),
                "the later reservation got a slot the older one holds"
            );
            releaser.join().expect("releaser");
            drop(first);
            let second = block_on(later).expect("the receiver is alive");
            drop(second);
            assert_eq!(tx.capacity(), 1);
            drop(rx);
        },
    );
}

/// The receiver closes while another thread's release completes a waiting
/// request for two slots, one of them already set aside for it. The request
/// ends exactly once: granted (its two slots then send values the receiver
/// gets before the end) or failed by the close. Either way every slot comes
/// back, and the receiver finishes. Both outcomes must be reached.
#[test]
fn a_close_racing_a_grant_ends_the_wait_once_and_returns_every_slot() {
    let granted = std::sync::Arc::new(StdAtomicUsize::new(0));
    let failed = std::sync::Arc::new(StdAtomicUsize::new(0));
    let outcomes = (
        std::sync::Arc::clone(&granted),
        std::sync::Arc::clone(&failed),
    );
    explore_handoffs(
        "a_close_racing_a_grant_ends_the_wait_once_and_returns_every_slot",
        3,
        200,
        &[RETURNED_TO_WORD],
        move || {
            let (tx, mut rx) = mpsc::channel::<u8>(2);
            let held = tx
                .clone()
                .try_reserve_owned()
                .expect("the first slot is free");
            let last = tx
                .clone()
                .try_reserve_owned()
                .expect("the second slot is free");
            let mut many = Box::pin(tx.reserve_many(2));
            assert!(poll_once(&mut many).is_pending());
            drop(held);
            let releaser = thread::spawn(move || drop(last));
            rx.close();
            releaser.join().expect("releaser");
            // Every pass has run: the request has its answer.
            let sent = match poll_once(&mut many) {
                Poll::Ready(Ok(permits)) => {
                    for (permit, value) in permits.zip(1..) {
                        permit.send(value);
                    }
                    outcomes.0.fetch_add(1, StdOrdering::Relaxed);
                    vec![1, 2]
                }
                Poll::Ready(Err(SendError(()))) => {
                    outcomes.1.fetch_add(1, StdOrdering::Relaxed);
                    vec![]
                }
                Poll::Pending => panic!("the request was neither granted nor failed"),
            };
            drop(many);
            let mut received = Vec::new();
            while let Some(value) = block_on(rx.recv()) {
                received.push(value);
            }
            assert_eq!(received, sent);
            assert_eq!(tx.capacity(), 2, "a slot was lost or made up");
        },
    );
    assert!(
        granted.load(StdOrdering::Relaxed) > 0,
        "the grant never won"
    );
    assert!(failed.load(StdOrdering::Relaxed) > 0, "the close never won");
}
