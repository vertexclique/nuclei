//! A UDP echo server whose receives go through io_uring's provided buffers.
//!
//! The shard's receive buffers are made large enough for any datagram, so
//! on io_uring the socket keeps one multishot receive armed: the kernel
//! picks a buffer of the shard's group for each datagram as it comes (buffer
//! select), and no receive holds a buffer of its own while it waits. On a
//! kernel without io_uring, or on another system, the same code runs on the
//! platform's poller.
//!
//! Run it, then send it datagrams: `nc -u 127.0.0.1 7001`.

use std::io;
use std::net::UdpSocket;

use nuclei::config::{IoUringConfiguration, NucleiConfig};
use nuclei::{Handle, Proactor};

fn main() -> io::Result<()> {
    // Buffers that hold any datagram whole: smaller ones keep UDP receives
    // single, so that none is ever cut at a buffer's end.
    let iouring = IoUringConfiguration {
        recv_buffers: 64,
        recv_buffer_size: 66 * 1024,
        ..IoUringConfiguration::default()
    };
    Proactor::with_config(NucleiConfig {
        iouring,
        backend: None,
    });
    println!("IO runs on {:?}", Proactor::backend());

    nuclei::block_on(async {
        let socket = Handle::<UdpSocket>::bind("127.0.0.1:7001")?;
        println!("Echoing datagrams on {}", socket.local_addr()?);
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let (n, peer) = socket.recv_from(&mut buf).await?;
            socket.send_to(&buf[..n], peer).await?;
        }
    })
}
