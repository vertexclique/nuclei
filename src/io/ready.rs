//! What a socket's readiness calls ask for and answer: [`Interest`] and
//! [`Ready`]. Nuclei's sockets are completed by the driver, so "ready" is
//! about the handle's own buffers: readable once a read would end at once
//! (bytes read ahead, the end of the stream, or an error), writable once a
//! write would be taken at once (room in the write-behind, or an error).

use core::ops::BitOr;

/// What a readiness call waits for: reading, writing, or either.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interest(u8);

impl Interest {
    /// A read would end at once.
    pub const READABLE: Interest = Interest(1);
    /// A write would be taken at once.
    pub const WRITABLE: Interest = Interest(2);

    /// Whether it asks for reading.
    pub fn is_readable(self) -> bool {
        self.0 & 1 != 0
    }

    /// Whether it asks for writing.
    pub fn is_writable(self) -> bool {
        self.0 & 2 != 0
    }
}

impl BitOr for Interest {
    type Output = Interest;

    fn bitor(self, other: Interest) -> Interest {
        Interest(self.0 | other.0)
    }
}

/// What a readiness call found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ready(u8);

impl Ready {
    /// Nothing is ready.
    pub const EMPTY: Ready = Ready(0);
    /// A read would end at once.
    pub const READABLE: Ready = Ready(1);
    /// A write would be taken at once.
    pub const WRITABLE: Ready = Ready(2);
    /// The stream ended: reads answer zero from now on.
    pub const READ_CLOSED: Ready = Ready(4);
    /// Writing failed: writes answer the error from now on.
    pub const WRITE_CLOSED: Ready = Ready(8);

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn is_readable(self) -> bool {
        self.0 & (Self::READABLE.0 | Self::READ_CLOSED.0) != 0
    }

    pub fn is_writable(self) -> bool {
        self.0 & (Self::WRITABLE.0 | Self::WRITE_CLOSED.0) != 0
    }

    pub fn is_read_closed(self) -> bool {
        self.0 & Self::READ_CLOSED.0 != 0
    }

    pub fn is_write_closed(self) -> bool {
        self.0 & Self::WRITE_CLOSED.0 != 0
    }
}

impl BitOr for Ready {
    type Output = Ready;

    fn bitor(self, other: Ready) -> Ready {
        Ready(self.0 | other.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interests_and_readiness_combine() {
        let both = Interest::READABLE | Interest::WRITABLE;
        assert!(both.is_readable() && both.is_writable());
        assert!(!Interest::READABLE.is_writable());
        let ready = Ready::READABLE | Ready::READ_CLOSED;
        assert!(ready.is_readable() && ready.is_read_closed() && !ready.is_writable());
        assert!(Ready::EMPTY.is_empty() && !ready.is_empty());
        assert!(Ready::WRITE_CLOSED.is_writable());
    }
}
