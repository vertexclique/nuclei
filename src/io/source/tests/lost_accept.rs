//! An accept that met a connection its peer reset or aborted in the backlog: its error is told
//! from the listener's own, and on a completion driver the side keeps nothing of it for its
//! tasks, which ask again, while an error of the listener's stays for them (proofs/tla/accept).

use std::io;

use crate::rt::counters::lost_before_accept;

/// The errors an accept meets for a connection reset or aborted in the backlog, as each system
/// reports it.
fn lost() -> Vec<io::Error> {
    #[cfg(unix)]
    let lost = vec![
        io::Error::from_raw_os_error(libc::ECONNABORTED),
        // A multishot accept's connection whose peer went before it was taken (`peer_of`).
        io::ErrorKind::ConnectionAborted.into(),
    ];
    // The network errors Linux's accept(2) says to retry on, as it would for `EAGAIN`.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let lost = {
        let mut lost = lost;
        lost.extend(
            [
                libc::EPROTO,
                libc::ENETDOWN,
                libc::ENOPROTOOPT,
                libc::EHOSTDOWN,
                libc::ENONET,
                libc::EHOSTUNREACH,
                libc::EOPNOTSUPP,
                libc::ENETUNREACH,
            ]
            .map(io::Error::from_raw_os_error),
        );
        lost
    };
    #[cfg(windows)]
    let lost = {
        use crate::syscore::windows::{ERROR_NETNAME_DELETED, WSAECONNABORTED, WSAECONNRESET};
        [WSAECONNRESET, WSAECONNABORTED, ERROR_NETNAME_DELETED]
            .map(io::Error::from_raw_os_error)
            .into()
    };
    lost
}

/// Errors of the listener itself, which end an accept.
fn the_listeners() -> Vec<io::Error> {
    #[cfg(unix)]
    let codes = [
        libc::EMFILE,
        libc::ENFILE,
        libc::ENOBUFS,
        libc::ENOMEM,
        libc::EINVAL,
        libc::EBADF,
        libc::ENOTSOCK,
    ];
    #[cfg(windows)]
    let codes = {
        use crate::syscore::windows::{WSAEINVAL, WSAEMFILE, WSAENOBUFS, WSAENOTSOCK};
        [WSAEMFILE, WSAENOBUFS, WSAEINVAL, WSAENOTSOCK]
    };
    codes.map(io::Error::from_raw_os_error).into()
}

/// The errors of a socket the system cannot make for an accept, which pause `accept_multi`'s
/// stream until one can be made, and errors of the connection or of the listener, which do not.
#[test]
fn a_socket_the_system_cannot_make_is_out_of_descriptors() {
    use crate::rt::counters::out_of_descriptors;
    #[cfg(unix)]
    let (out, not) = (
        [libc::EMFILE, libc::ENFILE].as_slice(),
        [libc::ECONNABORTED, libc::EINVAL, libc::EBADF].as_slice(),
    );
    #[cfg(windows)]
    let (out, not) = {
        use crate::syscore::windows::{
            ERROR_NO_SYSTEM_RESOURCES, WSAECONNRESET, WSAEINVAL, WSAEMFILE, WSAENOBUFS, WSAENOTSOCK,
        };
        (
            [WSAEMFILE, WSAENOBUFS, ERROR_NO_SYSTEM_RESOURCES].as_slice(),
            [WSAECONNRESET, WSAEINVAL, WSAENOTSOCK].as_slice(),
        )
    };
    for &code in out {
        assert!(
            out_of_descriptors(&io::Error::from_raw_os_error(code)),
            "{code}"
        );
    }
    for &code in not {
        assert!(
            !out_of_descriptors(&io::Error::from_raw_os_error(code)),
            "{code}"
        );
    }
}

#[test]
fn a_reset_connections_error_is_told_from_the_listeners() {
    for err in lost() {
        assert!(lost_before_accept(&err), "{err:?} is the connection's");
    }
    for err in the_listeners() {
        assert!(!lost_before_accept(&err), "{err:?} is the listener's");
    }
}

#[cfg(completion)]
mod ended {
    use std::io;

    use super::{lost, the_listeners};
    use crate::io::side::After;
    use crate::io::source::ops::{Answer, Done, Outcome};
    use crate::io::source::read::ReadState;

    /// Hands each of `lost` and then each of `the_listeners` to a fresh side as the end of its
    /// accept, made by `outcome`: the first leave the side nothing to give its tasks, the second
    /// leave the error for them.
    fn check(outcome: fn(io::Error) -> Outcome) {
        let mut after = After::default();
        let skipped = || crate::stats().off_shard_errors.accepts_skipped;
        // Off a shard, so the skips are counted among the off-shard errors, which only grow:
        // other tests may add to them, none takes away.
        let before = skipped();
        for err in lost() {
            let mut st = ReadState::default();
            let shown = format!("{err:?}");
            st.op_done(
                Done {
                    outcome: outcome(err),
                    epoch: 0,
                    read: true,
                },
                None,
                &mut after,
            );
            assert!(!st.has_result(), "{shown}: kept for the tasks");
        }
        assert!(
            skipped() >= before + lost().len() as u64,
            "every skipped connection is counted"
        );
        for err in the_listeners() {
            let mut st = ReadState::default();
            let shown = format!("{err:?}");
            st.op_done(
                Done {
                    outcome: outcome(err),
                    epoch: 0,
                    read: true,
                },
                None,
                &mut after,
            );
            assert!(st.has_result(), "{shown}: not kept for the tasks");
        }
    }

    #[test]
    fn an_accept_that_met_a_reset_connection_keeps_nothing_and_a_listener_error_stays() {
        check(|err| Outcome::Accept(Answer::Done(Err(err))));
    }

    #[cfg(ring)]
    #[test]
    fn a_multishot_accept_that_met_a_reset_connection_keeps_nothing_and_a_listener_error_stays() {
        check(|err| Outcome::AcceptMultiEnd(Answer::Done(Err(err))));
    }
}
