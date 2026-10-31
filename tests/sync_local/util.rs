use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use crate::sync_support::Probe;

pub fn now<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("the future is still pending"),
    }
}

pub fn poll<F: Future + Unpin>(probe: &mut Probe, future: &mut F) -> Poll<F::Output> {
    probe.enter(|cx| Pin::new(future).poll(cx))
}

pub fn pending<F: Future + Unpin>(probe: &mut Probe, future: &mut F) {
    assert!(
        poll(probe, future).is_pending(),
        "the future completed early"
    );
}

pub fn ready<F: Future + Unpin>(probe: &mut Probe, future: &mut F) -> F::Output {
    match poll(probe, future) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("the future is still pending"),
    }
}
