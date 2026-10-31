//! Many positional reads in one call: each result at its own place, for
//! counts around the window, after the writes before the call, and on the
//! shard's ring together.

#[cfg(target_os = "linux")]
use super::durable::one_blocking_thread_kept;
#[cfg(ring)]
use super::durable::ring_calls;
#[cfg(target_os = "linux")]
use super::owned::hold_the_pool;
use super::*;
use crate::fs::file::MANY_AT_ONCE;
use crate::io::AlignedBuf;

fn file_of(tmp: &Temp, bytes: &[u8]) -> Handle<File> {
    let path = tmp.file("data");
    std::fs::write(&path, bytes).unwrap();
    Handle::new(
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn every_read_comes_back_at_its_own_place_for_counts_around_the_window() {
    let rt = runtime();
    let tmp = Temp::new("many-counts");
    let data = pattern(1 << 20);
    rt.block_on(async {
        let file = file_of(&tmp, &data);
        for count in [
            0,
            1,
            MANY_AT_ONCE - 1,
            MANY_AT_ONCE,
            MANY_AT_ONCE + 1,
            3 * MANY_AT_ONCE + 7,
        ] {
            let reads: Vec<(Vec<u8>, u64)> = (0..count)
                .map(|i| {
                    (
                        Vec::with_capacity(97 + i % 13),
                        (i * 4099 % (1 << 20)) as u64,
                    )
                })
                .collect();
            let wants: Vec<_> = reads.iter().map(|(b, at)| (b.capacity(), *at)).collect();
            let ends = within(file.read_many_at(reads)).await;
            assert_eq!(ends.len(), count);
            for ((result, buf), (cap, at)) in ends.into_iter().zip(wants) {
                let at = at as usize;
                let n = result.unwrap();
                assert_eq!(n, cap.min(data.len() - at), "count {count} at {at}");
                assert_eq!(buf, &data[at..at + n]);
            }
        }
    });
}

#[test]
fn reads_at_and_past_the_end_and_full_buffers_answer_as_read_at() {
    let rt = runtime();
    let tmp = Temp::new("many-edges");
    rt.block_on(async {
        let file = file_of(&tmp, b"0123456789");
        let mut held = Vec::with_capacity(6);
        held.extend_from_slice(b">>");
        let reads = vec![
            (held, 8),
            (Vec::with_capacity(4), 10),
            (Vec::with_capacity(4), 1 << 40),
            (b"full".to_vec(), 0),
            (Vec::with_capacity(3), 0),
            (Vec::with_capacity(3), 0),
        ];
        let ends = within(file.read_many_at(reads)).await;
        let got: Vec<(usize, Vec<u8>)> = ends.into_iter().map(|(r, b)| (r.unwrap(), b)).collect();
        assert_eq!(
            got,
            [
                (2, b">>89".to_vec()),
                (0, Vec::new()),
                (0, Vec::new()),
                (0, b"full".to_vec()),
                (3, b"012".to_vec()),
                (3, b"012".to_vec()),
            ]
        );
    });
}

#[test]
fn the_reads_see_the_writes_accepted_before_the_call() {
    let rt = runtime();
    let tmp = Temp::new("many-order");
    rt.block_on(async {
        let mut file = file_of(&tmp, &[0; 4096]);
        within(write_all(&mut file, b"written before"))
            .await
            .unwrap();
        let (n, _) = within(file.write_at_owned(b"AT".to_vec(), 100)).await;
        n.unwrap();
        let ends = within(file.read_many_at(vec![
            (Vec::with_capacity(14), 0),
            (Vec::with_capacity(2), 100),
        ]))
        .await;
        assert_eq!(ends[0].1, b"written before");
        assert_eq!(ends[1].1, b"AT");
    });
}

#[test]
fn aligned_and_callers_buffers_read_as_vecs_do() {
    let rt = runtime();
    let tmp = Temp::new("many-aligned");
    let data = pattern(16 << 10);
    rt.block_on(async {
        let file = file_of(&tmp, &data);
        let reads: Vec<(AlignedBuf, u64)> = (0..4)
            .map(|i| (AlignedBuf::new(4096, 4096).unwrap(), i * 4096))
            .collect();
        let ptrs: Vec<_> = reads.iter().map(|(b, _)| b.as_ptr()).collect();
        let ends = within(file.read_many_at(reads)).await;
        for (i, (result, buf)) in ends.into_iter().enumerate() {
            assert_eq!(result.unwrap(), 4096);
            assert_eq!(buf.as_ptr(), ptrs[i], "read into the caller's memory");
            assert_eq!(&buf[..], &data[i * 4096..(i + 1) * 4096]);
        }
    });
}

#[test]
fn a_dropped_call_leaves_the_file_to_the_next_one() {
    let rt = runtime();
    let tmp = Temp::new("many-drop");
    let data = pattern(1 << 20);
    rt.block_on(async {
        let file = file_of(&tmp, &data);
        let reads: Vec<_> = (0..200)
            .map(|i| (Vec::with_capacity(4096), i * 4096))
            .collect();
        let mut call = Box::pin(file.read_many_at(reads));
        let _ = poll_fn(|cx| {
            let _ = call.as_mut().poll(cx);
            core::task::Poll::Ready(())
        })
        .await;
        drop(call);
        let ends = within(file.read_many_at(vec![(Vec::with_capacity(8), 8)])).await;
        assert_eq!(ends[0].1, &data[8..16]);
    });
}

#[cfg(unix)]
#[test]
fn a_stream_answers_every_read_with_its_error() {
    let rt = runtime();
    rt.block_on(async {
        let (r, _w) = std::io::pipe().unwrap();
        let file = Handle::new(File::from(std::os::fd::OwnedFd::from(r))).unwrap();
        let ends =
            within(file.read_many_at(vec![(Vec::with_capacity(4), 0), (Vec::with_capacity(4), 4)]))
                .await;
        assert_eq!(ends.len(), 2);
        for (result, buf) in ends {
            assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::ESPIPE));
            assert_eq!(buf.capacity(), 4);
        }
    });
}

