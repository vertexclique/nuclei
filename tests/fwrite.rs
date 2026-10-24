mod io_support;

use std::fs::{File, OpenOptions};
use std::io::SeekFrom;

use io_support::{TempFile, flush, read_to_end, seek, write_all};
use nuclei::Handle;

const DARK_MATTER_TEXT: &str = "\
Dark matter is a form of matter thought to account for approximately \
85% of the matter in the universe and about a quarter of its total \
mass-energy density or about 2.241x10^-27 kg/m3. Its presence is implied \
in a variety of astrophysical observations, including gravitational effects \
that cannot be explained by accepted theories of gravity unless more matter \
is present than can be seen. For this reason, most experts think that dark \
matter is abundant in the universe and that it has had a strong influence \
on its structure and evolution. Dark matter is called dark because it does \
not appear to interact with the electromagnetic field, which means it doesn't \
absorb, reflect or emit electromagnetic radiation, and is therefore difficult \
to detect.[1]\
";

#[nuclei::test]
async fn write_file() {
    // About 75 MB.
    let dark_matter = vec![DARK_MATTER_TEXT; 100_000].join("\n");
    let tmp = TempFile::new("dark-matter");
    let fo = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&tmp.0)
        .unwrap();
    let mut file = Handle::<File>::new(fo).unwrap();
    write_all(&mut file, dark_matter.as_bytes()).await.unwrap();
    assert_eq!(seek(&mut file, SeekFrom::Start(0)).await.unwrap(), 0);
    let buf = read_to_end(&mut file).await.unwrap();
    assert_eq!(buf.len(), dark_matter.len());
    assert!(buf == dark_matter.as_bytes());
    flush(&mut file).await.unwrap();
    assert_eq!(
        std::fs::metadata(&tmp.0).unwrap().len(),
        dark_matter.len() as u64
    );
}
