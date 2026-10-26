//! `select!`: waits on several futures at once and runs the handler of the
//! first one to finish whose output matches its pattern.
//!
//! The expansion keeps the futures pinned in a tuple and polls them from one
//! `poll_fn`; a future that finishes with an output its pattern rejects
//! disables its branch and the others go on. The handlers run after the
//! futures are dropped and outside any closure or loop of the expansion, so
//! `return`, `?`, `break` and `continue` in a handler act on the code around
//! the `select!`, and a handler may use again what the futures borrowed.

/// Waits on several branches at once and runs the handler of the first to
/// finish, dropping the others.
///
/// Each branch is `<pattern> = <future> (, if <precondition>)? => <handler>`,
/// with commas between the branches (optional after a block handler), and
/// an `else => <expression>` branch may come last. `select!`:
///
/// 1. Evaluates every precondition; a branch whose precondition is `false`
///    is disabled.
/// 2. Evaluates every future expression (of disabled branches too), each
///    an [`IntoFuture`](core::future::IntoFuture).
/// 3. Polls the futures of the enabled branches, starting at a random one so
///    no branch is favoured, or in the order written after `biased;`.
/// 4. When a future finishes, its branch is disabled. If the output matches
///    the branch's pattern, the other futures are dropped and the handler
///    runs with the pattern's bindings; if not, the output is dropped and
///    the other branches go on.
/// 5. When every branch is disabled, the `else` expression is the value.
///
/// The value of `select!` is the value of the handler that ran, so every
/// handler and `else` has the same type. A `select!` takes at most 64
/// branches.
///
/// # Panics
///
/// When every branch is disabled and there is no `else` branch.
///
/// # Examples
///
/// ```
/// use nuclei::sync::mpsc;
///
/// # nuclei::block_on(async {
/// let (tx, mut numbers) = mpsc::unbounded_channel::<u32>();
/// let (tick_tx, mut ticks) = mpsc::unbounded_channel::<()>();
/// tx.send(1).unwrap();
/// tx.send(2).unwrap();
/// tick_tx.send(()).unwrap();
/// drop((tx, tick_tx));
/// let (mut seen, mut ticked) = (Vec::new(), 0);
/// // Each closed channel gives `None`, which disables its branch; once
/// // both are closed, `else` ends the loop.
/// loop {
///     nuclei::select! {
///         Some(n) = numbers.recv() => seen.push(n),
///         Some(()) = ticks.recv() => ticked += 1,
///         else => break,
///     }
/// }
/// assert_eq!(seen, [1, 2]);
/// assert_eq!(ticked, 1);
/// # });
/// ```
///
/// Patterns bind as written, `mut`, `ref` and `&` included, in every
/// edition:
///
/// ```edition2021
/// # nuclei::block_on(async {
/// let label = String::from("one");
/// let n = nuclei::select! {
///     (mut n, ref name) = async { (1u32, &label) } => {
///         n += name.len() as u32;
///         n
///     }
/// };
/// assert_eq!(n, 4);
/// # });
/// ```
#[macro_export]
macro_rules! select {
    (biased;) => {
        ::core::compile_error!("`select!` needs at least one branch")
    };
    (biased; $($branches:tt)+) => {
        $crate::__select!(@branch [biased] [] [] $($branches)+)
    };
    () => {
        ::core::compile_error!("`select!` needs at least one branch")
    };
    ($($branches:tt)+) => {
        $crate::__select!(@branch [fair] [] [] $($branches)+)
    };
}

