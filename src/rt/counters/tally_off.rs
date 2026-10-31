//! The IO counts built without the `io-stats` feature: nothing is counted, and
//! each count below is an empty inline function, so the IO paths that call
//! them carry no counting code at all. The counting items of `stats`
//! (`Builder::count_io`, `ShardStats::io`) do not exist then.

use std::io;

/// No thread counts its IO.
#[inline(always)]
pub(crate) fn counting() -> bool {
    false
}

#[inline(always)]
pub(crate) fn read(_bytes: usize) {}

#[inline(always)]
pub(crate) fn wrote(_bytes: usize) {}

#[cfg(any(ring, port))]
#[inline(always)]
pub(crate) fn accepted(_count: usize) {}

#[inline(always)]
pub(crate) fn reading(_result: &io::Result<usize>) {}

#[inline(always)]
pub(crate) fn received<A>(_result: &io::Result<(usize, A)>) {}

#[inline(always)]
pub(crate) fn writing(_result: &io::Result<usize>) {}

#[cfg(readiness)]
#[inline(always)]
pub(crate) fn accepting<A>(_result: &io::Result<A>) {}

#[inline(always)]
pub(crate) fn add_read(_bytes: usize) {}

#[cfg(ring)]
#[inline(always)]
pub(crate) fn add_write(_bytes: usize) {}
