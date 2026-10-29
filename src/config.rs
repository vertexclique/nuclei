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
    /// Entries in each ring's submission queue (default 2048); the completion
    /// queue holds twice as many.
    ///
    /// At least 64, the
    /// floor a ring is shrunk to when the system has no memory for more; a
    /// smaller value is refused as the configuration's mistake when the first
    /// shard starts. The kernel rounds the count up to a power of two and
    /// clamps it to its own maximum (32768 on current kernels), so a larger
    /// value is accepted and gets what the kernel allows.
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

    /// Buffers in each shard's group for multishot receives (default 256, or
    /// [`queue_len`](Self::queue_len) rounded up to a power of two when that is
    /// fewer): the kernel fills one as bytes arrive, so a stream's receive
    /// stays armed for every arrival and a receive that waits holds no buffer
    /// of its own.
    ///
    /// 0 turns multishot receives off, and each read then takes a buffer of
    /// the shard's own pool for its receive. Otherwise a power of two from 1 to
    /// 32768 and below twice `queue_len`, so the group never takes more than
    /// half the completion queue (twice `queue_len`, rounded up to a power of
    /// two); any other value is refused as the configuration's mistake when
    /// the first shard starts. A ring the system could only make with fewer
    /// entries than `queue_len` (it had no memory for more) gets a group cut to
    /// the same rule.
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
    /// From 512 (smaller buffers split a stream into chunks of a few packets
    /// each) to 1 GiB; a
    /// value outside is refused as the configuration's mistake when the first
    /// shard starts. Not checked while `recv_buffers` is 0.
    ///
    /// A receive is stopped once the bytes it handed out and its reader has
    /// not taken pass an eighth of the group (at least one buffer), and asked
    /// for again when its reader catches up. What the kernel took for it
    /// before the stop stays with it, so a reader with a long backlog may
    /// hold more until it reads; other receives take buffers of the shard's
    /// own pool meanwhile.
    pub recv_buffer_size: u32,
}

/// The fewest entries a ring may be asked for: the floor a ring made short of
/// memory is shrunk to.
pub(crate) const MIN_QUEUE_LEN: u32 = 64;
/// The smallest receive buffer.
pub(crate) const MIN_RECV_BUFFER_SIZE: u32 = 512;
/// The largest receive buffer.
pub(crate) const MAX_RECV_BUFFER_SIZE: u32 = 1 << 30;

/// The error for a setting outside what the runtime can work with: the
/// configuration's mistake, naming the setting, its value and what is allowed.
/// The one place every refused setting is worded.
pub(crate) fn refuse(
    setting: &str,
    value: impl core::fmt::Display,
    allowed: impl core::fmt::Display,
) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("{setting} is {value}; it must be {allowed}"),
    )
}

/// The default of [`IoUringConfiguration::recv_buffers`].
const DEFAULT_RECV_BUFFERS: u16 = 256;

/// The default receive buffers of a ring of `queue_len` entries: as many as
/// half its completion queue holds, up to [`DEFAULT_RECV_BUFFERS`].
fn default_recv_buffers(queue_len: u32) -> u16 {
    u16::try_from(
        queue_len
            .min(u32::from(DEFAULT_RECV_BUFFERS))
            .next_power_of_two(),
    )
    .unwrap_or(DEFAULT_RECV_BUFFERS)
}
/// The default of [`IoUringConfiguration::recv_buffer_size`].
const DEFAULT_RECV_BUFFER_SIZE: u32 = 16 * 1024;

impl IoUringConfiguration {
    /// Checks the sizes against what a ring can work with.
    ///
    /// # Errors
    ///
    /// `InvalidInput` naming the setting, its value and the allowed range.
    pub(crate) fn validate(&self) -> std::io::Result<()> {
        if self.queue_len < MIN_QUEUE_LEN {
            return Err(refuse(
                "io_uring queue_len",
                self.queue_len,
                format_args!("at least {MIN_QUEUE_LEN}"),
            ));
        }
        if self.recv_buffers == 0 {
            return Ok(());
        }
        let twice = self.queue_len.saturating_mul(2);
        if !self.recv_buffers.is_power_of_two() || u32::from(self.recv_buffers) >= twice {
            return Err(refuse(
                "io_uring recv_buffers",
                self.recv_buffers,
                format_args!(
                    "0, or a power of two from 1 to 32768 and below twice queue_len ({twice})"
                ),
            ));
        }
        if !(MIN_RECV_BUFFER_SIZE..=MAX_RECV_BUFFER_SIZE).contains(&self.recv_buffer_size) {
            return Err(refuse(
                "io_uring recv_buffer_size",
                self.recv_buffer_size,
                format_args!("from {MIN_RECV_BUFFER_SIZE} to {MAX_RECV_BUFFER_SIZE}"),
            ));
        }
        Ok(())
    }

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
            recv_buffers: default_recv_buffers(queue_len),
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
            recv_buffers: default_recv_buffers(queue_len),
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
            recv_buffers: default_recv_buffers(queue_len),
            ..Self::default()
        }
    }

    /// Busy-waits for file completions ([`iopoll_enabled`](Self::iopoll_enabled)):
    /// low-latency IO on files opened with `O_DIRECT`.
    pub fn io_poll(queue_len: u32) -> Self {
        Self {
            queue_len,
            iopoll_enabled: true,
            recv_buffers: default_recv_buffers(queue_len),
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

#[cfg(test)]
mod tests;
