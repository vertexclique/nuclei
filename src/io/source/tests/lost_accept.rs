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
