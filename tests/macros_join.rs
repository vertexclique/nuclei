//! `join!` and `try_join!`: outputs in the order written, concurrent waits,
//! fair and biased polling, the first error ending a `try_join!` and
//! dropping the other futures.

use std::cell::RefCell;
use std::future::{IntoFuture, ready};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use nuclei::sync::oneshot;

/// Counts its drops.
struct Counted(Arc<AtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Yields `turns` times, logging its name before each turn.
async fn take_turns(name: char, turns: usize, log: Rc<RefCell<Vec<char>>>) -> char {
    for _ in 0..turns {
        log.borrow_mut().push(name);
        nuclei::task::yield_now().await;
    }
    name
}

#[test]
fn outputs_come_in_the_order_written() {
    nuclei::block_on(async {
        let (a, b, c) = nuclei::join!(ready(1u8), async { "two" }, ready(3.0f64));
        assert_eq!((a, b, c), (1, "two", 3.0));
        let (only,) = nuclei::join!(ready(5u32));
        assert_eq!(only, 5);
        let () = nuclei::join!();
        let trailing = nuclei::join!(ready(1), ready(2),);
        assert_eq!(trailing, (1, 2));
    });
}

#[test]
fn the_futures_wait_concurrently() {
    nuclei::block_on(async {
        let (ping_tx, ping_rx) = oneshot::channel::<u32>();
        let (pong_tx, pong_rx) = oneshot::channel::<u32>();
        let (a, b) = nuclei::join!(
            async move {
                ping_tx.send(1).unwrap();
                pong_rx.await.unwrap()
            },
            async move {
                let n = ping_rx.await.unwrap();
                pong_tx.send(n + 1).unwrap();
                n
            },
        );
        assert_eq!((a, b), (2, 1));
    });
}

#[test]
fn fair_join_rotates_and_biased_keeps_the_written_order() {
    nuclei::block_on(async {
        let log = Rc::new(RefCell::new(Vec::new()));
        nuclei::join!(
            biased;
            take_turns('a', 3, Rc::clone(&log)),
            take_turns('b', 3, Rc::clone(&log)),
        );
        assert_eq!(*log.borrow(), ['a', 'b', 'a', 'b', 'a', 'b']);

        let log = Rc::new(RefCell::new(Vec::new()));
        nuclei::join!(
            take_turns('a', 3, Rc::clone(&log)),
            take_turns('b', 3, Rc::clone(&log)),
        );
        // Each poll starts one later: the order alternates which goes first.
        assert_eq!(*log.borrow(), ['a', 'b', 'b', 'a', 'a', 'b']);
    });
}

#[test]
fn into_futures_are_joined() {
    struct Later(u32);
    impl IntoFuture for Later {
        type Output = u32;
        type IntoFuture = std::future::Ready<u32>;
        fn into_future(self) -> Self::IntoFuture {
            ready(self.0)
        }
    }
    nuclei::block_on(async {
        assert_eq!(nuclei::join!(Later(1), Later(2)), (1, 2));
        assert_eq!(
            nuclei::try_join!(async { Ok::<_, ()>(Later(3).await) }),
            Ok((3,))
        );
    });
}

#[test]
fn try_join_gives_every_output_when_all_succeed() {
    nuclei::block_on(async {
        let (tx, rx) = oneshot::channel::<u32>();
        let out: Result<(u32, &str), String> =
            nuclei::try_join!(async { rx.await.map_err(|e| e.to_string()) }, async {
                tx.send(8).unwrap();
                Ok("sent")
            },);
        assert_eq!(out, Ok((8, "sent")));
        assert_eq!(nuclei::try_join!(), Ok::<(), ()>(()));
    });
}

#[test]
fn try_join_ends_at_the_first_error_and_drops_the_rest() {
    nuclei::block_on(async {
        let drops = Arc::new(AtomicUsize::new(0));
        let held = Counted(Arc::clone(&drops));
        let out: Result<(u32, u32, ()), &str> = nuclei::try_join!(
            // Still running when the error comes: it is dropped unfinished.
            async move {
                let _held = held;
                for _ in 0..5 {
                    nuclei::task::yield_now().await;
                }
                Ok::<u32, &str>(5)
            },
            ready(Ok::<u32, &str>(1)),
            async {
                nuclei::task::yield_now().await;
                Err::<(), &str>("second")
            },
        );
        assert_eq!(out, Err("second"));
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "the pending future was dropped"
        );
    });
}

#[test]
fn join_drops_every_future_and_output_once() {
    nuclei::block_on(async {
        let drops = Arc::new(AtomicUsize::new(0));
        let make = |drops: &Arc<AtomicUsize>| {
            let counted = Counted(Arc::clone(drops));
            async move {
                nuclei::task::yield_now().await;
                counted
            }
        };
        let outputs = nuclei::join!(make(&drops), make(&drops), make(&drops));
        assert_eq!(
            drops.load(Ordering::SeqCst),
            0,
            "the outputs are handed out, not dropped"
        );
        drop(outputs);
        assert_eq!(drops.load(Ordering::SeqCst), 3);
    });
}
