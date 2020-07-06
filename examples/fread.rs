use nuclei::*;
use std::{thread, io};
use std::time::Duration;
use std::fs::File;
use futures::AsyncRead;
use futures_util::io::AsyncReadExt;


fn main() -> io::Result<()> {
    let x = run(async {
        let fo = File::open("test").unwrap();
        // Handle<File> implements AsyncRead.
        let mut file = Handle::<File>::new(fo).unwrap();
        // let file = read(fo).await;
        let mut buffer = String::new();
        file.read_to_string(&mut buffer).await;
        buffer
    });

    dbg!(x);

    Ok(())
}