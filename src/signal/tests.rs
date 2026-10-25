use core::future::Future;
#[cfg(unix)]
use core::task::{Context, Poll, Waker};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// A future's outcome, woken well within ten seconds: the timeout's own
/// wake polls the future again, so an outcome only that wake found (one
/// whose wake was lost) fails too.
async fn within<F: Future>(fut: F) -> F::Output {
    let start = Instant::now();
    let out = crate::time::timeout(Duration::from_secs(10), fut)
        .await
        .expect("nothing came within ten seconds");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "the outcome came only as the timeout looked again: its wake was lost"
    );
    out
}

/// Polls `f` once with a waker that does nothing.
#[cfg(unix)]
fn poll_once<T>(f: impl FnOnce(&mut Context<'_>) -> Poll<T>) -> Poll<T> {
    f(&mut Context::from_waker(Waker::noop()))
}

#[cfg(unix)]
mod unix {
    use core::pin::pin;

    use super::*;
    use crate::signal::unix::{SignalKind, signal};

    fn raise(signum: libc::c_int) {
        // SAFETY: a plain call; the signal goes to this thread, whose
        // handler runs before the call returns.
        assert_eq!(unsafe { libc::raise(signum) }, 0);
    }

    fn kill_self(signum: libc::c_int) {
        // SAFETY: plain calls; the process sends itself the signal.
        assert_eq!(unsafe { libc::kill(libc::getpid(), signum) }, 0);
    }

    /// A listener that waits is woken by a delivery to the process from
    /// another thread: through the pump on its shard, the one thing that
    /// can wake it.
    #[test]
    fn a_signal_from_another_thread_wakes_a_waiting_listener() {
        let mut usr1 = signal(SignalKind::user_defined1()).unwrap();
        let waiting = Arc::new(AtomicBool::new(false));
        let sender = {
            let waiting = Arc::clone(&waiting);
            std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(10);
                while !waiting.load(Ordering::SeqCst) {
                    assert!(Instant::now() < deadline, "the listener never waited");
                    std::thread::yield_now();
                }
                kill_self(libc::SIGUSR1);
            })
        };
        crate::block_on(within(core::future::poll_fn(|cx| {
            let poll = usr1.poll_recv(cx);
            if poll.is_pending() {
                waiting.store(true, Ordering::SeqCst);
            }
            poll
        })));
        sender.join().unwrap();
    }

    /// Deliveries between two items come as one.
    #[test]
    fn deliveries_between_receives_come_as_one_item() {
        let mut usr2 = signal(SignalKind::user_defined2()).unwrap();
        assert!(poll_once(|cx| usr2.poll_recv(cx)).is_pending());
        raise(libc::SIGUSR2);
        raise(libc::SIGUSR2);
        assert_eq!(poll_once(|cx| usr2.poll_recv(cx)), Poll::Ready(Some(())));
        assert!(poll_once(|cx| usr2.poll_recv(cx)).is_pending());
    }

    /// Every listener of a signal gets each delivery.
    #[test]
    fn every_listener_of_a_signal_gets_each_delivery() {
        let mut first = signal(SignalKind::window_change()).unwrap();
        let mut second = signal(SignalKind::window_change()).unwrap();
        raise(libc::SIGWINCH);
        assert_eq!(poll_once(|cx| first.poll_recv(cx)), Poll::Ready(Some(())));
        assert_eq!(poll_once(|cx| second.poll_recv(cx)), Poll::Ready(Some(())));
    }

    /// A listener waiting in a worker's task is woken.
    #[test]
    fn a_listener_on_a_worker_is_woken() {
        let rt = crate::runtime::Runtime::builder()
            .worker_threads(2)
            .pin_workers(false)
            .build()
            .unwrap();
        let waiting = Arc::new(AtomicBool::new(false));
        let task = {
            let waiting = Arc::clone(&waiting);
            rt.spawn(async move {
                let mut urg = signal(SignalKind::from_raw(libc::SIGURG)).unwrap();
                within(core::future::poll_fn(|cx| {
                    let poll = urg.poll_recv(cx);
                    if poll.is_pending() {
                        waiting.store(true, Ordering::SeqCst);
                    }
                    poll
                }))
                .await
            })
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !waiting.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "the listener never waited");
            std::thread::yield_now();
        }
        kill_self(libc::SIGURG);
        assert_eq!(rt.block_on(task), Some(()));
    }

    #[test]
    fn signals_no_handler_can_take_are_refused() {
        for signum in [libc::SIGKILL, libc::SIGSTOP, libc::SIGSEGV, 0, -1, 1000] {
            let err = signal(SignalKind::from_raw(signum)).unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{signum}");
        }
    }

    static BEFORE_RAN: AtomicBool = AtomicBool::new(false);

    extern "C" fn before(_: libc::c_int) {
        BEFORE_RAN.store(true, Ordering::SeqCst);
    }

    /// The handler that was in place before still runs on each delivery.
    #[test]
    fn the_handler_in_place_before_still_runs() {
        // SAFETY: plain data and calls; the handler lives for the process.
        unsafe {
            let mut action: libc::sigaction = core::mem::zeroed();
            action.sa_sigaction = before as *const () as libc::sighandler_t;
            libc::sigemptyset(&mut action.sa_mask);
            assert_eq!(
                libc::sigaction(libc::SIGVTALRM, &action, core::ptr::null_mut()),
                0
            );
        }
        let mut vtalrm = signal(SignalKind::from_raw(libc::SIGVTALRM)).unwrap();
        raise(libc::SIGVTALRM);
        assert_eq!(poll_once(|cx| vtalrm.poll_recv(cx)), Poll::Ready(Some(())));
        assert!(
            BEFORE_RAN.load(Ordering::SeqCst),
            "the handler before did not run"
        );
    }

    /// `ctrl_c` completes on `SIGINT`. Alone in its process: from then on
    /// the process's `SIGINT` is nuclei's.
    #[test]
    fn ctrl_c_completes_on_sigint() {
        if !crate::alone::alone("signal::tests::unix::ctrl_c_completes_on_sigint") {
            return;
        }
        let mut ctrl_c = pin!(crate::signal::ctrl_c());
        assert!(poll_once(|cx| ctrl_c.as_mut().poll(cx)).is_pending());
        raise(libc::SIGINT);
        assert!(matches!(
            poll_once(|cx| ctrl_c.as_mut().poll(cx)),
            Poll::Ready(Ok(()))
        ));
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use crate::signal::windows::{ctrl_break, ctrl_c, deliver_for_test};

    /// A Ctrl-C with a listener is taken, and wakes a listener waiting on
    /// another thread.
    #[test]
    fn a_ctrl_c_with_a_listener_is_taken_and_wakes_it() {
        let mut listener = ctrl_c().unwrap();
        let waiting = Arc::new(AtomicBool::new(false));
        let sender = {
            let waiting = Arc::clone(&waiting);
            std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(10);
                while !waiting.load(Ordering::SeqCst) {
                    assert!(Instant::now() < deadline, "the listener never waited");
                    std::thread::yield_now();
                }
                deliver_for_test(0)
            })
        };
        crate::block_on(within(core::future::poll_fn(|cx| {
            let poll = listener.poll_recv(cx);
            if poll.is_pending() {
                waiting.store(true, Ordering::SeqCst);
            }
            poll
        })));
        assert!(sender.join().unwrap(), "the handler did not take the event");
    }

    /// An event with no listener goes on to the next handler.
    #[test]
    fn an_event_with_no_listener_is_left_to_the_next_handler() {
        assert!(!deliver_for_test(5));
        let listener = ctrl_break().unwrap();
        assert!(deliver_for_test(1));
        drop(listener);
        assert!(!deliver_for_test(1));
    }
}