/// The branch muncher of [`select!`]. The state is
/// `[mode] [parsed branches] [one _ per parsed branch]`; a parsed branch is
/// `([its _ list] [its pattern] future, precondition => handler)`.
#[doc(hidden)]
#[macro_export]
macro_rules! __select {
    // The `else` branch, last.
    (@branch $mode:tt [$($done:tt)*] [$($all:tt)*] else => $else:expr $(,)?) => {
        $crate::__select!(@emit $mode [$($done)*] [$($all)*] [$else])
    };
    // No branch left and no `else`.
    (@branch $mode:tt [$($done:tt)*] [$($all:tt)*]) => {
        $crate::__select!(
            @emit $mode [$($done)*] [$($all)*]
            [::core::panic!("all branches of `select!` are disabled and there is no `else` branch")]
        )
    };
    // A branch: its pattern runs up to the first `=` outside a group.
    (@branch $mode:tt [$($done:tt)*] [$($all:tt)*] $($rest:tt)+) => {
        $crate::__select!(@pat $mode [$($done)*] [$($all)*] [] $($rest)+)
    };

    (@pat $mode:tt [$($done:tt)*] [$($all:tt)*] [$($pat:tt)+] = $future:expr, if $cond:expr => $($rest:tt)+) => {
        $crate::__select!(@handler $mode [$($done)*] [$($all)*] [$($pat)+] $future, $cond => $($rest)+)
    };
    (@pat $mode:tt [$($done:tt)*] [$($all:tt)*] [$($pat:tt)+] = $future:expr => $($rest:tt)+) => {
        $crate::__select!(@handler $mode [$($done)*] [$($all)*] [$($pat)+] $future, true => $($rest)+)
    };
    (@pat $mode:tt [$($done:tt)*] [$($all:tt)*] [$($pat:tt)*] $next:tt $($rest:tt)*) => {
        $crate::__select!(@pat $mode [$($done)*] [$($all)*] [$($pat)* $next] $($rest)*)
    };

    (@handler $mode:tt [$($done:tt)*] [$($all:tt)*] [$($pat:tt)+] $future:expr, $cond:expr => $handler:block, $($rest:tt)*) => {
        $crate::__select!(@branch $mode [$($done)* ([$($all)*] [$($pat)+] $future, $cond => $handler)] [$($all)* _] $($rest)*)
    };
    (@handler $mode:tt [$($done:tt)*] [$($all:tt)*] [$($pat:tt)+] $future:expr, $cond:expr => $handler:block $($rest:tt)*) => {
        $crate::__select!(@branch $mode [$($done)* ([$($all)*] [$($pat)+] $future, $cond => $handler)] [$($all)* _] $($rest)*)
    };
    (@handler $mode:tt [$($done:tt)*] [$($all:tt)*] [$($pat:tt)+] $future:expr, $cond:expr => $handler:expr, $($rest:tt)*) => {
        $crate::__select!(@branch $mode [$($done)* ([$($all)*] [$($pat)+] $future, $cond => { $handler })] [$($all)* _] $($rest)*)
    };
    (@handler $mode:tt [$($done:tt)*] [$($all:tt)*] [$($pat:tt)+] $future:expr, $cond:expr => $handler:expr) => {
        $crate::__select!(@branch $mode [$($done)* ([$($all)*] [$($pat)+] $future, $cond => { $handler })] [$($all)* _])
    };

    (@emit [$mode:ident]
        [$( ([$($skip:tt)*] [$($pat:tt)+] $future:expr, $cond:expr => $handler:tt) )+]
        [$($all:tt)*]
        [$else:expr]
    ) => {{
        const _: () = ::core::assert!(
            $crate::__count!($($all)*) <= 64,
            "`select!` takes at most 64 branches"
        );
        // The futures live in this block only: they are dropped before the
        // handler runs, so the handler may use what they borrowed.
        let __output = {
            let mut __disabled: u64 = 0;
            $(
                if !$cond {
                    __disabled |= 1u64 << $crate::__count!($($skip)*);
                }
            )+
            let mut __futures = ($(
                ::core::pin::pin!(::core::future::IntoFuture::into_future($future)),
            )+);
            $crate::macros::support::poll_fn(|__cx: &mut ::core::task::Context<'_>| {
                if !$crate::macros::support::proceed(__cx) {
                    return ::core::task::Poll::Pending;
                }
                let __branches: u32 = $crate::__count!($($all)*);
                let __start: u32 = $crate::__select_start!($mode, __branches);
                let mut __pending = false;
                for __turn in 0..__branches {
                    let __branch = (__start + __turn) % __branches;
                    $(
                        if __branch == $crate::__count!($($skip)*) {
                            if __disabled & (1u64 << __branch) == 0 {
                                let ($($skip,)* __future, ..) = &mut __futures;
                                match ::core::future::Future::poll(::core::pin::Pin::as_mut(__future), __cx) {
                                    ::core::task::Poll::Ready(__value) => {
                                        __disabled |= 1u64 << __branch;
                                        let __matches = match &__value {
                                            #[allow(unused_variables)]
                                            $crate::__select_clean!($($pat)+) => true,
                                            #[allow(unreachable_patterns)]
                                            _ => false,
                                        };
                                        if __matches {
                                            return ::core::task::Poll::Ready($crate::__select_wrap!(
                                                [$($skip)*]
                                                $crate::macros::support::Branch::Taken(__value)
                                            ));
                                        }
                                    }
                                    ::core::task::Poll::Pending => __pending = true,
                                }
                            }
                            continue;
                        }
                    )+
                }
                if __pending {
                    ::core::task::Poll::Pending
                } else {
                    ::core::task::Poll::Ready($crate::__select_wrap!(
                        [$($all)*]
                        $crate::macros::support::AllDisabled
                    ))
                }
            })
            .await
        };
        match __output {
            $(
                $crate::__select_wrap!(
                    [$($skip)*]
                    $crate::macros::support::Branch::Taken($($pat)+)
                ) => $handler,
            )+
            $crate::__select_wrap!([$($all)*] $crate::macros::support::AllDisabled) => $else,
            // An output reaches here only through a pattern it matched.
            #[allow(unreachable_patterns)]
            _ => ::core::unreachable!("a `select!` output its pattern rejected"),
        }
    }};
}

