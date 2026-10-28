mod io_support;

use std::fs::File;

use io_support::{read_to_end, sample_file};
use nuclei::Handle;

#[nuclei::test]
async fn read_file() {
    let (sample, bytes) = sample_file();
    let mut file = Handle::<File>::new(File::open(&sample.0).unwrap()).unwrap();
    let got = read_to_end(&mut file).await.unwrap();
    assert_eq!(got.len(), 11587);
    assert_eq!(got, bytes);
}
