//! A file's vectored calls on the ring: one `IORING_OP_WRITEV` or
//! `IORING_OP_READV` of every buffer, with the blocking pool held.

use super::durable::{one_blocking_thread_kept, ring_calls};
use super::owned::hold_the_pool;
use super::*;
use crate::syscore::linux::iouring::ring::sys::Opcode;

#[test]
fn a_vectored_write_and_read_are_one_entry_of_the_ring_each() {
    let rt = one_blocking_thread_kept();
    let tmp = Temp::new("vectored-ring");
    let path = tmp.file("data");
    rt.block_on(async {
        within(spawn(async move {
            if ring_calls().is_none() {
                eprintln!("skipping: the shard runs no ring (NUCLEI_BACKEND or no io_uring)");
                return;
            }
            let file = Handle::new(read_write(&path)).unwrap();
            let _ = ring_calls();
            let (hold, busy) = hold_the_pool();
            let bufs = vec![
                b"one,".to_vec(),
                Vec::new(),
                b"two,".to_vec(),
                b"three".to_vec(),
            ];
            let (n, _) = within(file.write_vectored_owned(bufs)).await;
            assert_eq!(n.unwrap(), 13);
            let (_, entries) = ring_calls().unwrap();
            let writes: Vec<_> = entries
                .iter()
                .filter(|e| e.0 == Opcode::Writev as u8)
                .collect();
            assert_eq!(writes.len(), 1, "one write of every buffer: {entries:?}");
            assert_eq!(
                (writes[0].1, writes[0].2),
                (3, 0),
                "three buffers with bytes, at the start"
            );

            let reader = Handle::new(OpenOptions::new().read(true).open(&path).unwrap()).unwrap();
            let rooms = [
                Vec::with_capacity(4),
                Vec::with_capacity(4),
                Vec::with_capacity(16),
            ];
            let (n, back) = within(reader.read_vectored_owned(rooms)).await;
            assert_eq!(n.unwrap(), 13);
            assert_eq!(back.concat(), b"one,two,three");
            assert_eq!((&back[0][..], &back[1][..]), (&b"one,"[..], &b"two,"[..]));
            let (_, entries) = ring_calls().unwrap();
            let reads: Vec<_> = entries
                .iter()
                .filter(|e| e.0 == Opcode::Readv as u8)
                .collect();
            assert_eq!(reads.len(), 1, "one read of every room: {entries:?}");
            assert_eq!(reads[0].1, 3);
            hold.send(()).unwrap();
            within(busy).await;
        }))
        .await;
    });
}

#[test]
fn a_vectored_write_the_kernel_takes_part_of_goes_on_from_the_byte_it_stopped_at() {
    let rt = runtime();
    let tmp = Temp::new("vectored-big");
    let path = tmp.file("data");
    rt.block_on(async {
        let file = Handle::new(read_write(&path)).unwrap();
        let bufs: Vec<Vec<u8>> = (0..9).map(|i| pattern((i + 1) * 300_000)).collect();
        let want = bufs.concat();
        let (n, _) = within(file.write_vectored_owned(bufs)).await;
        assert_eq!(n.unwrap(), want.len());
        within(file.sync_data()).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), want);
    });
}
