use nuclei::*;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use futures::io::SeekFrom;
use futures::*;

const DARK_MATTER_TEXT: &'static str = "\
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
to detect.[1]\
\
";

#[test]
fn write_file() {
    // Approximately ~75,9 MB
    let dark_matter = vec![DARK_MATTER_TEXT; 100_000].join("\n");

    let x = drive(async {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("testdata");
        path.push("dark-matter");

        let fo = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mut file = Handle::<File>::new(fo).unwrap();
        file.write_all(dark_matter.as_bytes()).await.unwrap();

        let mut buf = vec![];
        assert!(file.seek(SeekFrom::Start(0)).await.is_ok());
        assert_eq!(file.read_to_end(&mut buf).await.unwrap(), dark_matter.len());
        assert_eq!(&buf[0..dark_matter.len()], dark_matter.as_bytes());
        buf
    });

    assert_eq!(75899999, x.len());
}
