//! The addresses an operation takes: numbers, used at once, or names,
//! looked up on the blocking pool, so no shard ever waits on a resolver.
//!
//! Numbers make no allocation: one address, or a slice of them, is walked
//! where it is. A name is copied once, to cross to the pool's thread, and
//! the resolver's own list of results is handed back as it is.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::{iter, option, slice, vec};

/// What an operation that takes an address accepts: a `SocketAddr`, an
/// `(IpAddr, u16)` pair, a slice or array of addresses, or a string,
/// `"host:port"` or `("host", port)`. Numbers are used as they are; a name
/// is looked up on the blocking pool (`getaddrinfo`), never on the calling
/// thread.
///
/// The trait is sealed: its implementations are the ones listed here.
pub trait ToSocketAddrs: sealed::Sealed {}

pub(crate) mod sealed {
    /// Each implementation's addresses, or the name to look up.
    pub trait Sealed {
        fn resolve(&self) -> super::Resolve<'_>;
    }
}

/// What an address names before any lookup, borrowed from it.
pub enum Resolve<'a> {
    One(SocketAddr),
    Slice(&'a [SocketAddr]),
    Name(&'a str),
    HostPort(&'a str, u16),
}

/// The addresses to try, in order.
pub(crate) enum Addrs<'a> {
    One(option::IntoIter<SocketAddr>),
    Slice(iter::Copied<slice::Iter<'a, SocketAddr>>),
    /// The resolver's own list.
    Looked(vec::IntoIter<SocketAddr>),
}

impl Iterator for Addrs<'_> {
    type Item = SocketAddr;

    fn next(&mut self) -> Option<SocketAddr> {
        match self {
            Self::One(one) => one.next(),
            Self::Slice(slice) => slice.next(),
            Self::Looked(looked) => looked.next(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            Self::One(one) => one.size_hint(),
            Self::Slice(slice) => slice.size_hint(),
            Self::Looked(looked) => looked.size_hint(),
        }
    }
}

impl<'a> Resolve<'a> {
    /// The addresses, looked up on the blocking pool if they are a name.
    async fn addrs(self) -> io::Result<Addrs<'a>> {
        use std::net::ToSocketAddrs as _;
        let looked = match self {
            Self::One(addr) => return Ok(Addrs::One(Some(addr).into_iter())),
            Self::Slice(addrs) => return Ok(Addrs::Slice(addrs.iter().copied())),
            // Owned to cross threads: the pool's job may outlive a caller
            // that was cancelled.
            Self::Name(name) => {
                let name = name.to_owned();
                crate::rt::blocking::call(move || name.as_str().to_socket_addrs()).await?
            }
            Self::HostPort(host, port) => {
                let host = host.to_owned();
                crate::rt::blocking::call(move || (host.as_str(), port).to_socket_addrs()).await?
            }
        };
        Ok(Addrs::Looked(looked))
    }
}

macro_rules! at_once {
    ($($ty:ty, $this:ident => $addr:expr;)*) => {$(
        impl ToSocketAddrs for $ty {}
        impl sealed::Sealed for $ty {
            fn resolve(&self) -> Resolve<'_> {
                let $this = self;
                Resolve::One($addr)
            }
        }
    )*};
}

at_once! {
    SocketAddr, a => *a;
    SocketAddrV4, a => SocketAddr::V4(*a);
    SocketAddrV6, a => SocketAddr::V6(*a);
    (IpAddr, u16), a => SocketAddr::new(a.0, a.1);
    (Ipv4Addr, u16), a => SocketAddr::new(a.0.into(), a.1);
    (Ipv6Addr, u16), a => SocketAddr::new(a.0.into(), a.1);
}

impl ToSocketAddrs for [SocketAddr] {}
impl sealed::Sealed for [SocketAddr] {
    fn resolve(&self) -> Resolve<'_> {
        Resolve::Slice(self)
    }
}

impl<const N: usize> ToSocketAddrs for [SocketAddr; N] {}
impl<const N: usize> sealed::Sealed for [SocketAddr; N] {
    fn resolve(&self) -> Resolve<'_> {
        Resolve::Slice(self)
    }
}

