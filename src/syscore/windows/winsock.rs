//! Winsock set up once per process, and the extension functions the driver
//! needs (`AcceptEx`, `ConnectEx`, `GetAcceptExSockaddrs`), which the TCP/IP
//! provider hands out through `WSAIoctl` rather than any library exporting
//! them. Both address families share that provider, so one lookup serves
//! every socket.

use core::ffi::c_void;
use std::io;
use std::sync::OnceLock;

use super::{
    AF_INET, BOOL, DWORD, GUID, IPPROTO_TCP, OVERLAPPED, SIO_GET_EXTENSION_FUNCTION_POINTER,
    SOCK_STREAM, SOCKADDR, SOCKET, SOCKET_ERROR, WSA_FLAG_NO_HANDLE_INHERIT, WSA_FLAG_OVERLAPPED,
    WSADATA, WSAIoctl, WSASocketW, WSAStartup, closesocket, wsa_error,
};

pub(crate) type AcceptExFn = unsafe extern "system" fn(
    sListenSocket: SOCKET,
    sAcceptSocket: SOCKET,
    lpOutputBuffer: *mut c_void,
    dwReceiveDataLength: DWORD,
    dwLocalAddressLength: DWORD,
    dwRemoteAddressLength: DWORD,
    lpdwBytesReceived: *mut DWORD,
    lpOverlapped: *mut OVERLAPPED,
) -> BOOL;

pub(crate) type ConnectExFn = unsafe extern "system" fn(
    s: SOCKET,
    name: *const SOCKADDR,
    namelen: i32,
    lpSendBuffer: *const c_void,
    dwSendDataLength: DWORD,
    lpdwBytesSent: *mut DWORD,
    lpOverlapped: *mut OVERLAPPED,
) -> BOOL;

pub(crate) type TransmitFileFn = unsafe extern "system" fn(
    hSocket: SOCKET,
    hFile: super::HANDLE,
    nNumberOfBytesToWrite: DWORD,
    nNumberOfBytesPerSend: DWORD,
    lpOverlapped: *mut OVERLAPPED,
    lpTransmitBuffers: *const c_void,
    dwReserved: DWORD,
) -> BOOL;

pub(crate) type GetAcceptExSockaddrsFn = unsafe extern "system" fn(
    lpOutputBuffer: *const c_void,
    dwReceiveDataLength: DWORD,
    dwLocalAddressLength: DWORD,
    dwRemoteAddressLength: DWORD,
    LocalSockaddr: *mut *mut SOCKADDR,
    LocalSockaddrLength: *mut i32,
    RemoteSockaddr: *mut *mut SOCKADDR,
    RemoteSockaddrLength: *mut i32,
);

const WSAID_ACCEPTEX: GUID = GUID {
    data1: 0xb536_7df1,
    data2: 0xcbac,
    data3: 0x11cf,
    data4: [0x95, 0xca, 0x00, 0x80, 0x5f, 0x48, 0xa1, 0x92],
};
const WSAID_GETACCEPTEXSOCKADDRS: GUID = GUID {
    data1: 0xb536_7df2,
    data2: 0xcbac,
    data3: 0x11cf,
    data4: [0x95, 0xca, 0x00, 0x80, 0x5f, 0x48, 0xa1, 0x92],
};
const WSAID_TRANSMITFILE: GUID = GUID {
    data1: 0xb536_7df0,
    data2: 0xcbac,
    data3: 0x11cf,
    data4: [0x95, 0xca, 0x00, 0x80, 0x5f, 0x48, 0xa1, 0x92],
};
const WSAID_CONNECTEX: GUID = GUID {
    data1: 0x25a2_07b9,
    data2: 0xddf3,
    data3: 0x4660,
    data4: [0x8e, 0xe9, 0x76, 0xe5, 0x8c, 0x74, 0x06, 0x3e],
};

/// The extension functions.
#[derive(Clone, Copy)]
pub(crate) struct Ext {
    pub(crate) accept_ex: AcceptExFn,
    pub(crate) get_accept_ex_sockaddrs: GetAcceptExSockaddrsFn,
    pub(crate) connect_ex: ConnectExFn,
    /// `TransmitFile`, which a provider may lack: a file is then sent by reading and writing.
    pub(crate) transmit_file: Option<TransmitFileFn>,
}

/// Winsock started, and the extension functions, looked up once.
pub(crate) fn ext() -> io::Result<Ext> {
    static EXT: OnceLock<Result<Ext, i32>> = OnceLock::new();
    EXT.get_or_init(|| look_up().map_err(|e| e.raw_os_error().unwrap_or(0)))
        .map_err(io::Error::from_raw_os_error)
}

/// Starts Winsock 2.2 for the process (each call counts; it is never
/// stopped, as the process may use sockets until it exits).
pub(crate) fn start() -> io::Result<()> {
    static STARTED: OnceLock<i32> = OnceLock::new();
    let code = *STARTED.get_or_init(|| {
        let mut data = WSADATA::zeroed();
        // SAFETY: `data` is room for what the call writes.
        unsafe { WSAStartup(0x0202, &mut data) }
    });
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code))
    }
}

fn look_up() -> io::Result<Ext> {
    start()?;
    // SAFETY: plain arguments; the socket is closed below.
    let probe = unsafe {
        WSASocketW(
            i32::from(AF_INET),
            SOCK_STREAM,
            IPPROTO_TCP,
            core::ptr::null_mut(),
            0,
            WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT,
        )
    };
    if probe == super::INVALID_SOCKET {
        return Err(wsa_error());
    }
    let found = (|| {
        // SAFETY: each GUID names the function whose type it is cast to.
        unsafe {
            Ok(Ext {
                accept_ex: core::mem::transmute::<*const c_void, AcceptExFn>(pointer(
                    probe,
                    &WSAID_ACCEPTEX,
                )?),
                get_accept_ex_sockaddrs: core::mem::transmute::<
                    *const c_void,
                    GetAcceptExSockaddrsFn,
                >(pointer(
                    probe,
                    &WSAID_GETACCEPTEXSOCKADDRS,
                )?),
                connect_ex: core::mem::transmute::<*const c_void, ConnectExFn>(pointer(
                    probe,
                    &WSAID_CONNECTEX,
                )?),
                transmit_file: pointer(probe, &WSAID_TRANSMITFILE)
                    .ok()
                    .map(|f| core::mem::transmute::<*const c_void, TransmitFileFn>(f)),
            })
        }
    })();
    // SAFETY: the probe socket is ours and used no more.
    unsafe { closesocket(probe) };
    found
}

/// The extension function `guid` names, from the provider of `socket`.
fn pointer(socket: SOCKET, guid: &GUID) -> io::Result<*const c_void> {
    let mut out: *const c_void = core::ptr::null();
    let mut bytes: DWORD = 0;
    // SAFETY: the in and out buffers are a GUID and a function pointer, as
    // the control code takes them.
    let r = unsafe {
        WSAIoctl(
            socket,
            SIO_GET_EXTENSION_FUNCTION_POINTER,
            core::ptr::from_ref(guid).cast(),
            size_of::<GUID>() as DWORD,
            core::ptr::from_mut(&mut out).cast(),
            size_of::<*const c_void>() as DWORD,
            &mut bytes,
            core::ptr::null_mut(),
            core::ptr::null(),
        )
    };
    if r == SOCKET_ERROR || out.is_null() {
        return Err(wsa_error());
    }
    Ok(out)
}
