use nuclei::fs::{self, DirBuilder};

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    fs::create_dir_all("site/assets/img").await?;
    DirBuilder::new()
        .recursive(true)
        .create("site/pages")
        .await?;
    fs::write("site/index.html", "<h1>hi</h1>").await?;

    // read_dir is a stream of entries, read a page at a time.
    let mut names = Vec::new();
    let mut entries = fs::read_dir("site").await?;
    while let Some(entry) = entries.next_entry().await? {
        let mut name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type().await?.is_dir() {
            name.push('/');
        }
        names.push(name);
    }
    names.sort();
    assert_eq!(names, ["assets/", "index.html", "pages/"]);

    fs::remove_dir_all("site").await
}
