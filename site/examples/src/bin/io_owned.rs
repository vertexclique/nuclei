use std::sync::Arc;

use nuclei::io::{AlignedBuf, IoBuf};
use nuclei::net::{TcpListener, TcpStream};

// A message in a shared allocation, sent to many peers without a copy.
#[derive(Default)]
struct Shared(Arc<[u8]>);

// SAFETY: the bytes live in the Arc's allocation, which stays in place when
// the value moves, and every one of them is initialized.
unsafe impl IoBuf for Shared {
    fn stable_ptr(&self) -> *const u8 {
        self.0.as_ptr()
    }

    fn bytes_init(&self) -> usize {
        self.0.len()
    }

    fn bytes_total(&self) -> usize {
        self.0.len()
    }
}

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let client = TcpStream::connect(listener.local_addr()?).await?;
    let (server, _) = listener.accept().await?;

    // A buffer of your own type goes in, and the same value comes back.
    let news: Arc<[u8]> = Arc::from(&b"shared, not copied"[..]);
    let (sent, back) = server.write_owned(Shared(Arc::clone(&news))).await;
    assert_eq!(sent?, news.len());
    assert!(Arc::ptr_eq(&back.0, &news));

    // An AlignedBuf fills like a vector: its room, up to its capacity.
    let room = AlignedBuf::new(64, 64)?;
    let (n, got) = client.read_owned(room).await;
    assert_eq!(&got[..n?], &news[..]);
    assert_eq!(got.as_ptr() as usize % 64, 0);
    Ok(())
}