/// With the one thread of the blocking pool held, the reads still end:
/// they are entries of the ring.
#[cfg(ring)]
#[test]
fn the_reads_are_entries_of_the_ring_out_together() {
    use crate::syscore::linux::iouring::ring::sys::Opcode;
    let rt = one_blocking_thread_kept();
    let tmp = Temp::new("many-ring");
    let data = pattern(1 << 20);
    let path = tmp.file("data");
    std::fs::write(&path, &data).unwrap();
    rt.block_on(async {
        within(spawn(async move {
            if ring_calls().is_none() {
                eprintln!("skipping: the shard runs no ring (NUCLEI_BACKEND or no io_uring)");
                return;
            }
            let file = Handle::new(OpenOptions::new().read(true).open(&path).unwrap()).unwrap();
            let _ = ring_calls();
            let (hold, busy) = hold_the_pool();
            let count = MANY_AT_ONCE + 5;
            let reads: Vec<_> = (0..count)
                .map(|i| (Vec::with_capacity(512), i as u64 * 512))
                .collect();
            let ends = within(file.read_many_at(reads)).await;
            let (_, entries) = ring_calls().unwrap();
            let reads: Vec<_> = entries
                .iter()
                .filter(|e| e.0 == Opcode::Read as u8)
                .collect();
            assert_eq!(reads.len(), count, "every read was the ring's");
            for (i, (result, buf)) in ends.into_iter().enumerate() {
                assert_eq!(result.unwrap(), 512);
                assert_eq!(buf, &data[i * 512..(i + 1) * 512]);
            }
            hold.send(()).unwrap();
            within(busy).await;
        }))
        .await;
    });
}
