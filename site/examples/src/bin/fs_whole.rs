use nuclei::fs;

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    // Whole files in one call each.
    fs::write("notes.txt", "first line\n").await?;
    let text = fs::read_to_string("notes.txt").await?;
    assert_eq!(text, "first line\n");

    fs::copy("notes.txt", "copy.txt").await?;
    fs::rename("copy.txt", "moved.txt").await?;
    assert!(fs::try_exists("moved.txt").await?);
    assert_eq!(fs::read("moved.txt").await?, b"first line\n");

    let meta = fs::metadata("moved.txt").await?;
    assert!(meta.is_file());
    assert_eq!(meta.len(), 11);

    fs::remove_file("moved.txt").await?;
    fs::remove_file("notes.txt").await
}
