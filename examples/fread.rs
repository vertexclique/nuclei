use nuclei::*;
use std::io;
use std::time::Duration;
use std::fs::File;
use std::path::PathBuf;

use futures::AsyncRead;
use futures_util::io::AsyncReadExt;
// use futures::io::BufReader;


fn main() -> io::Result<()> {
    let x = drive(async {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("data");
        path.push("quark-gluon-plasma");
        let fo = File::open(&path).unwrap();
        let mut file = Handle::<File>::new(fo).unwrap();
        let mut buffer = String::new();
        // let mut buffer = Vec::new();
        file.read_to_string(&mut buffer).await;
        dbg!(&buffer.len());
        buffer
    });

    // println!("{}", x);

    Ok(())
}