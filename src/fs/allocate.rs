//! What [`File::allocate`](super::File::allocate) does with a range of a file.

/// How a call to [`File::allocate`](super::File::allocate) changes the space
/// a range of a file uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Allocate {
    /// Reserves disk space for the range, so a later write into it does not
    /// run out of space, and grows the file to cover the range when the range
    /// ends past its end. The new part reads as zeros.
    Extend,
    /// Reserves disk space for the range and leaves the file's length as it
    /// is.
    KeepSize,
    /// Frees the disk space of the range, which then reads as zeros, and
    /// leaves the file's length as it is. A range that ends past the end of
    /// the file does not grow it.
    PunchHole,
    /// Makes the range read as zeros, without a write of zeros, and grows the
    /// file to cover the range when it ends past the end, as a write of zeros
    /// would.
    ZeroRange,
}
