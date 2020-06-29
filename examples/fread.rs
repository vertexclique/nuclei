use proactor::*;
use std::{thread, io};
use std::time::Duration;
use std::fs::File;
use futures::AsyncRead;

fn main() -> io::Result<()> {
    let x = run(async {
        let fo = File::open("test").unwrap();
        // Handle<File> implements AsyncRead.
        let file = Handle::<File>::new(fo).unwrap();
        // let file = read(fo).await;
        file
    });

    // dbg!(x);

    Ok(())
}