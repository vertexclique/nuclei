/// Nuclei's proactor configuration: the io_uring settings of every shard, and
/// the backend of every runtime that names none. It is set once, before any
/// IO, with [`Proactor::with_config`](crate::Proactor::with_config).
#[derive(Clone, Debug, Default)]
pub struct NucleiConfig {
    /// The settings every shard's io_uring ring is made with.
    pub iouring: IoUringConfiguration,
    /// The kernel interface every runtime that names none runs its IO on
    /// ([`Builder::backend`](crate::Builder::backend) names one for one
    /// runtime). `None` (the default) picks io_uring where the kernel lets
    /// this process have it, and the platform's poller otherwise. Having it
    /// means every kernel feature a shard's ring needs with this
    /// configuration: the buffer ring for multishot receives while
    /// [`recv_buffers`](IoUringConfiguration::recv_buffers) is not 0, and the
    /// table of fixed files. A backend named here that this process cannot
    /// have makes the first shard fail with the reason. A non-empty
    /// `NUCLEI_BACKEND` overrides it.
    pub backend: Option<crate::IoBackend>,
}

/// The settings of every shard's io_uring ring (Linux 6.1 or later).
///
/// The default is [`interrupt_driven`](Self::interrupt_driven) with 2048
/// queue entries and the io-wq worker limits `Some(256)` and `Some(512)`.
#[derive(Clone, Debug)]
pub struct IoUringConfiguration {
    /// Entries in each ring's submission and completion queues (default
    /// 2048).
    pub queue_len: u32,
    /// With `Some(ms)`, a kernel thread polls the submission queue and sleeps
    /// after that many idle milliseconds: fewer system calls for more CPU
    /// (default `None`: no polling thread).
    ///
    /// Before version 5.11 of the Linux kernel, to successfully use this feature, the application
    /// must register a set of files to be used for IO through io_uring_register(2) using the
    /// `IORING_REGISTER_FILES` opcode. Failure to do so will result in submitted IO being errored
    /// with EBADF. The presence of this feature can be detected by the `IORING_FEAT_SQPOLL_NONFIXED`
    /// feature flag. In version 5.11 and later, it is no longer necessary to register files to use
    /// this feature. 5.11 also allows using this as non-root, if the user has the CAP_SYS_NICE
    /// capability. In 5.13 this requirement was also relaxed, and no special privileges are needed
    /// for SQPOLL in newer kernels. Certain stable kernels older than 5.13 may also support
    /// unprivileged SQPOLL.
    ///
    /// A shorter interval keeps the kernel thread awake more, for more CPU; a
    /// longer one lets it sleep, and a submission after it slept wakes it.
    pub sqpoll_wake_interval: Option<u32>,

    /// The most io_uring worker threads per NUMA node for IO bound in time,
    /// on regular files and block devices (default `Some(256)`).
    ///
    /// `Some(0)` leaves the kernel's limit as it is; with `None` the kernel
    /// limits them to the ring's submission entries or four times the online
    /// CPUs, whichever is smaller.
    pub per_numa_bounded_worker_count: Option<u32>,

    /// The most io_uring worker threads per NUMA node for IO that may never
    /// complete, such as on sockets (default `Some(512)`).
    ///
    /// `Some(0)` leaves the kernel's limit as it is; with `None` the
    /// process's task limit (`RLIMIT_NPROC`) bounds them.
    pub per_numa_unbounded_worker_count: Option<u32>,

    /// Kept for code written against earlier versions; the ring does not read
    /// it (default `false`).
    pub aggressive_poll: bool,

    /// Busy-waits for file completions instead of taking interrupts (default
    /// `false`): lower latency for more CPU, and only for files opened with
    /// `O_DIRECT` on file systems that support polling.
    pub iopoll_enabled: bool,

    /// Buffers in each shard's group for multishot receives (default 256): the
    /// kernel fills one as bytes arrive, so a stream's receive stays armed for
    /// every arrival and a receive that waits holds no buffer of its own.
    ///
    /// A power of two up to 32768; 0 turns multishot receives off, and each
    /// read then takes a buffer of the shard's own pool for its receive.
    ///
    /// A kernel that refuses provided buffer rings makes the default backend
    /// choice pick the poller; 0 keeps io_uring there. Ubuntu's 6.8 kernels
    /// from 6.8.0-136 on refuse every valid registration: a backport inverted
    /// the check of its reserved words
    /// (<https://bugs.launchpad.net/ubuntu/+source/linux/+bug/2162843>).
    pub recv_buffers: u16,

    /// The size in bytes of each of those buffers (default 16384); for UDP,
    /// one that holds the largest datagram expected.
    ///
    /// A receive is stopped once the bytes it handed out and its reader has
    /// not taken pass an eighth of the group (at least one buffer), and asked
    /// for again when its reader catches up. What the kernel took for it
    /// before the stop stays with it, so a reader with a long backlog may
    /// hold more until it reads; other receives take buffers of the shard's
    /// own pool meanwhile.
    pub recv_buffer_size: u32,
}

/// The default of [`IoUringConfiguration::recv_buffers`].
const DEFAULT_RECV_BUFFERS: u16 = 256;
/// The default of [`IoUringConfiguration::recv_buffer_size`].
const DEFAULT_RECV_BUFFER_SIZE: u32 = 16 * 1024;

impl IoUringConfiguration {
    /// No polling: a completion wakes its shard by interrupt. The one to pick
    /// when unsure.
    pub fn interrupt_driven(queue_len: u32) -> Self {
        Self {
            queue_len,
            sqpoll_wake_interval: None,
            per_numa_bounded_worker_count: None,
            per_numa_unbounded_worker_count: None,
            aggressive_poll: false,
            iopoll_enabled: false,
            recv_buffers: DEFAULT_RECV_BUFFERS,
            recv_buffer_size: DEFAULT_RECV_BUFFER_SIZE,
        }
    }

    /// A kernel thread polls the submission queue and sleeps after 2 idle
    /// milliseconds: lower latency under steady traffic, for more CPU.
    pub fn low_latency_driven(queue_len: u32) -> Self {
        Self {
            queue_len,
            sqpoll_wake_interval: Some(2),
            aggressive_poll: true,
            ..Self::default()
        }
    }

    /// A kernel thread polls the submission queue and sleeps after 2 idle
    /// milliseconds, as [`low_latency_driven`](Self::low_latency_driven).
    pub fn kernel_poll_only(queue_len: u32) -> Self {
        Self {
            queue_len,
            sqpoll_wake_interval: Some(2),
            aggressive_poll: false,
            ..Self::default()
        }
    }

    /// Busy-waits for file completions ([`iopoll_enabled`](Self::iopoll_enabled)):
    /// low-latency IO on files opened with `O_DIRECT`.
    pub fn io_poll(queue_len: u32) -> Self {
        Self {
            queue_len,
            iopoll_enabled: true,
            ..Self::default()
        }
    }
}

/// The interrupt-driven profile, with the io-wq worker limits nuclei sets on
/// every ring.
impl Default for IoUringConfiguration {
    fn default() -> Self {
        Self {
            per_numa_bounded_worker_count: Some(1 << 8),
            per_numa_unbounded_worker_count: Some(1 << 9),
            ..Self::interrupt_driven(1 << 11)
        }
    }
}
