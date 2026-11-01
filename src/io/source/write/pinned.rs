//! A zero-copy write's buffer, shared by its task's lent write and the sends of it: the kernel
//! reads it until each send's notification, after the write ended for the task, so the buffer is
//! only read meanwhile, and whoever lets go of it last hands it back through the write's
//! `Released`, or drops it if that went. `proofs/tla/lent/LentZc.tla` checks the rule.

use std::sync::Arc;

use crate::buf::owned::OwnedBuf;
use crate::sync::oneshot;

/// A hold on a zero-copy write's buffer: the side's lent write keeps one, and each send of it
/// one until the kernel let go of what it sent.
#[derive(Clone)]
pub(crate) struct Pinned(Arc<Shared>);

struct Shared {
    buf: OwnedBuf,
    back: Option<oneshot::Sender<OwnedBuf>>,
}

impl Pinned {
    /// Shares `buf`, with the receiver it comes back through once every hold let go.
    pub(crate) fn new(buf: OwnedBuf) -> (Self, oneshot::Receiver<OwnedBuf>) {
        let (back, rx) = oneshot::channel();
        let shared = Shared {
            buf,
            back: Some(back),
        };
        (Self(Arc::new(shared)), rx)
    }

    /// The buffer's bytes, which the kernel may be reading.
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.0.buf
    }
}

impl Drop for Shared {
    /// The last hold let go: the kernel is done with every send of the buffer.
    fn drop(&mut self) {
        if let Some(back) = self.back.take() {
            // A receiver that went gives the buffer back here, to be dropped.
            drop(back.send(core::mem::take(&mut self.buf)));
        }
    }
}
