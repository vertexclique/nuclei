//! `join!` and `try_join!`: wait on several futures at once, in the task
//! that awaits them, and give their outputs as one tuple.
//!
//! Each future sits pinned in a `MaybeDone` in one tuple; every poll polls
//! the unfinished ones, starting one later each time so a future that is
//! always ready cannot starve the ones after it (or in the order written
//! after `biased;`).

/// Waits on every future given, all at once, and gives their outputs as a
/// tuple in the order written.
///
/// The futures run concurrently in the task that awaits `join!`, not in
/// tasks of their own: [`spawn`](crate::spawn) them for parallelism. Each
/// argument is an [`IntoFuture`](core::future::IntoFuture). Each poll polls
/// the unfinished futures, starting one later each time; after `biased;`
/// they are polled in the order written. `join!()` is `()`.
///
/// # Examples
///
/// ```
/// use nuclei::sync::oneshot;
///
/// # nuclei::block_on(async {
/// let (tx, rx) = oneshot::channel::<u32>();
/// // Each needs the other: only a concurrent wait finishes.
/// let (sent, got) = nuclei::join!(async { tx.send(7) }, rx);
/// assert!(sent.is_ok());
/// assert_eq!(got.unwrap(), 7);
/// # });
/// ```
#[macro_export]
macro_rules! join {
    () => {
        ()
    };
    (biased; $($future:expr),+ $(,)?) => {
        $crate::__join!(@munch join [biased] [] [] $($future,)+)
    };
    ($($future:expr),+ $(,)?) => {
        $crate::__join!(@munch join [fair] [] [] $($future,)+)
    };
}

/// Waits on every future given, all at once, each giving a `Result` with
/// the same error type, and gives `Ok` with their outputs as a tuple in the
/// order written, or the first error.
///
/// On the first `Err`, `try_join!` gives it at once and drops the other
/// futures. Otherwise it runs as [`join!`](crate::join) does: concurrently,
/// in the awaiting task, polled from one later each time or in order after
/// `biased;`. `try_join!()` is `Ok(())`.
///
/// # Examples
///
/// ```
/// # nuclei::block_on(async {
/// let ok: Result<(u8, u16), &str> = nuclei::try_join!(async { Ok(1) }, async { Ok(2) });
/// assert_eq!(ok, Ok((1, 2)));
///
/// let failed: Result<(u8, ()), &str> =
///     nuclei::try_join!(async { Err("first") }, std::future::pending::<Result<(), &str>>());
/// assert_eq!(failed, Err("first"));
/// # });
/// ```
#[macro_export]
macro_rules! try_join {
    () => {
        ::core::result::Result::Ok(())
    };
    (biased; $($future:expr),+ $(,)?) => {
        $crate::__join!(@munch try_join [biased] [] [] $($future,)+)
    };
    ($($future:expr),+ $(,)?) => {
        $crate::__join!(@munch try_join [fair] [] [] $($future,)+)
    };
}

/// The muncher of [`join!`] and [`try_join!`]: gives each future its `_`
/// list (one `_` per future before it), then expands.
#[doc(hidden)]
#[macro_export]
macro_rules! __join {
    (@munch $kind:ident $mode:tt [$($done:tt)*] [$($all:tt)*] $future:expr, $($rest:tt)*) => {
        $crate::__join!(@munch $kind $mode [$($done)* ([$($all)*] $future)] [$($all)* _] $($rest)*)
    };
    (@munch $kind:ident [$mode:ident] [$( ([$($skip:tt)*] $future:expr) )+] [$($all:tt)*]) => {{
        let mut __futures = ($(
            ::core::pin::pin!($crate::macros::support::MaybeDone::new(
                ::core::future::IntoFuture::into_future($future),
            )),
        )+);
        let mut __next: u32 = 0;
        $crate::macros::support::poll_fn(|__cx: &mut ::core::task::Context<'_>| {
            if !$crate::macros::support::proceed(__cx) {
                return ::core::task::Poll::Pending;
            }
            let __count: u32 = $crate::__count!($($all)*);
            let __start: u32 = $crate::__join!(@start $mode __next);
            __next = (__start + 1) % __count;
            let mut __done = true;
            for __turn in 0..__count {
                let __index = (__start + __turn) % __count;
                $(
                    if __index == $crate::__count!($($skip)*) {
                        let ($($skip,)* __future, ..) = &mut __futures;
                        if $crate::macros::support::MaybeDone::poll_done(::core::pin::Pin::as_mut(__future), __cx) {
                            $crate::__join!(@check $kind __future);
                        } else {
                            __done = false;
                        }
                        continue;
                    }
                )+
            }
            if !__done {
                return ::core::task::Poll::Pending;
            }
            ::core::task::Poll::Ready($crate::__join!(@output $kind ($({
                let ($($skip,)* __future, ..) = &mut __futures;
                match $crate::macros::support::MaybeDone::take_output(::core::pin::Pin::as_mut(__future)) {
                    ::core::option::Option::Some(__output) => __output,
                    ::core::option::Option::None => ::core::unreachable!("a joined output taken twice"),
                }
            },)+)))
        })
        .await
    }};

    (@start biased $next:ident) => {
        0
    };
    (@start fair $next:ident) => {
        $next
    };

    // `join!` waits for every output; `try_join!` ends at the first error.
    (@check join $future:ident) => {};
    (@check try_join $future:ident) => {
        if let ::core::option::Option::Some(::core::result::Result::Err(_)) =
            $crate::macros::support::MaybeDone::output_mut(::core::pin::Pin::as_mut($future))
        {
            if let ::core::option::Option::Some(::core::result::Result::Err(__error)) =
                $crate::macros::support::MaybeDone::take_output(::core::pin::Pin::as_mut($future))
            {
                return ::core::task::Poll::Ready(::core::result::Result::Err(__error));
            }
        }
    };

    (@output join ($($output:expr,)+)) => {
        ($($output,)+)
    };
    (@output try_join ($($output:expr,)+)) => {
        ::core::result::Result::Ok(($(
            match $output {
                ::core::result::Result::Ok(__value) => __value,
                ::core::result::Result::Err(_) => ::core::unreachable!("a `try_join!` error not given at once"),
            },
        )+))
    };
}
