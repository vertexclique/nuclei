use nuclei::*;
use std::fs::File;

use std::path::PathBuf;

use futures::io::IoSliceMut;
use futures::AsyncReadExt;

const IOVEC_WIDTH: usize = 1 << 10;

#[nuclei::test]
#[cfg(target_os = "linux")]
async fn read_vectored() {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("testdata");
    path.push("quark-gluon-plasma");

    let mut buf1 = [0; IOVEC_WIDTH];
    let mut buf2 = [0; IOVEC_WIDTH];
    let mut buf3 = [0; IOVEC_WIDTH];
    let mut bufs = [
        IoSliceMut::new(&mut buf1),
        IoSliceMut::new(&mut buf2),
        IoSliceMut::new(&mut buf3),
    ];

    let fo = File::open(&path).unwrap();
    let mut file = Handle::<File>::new(fo).unwrap();
    file.read_vectored(&mut bufs[..]).await.unwrap();

    let bufs = vec![buf1, buf2, buf3];


    bufs.iter().enumerate().for_each(|(_idx, e)| {
        assert_eq!(IOVEC_WIDTH, String::from_utf8_lossy(&e[..]).len());
    });
}
