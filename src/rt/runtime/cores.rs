//! The cores a runtime's threads run on: [`Builder::worker_cores`] and
//! [`Builder::blocking_cores`], checked against the cores the process may
//! run on, and the affinity masks made from them.

use std::io;

use super::{Builder, CpuMask};

impl Builder {
    /// The cores the workers run on, in the order the workers take them:
    /// worker `k` runs on `cores[k % cores.len()]`, pinned to it, or, with
    /// [`pin_workers(false)`](Self::pin_workers), on any core of the list.
    /// The worker count defaults to the list's length. Every core must be
    /// one the process may run on and the list must not be empty;
    /// [`build`](Self::build) fails with `InvalidInput` otherwise. Pinning
    /// takes effect on Linux and Windows; elsewhere the list sets the
    /// worker count and is checked against the cores the machine has.
    pub fn worker_cores(mut self, cores: impl IntoIterator<Item = usize>) -> Self {
        self.worker_cores = Some(cores.into_iter().collect());
        self
    }

    /// The cores the blocking pool's threads run on, any of them (default:
    /// the cores of the thread that builds the runtime). Checked as
    /// [`worker_cores`](Self::worker_cores) are, and in effect on Linux and
    /// Windows.
    pub fn blocking_cores(mut self, cores: impl IntoIterator<Item = usize>) -> Self {
        self.blocking_cores = Some(cores.into_iter().collect());
        self
    }
}

/// The cores the process may run on: its affinity mask's (Linux, Windows),
/// or every core of the machine where no mask is known.
fn process_cores() -> Vec<usize> {
    let cores = super::allowed_cores();
    if !cores.is_empty() {
        return cores;
    }
    (0..std::thread::available_parallelism().map_or(1, usize::from)).collect()
}

/// Refuses an empty list of cores, or one naming a core the process may not
/// run on, as the configuration of `setting`.
pub(super) fn validate(setting: &str, cores: Option<&[usize]>) -> io::Result<()> {
    let Some(cores) = cores else {
        return Ok(());
    };
    if cores.is_empty() {
        return Err(crate::config::refuse(setting, "empty", "at least one core"));
    }
    let allowed = process_cores();
    if let Some(core) = cores.iter().find(|core| !allowed.contains(core)) {
        return Err(crate::config::refuse(
            setting,
            format_args!("a list naming core {core}"),
            format_args!("a list of the cores this process may run on ({allowed:?})"),
        ));
    }
    Ok(())
}

/// The affinity mask of `cores`; `None` where threads take no mask.
pub(super) fn mask_of(cores: &[usize]) -> Option<CpuMask> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: a zeroed set of our own, and plain writes into it.
        let mut set: libc::cpu_set_t = unsafe { core::mem::zeroed() };
        for &core in cores.iter().filter(|&&c| c < libc::CPU_SETSIZE as usize) {
            // SAFETY: as above; the core is inside the set.
            unsafe { libc::CPU_SET(core, &mut set) };
        }
        Some(set)
    }
    #[cfg(windows)]
    {
        Some(
            cores
                .iter()
                .filter(|&&c| c < usize::BITS as usize)
                .fold(0usize, |mask, &c| mask | (1 << c)),
        )
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = cores;
        None
    }
}
