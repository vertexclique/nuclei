use futures::io::{
    AsyncRead, AsyncWrite, AsyncBufRead, AsyncSeek
};
use std::marker::PhantomData as marker;
use std::{task::{Context, Poll}, io, pin::Pin, future::Future, ops::{DerefMut, Deref}};
use super::handle::{Handle, HandleOpRegisterer};


pub struct CompletionHandler<T>(marker<T>)
where
     T: Unpin;

impl<T> CompletionHandler<T>
where
    T: Unpin + HandleOpRegisterer
{
    pub fn handle_read(
        handle: Pin<&mut T>,
        cx: &mut Context,
        completion_dispatcher: impl Future<Output=io::Result<usize>> + 'static
    ) -> Poll<io::Result<usize>> {
        let handle = handle.get_mut();
        let read_result = handle.read();

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
}

// pub trait CompletionHandler<'a, 'cb, T>
// where
//     T: Unpin,
// {
//     type CompOut;

//     fn handle_completion(handle: Pin<&'cb mut T>,
//                          cx: &mut Context,
//                          process_result: impl Future<Output=io::Result<Self::CompOut>> + 'static
//     ) -> Poll<io::Result<Self::CompOut>>;
// }

// impl<'a, 'cb, T> CompletionHandler<'a, 'cb, T> for T
// where
//     T: AsyncRead + HandleOpRegisterer + Unpin,
// {
//     type CompOut = usize;

//     fn handle_completion(handle: Pin<&'cb mut T>,
//                          cx: &mut Context,
//                          process_result: impl Future<Output=io::Result<Self::CompOut>> + 'static
//     ) -> Poll<io::Result<Self::CompOut>> {
//         let handle = handle.get_mut();
//         let read_result = handle.read();

//         let mut result = match read_result.try_lock() {
//             Some(result) => result,
//             None => return Poll::Pending,
//         };

//         if result.is_none() {
//             *result = Some(Box::pin(process_result));
//         }

//         let poll = {
//             let result = result.as_mut().unwrap();
//             result.as_mut().poll(cx).map_ok(|s| s)
//         };

//         if poll.is_ready() {
//             *result = None;
//         }

//         poll
//     }
// }

// impl<'a> CompletionHandler<'a> for AsyncRead {
//     type T = usize;

//     fn handle_completion(&'a self, handle: Pin<&mut Self>, process_result: impl Future<Output=Poll<io::Result<Self::T>>>) -> Poll<io::Result<Self::T>> {
//         let this = handle.get_mut();
//     }
// }

// impl<'a> CompletionHandler<'a> for AsyncWrite {
//     type T = usize;

//     fn handle_completion(&self) -> Poll<io::Result<Self::T>> { todo!() }
// }

// impl<'a> CompletionHandler<'a> for AsyncBufRead {
//     type T = &'a [u8];

//     fn handle_completion(&self) -> Poll<io::Result<Self::T>> { todo!() }
// }

// impl<'a> CompletionHandler<'a> for AsyncSeek {
//     type T = u64;

//     fn handle_completion(&self) -> Poll<io::Result<Self::T>> { todo!() }
// }
