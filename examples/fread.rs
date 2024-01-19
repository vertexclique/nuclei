use nuclei::*;
use std::fs::File;
use std::io;
use std::io::{Seek, SeekFrom};
use std::path::PathBuf;

use futures::AsyncReadExt;

fn main() -> io::Result<()> {
    let x: io::Result<String> = drive(async {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("data");
        path.push("quark-gluon-plasma");
        dbg!(&path);

        let fo = File::open(&path).unwrap();
        let mut file = Handle::<File>::new(fo).unwrap();
        let mut buffer = String::new();
        file.read_to_string(&mut buffer).await?;
        Ok(buffer)
    });
    let x = x?;

    println!("Content: {}", x);
    println!("Length of file is {}", x.len());

    Ok(())
}
