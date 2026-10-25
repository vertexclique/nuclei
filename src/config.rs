///
/// Nuclei's proactor configuration.
#[derive(Clone, Debug, Default)]
pub struct NucleiConfig {
    /// **IO_URING Configuration** allows you to configure [io_uring](https://unixism.net/loti/what_is_io_uring.html) backend.
    pub iouring: IoUringConfiguration,
    /// The kernel interface every runtime that names none runs its IO on
    /// ([`Builder::backend`](crate::Builder::backend) names one for one
    /// runtime). `None` (the default) picks io_uring where the kernel lets
    /// this process have it and the platform's poller otherwise; a backend
    /// named here that this process cannot have makes the first shard fail
    /// with the reason. A non-empty `NUCLEI_BACKEND` overrides it.
    pub backend: Option<crate::IoBackend>,
}

/// **IO_URING Configuration**
/// 5.19+ Kernel is required to operate IO_URING.
#[derive(Clone, Debug)]
pub struct IoUringConfiguration {
    /// Allowed entries in both submission and completion queues.
    ///
    /// **Default**: 2048.
    pub queue_len: u32,
    /// SQPOLL kernel thread wake up interval.
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
    /// Decreasing this will put more pressure to kernel, increases cpu usage.
    /// Increasing it will slow down completion push rate from kernel.
    /// This config is in milliseconds.
    ///
    /// **Default**: `None`: no SQPOLL thread.
    pub sqpoll_wake_interval: Option<u32>,

    /// Get and/or set the limit for number of io_uring worker threads per NUMA
    /// node. `per_numa_bounded_worker_count` holds the limit for bounded workers,
    /// which process I/O operations expected to be bound in time,
    /// that is I/O on regular files or block devices. Passing `0` does not change
    /// the current limit.
    ///
    /// **Default**: `Some(256)`.
    ///
    /// If [None] passed, by default, the amount of bounded IO workers is limited to how
    /// many SQ entries the ring was setup with, or 4 times the number of
    /// online CPUs in the system, whichever is smaller.
    pub per_numa_bounded_worker_count: Option<u32>,

    /// This `per_numa_unbounded_worker_count` holds the limit for unbounded workers,
    /// which carry out I/O operations that can never complete, for instance I/O
    /// on sockets. Passing `0` does not change the current limit.
    ///
    /// **Default**: `Some(512)`.
    ///
    /// If [None] passed unbounded workers will be limited by the process task limit,
    /// as indicated by the rlimit [RLIMIT_NPROC](https://man7.org/linux/man-pages/man2/getrlimit.2.html) limit.
    pub per_numa_unbounded_worker_count: Option<u32>,

    /// This argument allows aggressively waiting on CQ(completion queue) to have low latency of IO completions.
    /// Basically, this argument polls CQEs(completion queue events) directly on cq at userspace.
    /// Mind that, using this increase pressure on CPUs from userspace side. With SQPOLL as well, this is
    /// a double polling approach, where the kernel takes submissions by itself, processes them and puts
    /// completions on the completion queue, which nuclei picks up without latency.
    ///
    /// **Default**: `false`.
    pub aggressive_poll: bool,

    /// Perform busy-waiting for I/O completion events, as opposed to getting notifications via an
    /// asynchronous IRQ (Interrupt Request). This will reduce latency, but increases CPU usage.
    /// This is only usable on file systems that support polling and files opened with `O_DIRECT`.
    ///
    /// **Default**: `false`.
    pub iopoll_enabled: bool,

    /// Buffers in each shard's pool for multishot receives: the kernel fills
    /// one as bytes arrive, so a stream's receive stays armed for every
    /// arrival and a receive that waits holds no buffer of its own. A power
    /// of two up to 32768; 0 turns multishot receives off, and each read then
    /// takes a buffer of the shard's own pool for its receive.
    ///
    /// **Default**: 256.
    pub recv_buffers: u16,

    /// The size in bytes of each of those buffers. A receive is stopped once
    /// the bytes it handed out and its reader has not taken pass an eighth of
    /// the pool (at least one buffer), and asked for again when its reader
    /// catches up. What the kernel took for it before the stop stays with
    /// it, so a reader with a long backlog may hold more until it reads;
    /// other receives take buffers of the shard's own pool meanwhile.
    ///
    /// **Default**: 16384.
    pub recv_buffer_size: u32,
    // XXX: `redrive_kthread_wake` = bool, syncs queue changes so kernel threads got awakened. increased cpu usage.
}

/// The default of [`IoUringConfiguration::recv_buffers`].
const DEFAULT_RECV_BUFFERS: u16 = 256;
/// The default of [`IoUringConfiguration::recv_buffer_size`].
const DEFAULT_RECV_BUFFER_SIZE: u32 = 16 * 1024;

impl IoUringConfiguration {
    ///
    /// Standard way to use IO_URING. No polling, purely IRQ awaken IO completion.
    /// This is a normal way to process IO, mind that with this approach
    /// `actual completion time != userland reception of completion`.
    /// Throughput is low compared to all the other config alternatives.
    ///
    /// **NOTE:** If you don't know what to select as configuration, please select this.
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

    ///
    /// Low Latency Driven version of IO_URING, where it is suitable for high traffic environments.
    /// High throughput low latency solution where it consumes a lot of resources.
    pub fn low_latency_driven(queue_len: u32) -> Self {
        Self {
            queue_len,
            sqpoll_wake_interval: Some(2),
            aggressive_poll: true,
            ..Self::default()
        }
    }

    ///
    /// Kernel poll only version of IO_URING, where it is suitable for high traffic environments.
    /// This version won't allow aggressive polling on completion queue(CQ).
    pub fn kernel_poll_only(queue_len: u32) -> Self {
        Self {
            queue_len,
            sqpoll_wake_interval: Some(2),
            aggressive_poll: false,
            ..Self::default()
        }
    }

    ///
    /// IOPOLL enabled ring configuration for operating on files with low-latency.
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
