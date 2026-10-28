//! The IO utilities: the extension traits' reads and writes, `ReadBuf`,
//! copying both ways, the buffered wrappers, the in-memory pipes, `split`,
//! and the readers and writers with nothing behind them.

use std::io::{ErrorKind, SeekFrom};
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::task::{Context, Poll};

use nuclei::io::{
    self, AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader, BufStream,
    BufWriter, ReadBuf,
};
use nuclei::runtime::Runtime;
use proptest::prelude::*;

/// A seekable reader over bytes in memory.
struct MemCursor {
    data: Vec<u8>,
    pos: u64,
}

impl MemCursor {
    fn new(data: &[u8]) -> Self {
        Self {
            data: data.to_vec(),
            pos: 0,
        }
    }
}

impl nuclei::io::AsyncRead for MemCursor {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let at = (self.pos as usize).min(self.data.len());
        let n = buf.len().min(self.data.len() - at);
        buf[..n].copy_from_slice(&self.data[at..at + n]);
        self.pos += n as u64;
        Poll::Ready(Ok(n))
    }
}

impl nuclei::io::AsyncSeek for MemCursor {
    fn poll_seek(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        pos: SeekFrom,
    ) -> Poll<std::io::Result<u64>> {
        let next = match pos {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::Current(n) => self.pos as i64 + n,
            SeekFrom::End(n) => self.data.len() as i64 + n,
        };
        if next < 0 {
            return Poll::Ready(Err(ErrorKind::InvalidInput.into()));
        }
        self.pos = next as u64;
        Poll::Ready(Ok(self.pos))
    }
}

fn runtime(workers: usize) -> Runtime {
    Runtime::builder()
        .worker_threads(workers)
        .pin_workers(false)
        .build()
        .expect("the runtime starts")
}

#[test]
fn read_exact_fills_or_reports_an_early_end() {
    nuclei::block_on(async {
        let mut reader: &[u8] = b"abcdef";
        let mut four = [0u8; 4];
        assert_eq!(reader.read_exact(&mut four).await.unwrap(), 4);
        assert_eq!(&four, b"abcd");
        let mut more = [0u8; 4];
        let err = reader.read_exact(&mut more).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
    });
}

#[test]
fn read_to_end_and_to_string_append_and_reject_bad_utf8() {
    nuclei::block_on(async {
        let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let mut reader: &[u8] = &big;
        let mut out = b"head".to_vec();
        assert_eq!(reader.read_to_end(&mut out).await.unwrap(), big.len());
        assert_eq!(&out[..4], b"head");
        assert_eq!(&out[4..], &big[..]);

        let mut text = String::from("x");
        let mut reader: &[u8] = "héllo".as_bytes();
        assert_eq!(reader.read_to_string(&mut text).await.unwrap(), 6);
        assert_eq!(text, "xhéllo");

        let mut bad: &[u8] = &[0x66, 0xff, 0x67];
        let mut kept = String::from("kept");
        let err = bad.read_to_string(&mut kept).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert_eq!(kept, "kept", "nothing appended on bad UTF-8");
    });
}

#[test]
fn take_and_chain_bound_and_join_readers() {
    nuclei::block_on(async {
        let mut taken = (&b"abcdefgh"[..]).take(3);
        let mut out = Vec::new();
        taken.read_to_end(&mut out).await.unwrap();
        assert_eq!(out, b"abc");
        assert_eq!(taken.limit(), 0);

        let mut chained = (&b"ab"[..]).chain(&b"cd"[..]);
        let mut out = String::new();
        chained.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "abcd");
    });
}

proptest! {
    #[test]
    fn typed_numbers_round_trip_both_endians(
        a in any::<u16>(), b in any::<i32>(), c in any::<u64>(), d in any::<i128>(), e in any::<f64>(),
    ) {
        nuclei::block_on(async {
            let mut buf = Vec::new();
            buf.write_u16(a).await.unwrap();
            buf.write_i32_le(b).await.unwrap();
            buf.write_u64(c).await.unwrap();
            buf.write_i128_le(d).await.unwrap();
            buf.write_f64(e).await.unwrap();
            buf.write_u8(7).await.unwrap();
            assert_eq!(buf.len(), 2 + 4 + 8 + 16 + 8 + 1);
            assert_eq!(&buf[..2], &a.to_be_bytes());
            let mut reader: &[u8] = &buf;
            assert_eq!(reader.read_u16().await.unwrap(), a);
            assert_eq!(reader.read_i32_le().await.unwrap(), b);
            assert_eq!(reader.read_u64().await.unwrap(), c);
            assert_eq!(reader.read_i128_le().await.unwrap(), d);
            assert_eq!(reader.read_f64().await.unwrap().to_bits(), e.to_bits());
            assert_eq!(reader.read_u8().await.unwrap(), 7);
            assert_eq!(reader.read_u8().await.unwrap_err().kind(), ErrorKind::UnexpectedEof);
        });
    }
}

