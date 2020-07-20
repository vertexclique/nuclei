use nuclei::*;
use std::fs::File;

use std::path::PathBuf;

use futures::io::IoSliceMut;

use futures_util::io::AsyncReadExt;

const IOVEC_WIDTH: usize = 1 << 10;

#[test]
fn read_vectored() {
    let x = drive(async {
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

        vec![buf1, buf2, buf3]
    });

    x.iter().enumerate().for_each(|(_idx, e)| {
        assert_eq!(IOVEC_WIDTH, String::from_utf8_lossy(&e[..]).len());
    });
}
