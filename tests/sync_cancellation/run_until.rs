//! `run_until_cancelled` and its owned form.

use super::*;

#[test]
fn run_until_cancelled_test() {
    let mut probe = Probe::new();

    {
        let token = CancellationToken::new();
        let fut = token.run_until_cancelled(std::future::pending::<()>());
        let mut fut = pin!(fut);
        assert_eq!(probe.enter(|cx| fut.as_mut().poll(cx)), Poll::Pending);
        token.cancel();
        assert_eq!(probe.enter(|cx| fut.as_mut().poll(cx)), Poll::Ready(None));
    }

    {
        let (tx, rx) = oneshot::channel::<()>();
        let token = CancellationToken::new();
        let fut = token.run_until_cancelled(async move {
            rx.await.unwrap();
            42
        });
        let mut fut = pin!(fut);
        assert_eq!(probe.enter(|cx| fut.as_mut().poll(cx)), Poll::Pending);
        tx.send(()).unwrap();
        assert_eq!(
            probe.enter(|cx| fut.as_mut().poll(cx)),
            Poll::Ready(Some(42))
        );
    }

    // The future is not polled when the token is already cancelled.
    {
        let token = CancellationToken::new();
        let fut = token.run_until_cancelled(async { panic!("fut polled after cancellation") });
        let mut fut = pin!(fut);
        token.cancel();
        assert_eq!(probe.enter(|cx| fut.as_mut().poll(cx)), Poll::Ready(None));
    }
}

#[test]
fn run_until_cancelled_prefers_the_future_when_both_are_ready() {
    let mut probe = Probe::new();
    let token = CancellationToken::new();
    let fut = token.run_until_cancelled(async { 7 });
    let mut fut = pin!(fut);
    // Cancelled before the first poll: the async fn checks first and never
    // polls the future.
    token.cancel();
    assert_eq!(probe.enter(|cx| fut.as_mut().poll(cx)), Poll::Ready(None));

    let token = CancellationToken::new();
    let (tx, rx) = oneshot::channel::<u8>();
    let fut = token.run_until_cancelled(async move { rx.await.unwrap() });
    let mut fut = pin!(fut);
    assert_eq!(probe.enter(|cx| fut.as_mut().poll(cx)), Poll::Pending);
    tx.send(5).unwrap();
    token.cancel();
    assert_eq!(
        probe.enter(|cx| fut.as_mut().poll(cx)),
        Poll::Ready(Some(5)),
        "both are ready on this poll: the future wins"
    );
}

#[test]
fn run_until_cancelled_owned_test() {
    let mut probe = Probe::new();

    {
        let token = CancellationToken::new();
        let to_cancel = token.clone();
        let takes_ownership = move |token: CancellationToken| {
            token.run_until_cancelled_owned(std::future::pending::<()>())
        };
        let fut = takes_ownership(token);
        let mut fut = pin!(fut);
        assert_eq!(probe.enter(|cx| fut.as_mut().poll(cx)), Poll::Pending);
        to_cancel.cancel();
        assert_eq!(probe.enter(|cx| fut.as_mut().poll(cx)), Poll::Ready(None));
    }

    {
        let (tx, rx) = oneshot::channel::<()>();
        let token = CancellationToken::new();
        let takes_ownership = move |token: CancellationToken, rx: oneshot::Receiver<()>| {
            token.run_until_cancelled_owned(async move {
                rx.await.unwrap();
                42
            })
        };
        let fut = takes_ownership(token, rx);
        let mut fut = pin!(fut);
        assert_eq!(probe.enter(|cx| fut.as_mut().poll(cx)), Poll::Pending);
        tx.send(()).unwrap();
        assert_eq!(
            probe.enter(|cx| fut.as_mut().poll(cx)),
            Poll::Ready(Some(42))
        );
    }
}
