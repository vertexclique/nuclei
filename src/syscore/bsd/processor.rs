use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd};

pub struct Processor;

impl Processor {
    pub async fn read_processor<R: AsRawFd>(io: &R, buf: &mut [u8]) -> io::Result<usize> {
        todo!()
    }

    pub async fn write_processor<R: AsRawFd>(io: &R, buf: &[u8]) -> io::Result<usize> {
        todo!()
    }
}
