use nuclei::fs::{self, File};
use nuclei::io::AsyncWriteExt;

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    fs::write("config.toml", "version = 1\n").await?;

    // Replace it so a crash leaves the old contents or the new, never a mix.
    let mut tmp = File::create("config.toml.tmp").await?;
    tmp.write_all(b"version = 2\n").await?;
    tmp.sync_all().await?; // the bytes, on the device
    drop(tmp);
    fs::rename("config.toml.tmp", "config.toml").await?;
    fs::sync_dir(".").await?; // the new name, on the device

    assert_eq!(fs::read_to_string("config.toml").await?, "version = 2\n");
    fs::remove_file("config.toml").await
}
