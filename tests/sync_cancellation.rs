//! Tests of `nuclei::sync::CancellationToken`: the behaviour of the established async
//! token API it follows, checked call by call; drop and cancel safety of every
//! future; threads creating, cancelling and dropping trees at once; and a
//! chain of a million nested tokens dropped on a small stack.
#![cfg(not(target_arch = "wasm32"))]

mod sync_support;

use std::future::Future;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::pin::pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::task::Poll;

use nuclei::sync::{CancellationToken, DropGuard, DropGuardRef, oneshot};
use sync_support::{Probe, Task, block_on};

#[path = "sync_cancellation/cancel.rs"]
mod cancel;
#[path = "sync_cancellation/children.rs"]
mod children;
#[path = "sync_cancellation/depth.rs"]
mod depth;
#[path = "sync_cancellation/model.rs"]
mod model;
#[path = "sync_cancellation/run_until.rs"]
mod run_until;
#[path = "sync_cancellation/surface.rs"]
mod surface;
#[path = "sync_cancellation/threads.rs"]
mod threads;
