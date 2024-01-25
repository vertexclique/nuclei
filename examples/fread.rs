use nuclei::*;
use std::fs::File;
use std::io;
use std::io::{Seek, SeekFrom};
use std::path::PathBuf;

use futures::AsyncReadExt;

#[nuclei::main]
async fn main() -> io::Result<()> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("data");
    path.push("quark-gluon-plasma");

    let fo = File::open(&path).unwrap();
    let mut file = Handle::<File>::new(fo).unwrap();
    let mut buffer = String::new();
    file.read_to_string(&mut buffer).await?;

    println!("Content: {}", buffer);
    println!("Length of file is {}", buffer.len());

    Ok(())
}
