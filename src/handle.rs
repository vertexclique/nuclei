use lever::prelude::*;

#[cfg(unix)]
use crate::syscore::{CompletionChan, StoreFile};
use pin_utils::unsafe_unpinned;
use std::fmt;
use std::{
    future::Future,
    io,
    ops::{Deref, DerefMut},
    pin::Pin,
    sync::Arc,
};

pub type AsyncOp<T> = Pin<Box<dyn Future<Output = io::Result<T>>>>;

pub trait HandleOpRegisterer {
    fn read_registerer(&self) -> Arc<TTas<Option<AsyncOp<usize>>>>;
    fn write_registerer(&self) -> Arc<TTas<Option<AsyncOp<usize>>>>;
}

pub struct Handle<T> {
    /// IO task element
    pub(crate) io_task: Option<T>,
    /// Notification channel
    #[cfg(unix)]
    pub(crate) chan: Option<CompletionChan>,
    /// File operation storage
    #[cfg(unix)]
    pub(crate) store_file: Option<StoreFile>,
    /// Completion callback for read
    pub(crate) read: Arc<TTas<Option<AsyncOp<usize>>>>,
    /// Completion callback for write
    pub(crate) write: Arc<TTas<Option<AsyncOp<usize>>>>,
}

unsafe impl<T> Send for Handle<T> {}
unsafe impl<T> Sync for Handle<T> {}

impl<T> Handle<T> {
    pub fn get_ref(&self) -> &T {
        self.io_task.as_ref().unwrap()
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.io_task.as_mut().unwrap()
    }

    pub fn into_inner(mut self) -> T {
        self.io_task.take().unwrap()
    }

    // #[cfg(all(feature = "iouring", target_os = "linux"))]
    // unsafe_unpinned!(store_file: Option<StoreFile>);
    //
    // pub(crate) fn get_file(mut self: Pin<&mut Self>) -> &mut Option<StoreFile> {
    //     self.store_file()
    // }
}

impl<T> HandleOpRegisterer for Handle<T> {
    fn read_registerer(&self) -> Arc<TTas<Option<AsyncOp<usize>>>> {
        self.read.clone()
    }

    fn write_registerer(&self) -> Arc<TTas<Option<AsyncOp<usize>>>> {
        self.write.clone()
    }
}

impl<T> HandleOpRegisterer for &Handle<T> {
    fn read_registerer(&self) -> Arc<TTas<Option<AsyncOp<usize>>>> {
        self.read.clone()
    }

    fn write_registerer(&self) -> Arc<TTas<Option<AsyncOp<usize>>>> {
        self.write.clone()
    }
}

impl<T> Deref for Handle<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.io_task.as_ref().unwrap()
    }
}

impl<T> DerefMut for Handle<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.io_task.as_mut().unwrap()
    }
}

impl<T: fmt::Debug> fmt::Debug for Handle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handle")
            .field("io_task", self.io_task.as_ref().unwrap())
            .finish()
    }
}