/// The first branch a `select!` polls.
#[doc(hidden)]
#[macro_export]
macro_rules! __select_start {
    (biased, $branches:expr) => {
        0
    };
    (fair, $branches:expr) => {
        $crate::macros::support::start($branches)
    };
}

/// Wraps a value or a pattern in one `Branch::Next` per `_`.
#[doc(hidden)]
#[macro_export]
macro_rules! __select_wrap {
    ([] $($inner:tt)+) => {
        $($inner)+
    };
    ([_ $($skip:tt)*] $($inner:tt)+) => {
        $crate::macros::support::Branch::Next($crate::__select_wrap!([$($skip)*] $($inner)+))
    };
}

/// A branch's pattern as a test against a reference to the output: the same
/// pattern with its `ref`, `mut` and `&` taken out, so its bindings borrow
/// and match ergonomics apply in every edition. The state is
/// `@ [cleaned so far] [enclosing groups]`; a group is cleaned on its own
/// with the outer state saved on the stack, then put back in its delimiters.
#[doc(hidden)]
#[macro_export]
macro_rules! __select_clean {
    (@ [$($out:tt)*] []) => {
        $($out)*
    };
    (@ [$($inner:tt)*] [(paren [$($out:tt)*] [$($rest:tt)*]) $($stack:tt)*]) => {
        $crate::__select_clean!(@ [$($out)* ($($inner)*)] [$($stack)*] $($rest)*)
    };
    (@ [$($inner:tt)*] [(bracket [$($out:tt)*] [$($rest:tt)*]) $($stack:tt)*]) => {
        $crate::__select_clean!(@ [$($out)* [$($inner)*]] [$($stack)*] $($rest)*)
    };
    (@ [$($inner:tt)*] [(brace [$($out:tt)*] [$($rest:tt)*]) $($stack:tt)*]) => {
        $crate::__select_clean!(@ [$($out)* {$($inner)*}] [$($stack)*] $($rest)*)
    };
    (@ [$($out:tt)*] [$($stack:tt)*] ref $($rest:tt)*) => {
        $crate::__select_clean!(@ [$($out)*] [$($stack)*] $($rest)*)
    };
    (@ [$($out:tt)*] [$($stack:tt)*] mut $($rest:tt)*) => {
        $crate::__select_clean!(@ [$($out)*] [$($stack)*] $($rest)*)
    };
    (@ [$($out:tt)*] [$($stack:tt)*] & $($rest:tt)*) => {
        $crate::__select_clean!(@ [$($out)*] [$($stack)*] $($rest)*)
    };
    (@ [$($out:tt)*] [$($stack:tt)*] && $($rest:tt)*) => {
        $crate::__select_clean!(@ [$($out)*] [$($stack)*] $($rest)*)
    };
    (@ [$($out:tt)*] [$($stack:tt)*] ($($group:tt)*) $($rest:tt)*) => {
        $crate::__select_clean!(@ [] [(paren [$($out)*] [$($rest)*]) $($stack)*] $($group)*)
    };
    (@ [$($out:tt)*] [$($stack:tt)*] [$($group:tt)*] $($rest:tt)*) => {
        $crate::__select_clean!(@ [] [(bracket [$($out)*] [$($rest)*]) $($stack)*] $($group)*)
    };
    (@ [$($out:tt)*] [$($stack:tt)*] {$($group:tt)*} $($rest:tt)*) => {
        $crate::__select_clean!(@ [] [(brace [$($out)*] [$($rest)*]) $($stack)*] $($group)*)
    };
    (@ [$($out:tt)*] [$($stack:tt)*] $next:tt $($rest:tt)*) => {
        $crate::__select_clean!(@ [$($out)* $next] [$($stack)*] $($rest)*)
    };
    ($($pat:tt)+) => {
        $crate::__select_clean!(@ [] [] $($pat)+)
    };
}
