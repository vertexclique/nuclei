use std::fmt;
use std::{pin::Pin, future::Future, io, ops::{DerefMut, Deref}, sync::Arc};
use lever::prelude::*;

pub type AsyncOp<T> = Pin<Box<dyn Future<Output = io::Result<T>>>>;

pub trait HandleOpRegisterer {
    fn read_registerer(&self) -> Arc<TTas<Option<AsyncOp<usize>>>>;
    fn write_registerer(&self) -> Arc<TTas<Option<AsyncOp<usize>>>>;
}

pub struct Handle<T> {
    /// IO task element
    pub(crate) io_task: Option<T>,
    /// Completion callback for read
    pub(crate) read: Arc<TTas<Option<AsyncOp<usize>>>>,
    /// Completion callback for write
    pub(crate) write: Arc<TTas<Option<AsyncOp<usize>>>>,
}

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
