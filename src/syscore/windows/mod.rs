//! Windows: the hand-declared kernel32, ws2_32 and ntdll interface the IOCP
//! driver uses, with the layout of every structure the kernel reads or
//! writes checked at compile time.
//!
//! Only what the driver calls is declared. The Winsock extension functions
//! (`AcceptEx`, `ConnectEx`, `GetAcceptExSockaddrs`) are not exported by any
//! library: [`winsock`] asks the provider for them once.

#![allow(non_camel_case_types, non_snake_case, clippy::upper_case_acronyms)]

pub(crate) mod winsock;

use core::ffi::c_void;

pub(crate) type HANDLE = *mut c_void;
pub(crate) type SOCKET = usize;
pub(crate) type BOOL = i32;
pub(crate) type BOOLEAN = u8;
pub(crate) type DWORD = u32;
pub(crate) type ULONG = u32;
pub(crate) type ULONG_PTR = usize;
pub(crate) type NTSTATUS = i32;

pub(crate) const INVALID_HANDLE_VALUE: HANDLE = usize::MAX as HANDLE;
pub(crate) const INVALID_SOCKET: SOCKET = !0;
pub(crate) const SOCKET_ERROR: i32 = -1;
pub(crate) const INFINITE: DWORD = 0xFFFF_FFFF;

/// `OVERLAPPED`: the kernel writes the status into `Internal` and the byte
/// count into `InternalHigh` while the operation is out.
#[repr(C)]
pub(crate) struct OVERLAPPED {
    pub(crate) Internal: ULONG_PTR,
    pub(crate) InternalHigh: ULONG_PTR,
    pub(crate) Offset: DWORD,
    pub(crate) OffsetHigh: DWORD,
    pub(crate) hEvent: HANDLE,
}

impl OVERLAPPED {
    pub(crate) const fn zeroed() -> Self {
        Self {
            Internal: 0,
            InternalHigh: 0,
            Offset: 0,
            OffsetHigh: 0,
            hEvent: core::ptr::null_mut(),
        }
    }
}

/// One dequeued completion packet.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct OVERLAPPED_ENTRY {
    pub(crate) lpCompletionKey: ULONG_PTR,
    pub(crate) lpOverlapped: *mut OVERLAPPED,
    /// The operation's NTSTATUS.
    pub(crate) Internal: ULONG_PTR,
    pub(crate) dwNumberOfBytesTransferred: DWORD,
}

impl OVERLAPPED_ENTRY {
    pub(crate) const fn zeroed() -> Self {
        Self {
            lpCompletionKey: 0,
            lpOverlapped: core::ptr::null_mut(),
            Internal: 0,
            dwNumberOfBytesTransferred: 0,
        }
    }
}

/// One buffer of a Winsock send or receive.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct WSABUF {
    pub(crate) len: ULONG,
    pub(crate) buf: *mut u8,
}

/// What `WSAStartup` fills in; never read here, so kept opaque (408 bytes is
/// its size on 64-bit, more than on 32-bit).
#[repr(C, align(8))]
pub(crate) struct WSADATA([u8; 408]);

