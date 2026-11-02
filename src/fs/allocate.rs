//! What [`File::allocate`](super::File::allocate) does with a range of a file.

/// How a call to [`File::allocate`](super::File::allocate) changes the space
/// a range of a file uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Allocate {
    /// Reserves disk space for the range, so a later write into it does not
    /// run out of space, and grows the file to cover the range when the range
    /// ends past its end. The new part reads as zeros. On macOS the space is
    /// reserved from the end of what the file has allocated
    /// (`F_PREALLOCATE`): a hole inside the file, as a `set_len` that grew
    /// it leaves, stays a hole and is not reserved.
    Extend,
    /// Reserves disk space for the range and leaves the file's length as it
    /// is. On FreeBSD only inside the file: a range that ends past its end
    /// answers `Unsupported`. On macOS as for `Extend`, and HFS+ frees what
    /// was reserved past the end of the file when the file is closed (APFS
    /// keeps it).
    KeepSize,
    /// Frees the disk space of the range, which then reads as zeros, and
    /// leaves the file's length as it is. A range that ends past the end of
    /// the file does not grow it. On FreeBSD a file system that cannot free
    /// a range writes zeros over it instead (UFS).
    PunchHole,
    /// Makes the range read as zeros, without a write of zeros, and grows the
    /// file to cover the range when it ends past the end, as a write of zeros
    /// would. On FreeBSD the system writes the zeros where the file system
    /// cannot free the range (UFS), and grows the file with a write of one
    /// zero at the range's last byte.
    ZeroRange,
}
