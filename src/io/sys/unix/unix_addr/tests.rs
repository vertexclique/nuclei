//! A socket path's address: a NUL anywhere in the path is refused, and the
//! path's bytes go into `sun_path` whole, followed by the zero the kernel
//! counts.

use proptest::prelude::*;

use super::*;

/// Paths of every length up to past a few words, with a NUL at each place,
/// or none.
fn path_bytes() -> impl Strategy<Value = Vec<u8>> {
    (0usize..40, proptest::option::of(0usize..40), 1u8..=255).prop_map(|(len, nul, fill)| {
        let mut bytes = vec![fill; len];
        if let Some(at) = nul.filter(|&at| at < len) {
            bytes[at] = 0;
        }
        bytes
    })
}

proptest! {
    #[test]
    fn a_nul_is_found_where_a_byte_scan_finds_it(bytes in path_bytes()) {
        prop_assert_eq!(has_nul(&bytes), bytes.contains(&0));
    }

    #[test]
    fn a_path_goes_into_the_address_whole(bytes in path_bytes()) {
        let path = Path::new(std::ffi::OsStr::from_bytes(&bytes));
        match UnixAddr::path(path) {
            Ok(addr) => {
                prop_assert!(!bytes.contains(&0));
                let base = core::mem::offset_of!(libc::sockaddr_un, sun_path);
                prop_assert_eq!(addr.len as usize, base + bytes.len() + 1);
                let held: Vec<u8> = addr.raw.sun_path[..=bytes.len()].iter().map(|&c| c as u8).collect();
                prop_assert_eq!(&held[..bytes.len()], &bytes[..]);
                prop_assert_eq!(held[bytes.len()], 0);
            }
            Err(err) => {
                prop_assert!(bytes.contains(&0));
                prop_assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
            }
        }
    }
}

/// An empty path, whose bytes start at no allocation, holds no NUL.
#[test]
fn an_empty_path_holds_no_nul() {
    assert!(!has_nul(&Vec::new()));
    assert!(UnixAddr::path(&std::path::PathBuf::new()).is_ok());
}

#[test]
fn a_nul_at_each_place_is_found() {
    for len in 8..=24 {
        for at in 0..len {
            let mut bytes = vec![0x80u8; len];
            bytes[at] = 0;
            assert!(has_nul(&bytes), "a NUL at {at} of {len}");
        }
        assert!(!has_nul(&vec![0x80u8; len]));
        assert!(!has_nul(&vec![0x01u8; len]));
        assert!(!has_nul(&vec![0xffu8; len]));
    }
}