#[test]
fn read_buf_fills_uninitialized_memory() {
    nuclei::block_on(async {
        let mut mem = [MaybeUninit::<u8>::uninit(); 8];
        let mut buf = ReadBuf::uninit(&mut mem);
        let mut reader: &[u8] = b"0123456789";
        assert_eq!(reader.read_buf(&mut buf).await.unwrap(), 8);
        assert_eq!(buf.filled(), b"01234567");
        assert_eq!(
            reader.read_buf(&mut buf).await.unwrap(),
            0,
            "a full buffer reads nothing"
        );
    });
}

#[test]
fn buffered_reads_split_lines_and_segments() {
    nuclei::block_on(async {
        let mut reader = BufReader::with_capacity(4, &b"one\r\ntwo\nthree"[..]);
        let mut line = String::new();
        assert_eq!(reader.read_line(&mut line).await.unwrap(), 5);
        assert_eq!(line, "one\r\n");
        let mut until = Vec::new();
        reader.read_until(b'\n', &mut until).await.unwrap();
        assert_eq!(until, b"two\n");
        let rest = reader.fill_buf().await.unwrap().to_vec();
        // Four-byte fills: "one\r", "\ntwo", "\nthr"; what is left of the last.
        assert_eq!(rest, b"thr");
        reader.consume(2);
        let mut tail = String::new();
        reader.read_to_string(&mut tail).await.unwrap();
        assert_eq!(tail, "ree");

        let mut lines = BufReader::new(&b"a\r\nb\n\nc"[..]).lines();
        let mut got = Vec::new();
        while let Some(line) = lines.next_line().await.unwrap() {
            got.push(line);
        }
        assert_eq!(got, ["a", "b", "", "c"]);

        let mut segments = BufReader::new(&b"x,yy,,z"[..]).split(b',');
        let mut got = Vec::new();
        while let Some(segment) = segments.next_segment().await.unwrap() {
            got.push(String::from_utf8(segment).unwrap());
        }
        assert_eq!(got, ["x", "yy", "", "z"]);
    });
}

#[test]
fn a_buffered_reader_seeks_from_the_next_byte_it_gives() {
    nuclei::block_on(async {
        let mut reader = BufReader::with_capacity(8, MemCursor::new(b"0123456789abcdef"));
        let mut two = [0u8; 2];
        reader.read_exact(&mut two).await.unwrap();
        assert_eq!(&two, b"01");
        // The buffer read ahead to 8, yet a relative seek counts from 2.
        assert_eq!(reader.seek(SeekFrom::Current(3)).await.unwrap(), 5);
        reader.read_exact(&mut two).await.unwrap();
        assert_eq!(&two, b"56");
        assert_eq!(reader.stream_position().await.unwrap(), 7);
        assert_eq!(reader.rewind().await.unwrap(), 0);
        reader.read_exact(&mut two).await.unwrap();
        assert_eq!(&two, b"01");
    });
}

#[test]
fn a_buffered_writer_gathers_until_flushed() {
    nuclei::block_on(async {
        let mut writer = BufWriter::with_capacity(16, Vec::new());
        writer.write_all(b"small").await.unwrap();
        assert!(writer.get_ref().is_empty(), "held in the buffer");
        assert_eq!(writer.buffer(), b"small");
        writer.write_all(&[b'x'; 20]).await.unwrap();
        assert_eq!(
            writer.get_ref().len(),
            25,
            "a write past the buffer flushed it"
        );
        writer.write_all(b"tail").await.unwrap();
        writer.flush().await.unwrap();
        assert_eq!(&writer.get_ref()[25..], b"tail");
    });
}

#[test]
fn the_duplex_pipe_delivers_in_order_waits_when_full_and_ends() {
    let rt = runtime(2);
    rt.block_on(async {
        let (mut a, mut b) = io::duplex(64);
        let data: Vec<u8> = (0..1_000_000u32).map(|i| (i * 7 % 256) as u8).collect();
        let expected = data.clone();
        // A writer on one worker, a reader on another: 1 MB through 64 bytes.
        let writer = nuclei::spawn(async move {
            a.write_all(&data).await.unwrap();
            a.shutdown().await.unwrap();
            a
        });
        let reader = nuclei::spawn(async move {
            let mut got = Vec::new();
            b.read_to_end(&mut got).await.unwrap();
            (b, got)
        });
        let a = writer.await;
        let (b, got) = reader.await;
        assert_eq!(got.len(), expected.len());
        assert!(got == expected, "bytes lost or out of order");
        drop(a);
        let mut b = b;
        let err = b.write_all(b"after").await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::BrokenPipe, "the peer is gone");
    });
}

