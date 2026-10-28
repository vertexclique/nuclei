use nuclei::Proactor;
use nuclei::config::{IoUringConfiguration, NucleiConfig};

fn main() {
    // Once, before any IO: the io_uring settings of every shard.
    Proactor::with_config(NucleiConfig {
        iouring: IoUringConfiguration {
            queue_len: 4096,
            recv_buffers: 512,
            recv_buffer_size: 16 * 1024,
            ..IoUringConfiguration::default()
        },
        backend: None, // io_uring where this process may have it, else the poller
    });
    nuclei::block_on(async {
        println!("IO runs on {:?}", Proactor::backend());
    });
}
