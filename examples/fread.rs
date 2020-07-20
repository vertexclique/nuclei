use nuclei::*;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Duration;

use futures::AsyncRead;
use futures_util::io::AsyncReadExt;

fn main() -> io::Result<()> {
    let x = drive(async {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("data");
        path.push("quark-gluon-plasma");

        let fo = File::open(&path).unwrap();
        let mut file = Handle::<File>::new(fo).unwrap();
        let mut buffer = String::new();
        file.read_to_string(&mut buffer).await;
        buffer
    });

    // println!("Content: {}", x);
    println!("Length of file is {}", x.len());

    Ok(())
}
