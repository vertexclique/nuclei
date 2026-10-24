//! Multi-producer, single-consumer channels for sending values between tasks.
//!
//! The module has two flavors. The bounded one ([`channel`]) holds at most a
//! fixed number of messages, and a send waits for room, which is backpressure.
//! The unbounded one ([`unbounded_channel`]) never makes a send wait, so
//! [`UnboundedSender::send`] is an ordinary function that synchronous and
//! asynchronous code can both call. The names, signatures, error types and
//! documented behavior are those of the established async channel API, so code
//! written against that API compiles against these; nothing here needs a
//! runtime, and every wait is a plain [`Waker`](core::task::Waker) handshake.
//!
//! ```
//! use nuclei::sync::mpsc;
//!
//! let (tx, mut rx) = mpsc::channel(8);
//! let senders: Vec<_> = (0..4)
//!     .map(|id| {
//!         let tx = tx.clone();
//!         std::thread::spawn(move || {
//!             for n in 0..100 {
//!                 tx.blocking_send((id, n)).expect("the receiver is still there");
//!             }
//!         })
//!     })
//!     .collect();
//! drop(tx);
//!
//! let mut seen = [0; 4];
//! while let Some((id, _n)) = rx.blocking_recv() {
//!     seen[id] += 1;
//! }
//! assert_eq!(seen, [100; 4]);
//! for sender in senders {
//!     sender.join().unwrap();
//! }
//! ```
//!
//! # Ordering
//!
//! What one sender sends comes out in the order it was sent, as the
//! established API promises. The message queue is kovan's segment queue, a
//! linearizable FIFO queue: a send claims the next cell, and the receiver
//! takes cells in the order they were claimed, so a send that returned before
//! another began comes out first. Values that different senders send at the
//! same time come out in the order their sends claimed cells.
//!
//! # Disconnection
//!
//! When every sender is dropped, no value can be sent. That is the end of the
//! stream: once the buffered values are received, `recv` returns `None`.
//!
//! If the receiver is dropped, its unread values are dropped with it, and
//! every later send fails with an error that holds the value.
//!
//! # Clean shutdown
//!
//! Dropping the receiver loses the values still buffered. For a clean
//! shutdown the receiver calls `close` first, which stops further sends but
//! keeps the buffered values (and any slot a sender reserved before) usable,
//! then receives until `recv` returns `None`.
//!
//! # Weak senders
//!
//! A [`WeakSender`] does not keep the channel open: when only weak senders
//! remain the channel is closed. [`WeakSender::upgrade`] gives a [`Sender`]
//! while one exists. An actor can hold a weak handle to its own mailbox
//! without keeping it alive.
//!
//! # Communicating between sync and async code
//!
//! Every method that is not `async` can be called from any thread. For
//! synchronous code on the receiving or sending end, the `blocking_*` methods
//! park the calling thread until their wait is over. On a nuclei shard they
//! run a nested `block_on` instead, which keeps the shard running its other
//! tasks; do not call them from a task of another runtime, whose thread they
//! block.
//!
//! # Differences from the established API
//!
//! - [`Sender::send_timeout`] waits on nuclei's own timers: on a thread that
//!   runs no nuclei shard, the default runtime fires it.
//! - The `blocking_*` methods called from inside a task do not panic: on a
//!   nuclei shard they run a nested `block_on`, and inside another runtime's
//!   task they block its thread (on `wasm32`, where nothing else can run, a
//!   call that would wait forever panics instead).
//! - When the receiver closes, a reservation still waiting gives back the
//!   slots set aside for it at the close, where the established API keeps
//!   them with the failed wait until its future is dropped. So a closed
//!   receiver's drain never waits for a failed reservation to be dropped.
//! - [`Receiver::try_recv`] does not park the thread for a send that is in
//!   progress on another thread. Such a value is not in the channel yet, and
//!   its sender wakes the receiver when it is; the established API parks the
//!   thread until the send finishes.
//! - There is no cooperative-scheduling budget: a receiver that always finds a
//!   value never yields.
//!
//! # Progress
//!
//! The message queue is kovan's segment queue. A send claims the next cell
//! with one fetch-add, writes its value into it and publishes it with a
//! compare-and-swap. It retries only when another send linked the next
//! segment first, so a send never waits for another thread, and some send
//! completes whenever any retries: the push is lock-free. A receive takes the
//! next cell with a compare-and-swap no other thread contends for, and never
//! waits for a sender: when the next cell is claimed by a send that has not
//! written it yet, the receive finds nothing for now, registers its waker and
//! returns `Pending`, and that send wakes it once its value is in. So a sender
//! the scheduler stops between its claim and its write holds back the values
//! behind its own until it runs again, but no call waits for it, and other
//! senders are never held up by it.
//!
//! - **Wait-free** (a bounded number of the caller's own atomic steps, whatever
//!   other threads do): the accessors (`is_closed`, `len`, `is_empty`,
//!   `capacity`, `max_capacity`, the sender counts, `same_channel`), `clone`,
//!   `downgrade`, and dropping a sender or weak sender (the last sender's
//!   drop also wakes the receiver, a two-step handshake on the waker cell).
//!   Registering the receiver's waker is wait-free too.
//! - **Lock-free**: every send, `try_send`, reservation and permit send;
//!   `closed`; `WeakSender::upgrade` (a compare-and-swap loop on the sender
//!   count); and `Receiver::close`. A bounded wait that has to queue is
//!   lock-free for the reason every wait queue is (see the
//!   [parent module](super#progress)): its poll pushes a node, and may serve
//!   the queue for one pass plus one for each operation of another thread
//!   that lands meanwhile.
//! - **When a waiting bounded send is served** is not a bound on its own
//!   calls. Its slot is set aside and granted by whichever thread serves the
//!   queue once the slot is free (the receiver taking a value, a permit
//!   dropped), so a thread the scheduler stops while it serves the queue
//!   delays that grant until it runs again, and a `try_send` fails meanwhile
//!   even if the last waiting send has just left. No call waits for it.
//! - **Lock-free**: every receive (`recv`, `recv_many`, `try_recv`,
//!   `poll_recv`, `poll_recv_many`), none of which waits for a sender inside
//!   the call, and the receiver's drop, which drops every value left and so
//!   takes time proportional to them.
//! - A [`Waker`](core::task::Waker) the channel calls is the executor's, and so
//!   is what it does; the destructor of a value runs in the call that drops
//!   it; and the `blocking_*` methods park the thread until their wait is over,
//!   which is what they are for.
//!
//! # Cancel safety
//!
//! `recv`, `recv_many` and `closed` are cancel safe: dropping the future loses
//! nothing. `send` is cancel safe in the sense that a dropped future sent
//! nothing, but it drops the value it held; `reserve` and its relatives hold no
//! slot when dropped, and a dropped `reserve_owned` also drops its sender.
//!
//! # Fairness
//!
//! The bounded channel serves waiting sends and reservations strictly in the
//! order they began to wait, as the established API does. Slots that come
//! free are set aside for the oldest waiting request until it has all it
//! asked for, so a `reserve_many(n)` is not starved by smaller requests and
//! nothing behind it is served first; a `try_send` or `try_reserve` never
//! takes a slot while a request waits. Dropping a waiting future gives up its
//! place and passes on whatever was set aside or granted to it, so no slot
//! and no wakeup is lost. The unbounded channel never waits, so it has no
//! order to keep.

mod bounded;
mod chan;
pub mod error;
mod permit;
mod queue;
mod receiver;
mod unbounded;

pub use self::bounded::{Sender, WeakSender, channel};
pub use self::permit::{OwnedPermit, Permit, PermitIterator};
pub use self::receiver::Receiver;
pub use self::unbounded::{
    UnboundedReceiver, UnboundedSender, WeakUnboundedSender, unbounded_channel,
};
