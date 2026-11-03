//! How a shard is made: every field written straight into the memory the shard then lives in,
//! its `Arc` or `Rc` (or, in tests, a value), and its shared half into its `Arc`, so a start
//! copies none of their kilobytes. The two timer wheels are most of them, each one copy of a
//! constant; the driver is moved once, from its constructor's result into its place.

use core::cell::{Cell, OnceCell, RefCell};
use core::ptr::{addr_of, addr_of_mut};
use std::collections::VecDeque;
use std::io;
use std::rc::Rc;
use std::sync::Arc;

use super::counted::Counted;
use super::{
    Counters, Driver, Inbox, Limits, ParkState, Poller, Shard, Shared, Sources, Spin, Wheel,
    children, counters, locals, reserve_group,
};
use crate::buf::Pool;
use crate::rt::handoff::Monitor;
use crate::rt::quartz::Quartz;
use crate::rt::tuning::Tuning;
use crate::sync::atomic_waker::AtomicWaker;
use crate::sync::internal::{AtomicBool, AtomicU64, AtomicUsize};

/// Writes every field of the `$ty` at `$slot` from one list, but those `$written` before; the
/// lists together, as a pattern with no rest, have the compiler refuse a field they leave out.
macro_rules! write_fields {
    ($ty:ident at $slot:ident, [$($written:ident),*], {
        $($(#[$attr:meta])* $field:ident: $value:expr,)*
    }) => {
        $(
            $(#[$attr])*
            {
                let value = $value;
                // SAFETY: `$slot` is valid for writes of a `$ty` (the contract of the function the
                // macro runs in), and each field is written once.
                unsafe { addr_of_mut!((*$slot).$field).write(value) };
            }
        )*
        let _every_field = |whole: &$ty| {
            let $ty { $($written: _,)* $($(#[$attr])* $field: _,)* } = whole;
        };
    };
}

impl Shard {
    /// A shard of its own, in its `Rc`: a thread's own shard.
    pub(crate) fn new_rc(limits: Limits) -> io::Result<Rc<Self>> {
        let mut cell = Rc::<Self>::new_uninit();
        let Some(slot) = Rc::get_mut(&mut cell) else {
            return Err(unshared());
        };
        // SAFETY: the memory of a new `Rc`, which nothing else reaches.
        unsafe { Self::write(slot.as_mut_ptr(), limits, None, Tuning::default(), None)? };
        // SAFETY: `write` wrote every field.
        Ok(unsafe { cell.assume_init() })
    }

    /// A shard of its own, as a value.
    #[cfg(test)]
    pub(crate) fn new(limits: Limits) -> io::Result<Self> {
        let mut shard = core::mem::MaybeUninit::<Self>::uninit();
        // SAFETY: memory of the shard's layout that nothing else reaches.
        unsafe { Self::write(shard.as_mut_ptr(), limits, None, Tuning::default(), None)? };
        // SAFETY: `write` wrote every field.
        Ok(unsafe { shard.assume_init() })
    }

    /// The shard of worker `index` of runtime `runtime`, run as `tuning` says, in its `Arc`.
    pub(crate) fn worker(
        limits: Limits,
        tuning: Tuning,
        runtime: u64,
        index: usize,
        monitor: Option<Arc<Monitor>>,
    ) -> io::Result<Arc<Self>> {
        let mut cell = Arc::<Self>::new_uninit();
        let Some(slot) = Arc::get_mut(&mut cell) else {
            return Err(unshared());
        };
        // SAFETY: the memory of a new `Arc`, which nothing else reaches.
        unsafe {
            Self::write(
                slot.as_mut_ptr(),
                limits,
                Some((runtime, index)),
                tuning,
                monitor,
            )?;
        };
        // SAFETY: `write` wrote every field.
        let shard = unsafe { cell.assume_init() };
        #[cfg(feature = "io-stats")]
        if shard.tuning.count_io {
            shard.shared.counters.count_io();
        }
        if shard.tuning.record_latencies {
            shard.shared.counters.record_latencies();
            #[cfg(all(completion, feature = "io-stats"))]
            shard.time_io();
        }
        shard.count_drive_from(0, shard.tuning.driver_interval);
        #[cfg(ring)]
        if let Some(ring) = shard.ring() {
            ring.set_zero_copy_min(shard.tuning.zero_copy_min);
        }
        Ok(shard)
    }

    /// Writes a shard at `slot`: worker `owner` (a runtime and a place) if it is one, run as
    /// `tuning` says, telling `monitor` when its park ends. Its counters know the place before
    /// `stats` can list it. A failure writes nothing.
    ///
    /// # Safety
    ///
    /// `slot` is valid for writes of a shard, and nothing reads it until this returns `Ok`.
    unsafe fn write(
        slot: *mut Self,
        limits: Limits,
        owner: Option<(u64, usize)>,
        tuning: Tuning,
        monitor: Option<Arc<Monitor>>,
    ) -> io::Result<()> {
        let mut shared = Arc::<Shared>::new_uninit();
        let Some(shared_slot) = Arc::get_mut(&mut shared).map(|cell| cell.as_mut_ptr()) else {
            return Err(unshared());
        };
        let pool = Pool::new(limits.buffer_cap);
        // The receive group asked for is charged before it is made; what the ring made in the end
        // is less on a ring made smaller, and nothing on the poller, and the rest is given back
        // below.
        let reserved = reserve_group(&pool, limits.backend)?;
        // SAFETY: the caller's contract. The driver is the first field written, and nothing after
        // it fails.
        let (notifier, kernel) = unsafe { driver_at(addr_of_mut!((*slot).driver), limits)? };
        // SAFETY: just written.
        let driver = unsafe { &*addr_of!((*slot).driver) };
        let made = match driver {
            #[cfg(ring)]
            Driver::Ring(ring) => ring.recv_reservation(),
            #[cfg(ring)]
            Driver::Gone => 0,
            Driver::Poll(_) => 0,
        };
        pool.unreserve(reserved.saturating_sub(made));
        write_fields!(Shared at shared_slot, [], {
            inbox: Inbox::new(),
            park: ParkState::new(),
            notifier: notifier,
            load: AtomicUsize::new(0),
            tasks: crate::rt::task::list::TaskList::new(),
            detached: AtomicUsize::new(0),
            stopped: AtomicBool::new(false),
            draining: AtomicBool::new(false),
            stop_waker: AtomicWaker::new(),
            timer_ops: Counted::new(),
            #[cfg(readiness)]
            registrar: kernel,
            #[cfg(port)]
            port: kernel,
            io_ops: Counted::new(),
            io_closed: AtomicBool::new(false),
            counters: Counters::new(driver.backend(), pool.cap(), owner),
            runtime: std::sync::OnceLock::new(),
            runtime_addr: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
            released: AtomicBool::new(false),
            timer_at: AtomicU64::new(u64::MAX),
            parks: AtomicU64::new(0),
            #[cfg(unix)]
            signal_pump: std::sync::atomic::AtomicBool::new(false),
        });
        // SAFETY: every field was just written.
        let shared = unsafe { shared.assume_init() };
        counters::register(&shared);
        write_fields!(Shard at slot, [driver], {
            shared: shared,
            queue: RefCell::new(VecDeque::new()),
            lifo: Cell::new(None),
            lifo_streak: Cell::new(0),
            until_inbox: Cell::new(tuning.inbox_interval),
            homes_first: Cell::new(false),
            drive_left: Cell::new(1),
            drive_at: Cell::new(1),
            last_drive: Cell::new(Quartz::now()),
            polls: Cell::new(0),
            sleeps: Cell::new(0),
            slept: Cell::new(core::time::Duration::ZERO),
            remote_wakes: Cell::new(0),
            looks: Cell::new(0),
            spin: Spin::new(),
            owner: Cell::new(owner),
            monitor: monitor,
            home: OnceCell::new(),
            limits: limits,
            tuning: tuning,
            wheel: const { RefCell::new(Wheel::new()) },
            precise: const { RefCell::new(Wheel::new()) },
            epoch: crate::time::clock::origin(),
            fired: RefCell::new(Vec::new()),
            side: RefCell::new(Vec::new()),
            foreign_poll: Cell::new(false),
            side_seq: Cell::new(0),
            serving: Cell::new(core::ptr::null()),
            signal: Cell::new(None),
            sources: RefCell::new(Sources::default()),
            children: RefCell::new(children::Children::default()),
            pool: pool,
            locals: locals::Locals::default(),
            #[cfg(port)]
            keeper: RefCell::new(None),
        });
        Ok(())
    }
}

/// Writes the driver `limits.backend` asks for at `dst`; returns its notifier and the poller's
/// registrar (on a port, the port its handles bind to). Nothing is written when it fails.
///
/// # Safety
///
/// `dst` is valid for writes of a driver.
#[cfg(readiness)]
unsafe fn driver_at(
    dst: *mut Driver,
    limits: Limits,
) -> io::Result<(super::Notifier, Option<super::Registrar>)> {
    let poller = || -> io::Result<_> {
        let poller = Poller::new()?;
        let kernel = (poller.notifier(), Some(poller.registrar()));
        // SAFETY: the caller's contract.
        unsafe { dst.write(Driver::Poll(RefCell::new(poller))) };
        Ok(kernel)
    };
    match limits.backend {
        crate::rt::backend::Backend::Poll => poller(),
        #[cfg(ring)]
        crate::rt::backend::Backend::Ring(cfg) | crate::rt::backend::Backend::RingOrPoll(cfg) => {
            // Under a tight memory limit the ring gets fewer entries; one `auto` chose runs the
            // poller if even the least finds none.
            let made = super::ring::ShardRing::for_shard(cfg);
            match made {
                // The notifier is taken from the ring where its constructor left it, which then
                // moves once, into its place.
                Ok(ref ring) => {
                    let notifier = ring.notifier();
                    // SAFETY: the caller's contract.
                    unsafe { dst.write(Driver::Ring(made?)) };
                    Ok((notifier, None))
                }
                // The choice of `auto` checked the kernel already (see `backend::usable`); a
                // shard that still finds the ring refused, or no memory, runs the poller rather
                // than fail.
                Err(err)
                    if matches!(limits.backend, crate::rt::backend::Backend::RingOrPoll(_))
                        && (err.raw_os_error() == Some(libc::ENOMEM)
                            || err.kind() == io::ErrorKind::Unsupported) =>
                {
                    poller()
                }
                Err(err) => Err(err),
            }
        }
    }
}

/// [`driver_at`] on a port.
///
/// # Safety
///
/// `dst` is valid for writes of a driver.
#[cfg(port)]
unsafe fn driver_at(
    dst: *mut Driver,
    _limits: Limits,
) -> io::Result<(super::Notifier, Arc<crate::rt::park::iocp::Port>)> {
    let poller = Poller::new()?;
    let kernel = (poller.notifier(), Arc::clone(poller.port()));
    // SAFETY: the caller's contract.
    unsafe { dst.write(Driver::Poll(RefCell::new(poller))) };
    Ok(kernel)
}

/// A new allocation another reference reached, which cannot happen; said rather than assumed.
fn unshared() -> io::Error {
    io::Error::other("a new shard's memory was shared before it was made")
}
