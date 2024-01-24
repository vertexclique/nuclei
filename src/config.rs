
///
/// Nuclei's proactor configuration.
#[derive(Clone, Debug, Default)]
pub struct NucleiConfig {
    /// **IO_URING Configuration** allows you to configure [io_uring](https://unixism.net/loti/what_is_io_uring.html) backend.
    pub iouring: IoUringConfiguration
}

/// **IO_URING Configuration**
/// 5.19+ Kernel is required to operate IO_URING.
#[derive(Clone, Debug)]
pub struct IoUringConfiguration {
    /// Allowed entries in both submission and completion queues.
    ///
    /// **[default]**: By default this value is 2048.
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
    /// **[default]**: If [None] then SQPOLL will be disabled.
    pub sqpoll_wake_interval: Option<u32>,

    /// Get and/or set the limit for number of io_uring worker threads per NUMA
    /// node. `per_numa_bounded_worker_count` holds the limit for bounded workers,
    /// which process I/O operations expected to be bound in time,
    /// that is I/O on regular files or block devices. Passing `0` does not change
    /// the current limit.
    ///
    /// **[default]**: If [None] then Nuclei default value of will be used `256`.
    /// If [None] passed, by default, the amount of bounded IO workers is limited to how
    /// many SQ entries the ring was setup with, or 4 times the number of
    /// online CPUs in the system, whichever is smaller.
    pub per_numa_bounded_worker_count: Option<u32>,

    /// This `per_numa_unbounded_worker_count` holds the limit for unbounded workers,
    /// which carry out I/O operations that can never complete, for instance I/O
    /// on sockets. Passing `0` does not change the current limit.
    ///
    /// **[default]**: If [None] then Nuclei default value of will be used `512`.
    /// If [None] passed unbounded workers will be limited by the process task limit,
    /// as indicated by the rlimit [RLIMIT_NPROC](https://man7.org/linux/man-pages/man2/getrlimit.2.html) limit.
    pub per_numa_unbounded_worker_count: Option<u32>,

    // XXX: `redrive_kthread_wake` = bool, syncs queue changes so kernel threads got awakened. increased cpu usage.
}

impl Default for IoUringConfiguration {
    fn default() -> Self {
        Self {
            queue_len: 1 << 11,
            sqpoll_wake_interval: Some(2),
            per_numa_bounded_worker_count: Some(1 << 8),
            per_numa_unbounded_worker_count: Some(1 << 9),
        }
    }
}