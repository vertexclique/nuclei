mod io_support;

use std::fs::{File, OpenOptions};
use std::io::{IoSlice, SeekFrom};

use io_support::{TempFile, read_to_end, seek, write_vectored};
use nuclei::Handle;

const IOVEC_WIDTH: usize = 1 << 10;

#[nuclei::test]
async fn write_vectored_gathers_every_buffer_in_order() {
    let tmp = TempFile::new("dark-matter-vect");
    let buf1 = [0x41; IOVEC_WIDTH];
    let buf2 = [0x42; IOVEC_WIDTH];
    let buf3 = [0x43; IOVEC_WIDTH];
    let bufs = [
        IoSlice::new(&buf1),
        IoSlice::new(&buf2),
        IoSlice::new(&buf3),
    ];
    let fo = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&tmp.0)
        .unwrap();
    let mut file = Handle::<File>::new(fo).unwrap();
    assert_eq!(
        write_vectored(&mut file, &bufs).await.unwrap(),
        3 * IOVEC_WIDTH
    );
    assert_eq!(seek(&mut file, SeekFrom::Start(0)).await.unwrap(), 0);
    let got = read_to_end(&mut file).await.unwrap();
    let expected: Vec<u8> = [buf1, buf2, buf3].concat();
    assert_eq!(got, expected);
}
