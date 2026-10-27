use std::net::{Ipv4Addr, SocketAddr};

use nuclei::net::{self, TcpListener};

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    // A name is looked up on the blocking pool, so no shard waits on the resolver.
    let found: Vec<SocketAddr> = net::lookup_host("localhost:8080").await?.collect();
    assert!(found.iter().all(|addr| addr.port() == 8080));

    // Every call that takes an address takes any of these forms.
    let a = TcpListener::bind("127.0.0.1:0").await?;
    let b = TcpListener::bind(("127.0.0.1", 0)).await?;
    let c = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let d = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
    for listener in [a, b, c, d] {
        assert!(listener.local_addr()?.ip().is_loopback());
    }
    Ok(())
}
