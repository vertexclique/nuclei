mod io_support;

use std::fs::File;
use std::io::IoSliceMut;

use io_support::{read_vectored, testdata};
use nuclei::Handle;

const IOVEC_WIDTH: usize = 1 << 10;

#[nuclei::test]
async fn read_vectored_fills_every_buffer_in_order() {
    let path = testdata("quark-gluon-plasma");
    let expected = std::fs::read(&path).unwrap();
    let mut buf1 = [0; IOVEC_WIDTH];
    let mut buf2 = [0; IOVEC_WIDTH];
    let mut buf3 = [0; IOVEC_WIDTH];
    let mut bufs = [
        IoSliceMut::new(&mut buf1),
        IoSliceMut::new(&mut buf2),
        IoSliceMut::new(&mut buf3),
    ];
    let mut file = Handle::<File>::new(File::open(&path).unwrap()).unwrap();
    let n = read_vectored(&mut file, &mut bufs).await.unwrap();
    assert_eq!(n, 3 * IOVEC_WIDTH, "one read fills all three buffers");
    assert_eq!(&buf1[..], &expected[..IOVEC_WIDTH]);
    assert_eq!(&buf2[..], &expected[IOVEC_WIDTH..2 * IOVEC_WIDTH]);
    assert_eq!(&buf3[..], &expected[2 * IOVEC_WIDTH..3 * IOVEC_WIDTH]);
}
