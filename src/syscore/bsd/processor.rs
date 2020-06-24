use std::io;
use std::io::{Read, Write};
use std::{fs::File, os::unix::io::{AsRawFd, FromRawFd}, mem::ManuallyDrop};

pub struct Processor;

impl Processor {
    pub async fn processor_read<R: AsRawFd>(io: &R, buf: &mut [u8]) -> io::Result<usize> {
        // let mut file = unsafe { File::from_raw_fd(io.as_raw_fd()) };
        // let res = file.read(buf);
        // let _ = ManuallyDrop::new(file);
        // res
        todo!()
    }

    pub async fn processor_write<R: AsRawFd>(io: &R, buf: &[u8]) -> io::Result<usize> {
        todo!()
    }
}
