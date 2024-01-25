use futures::AsyncReadExt;
use nuclei::*;
use std::fs::File;
use std::path::PathBuf;

#[nuclei::test]
async fn read_file() -> std::io::Result<()> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("testdata");
    path.push("quark-gluon-plasma");

    let fo = File::open(&path).unwrap();
    let mut file = Handle::<File>::new(fo).unwrap();
    let mut buffer = String::new();
    let _ = file.read_to_string(&mut buffer).await;

    assert_eq!(11587, buffer.len());

    Ok(())
}
