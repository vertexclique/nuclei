//! IO on the shards: sources, the drivers' half of every handle, and the
//! system calls they make.
//!
//! A handle's IO object stays its own (std's sockets and files); its source
//! is what the shard's driver reaches. The driver completes every
//! operation, into and out of the pool's buffers, at once when it can and
//! on readiness otherwise, and wakes the task, which takes the result (see
//! [`source`]).

mod handle;
pub(crate) mod raw;
pub(crate) mod side;
pub(crate) mod source;
pub(crate) mod sys;

pub use self::handle::Handle;
pub(crate) use self::source::Source;
