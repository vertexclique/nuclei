use nuclei::*;
use std::fs::File;

use std::path::PathBuf;

use futures_util::io::AsyncReadExt;

#[test]
fn read_file() {
    let x = drive(async {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("testdata");
        path.push("quark-gluon-plasma");

        let fo = File::open(&path).unwrap();
        let mut file = Handle::<File>::new(fo).unwrap();
        let mut buffer = String::new();
        let _ = file.read_to_string(&mut buffer).await;
        buffer
    });

    assert_eq!(11587, x.len());
}
