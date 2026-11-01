//! The epoll driver's pass over a write edge of a side only its own thread holds: a write the
//! socket refuses, on the owner's thread or on another, still gets the edge it waits for, and
//! the bytes arrive whole and in order.

use super::*;

/// Writes `data` through `a` on the worker that homes it while this thread reads it from `b`:
/// the socket fills, and the write waits for its edges.
fn owner_write_past_a_full_socket(rt: &Runtime, a: &Owned<Stream>, b: &Owned<Stream>, data: &[u8]) {
    let writer = {
        let (src, data) = (Arc::clone(&a.src), data.to_vec());
        rt.spawn(async move {
            write_all(&src, &data).await.unwrap();
            flush(&src).await.unwrap();
        })
    };
    let got = rt.block_on(async {
        let mut got = vec![0u8; data.len()];
        within(read_exact(&b.src, &mut got)).await.unwrap();
        got
    });
    rt.block_on(writer);
    assert!(got == data, "the bytes were damaged or misordered");
}

#[test]
fn an_owners_write_the_socket_refuses_still_gets_its_edge() {
    let rt = runtime(None);
    let (a, b) = pair();
    let (src_a, src_b) = (Arc::clone(&a.src), Arc::clone(&b.src));
    // Homes both on the worker, so its driver owns their sides.
    rt.block_on(rt.spawn(async move {
        write_all(&src_a, b"x").await.unwrap();
        let mut one = [0u8; 1];
        assert_eq!(within(read(&src_b, &mut one)).await.unwrap(), 1);
    }));
    owner_write_past_a_full_socket(&rt, &a, &b, &pattern(4 << 20));
}

#[test]
fn a_foreign_write_after_the_owners_idle_release_still_gets_its_edge() {
    let rt = runtime(None);
    let (a, b) = pair();
    let (src_a, src_b) = (Arc::clone(&a.src), Arc::clone(&b.src));
    rt.block_on(rt.spawn(async move {
        write_all(&src_a, b"x").await.unwrap();
        let mut one = [0u8; 1];
        assert_eq!(within(read(&src_b, &mut one)).await.unwrap(), 1);
    }));
    // The owner's last release left the write side idle; a thread that is not the owner now
    // writes past a full socket, and its edges must not be passed over.
    let data = pattern(4 << 20);
    let handle = rt.handle();
    let writer = {
        let (src, data) = (Arc::clone(&a.src), data.clone());
        std::thread::spawn(move || {
            handle.block_on(async {
                write_all(&src, &data).await.unwrap();
                flush(&src).await.unwrap();
            })
        })
    };
    let got = rt.block_on(async {
        let mut got = vec![0u8; data.len()];
        within(read_exact(&b.src, &mut got)).await.unwrap();
        got
    });
    writer.join().unwrap();
    assert!(got == data, "the bytes were damaged or misordered");
}
