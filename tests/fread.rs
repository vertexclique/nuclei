mod io_support;

use std::fs::File;

use io_support::{read_to_end, testdata};
use nuclei::Handle;

#[nuclei::test]
async fn read_file() {
    let path = testdata("quark-gluon-plasma");
    let mut file = Handle::<File>::new(File::open(&path).unwrap()).unwrap();
    let got = read_to_end(&mut file).await.unwrap();
    assert_eq!(got.len(), 11587);
    assert_eq!(got, std::fs::read(&path).unwrap());
}
