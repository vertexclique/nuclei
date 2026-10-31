//! Faults: what every byte API answers when its peer resets or half-closes,
//! what the peer sees of our shut and abort, a datagram flood fills the
//! buffers, the process has no descriptor or no memory left, the socket
//! buffers are as small as they go, a future is dropped at any of its awaits,
//! and a handle, a runtime or a shard goes with operations out. The answer is the one std gives for the same call on the
//! same kind of socket in the same situation, and where the kernel may give
//! either of two, one std can give; every case has a deadline, and the cases
//! that count what the process holds run alone in a process of their own.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

mod cancel;
mod datagram;
mod drops;
#[cfg(unix)]
mod emfile;
mod half_close;
mod reset;
mod short_io;
mod shut;
mod support;

struct Live;

static LIVE: AtomicIsize = AtomicIsize::new(0);

/// The heap bytes this process holds now.
fn live_bytes() -> isize {
    LIVE.load(Ordering::Relaxed)
}

// SAFETY: every call goes to the system allocator as it came.
unsafe impl GlobalAlloc for Live {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as above.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as above.
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            LIVE.fetch_add(
                new_size as isize - layout.size() as isize,
                Ordering::Relaxed,
            );
        }
        new
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        // SAFETY: as above.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Live = Live;
