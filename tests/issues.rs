//! Reproductions of reported issues, so each stays fixed.

use std::cell::Cell;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::rc::Rc;
use std::time::{Duration, Instant};

use nuclei::Handle;
use nuclei::sync::mpsc;

/// Issue 43: a runtime that runs everything on one thread, and a sleep.
#[test]
fn issue_43_a_runtime_on_one_thread_runs_its_tasks_there_and_sleeps() {
    let rt = nuclei::Builder::new().worker_threads(0).build().unwrap();
    let me = std::thread::current().id();
    let (slept, ran_on) = rt.block_on(async {
        let start = Instant::now();
        let ran_on = nuclei::spawn(async {
            nuclei::time::sleep(Duration::from_millis(20)).await;
            std::thread::current().id()
        })
        .await;
        (start.elapsed(), ran_on)
    });
    assert_eq!(ran_on, me, "a task ran on another thread");
    assert!(slept >= Duration::from_millis(20), "the sleep ended early");
}

/// Issue 45: a task of the thread's own shard that serves each connection
/// inside a nested `block_on` holds up none of the thread's other tasks: a
/// ticker and its receiver keep going while the connection waits for its
/// client.
#[test]
fn issue_45_a_nested_block_on_in_a_local_task_holds_up_no_other_local_task() {
    let listener = Handle::<TcpListener>::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    // A client that keeps the connection quiet for a while, then talks.
    let client = std::thread::spawn(move || {
        let mut conn = TcpStream::connect(addr).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        conn.write_all(b"ping").unwrap();
        let mut back = [0u8; 4];
        conn.read_exact(&mut back).unwrap();
        back
    });
    let received = Rc::new(Cell::new(0u32));
    let during = nuclei::block_on({
        let received = Rc::clone(&received);
        async move {
            let (tx, mut rx) = mpsc::channel::<u32>(10);
            nuclei::spawn_local(async move {
                for n in 0.. {
                    if tx.send(n).await.is_err() {
                        break;
                    }
                    nuclei::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .detach();
            let counter = Rc::clone(&received);
            nuclei::spawn_local(async move {
                while rx.recv().await.is_some() {
                    counter.set(counter.get() + 1);
                }
            })
            .detach();
            // The server of the report: the connection served inside a
            // nested `block_on` on its own task.
            let server = nuclei::spawn_local(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let before = received.get();
                nuclei::block_on(nuclei::spawn_local(async move {
                    let mut buf = [0u8; 4];
                    let mut at = 0;
                    while at < buf.len() {
                        at += stream.recv(&mut buf[at..]).await.unwrap();
                    }
                    stream.send(&buf).await.unwrap();
                }));
                received.get() - before
            });
            server.await
        }
    });
    assert_eq!(&client.join().unwrap(), b"ping");
    // About 30 ticks in the 300 ms the connection waited.
    assert!(
        during >= 10,
        "only {during} messages arrived while the connection was served"
    );
}
