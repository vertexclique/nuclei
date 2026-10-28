//! Tests of `nuclei::sync::watch`: the behaviour of the established async
//! channel API it follows, checked call by call; the deliberate differences (modifications are
//! copy-on-write, `send_replace` returns a clone); drop and cancel safety;
//! and threads racing sends, waits and drops.
#![cfg(not(target_arch = "wasm32"))]

mod sync_support;

use std::error::Error;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Poll;

use nuclei::sync::watch::{self, error::RecvError, error::SendError};
use sync_support::{Counted, Drops, Task, block_on, eventually};

#[path = "sync_watch/drop_safety.rs"]
mod drop_safety;
#[path = "sync_watch/model.rs"]
mod model;
#[path = "sync_watch/receiver.rs"]
mod receiver;
#[path = "sync_watch/senders.rs"]
mod senders;
#[path = "sync_watch/threads.rs"]
mod threads;
#[path = "sync_watch/wait_for.rs"]
mod wait_for;