impl ToSocketAddrs for str {}
impl sealed::Sealed for str {
    fn resolve(&self) -> Resolve<'_> {
        match self.parse() {
            Ok(addr) => Resolve::One(addr),
            Err(_) => Resolve::Name(self),
        }
    }
}

impl ToSocketAddrs for String {}
impl sealed::Sealed for String {
    fn resolve(&self) -> Resolve<'_> {
        self.as_str().resolve()
    }
}

/// A host, by number or by name, and a port.
fn host_port(host: &str, port: u16) -> Resolve<'_> {
    match host.parse::<IpAddr>() {
        Ok(ip) => Resolve::One(SocketAddr::new(ip, port)),
        Err(_) => Resolve::HostPort(host, port),
    }
}

impl ToSocketAddrs for (&str, u16) {}
impl sealed::Sealed for (&str, u16) {
    fn resolve(&self) -> Resolve<'_> {
        host_port(self.0, self.1)
    }
}

impl ToSocketAddrs for (String, u16) {}
impl sealed::Sealed for (String, u16) {
    fn resolve(&self) -> Resolve<'_> {
        host_port(&self.0, self.1)
    }
}

impl<T: ToSocketAddrs + ?Sized> ToSocketAddrs for &T {}
impl<T: ToSocketAddrs + ?Sized> sealed::Sealed for &T {
    fn resolve(&self) -> Resolve<'_> {
        (**self).resolve()
    }
}

/// Every address `host` names: numbers at once, a name looked up on the
/// blocking pool. The iterator owns what it walks, so a slice given here is
/// copied into it; the operations that take an address walk it in place.
///
/// ```
/// # nuclei::block_on(async {
/// let addrs: Vec<_> = nuclei::net::lookup_host("127.0.0.1:80").await?.collect();
/// assert_eq!(addrs, ["127.0.0.1:80".parse().unwrap()]);
/// # std::io::Result::Ok(())
/// # }).unwrap();
/// ```
///
/// # Errors
///
/// When the name cannot be looked up.
pub async fn lookup_host<T: ToSocketAddrs>(
    host: T,
) -> io::Result<impl Iterator<Item = SocketAddr> + Send + 'static> {
    Ok(match resolve(&host).await? {
        Addrs::One(one) => Addrs::One(one),
        Addrs::Slice(slice) => Addrs::Looked(slice.collect::<Vec<_>>().into_iter()),
        Addrs::Looked(looked) => Addrs::Looked(looked),
    })
}

/// The addresses `addr` names, walked in place for numbers.
pub(crate) async fn resolve<A: ToSocketAddrs + ?Sized>(addr: &A) -> io::Result<Addrs<'_>> {
    sealed::Sealed::resolve(addr).addrs().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn here<T: ToSocketAddrs + ?Sized>(addr: &T) -> Vec<SocketAddr> {
        crate::block_on(resolve(addr)).unwrap().collect()
    }

    #[test]
    fn numbers_are_used_as_they_are() {
        let a: SocketAddr = "127.0.0.1:80".parse().unwrap();
        let b: SocketAddr = "[::1]:443".parse().unwrap();
        assert_eq!(here(&a), [a]);
        assert_eq!(here("127.0.0.1:80"), [a]);
        assert_eq!(here(&("127.0.0.1", 80)), [a]);
        assert_eq!(here(&("::1", 443)), [b]);
        assert_eq!(here(&(IpAddr::from([127, 0, 0, 1]), 80)), [a]);
        assert_eq!(here(&[a, b]), [a, b]);
        assert_eq!(here(&[a, b][..]), [a, b]);
        assert!(matches!(
            sealed::Sealed::resolve("127.0.0.1:80"),
            Resolve::One(_)
        ));
        assert!(matches!(
            sealed::Sealed::resolve("localhost:80"),
            Resolve::Name(_)
        ));
    }

    #[test]
    fn a_name_is_looked_up_on_the_pool() {
        let addrs = here("localhost:80");
        assert!(!addrs.is_empty());
        assert!(addrs.iter().all(|a| a.ip().is_loopback() && a.port() == 80));
        assert!(crate::block_on(resolve("no.such.host.invalid:80")).is_err());
    }
}
