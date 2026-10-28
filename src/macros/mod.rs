//! The future-combining macros, [`select!`](crate::select),
//! [`join!`](crate::join) and [`try_join!`](crate::try_join), and the types
//! their expansions use. The macros run on any executor: nothing in them
//! needs a nuclei runtime, though under one they yield when the task's
//! [cooperative budget](crate::task::coop) is spent.

mod join;
mod select;

/// What the macro expansions name; not a public interface.
#[doc(hidden)]
pub mod support {
    use core::future::Future;
    use core::pin::Pin;
    use core::task::{Context, Poll};

    pub use core::future::poll_fn;

    /// The output of one `select!` branch, nested once per branch before it:
    /// branch `k` is `Next` `k` times around `Taken`, and the value with
    /// every branch disabled is `Next` once per branch around
    /// [`AllDisabled`]. The nesting gives each branch's output its own type
    /// without a type declared per call.
    pub enum Branch<T, R> {
        Taken(T),
        Next(R),
    }

    /// Every `select!` branch was disabled.
    pub struct AllDisabled;

    /// The branch a fair `select!` polls first.
    #[inline]
    pub fn start(branches: u32) -> u32 {
        crate::rt::rng::random_below(branches as usize) as u32
    }

    /// Whether the task may go on polling; when its cooperative budget is
    /// spent, it is woken to run again after the other tasks and answers
    /// `false`.
    #[inline]
    pub fn proceed(cx: &mut Context<'_>) -> bool {
        if crate::coop::has_budget_remaining() {
            true
        } else {
            cx.waker().wake_by_ref();
            false
        }
    }

    /// A future of `join!` or `try_join!`, then its output once it has
    /// one, then nothing once the output is taken.
    pub enum MaybeDone<F: Future> {
        Future(F),
        Done(F::Output),
        Gone,
    }

    impl<F: Future> MaybeDone<F> {
        pub fn new(future: F) -> Self {
            Self::Future(future)
        }

        /// Polls the future if it has not finished; `true` once the output
        /// is here.
        ///
        /// # Panics
        ///
        /// When the output was taken already.
        #[expect(
            clippy::panic,
            reason = "polled after its output was taken, which the macros never do"
        )]
        pub fn poll_done(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> bool {
            // SAFETY: the future is structurally pinned: it is polled where it
            // is and dropped in place by `set`, never moved out.
            let output = match unsafe { self.as_mut().get_unchecked_mut() } {
                // SAFETY: as above, the future stays where it is.
                Self::Future(future) => match unsafe { Pin::new_unchecked(future) }.poll(cx) {
                    Poll::Ready(output) => output,
                    Poll::Pending => return false,
                },
                Self::Done(_) => return true,
                Self::Gone => panic!("a joined future was polled after its output was taken"),
            };
            self.set(Self::Done(output));
            true
        }

        /// The output, if the future has finished and it was not taken.
        pub fn output_mut(self: Pin<&mut Self>) -> Option<&mut F::Output> {
            // SAFETY: the output is not structurally pinned; `Done` holds no
            // future.
            match unsafe { self.get_unchecked_mut() } {
                Self::Done(output) => Some(output),
                _ => None,
            }
        }

        /// Takes the output out, if the future has finished and it was not
        /// taken.
        pub fn take_output(self: Pin<&mut Self>) -> Option<F::Output> {
            if !matches!(&*self, Self::Done(_)) {
                return None;
            }
            // SAFETY: only a `Done` value is moved out, and it holds no
            // pinned future.
            match core::mem::replace(unsafe { self.get_unchecked_mut() }, Self::Gone) {
                Self::Done(output) => Some(output),
                _ => None,
            }
        }
    }
}

/// The number of `_` tokens given, as a `u32` constant expression (a
/// slice length, not a sum, which lints read as an off-by-one comparison).
#[doc(hidden)]
#[macro_export]
macro_rules! __count {
    ($($skip:tt)*) => {
        (<[()]>::len(&[$($crate::__unit!($skip)),*]) as u32)
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __unit {
    ($skip:tt) => {
        ()
    };
}
