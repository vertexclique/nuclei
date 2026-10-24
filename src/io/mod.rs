//! IO on the shards: sources, the drivers' half of every handle, and the
//! system calls they make.
//!
//! A handle's IO object stays its own (std's sockets and files); its source
//! is what the shard's driver reaches. Operations try their system call at
//! once and leave it to the driver when it would block; the driver
//! completes it on readiness and wakes the task (see [`source`]).

mod side;
pub(crate) mod source;
mod sys;

pub(crate) use self::source::Source;
