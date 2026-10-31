//! The options a socket takes before it binds, the same for a TCP socket to set up
//! ([`TcpSocket`](super::TcpSocket)) and a UDP one ([`UdpSocketBuilder`](super::UdpSocketBuilder)):
//! one macro writes them for both, so the two never disagree.

/// A network interface's name, as `bind_device` takes it: up to 16 bytes,
/// held in place.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct DeviceName {
    pub(super) bytes: [u8; libc::IFNAMSIZ],
    pub(super) len: u8,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl core::ops::Deref for DeviceName {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl core::fmt::Debug for DeviceName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", String::from_utf8_lossy(self))
    }
}

/// The options of [`sockopt`](self) on `$ty`, a type with a `raw(&self) -> Raw`.
macro_rules! socket_options {
    ($($ty:ty),* $(,)?) => {$(
        impl $ty {
            /// Sets `SO_REUSEADDR`: the socket may bind an address still held by a
            /// connection that is closing.
            pub fn set_reuseaddr(&self, reuseaddr: bool) -> std::io::Result<()> {
                opt::set_flag(self.raw(), opt::SOL_SOCKET, opt::SO_REUSEADDR, reuseaddr)
            }

            /// Whether `SO_REUSEADDR` is set.
            pub fn reuseaddr(&self) -> std::io::Result<bool> {
                opt::flag(self.raw(), opt::SOL_SOCKET, opt::SO_REUSEADDR)
            }

            /// Sets `SO_REUSEPORT`: several sockets may bind the same address and
            /// port, and the system shares what comes among them.
            #[cfg(all(unix, not(any(target_os = "solaris", target_os = "illumos"))))]
            pub fn set_reuseport(&self, reuseport: bool) -> std::io::Result<()> {
                opt::set_flag(self.raw(), opt::SOL_SOCKET, opt::SO_REUSEPORT, reuseport)
            }

            /// Whether `SO_REUSEPORT` is set.
            #[cfg(all(unix, not(any(target_os = "solaris", target_os = "illumos"))))]
            pub fn reuseport(&self) -> std::io::Result<bool> {
                opt::flag(self.raw(), opt::SOL_SOCKET, opt::SO_REUSEPORT)
            }

            /// Sets the size of the system's send buffer (`SO_SNDBUF`); the system
            /// may round it (Linux doubles it).
            pub fn set_send_buffer_size(&self, size: u32) -> std::io::Result<()> {
                opt::set_size(self.raw(), opt::SOL_SOCKET, opt::SO_SNDBUF, size)
            }

            /// The size of the system's send buffer (`SO_SNDBUF`).
            pub fn send_buffer_size(&self) -> std::io::Result<u32> {
                opt::size(self.raw(), opt::SOL_SOCKET, opt::SO_SNDBUF)
            }

            /// Sets the size of the system's receive buffer (`SO_RCVBUF`); the
            /// system may round it (Linux doubles it).
            pub fn set_recv_buffer_size(&self, size: u32) -> std::io::Result<()> {
                opt::set_size(self.raw(), opt::SOL_SOCKET, opt::SO_RCVBUF, size)
            }

            /// The size of the system's receive buffer (`SO_RCVBUF`).
            pub fn recv_buffer_size(&self) -> std::io::Result<u32> {
                opt::size(self.raw(), opt::SOL_SOCKET, opt::SO_RCVBUF)
            }

            /// Sets the type of service of the socket's IPv4 packets (`IP_TOS`).
            pub fn set_tos(&self, tos: u32) -> std::io::Result<()> {
                let tos = opt::int(u64::from(tos), "type of service")?;
                opt::set(self.raw(), opt::IPPROTO_IP, opt::IP_TOS, tos)
            }

            /// The type of service of the socket's IPv4 packets (`IP_TOS`).
            pub fn tos(&self) -> std::io::Result<u32> {
                opt::size(self.raw(), opt::IPPROTO_IP, opt::IP_TOS)
            }

            /// Sets the traffic class of the socket's IPv6 packets (`IPV6_TCLASS`).
            #[cfg(unix)]
            pub fn set_tclass_v6(&self, tclass: u32) -> std::io::Result<()> {
                let tclass = opt::int(u64::from(tclass), "traffic class")?;
                opt::set(self.raw(), opt::IPPROTO_IPV6, opt::IPV6_TCLASS, tclass)
            }

            /// The traffic class of the socket's IPv6 packets (`IPV6_TCLASS`).
            #[cfg(unix)]
            pub fn tclass_v6(&self) -> std::io::Result<u32> {
                opt::size(self.raw(), opt::IPPROTO_IPV6, opt::IPV6_TCLASS)
            }

            /// Sets `IPV6_V6ONLY` on an IPv6 socket: with `true` it takes IPv6 alone,
            /// else IPv4 too, through addresses mapped into IPv6.
            ///
            /// # Errors
            ///
            /// As `setsockopt(2)`: an IPv4 socket has no such option, and a bound one
            /// may no longer change it.
            pub fn set_only_v6(&self, only_v6: bool) -> std::io::Result<()> {
                opt::set_flag(self.raw(), opt::IPPROTO_IPV6, opt::IPV6_V6ONLY, only_v6)
            }

            /// Whether `IPV6_V6ONLY` is set.
            ///
            /// # Errors
            ///
            /// As `getsockopt(2)`: an IPv4 socket has no such option.
            pub fn only_v6(&self) -> std::io::Result<bool> {
                opt::flag(self.raw(), opt::IPPROTO_IPV6, opt::IPV6_V6ONLY)
            }

            /// Binds the socket to the network interface named `interface`
            /// (`SO_BINDTODEVICE`), or, with `None`, to none.
            #[cfg(any(target_os = "linux", target_os = "android"))]
            pub fn bind_device(&self, interface: Option<&[u8]>) -> std::io::Result<()> {
                let name = interface.unwrap_or_default();
                // SAFETY: the option takes the name's bytes as they are.
                unsafe {
                    opt::set_raw(
                        self.raw(),
                        opt::SOL_SOCKET,
                        libc::SO_BINDTODEVICE,
                        name.as_ptr(),
                        name.len(),
                    )
                }
            }

            /// The network interface the socket is bound to, if any
            /// (`SO_BINDTODEVICE`).
            #[cfg(any(target_os = "linux", target_os = "android"))]
            pub fn device(&self) -> std::io::Result<Option<super::DeviceName>> {
                let mut name = super::DeviceName {
                    bytes: [0; libc::IFNAMSIZ],
                    len: 0,
                };
                // SAFETY: the kernel writes at most the buffer's length.
                let len = unsafe {
                    opt::get_raw(
                        self.raw(),
                        opt::SOL_SOCKET,
                        libc::SO_BINDTODEVICE,
                        name.bytes.as_mut_ptr(),
                        name.bytes.len(),
                    )?
                };
                // The kernel counts the name's terminating zero.
                let len = name.bytes[..len.min(name.bytes.len())]
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(len.min(name.bytes.len()));
                name.len = len as u8;
                Ok((len > 0).then_some(name))
            }
        }
    )*};
}

pub(super) use socket_options;