#[test]
fn simplex_ends_on_the_writers_drop_and_fails_on_the_readers() {
    nuclei::block_on(async {
        let (mut reader, mut writer) = io::simplex(8);
        writer.write_all(b"12345").await.unwrap();
        drop(writer);
        let mut got = Vec::new();
        reader.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"12345");

        let (reader, mut writer) = io::simplex(8);
        drop(reader);
        assert_eq!(
            writer.write(b"x").await.unwrap_err().kind(),
            ErrorKind::BrokenPipe
        );
    });
}

#[test]
fn copy_bidirectional_relays_both_ways_and_passes_the_end_on() {
    let rt = runtime(2);
    rt.block_on(async {
        let (mut client, mut near) = io::duplex(32);
        let (mut far, mut server) = io::duplex(32);
        let relay = nuclei::spawn(async move { io::copy_bidirectional(&mut near, &mut far).await });
        client.write_all(b"request").await.unwrap();
        let mut got = [0u8; 7];
        server.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"request");
        server.write_all(b"response!").await.unwrap();
        let mut got = [0u8; 9];
        client.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"response!");
        // The client ends its side: the server reads the end, then ends its own.
        client.shutdown().await.unwrap();
        let mut rest = Vec::new();
        server.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty());
        server.shutdown().await.unwrap();
        let mut rest = Vec::new();
        client.read_to_end(&mut rest).await.unwrap();
        assert_eq!(relay.await.unwrap(), (7, 9));
    });
}

#[test]
fn copy_and_copy_buf_count_what_they_move() {
    nuclei::block_on(async {
        let data = vec![9u8; 100_000];
        let mut reader: &[u8] = &data;
        let mut out = Vec::new();
        assert_eq!(io::copy(&mut reader, &mut out).await.unwrap(), 100_000);
        assert_eq!(out, data);
        let mut buffered = BufReader::new(&data[..]);
        let mut out = Vec::new();
        assert_eq!(
            io::copy_buf(&mut buffered, &mut out).await.unwrap(),
            100_000
        );
        assert_eq!(out, data);
    });
}

#[test]
fn a_split_stream_reads_and_writes_from_two_tasks_and_comes_back_whole() {
    let rt = runtime(2);
    rt.block_on(async {
        let (left, mut right) = io::duplex(16);
        let (mut read_half, mut write_half) = io::split(left);
        let writer = nuclei::spawn(async move {
            write_half.write_all(&[1u8; 10_000]).await.unwrap();
            write_half
        });
        let echo = nuclei::spawn(async move {
            let mut buf = vec![0u8; 10_000];
            right.read_exact(&mut buf).await.unwrap();
            right.write_all(&buf).await.unwrap();
            right
        });
        let mut back = vec![0u8; 10_000];
        read_half.read_exact(&mut back).await.unwrap();
        assert!(back.iter().all(|&b| b == 1));
        let write_half = writer.await;
        let _right = echo.await;
        assert!(read_half.is_pair_of(&write_half));
        let _whole = read_half.unsplit(write_half);
    });
}

#[test]
fn empty_repeat_and_sink_have_nothing_behind_them() {
    nuclei::block_on(async {
        let mut out = Vec::new();
        assert_eq!(io::empty().read_to_end(&mut out).await.unwrap(), 0);
        let mut five = [0u8; 5];
        io::repeat(b'z').read_exact(&mut five).await.unwrap();
        assert_eq!(&five, b"zzzzz");
        let mut sink = io::sink();
        sink.write_all(&[0u8; 1000]).await.unwrap();
        sink.flush().await.unwrap();
        let copied = io::copy(&mut io::repeat(1).take(4096), &mut io::sink())
            .await
            .unwrap();
        assert_eq!(copied, 4096);
    });
}

#[test]
fn a_buffered_stream_reads_and_writes_through_its_buffers() {
    let rt = runtime(1);
    rt.block_on(async {
        let (a, mut b) = io::duplex(64);
        let mut stream = BufStream::new(a);
        stream.write_all(b"line one\n").await.unwrap();
        stream.flush().await.unwrap();
        let mut got = [0u8; 9];
        b.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"line one\n");
        b.write_all(b"reply\n").await.unwrap();
        let mut line = String::new();
        stream.read_line(&mut line).await.unwrap();
        assert_eq!(line, "reply\n");
    });
}