impl WSADATA {
    pub(crate) const fn zeroed() -> Self {
        Self([0; 408])
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct GUID {
    pub(crate) data1: u32,
    pub(crate) data2: u16,
    pub(crate) data3: u16,
    pub(crate) data4: [u8; 8],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct SOCKADDR {
    pub(crate) sa_family: u16,
    pub(crate) sa_data: [u8; 14],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct IN_ADDR {
    pub(crate) s_addr: [u8; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct SOCKADDR_IN {
    pub(crate) sin_family: u16,
    /// Network byte order.
    pub(crate) sin_port: u16,
    pub(crate) sin_addr: IN_ADDR,
    pub(crate) sin_zero: [u8; 8],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct SOCKADDR_IN6 {
    pub(crate) sin6_family: u16,
    /// Network byte order.
    pub(crate) sin6_port: u16,
    pub(crate) sin6_flowinfo: u32,
    pub(crate) sin6_addr: [u8; 16],
    pub(crate) sin6_scope_id: u32,
}

/// Room for any socket address.
#[repr(C, align(8))]
#[derive(Clone, Copy)]
pub(crate) struct SOCKADDR_STORAGE {
    pub(crate) ss_family: u16,
    pub(crate) pad: [u8; 126],
}

impl SOCKADDR_STORAGE {
    pub(crate) const fn zeroed() -> Self {
        Self {
            ss_family: 0,
            pad: [0; 126],
        }
    }
}

#[repr(C)]
pub(crate) struct IO_STATUS_BLOCK {
    pub(crate) Status: ULONG_PTR,
    pub(crate) Information: ULONG_PTR,
}

/// `FILE_COMPLETION_INFORMATION`: the port and key a handle's completions go to.
#[repr(C)]
pub(crate) struct FILE_COMPLETION_INFORMATION {
    pub(crate) Port: HANDLE,
    pub(crate) Key: *mut c_void,
}

const _: () = {
    use core::mem::{align_of, size_of};
    let p = size_of::<usize>();
    assert!(size_of::<OVERLAPPED>() == 3 * p + 8);
    assert!(size_of::<OVERLAPPED_ENTRY>() == if p == 8 { 32 } else { 16 });
    assert!(size_of::<WSABUF>() == if p == 8 { 16 } else { 8 });
    assert!(size_of::<GUID>() == 16);
    assert!(size_of::<SOCKADDR>() == 16);
    assert!(size_of::<SOCKADDR_IN>() == 16);
    assert!(size_of::<SOCKADDR_IN6>() == 28);
    assert!(size_of::<SOCKADDR_STORAGE>() == 128);
    assert!(align_of::<SOCKADDR_STORAGE>() == 8);
    assert!(size_of::<IO_STATUS_BLOCK>() == 2 * p);
    assert!(size_of::<FILE_COMPLETION_INFORMATION>() == 2 * p);
};

pub(crate) const AF_INET: u16 = 2;
pub(crate) const AF_INET6: u16 = 23;
pub(crate) const SOCK_STREAM: i32 = 1;
pub(crate) const SOCK_DGRAM: i32 = 2;
pub(crate) const SOCK_RAW: i32 = 3;
pub(crate) const SOCK_RDM: i32 = 4;
pub(crate) const IPPROTO_TCP: i32 = 6;
pub(crate) const SOL_SOCKET: i32 = 0xFFFF;
pub(crate) const SO_TYPE: i32 = 0x1008;
pub(crate) const SO_UPDATE_ACCEPT_CONTEXT: i32 = 0x700B;
pub(crate) const SO_UPDATE_CONNECT_CONTEXT: i32 = 0x7010;
pub(crate) const SO_ACCEPTCONN: i32 = 0x0002;
pub(crate) const SO_REUSEADDR: i32 = 0x0004;
pub(crate) const SO_KEEPALIVE: i32 = 0x0008;
pub(crate) const SO_LINGER: i32 = 0x0080;
pub(crate) const SO_SNDBUF: i32 = 0x1001;
pub(crate) const SO_RCVBUF: i32 = 0x1002;
pub(crate) const IPPROTO_IP: i32 = 0;
pub(crate) const IP_TOS: i32 = 3;

/// `LINGER`: whether a close waits for unsent data, and how long.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct LINGER {
    pub(crate) l_onoff: u16,
    pub(crate) l_linger: u16,
}
pub(crate) const SD_SEND: i32 = 1;
pub(crate) const WSA_FLAG_OVERLAPPED: DWORD = 0x01;
pub(crate) const WSA_FLAG_NO_HANDLE_INHERIT: DWORD = 0x80;
pub(crate) const SIO_GET_EXTENSION_FUNCTION_POINTER: DWORD = 0xC800_0006;
/// `_WSAIOW(IOC_VENDOR, 12)`: whether an ICMP port-unreachable fails the
/// next receive of a UDP socket.
pub(crate) const SIO_UDP_CONNRESET: DWORD = 0x9800_000C;
#[cfg(test)]
pub(crate) const MSG_OOB: i32 = 0x1;
pub(crate) const FILE_BEGIN: DWORD = 0;
pub(crate) const FILE_CURRENT: DWORD = 1;

pub(crate) const FILE_SKIP_SET_EVENT_ON_HANDLE: u8 = 0x2;

pub(crate) const CREATE_WAITABLE_TIMER_HIGH_RESOLUTION: DWORD = 0x2;
pub(crate) const TIMER_ALL_ACCESS: DWORD = 0x1F_0003;
/// `FileReplaceCompletionInformation`, the class of `NtSetInformationFile`
/// that moves a handle's completions to another port.
pub(crate) const FILE_REPLACE_COMPLETION_INFORMATION: u32 = 61;
pub(crate) const FILE_MODE_INFORMATION: u32 = 16;
/// `FileAccessInformation`: the access a handle was granted.
pub(crate) const FILE_ACCESS_INFORMATION: u32 = 8;
pub(crate) const FILE_WRITE_DATA: ULONG = 0x2;
pub(crate) const FILE_APPEND_DATA: ULONG = 0x4;
pub(crate) const FILE_SYNCHRONOUS_IO_ALERT: ULONG = 0x10;
pub(crate) const FILE_SYNCHRONOUS_IO_NONALERT: ULONG = 0x20;
pub(crate) const FILE_TYPE_DISK: DWORD = 1;
pub(crate) const DUPLICATE_SAME_ACCESS: DWORD = 2;
#[cfg(test)]
pub(crate) const PIPE_ACCESS_DUPLEX: DWORD = 0x3;
#[cfg(test)]
pub(crate) const FILE_FLAG_OVERLAPPED: DWORD = 0x4000_0000;
pub(crate) const ERROR_MORE_DATA: i32 = 234;

pub(crate) const ERROR_IO_PENDING: i32 = 997;
pub(crate) const ERROR_HANDLE_EOF: i32 = 38;
pub(crate) const ERROR_BROKEN_PIPE: i32 = 109;
pub(crate) const WAIT_TIMEOUT: i32 = 258;
/// `WaitForSingleObject`: the object is signalled (a process: it ended).
pub(crate) const WAIT_OBJECT_0: DWORD = 0;
pub(crate) const WSAEINVAL: i32 = 10022;
pub(crate) const WSAEMFILE: i32 = 10024;
pub(crate) const WSAEADDRINUSE: i32 = 10048;
pub(crate) const WSAENETUNREACH: i32 = 10051;
pub(crate) const WSAECONNABORTED: i32 = 10053;
pub(crate) const WSAECONNRESET: i32 = 10054;
pub(crate) const WSAENOBUFS: i32 = 10055;
pub(crate) const WSAENOTCONN: i32 = 10057;
pub(crate) const WSAETIMEDOUT: i32 = 10060;
pub(crate) const WSAECONNREFUSED: i32 = 10061;
pub(crate) const WSAEHOSTUNREACH: i32 = 10065;
pub(crate) const WSAENOTSOCK: i32 = 10038;
pub(crate) const WSA_OPERATION_ABORTED: i32 = 995;

pub(crate) const STATUS_SUCCESS: NTSTATUS = 0;
pub(crate) const STATUS_BUFFER_OVERFLOW: NTSTATUS = 0x8000_0005u32 as i32;
pub(crate) const STATUS_CANCELLED: NTSTATUS = 0xC000_0120u32 as i32;
pub(crate) const STATUS_INVALID_HANDLE: NTSTATUS = 0xC000_0008u32 as i32;
pub(crate) const STATUS_OBJECT_TYPE_MISMATCH: NTSTATUS = 0xC000_0024u32 as i32;
pub(crate) const STATUS_IO_TIMEOUT: NTSTATUS = 0xC000_00B5u32 as i32;
pub(crate) const STATUS_LOCAL_DISCONNECT: NTSTATUS = 0xC000_013Bu32 as i32;
pub(crate) const STATUS_REMOTE_DISCONNECT: NTSTATUS = 0xC000_013Cu32 as i32;
pub(crate) const STATUS_CONNECTION_RESET: NTSTATUS = 0xC000_020Du32 as i32;
pub(crate) const STATUS_ADDRESS_ALREADY_EXISTS: NTSTATUS = 0xC000_020Au32 as i32;
pub(crate) const STATUS_CONNECTION_REFUSED: NTSTATUS = 0xC000_0236u32 as i32;
pub(crate) const STATUS_GRACEFUL_DISCONNECT: NTSTATUS = 0xC000_0237u32 as i32;
pub(crate) const STATUS_NETWORK_UNREACHABLE: NTSTATUS = 0xC000_023Cu32 as i32;
pub(crate) const STATUS_HOST_UNREACHABLE: NTSTATUS = 0xC000_023Du32 as i32;
pub(crate) const STATUS_PORT_UNREACHABLE: NTSTATUS = 0xC000_023Fu32 as i32;
pub(crate) const STATUS_CONNECTION_ABORTED: NTSTATUS = 0xC000_0241u32 as i32;
pub(crate) const STATUS_INVALID_DEVICE_STATE: NTSTATUS = 0xC000_0184u32 as i32;

pub(crate) type LPWSAOVERLAPPED_COMPLETION_ROUTINE = *const c_void;
pub(crate) type PTIMERAPCROUTINE = *const c_void;

#[link(name = "kernel32")]
unsafe extern "system" {
    pub(crate) fn FlushProcessWriteBuffers();
    pub(crate) fn GetCurrentThreadId() -> DWORD;
    pub(crate) fn OpenThread(
        dwDesiredAccess: DWORD,
        bInheritHandle: BOOL,
        dwThreadId: DWORD,
    ) -> HANDLE;
    pub(crate) fn SetConsoleCtrlHandler(
        HandlerRoutine: Option<unsafe extern "system" fn(u32) -> BOOL>,
        Add: BOOL,
    ) -> BOOL;
    pub(crate) fn GetExitCodeProcess(hProcess: HANDLE, lpExitCode: *mut DWORD) -> BOOL;
    pub(crate) fn WaitForSingleObject(hHandle: HANDLE, dwMilliseconds: DWORD) -> DWORD;
    pub(crate) fn TerminateProcess(hProcess: HANDLE, uExitCode: u32) -> BOOL;
    pub(crate) fn CreateIoCompletionPort(
        FileHandle: HANDLE,
        ExistingCompletionPort: HANDLE,
        CompletionKey: ULONG_PTR,
        NumberOfConcurrentThreads: DWORD,
    ) -> HANDLE;
    pub(crate) fn GetQueuedCompletionStatusEx(
        CompletionPort: HANDLE,
        lpCompletionPortEntries: *mut OVERLAPPED_ENTRY,
        ulCount: ULONG,
        ulNumEntriesRemoved: *mut ULONG,
        dwMilliseconds: DWORD,
        fAlertable: BOOL,
    ) -> BOOL;
    pub(crate) fn PostQueuedCompletionStatus(
        CompletionPort: HANDLE,
        dwNumberOfBytesTransferred: DWORD,
        dwCompletionKey: ULONG_PTR,
        lpOverlapped: *mut OVERLAPPED,
    ) -> BOOL;
    pub(crate) fn CancelIoEx(hFile: HANDLE, lpOverlapped: *mut OVERLAPPED) -> BOOL;
    pub(crate) fn SetFileCompletionNotificationModes(FileHandle: HANDLE, Flags: u8) -> BOOL;
    pub(crate) fn CloseHandle(hObject: HANDLE) -> BOOL;
    pub(crate) fn GetLastError() -> DWORD;
    pub(crate) fn GetFileType(hFile: HANDLE) -> DWORD;
    #[cfg(test)]
    pub(crate) fn CreatePipe(
        hReadPipe: *mut HANDLE,
        hWritePipe: *mut HANDLE,
        lpPipeAttributes: *const c_void,
        nSize: DWORD,
    ) -> BOOL;
    #[cfg(test)]
    pub(crate) fn CreateNamedPipeW(
        lpName: *const u16,
        dwOpenMode: DWORD,
        dwPipeMode: DWORD,
        nMaxInstances: DWORD,
        nOutBufferSize: DWORD,
        nInBufferSize: DWORD,
        nDefaultTimeOut: DWORD,
        lpSecurityAttributes: *const c_void,
    ) -> HANDLE;
    pub(crate) fn CancelSynchronousIo(hThread: HANDLE) -> BOOL;
    pub(crate) fn DuplicateHandle(
        hSourceProcessHandle: HANDLE,
        hSourceHandle: HANDLE,
        hTargetProcessHandle: HANDLE,
        lpTargetHandle: *mut HANDLE,
        dwDesiredAccess: DWORD,
        bInheritHandle: BOOL,
        dwOptions: DWORD,
    ) -> BOOL;
    pub(crate) fn CreateWaitableTimerExW(
        lpTimerAttributes: *mut c_void,
        lpTimerName: *const u16,
        dwFlags: DWORD,
        dwDesiredAccess: DWORD,
    ) -> HANDLE;
    pub(crate) fn SetWaitableTimer(
        hTimer: HANDLE,
        lpDueTime: *const i64,
        lPeriod: i32,
        pfnCompletionRoutine: PTIMERAPCROUTINE,
        lpArgToCompletionRoutine: *mut c_void,
        fResume: BOOL,
    ) -> BOOL;
    pub(crate) fn CancelWaitableTimer(hTimer: HANDLE) -> BOOL;
    pub(crate) fn GetCurrentThread() -> HANDLE;
    pub(crate) fn GetCurrentProcess() -> HANDLE;
    pub(crate) fn GetProcessAffinityMask(
        hProcess: HANDLE,
        lpProcessAffinityMask: *mut usize,
        lpSystemAffinityMask: *mut usize,
    ) -> BOOL;
    pub(crate) fn ReadFile(
        hFile: HANDLE,
        lpBuffer: *mut c_void,
        nNumberOfBytesToRead: DWORD,
        lpNumberOfBytesRead: *mut DWORD,
        lpOverlapped: *mut OVERLAPPED,
    ) -> BOOL;
    pub(crate) fn WriteFile(
        hFile: HANDLE,
        lpBuffer: *const c_void,
        nNumberOfBytesToWrite: DWORD,
        lpNumberOfBytesWritten: *mut DWORD,
        lpOverlapped: *mut OVERLAPPED,
    ) -> BOOL;
    pub(crate) fn GetFileSizeEx(hFile: HANDLE, lpFileSize: *mut i64) -> BOOL;
    pub(crate) fn SetFilePointerEx(
        hFile: HANDLE,
        liDistanceToMove: i64,
        lpNewFilePointer: *mut i64,
        dwMoveMethod: DWORD,
    ) -> BOOL;
    pub(crate) fn SetThreadAffinityMask(hThread: HANDLE, dwThreadAffinityMask: usize) -> usize;
}

#[link(name = "ws2_32")]
unsafe extern "system" {
    pub(crate) fn WSAStartup(wVersionRequested: u16, lpWSAData: *mut WSADATA) -> i32;
    pub(crate) fn WSAGetLastError() -> i32;
    pub(crate) fn WSASocketW(
        af: i32,
        r#type: i32,
        protocol: i32,
        lpProtocolInfo: *mut c_void,
        g: u32,
        dwFlags: DWORD,
    ) -> SOCKET;
    pub(crate) fn WSARecv(
        s: SOCKET,
        lpBuffers: *mut WSABUF,
        dwBufferCount: DWORD,
        lpNumberOfBytesRecvd: *mut DWORD,
        lpFlags: *mut DWORD,
        lpOverlapped: *mut OVERLAPPED,
        lpCompletionRoutine: LPWSAOVERLAPPED_COMPLETION_ROUTINE,
    ) -> i32;
    pub(crate) fn WSASend(
        s: SOCKET,
        lpBuffers: *const WSABUF,
        dwBufferCount: DWORD,
        lpNumberOfBytesSent: *mut DWORD,
        dwFlags: DWORD,
        lpOverlapped: *mut OVERLAPPED,
        lpCompletionRoutine: LPWSAOVERLAPPED_COMPLETION_ROUTINE,
    ) -> i32;
    pub(crate) fn WSARecvFrom(
        s: SOCKET,
        lpBuffers: *mut WSABUF,
        dwBufferCount: DWORD,
        lpNumberOfBytesRecvd: *mut DWORD,
        lpFlags: *mut DWORD,
        lpFrom: *mut SOCKADDR,
        lpFromlen: *mut i32,
        lpOverlapped: *mut OVERLAPPED,
        lpCompletionRoutine: LPWSAOVERLAPPED_COMPLETION_ROUTINE,
    ) -> i32;
    pub(crate) fn WSASendTo(
        s: SOCKET,
        lpBuffers: *const WSABUF,
        dwBufferCount: DWORD,
        lpNumberOfBytesSent: *mut DWORD,
        dwFlags: DWORD,
        lpTo: *const SOCKADDR,
        iTolen: i32,
        lpOverlapped: *mut OVERLAPPED,
        lpCompletionRoutine: LPWSAOVERLAPPED_COMPLETION_ROUTINE,
    ) -> i32;
    pub(crate) fn WSAIoctl(
        s: SOCKET,
        dwIoControlCode: DWORD,
        lpvInBuffer: *const c_void,
        cbInBuffer: DWORD,
        lpvOutBuffer: *mut c_void,
        cbOutBuffer: DWORD,
        lpcbBytesReturned: *mut DWORD,
        lpOverlapped: *mut OVERLAPPED,
        lpCompletionRoutine: LPWSAOVERLAPPED_COMPLETION_ROUTINE,
    ) -> i32;
    pub(crate) fn setsockopt(
        s: SOCKET,
        level: i32,
        optname: i32,
        optval: *const u8,
        optlen: i32,
    ) -> i32;
    pub(crate) fn getsockopt(
        s: SOCKET,
        level: i32,
        optname: i32,
        optval: *mut u8,
        optlen: *mut i32,
    ) -> i32;
    pub(crate) fn getsockname(s: SOCKET, name: *mut SOCKADDR, namelen: *mut i32) -> i32;
    pub(crate) fn bind(s: SOCKET, name: *const SOCKADDR, namelen: i32) -> i32;
    pub(crate) fn listen(s: SOCKET, backlog: i32) -> i32;
    pub(crate) fn closesocket(s: SOCKET) -> i32;
    pub(crate) fn shutdown(s: SOCKET, how: i32) -> i32;
    #[cfg(test)]
    pub(crate) fn send(s: SOCKET, buf: *const u8, len: i32, flags: i32) -> i32;
}

#[link(name = "ntdll")]
unsafe extern "system" {
    pub(crate) fn NtCreateWaitCompletionPacket(
        WaitCompletionPacketHandle: *mut HANDLE,
        DesiredAccess: DWORD,
        ObjectAttributes: *mut c_void,
    ) -> NTSTATUS;
    pub(crate) fn NtAssociateWaitCompletionPacket(
        WaitCompletionPacketHandle: HANDLE,
        IoCompletionHandle: HANDLE,
        TargetObjectHandle: HANDLE,
        KeyContext: *mut c_void,
        ApcContext: *mut c_void,
        IoStatus: NTSTATUS,
        IoStatusInformation: ULONG_PTR,
        AlreadySignaled: *mut BOOLEAN,
    ) -> NTSTATUS;
    pub(crate) fn NtCancelWaitCompletionPacket(
        WaitCompletionPacketHandle: HANDLE,
        RemoveSignaledPacket: BOOLEAN,
    ) -> NTSTATUS;
    pub(crate) fn NtQueryInformationFile(
        FileHandle: HANDLE,
        IoStatusBlock: *mut IO_STATUS_BLOCK,
        FileInformation: *mut c_void,
        Length: ULONG,
        FileInformationClass: u32,
    ) -> NTSTATUS;
    pub(crate) fn NtSetInformationFile(
        FileHandle: HANDLE,
        IoStatusBlock: *mut IO_STATUS_BLOCK,
        FileInformation: *mut c_void,
        Length: ULONG,
        FileInformationClass: u32,
    ) -> NTSTATUS;
    pub(crate) fn RtlNtStatusToDosError(Status: NTSTATUS) -> ULONG;
}

/// `GENERIC_ALL` for the wait completion packet.
pub(crate) const GENERIC_ALL: DWORD = 0x1000_0000;

/// The error of an operation that completed with `status`, in the codes
/// `std::io::Error` knows (Winsock's for the network, Win32's else); `None`
/// for a success. A datagram cut to fit its buffer counts as a success, as
/// it does on unix: the bytes that fit are there.
pub(crate) fn status_error(status: NTSTATUS) -> Option<i32> {
    let code = match status {
        STATUS_SUCCESS | STATUS_BUFFER_OVERFLOW => return None,
        STATUS_CANCELLED => WSA_OPERATION_ABORTED,
        STATUS_INVALID_HANDLE | STATUS_OBJECT_TYPE_MISMATCH => WSAENOTSOCK,
        STATUS_IO_TIMEOUT => WSAETIMEDOUT,
        STATUS_LOCAL_DISCONNECT | STATUS_REMOTE_DISCONNECT | STATUS_CONNECTION_RESET => {
            WSAECONNRESET
        }
        STATUS_PORT_UNREACHABLE => WSAECONNRESET,
        STATUS_GRACEFUL_DISCONNECT => WSAENOTCONN,
        STATUS_ADDRESS_ALREADY_EXISTS => WSAEADDRINUSE,
        STATUS_CONNECTION_REFUSED => WSAECONNREFUSED,
        STATUS_NETWORK_UNREACHABLE => WSAENETUNREACH,
        STATUS_HOST_UNREACHABLE => WSAEHOSTUNREACH,
        STATUS_CONNECTION_ABORTED => WSAECONNABORTED,
        STATUS_INVALID_DEVICE_STATE => WSAEINVAL,
        // SAFETY: a pure lookup of the status table.
        other => (unsafe { RtlNtStatusToDosError(other) }) as i32,
    };
    Some(code)
}

/// The last Winsock error, as an `io::Error`.
pub(crate) fn wsa_error() -> std::io::Error {
    // SAFETY: reads the calling thread's last Winsock error.
    std::io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

/// The last Win32 error, as an `io::Error`.
pub(crate) fn last_error() -> std::io::Error {
    // SAFETY: reads the calling thread's last error.
    std::io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
}
