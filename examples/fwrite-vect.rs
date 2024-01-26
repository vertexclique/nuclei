use nuclei::*;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::PathBuf;

use futures::AsyncReadExt;
use futures::{AsyncSeekExt, AsyncWriteExt};
use std::io::{IoSlice, SeekFrom};

const IOVEC_WIDTH: usize = 1 << 10;

#[nuclei::main]
async fn main() -> io::Result<()> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("data");
    path.push("dark-matter-vect");

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
        .open(&path)
        .unwrap();
    let mut file = Handle::<File>::new(fo).unwrap();
    file.write_vectored(&bufs[..]).await.unwrap();

    let mut bufv = String::new();
    assert!(file.seek(SeekFrom::Start(0)).await.is_ok());
    file.read_to_string(&mut bufv).await.unwrap();

    assert_eq!(bufv.matches('A').count(), IOVEC_WIDTH);
    assert_eq!(bufv.matches('B').count(), IOVEC_WIDTH);
    assert_eq!(bufv.matches('C').count(), IOVEC_WIDTH);

    println!("SG write was: {}", bufv);

    Ok(())
}
