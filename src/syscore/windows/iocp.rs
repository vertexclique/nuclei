pub struct SysProactor {}

impl SysProactor {
    /// Returns a reference to the proactor.
    pub fn new() -> Result<Self, ()> {
        Ok(Self {})
    }

    /// Wakes the thread waiting on proactor.
    pub fn wake(&self) {
        todo!();
    }

    /// Wait for completion of IO object
    pub fn wait(&self, max_event_size: usize, duration: Option<Duration>) -> io::Result<usize> {
        todo!();
    }

    /// Get underlying proactor instance.
    pub(crate) fn inner(&self) -> &SysProactor {
        todo!();
    }
}
