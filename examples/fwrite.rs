use nuclei::*;
use std::io;
use std::time::Duration;
use std::fs::{File, OpenOptions};
use std::path::PathBuf;

use futures::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, AsyncSeek, AsyncSeekExt};
use futures::io::SeekFrom;

const DARK_MATTER: &'static str = "\
Dark matter is a form of matter thought to account for approximately \
85% of the matter in the universe and about a quarter of its total \
mass–energy density or about 2.241×10−27 kg/m3. Its presence is implied \
in a variety of astrophysical observations, including gravitational effects \
that cannot be explained by accepted theories of gravity unless more matter \
is present than can be seen. For this reason, most experts think that dark \
matter is abundant in the universe and that it has had a strong influence \
on its structure and evolution. Dark matter is called dark because it does \
not appear to interact with the electromagnetic field, which means it doesn't \
absorb, reflect or emit electromagnetic radiation, and is therefore difficult \
to detect.[1]";

fn main() -> io::Result<()> {
    let x = drive(async {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("data");
        path.push("dark-matter");

        let fo = File::open(&path).unwrap();
        let mut file = Handle::<File>::new(fo).unwrap();
        let len = file.write(DARK_MATTER.as_bytes()).await.unwrap();

        assert_eq!(len, DARK_MATTER.len());

        let mut buf = vec![];
        assert!(file.seek(SeekFrom::Start(0)).await.is_ok());
        assert_eq!(file.read_to_end(&mut buf).await.unwrap(), DARK_MATTER.len());
        assert_eq!(&buf[0..DARK_MATTER.len()], DARK_MATTER.as_bytes());
        buf
    });

    println!("Length of file is {}", x.len());

    Ok(())
}