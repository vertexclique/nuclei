use nuclei::*;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use futures::io::IoSliceMut;
use futures::AsyncRead;
use futures_util::io::AsyncReadExt;
use std::ops::Deref;

const IOVEC_WIDTH: usize = 1 << 10;

fn main() -> io::Result<()> {
    let x = drive(async {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("data");
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

    x.iter().enumerate().for_each(|(idx, e)| {
        println!(
            "::: iovec ::: {}, data ::: \n\n{}\n\n",
            idx,
            String::from_utf8_lossy(&e[..])
        );
    });

    Ok(())
}
