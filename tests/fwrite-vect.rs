/// Unfortunately, underlying implementation of writevec have problems.
/// Ref issue: https://github.com/rust-lang/rust/issues/68041
/// This should work fine with iouring.
#[cfg(feature = "iouring")]
#[test]
fn write_vectored() {
    use nuclei::*;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::path::PathBuf;
    use std::time::Duration;

    use futures::io::IoSliceMut;
    use futures::{AsyncRead, AsyncSeek, AsyncSeekExt, AsyncWriteExt};
    use futures_util::io::AsyncReadExt;
    use std::io::{IoSlice, Read, SeekFrom};
    use std::ops::Deref;

    const IOVEC_WIDTH: usize = 1 << 10;

    let x = drive(async {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("testdata");
        path.push("dark-matter-vect");

        let buf1 = [0x41; IOVEC_WIDTH];
        let buf2 = [0x42; IOVEC_WIDTH];
        let buf3 = [0x43; IOVEC_WIDTH];
        let mut bufs = [
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
        bufv
    });

    assert_eq!(x.matches('A').count(), IOVEC_WIDTH);
    assert_eq!(x.matches('B').count(), IOVEC_WIDTH);
    assert_eq!(x.matches('C').count(), IOVEC_WIDTH);

    println!("SG write was: {}", x);
}
