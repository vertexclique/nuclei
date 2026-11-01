//! Files: borrowed and owned calls, at an offset, appended, read to the end.

use super::*;

/// A file row: `run` writes and reads back a file through nuclei at every size, given the
/// directory, the size and the bytes, and answers what the file must hold.
macro_rules! file_rows {
    ($($name:ident: $run:ident;)*) => {$(
        #[test]
        fn $name() {
            let dir = temp_dir();
            for &n in sizes() {
                let what = format!("{} at {n}", stringify!($name));
                let path = dir.join(stringify!($name));
                let _ = std::fs::remove_file(&path);
                let expected = within(&what, $run(&path, n, pattern(n)));
                assert!(std::fs::read(&path).unwrap() == expected, "{what}: the file holds other bytes");
            }
            std::fs::remove_dir_all(dir).unwrap();
        }
    )*};
}

/// A file's head before the bytes an appending row writes after it.
const HEAD: &[u8] = b"head";

file_rows! {
    file_borrowed: borrowed_file;
    file_owned: owned_file;
    file_at_offset: at_offset_file;
    file_append: append_file;
    file_read_to_end: read_to_end_file;
}

async fn borrowed_file(path: &std::path::Path, n: usize, data: &'static [u8]) -> Vec<u8> {
    let mut file = File::create(path).await.unwrap();
    for chunk in data.chunks(n.max(1)) {
        file.write_all(chunk).await.unwrap();
    }
    file.flush().await.unwrap();
    drop(file);
    let mut file = File::open(path).await.unwrap();
    let got = read_exact_file(&mut file, n, data.len()).await;
    assert!(got == data, "not the bytes written");
    data.to_vec()
}

async fn read_exact_file(file: &mut File, n: usize, total: usize) -> Vec<u8> {
    let mut got = vec![0; total];
    for chunk in got.chunks_mut(n.max(1)) {
        file.read_exact(chunk).await.unwrap();
    }
    assert_eq!(
        file.read(&mut [0; 1]).await.unwrap(),
        0,
        "more past the end"
    );
    got
}

async fn owned_file(path: &std::path::Path, n: usize, data: &'static [u8]) -> Vec<u8> {
    let file = File::create(path).await.unwrap();
    let (wrote, back) = file.write_owned(data.to_vec()).await;
    assert_eq!(wrote.unwrap(), n);
    assert!(back == data, "the buffer came back changed");
    drop(file);
    let file = File::open(path).await.unwrap();
    let mut got = Vec::new();
    loop {
        let (came, buf) = file.read_owned(Vec::with_capacity(n.max(1))).await;
        if came.unwrap() == 0 {
            break;
        }
        got.extend_from_slice(&buf);
    }
    assert!(got == data, "not the bytes written");
    data.to_vec()
}

async fn at_offset_file(path: &std::path::Path, n: usize, data: &'static [u8]) -> Vec<u8> {
    let head = vec![0xA5; OFFSET as usize];
    std::fs::write(path, &head).unwrap();
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .await
        .unwrap();
    assert_eq!(
        file.seek(io::SeekFrom::Start(OFFSET)).await.unwrap(),
        OFFSET
    );
    for chunk in data.chunks(n.max(1)) {
        file.write_all(chunk).await.unwrap();
    }
    file.flush().await.unwrap();
    assert_eq!(
        file.seek(io::SeekFrom::Start(OFFSET)).await.unwrap(),
        OFFSET
    );
    let got = read_exact_file(&mut file, n, data.len()).await;
    assert!(got == data, "not the bytes written");
    [head, data.to_vec()].concat()
}

async fn append_file(path: &std::path::Path, n: usize, data: &'static [u8]) -> Vec<u8> {
    std::fs::write(path, HEAD).unwrap();
    let mut file = OpenOptions::new().append(true).open(path).await.unwrap();
    for chunk in data.chunks(n.max(1)) {
        file.write_all(chunk).await.unwrap();
    }
    file.flush().await.unwrap();
    [HEAD, data].concat()
}

async fn read_to_end_file(path: &std::path::Path, _: usize, data: &'static [u8]) -> Vec<u8> {
    std::fs::write(path, data).unwrap();
    let mut file = File::open(path).await.unwrap();
    let mut got = Vec::new();
    assert_eq!(file.read_to_end(&mut got).await.unwrap(), data.len());
    assert!(got == data, "not the bytes in the file");
    data.to_vec()
}
