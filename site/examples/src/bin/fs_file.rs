use std::io::SeekFrom;

use nuclei::fs::{File, OpenOptions};
use nuclei::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    let mut file = File::create("log.txt").await?;
    file.write_all(b"one\n").await?;
    // flush hands the bytes to the system; sync_all puts them on the device.
    file.flush().await?;
    file.sync_all().await?;
    drop(file);

    // Open for appending: every write lands at the end.
    let mut log = OpenOptions::new().append(true).open("log.txt").await?;
    log.write_all(b"two\n").await?;
    log.flush().await?;
    drop(log);

    let mut file = File::open("log.txt").await?;
    let mut text = String::new();
    file.read_to_string(&mut text).await?;
    assert_eq!(text, "one\ntwo\n");

    // A file has a cursor: seek moves it.
    file.seek(SeekFrom::Start(4)).await?;
    let mut second = String::new();
    file.read_to_string(&mut second).await?;
    assert_eq!(second, "two\n");
    nuclei::fs::remove_file("log.txt").await
}
