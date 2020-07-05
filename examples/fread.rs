use proactor::*;
use std::{thread, io};
use std::time::Duration;
use std::fs::File;
use futures::AsyncRead;
use futures_util::io::AsyncReadExt;
use std::path::PathBuf;



fn main() -> io::Result<()> {
    run(async {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("test");
        let fo = File::open(&path).unwrap();
        // Handle<File> implements AsyncRead.
        let mut file = Handle::<File>::new(fo).unwrap();
        // let file = read(fo).await;
        let mut buffer = String::new();
        dbg!("buffer");
        // file.read_to_string(&mut buffer).await;
        file.read_to_string(&mut buffer).await;
        dbg!("read complete");
        dbg!(buffer);
    });

    Ok(())
}