use super::handle::HandleOpRegisterer;

use std::marker::PhantomData as marker;
use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
};

pub struct SubmissionHandler<T>(marker<T>)
where
    T: Unpin;

impl<T> SubmissionHandler<T>
where
    T: Unpin + HandleOpRegisterer,
{
    pub fn handle_read(
        handle: Pin<&mut T>,
        cx: &mut Context,
        completion_dispatcher: impl Future<Output = io::Result<usize>> + 'static,
    ) -> Poll<io::Result<usize>> {
        let handle = handle.get_mut();
        let read_result = handle.read_registerer();

        let mut result = match read_result.try_lock() {
            Some(result) => result,
            None => return Poll::Pending,
        };

        if result.is_none() {
            *result = Some(Box::pin(completion_dispatcher));
        }

        let poll = {
            let result = result.as_mut().unwrap();
            result.as_mut().poll(cx).map_ok(|s| s)
        };

        if poll.is_ready() {
            *result = None;
        }

        poll
    }

    pub fn handle_write(
        handle: Pin<&mut T>,
        cx: &mut Context,
        completion_dispatcher: impl Future<Output = io::Result<usize>> + 'static,
    ) -> Poll<io::Result<usize>> {
        let handle = handle.get_mut();
        let write_result = handle.write_registerer();

        let mut result = match write_result.try_lock() {
            Some(result) => result,
            None => return Poll::Pending,
        };

        if result.is_none() {
            *result = Some(Box::pin(completion_dispatcher));
        }

        let poll = {
            let result = result.as_mut().unwrap();
            result.as_mut().poll(cx).map_ok(|s| s)
        };

        if poll.is_ready() {
            *result = None;
        }

        poll
    }

    // pub fn handle_seek(
    //     handle: Pin<&mut T>,
    //     cx: &mut Context,
    //     completion_dispatcher: impl Future<Output = io::Result<u64>> + 'static,
    // ) -> Poll<Result<u64, Error>> {
    //     let handle = handle.get_mut();
    //     let read_result = handle.read_registerer();
    //
    //     let mut result = match read_result.try_lock() {
    //         Some(result) => result,
    //         None => return Poll::Pending,
    //     };
    //
    //     if result.is_none() {
    //         *result = Some(Box::pin(completion_dispatcher));
    //     }
    //
    //     let poll = {
    //         let result = result.as_mut().unwrap();
    //         result.as_mut().poll(cx).map_ok(|s| s as u64)
    //     };
    //
    //     if poll.is_ready() {
    //         *result = None;
    //     }
    //
    //     poll
    // }
}
