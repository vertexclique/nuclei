//! The rows on tokio::fs, whose calls run on the blocking pool of a current-thread
//! runtime. tokio::fs::File stages every read and write in a `Vec<u8>` of its own,
//! which is not block-aligned (where the kernel then refuses O_DIRECT, or, as btrfs
//! does, quietly serves it buffered), so the O_DIRECT rows run std's positional calls
//! on `spawn_blocking`, from aligned memory: tokio's documented way to block.

use super::{
    APPEND, AlignedBuf, Ctx, LOG_MAX, Meter, Offsets, Outcome, Row, Run, SyncKind, WRITERS, Window,
    cpu_job, random,
};
use std::io::{self, SeekFrom};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

const DIRECT_NOTE: &str = "tokio::fs::File stages IO in an unaligned Vec, so O_DIRECT runs std pread/pwrite on spawn_blocking";

pub fn run(row: Row, ctx: &Ctx) -> io::Result<Run> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        Ok(Run::Done(match row {
            Row::Sync { kind, size } => sync_row(ctx, kind, size).await?,
            Row::DirectWrite { size } => direct(ctx, size, true).await?,
            Row::DirectRead { size } => direct(ctx, size, false).await?,
            Row::Append { kind, group } => append(ctx, kind, group).await?,
            Row::WritersOne => writers(ctx, false).await?,
            Row::WritersMany => writers(ctx, true).await?,
            Row::BusyPool { jobs } => busy(ctx, jobs).await?,
        }))
    })
}

async fn sync(file: &File, kind: SyncKind) -> io::Result<()> {
    match kind {
        SyncKind::Fsync => file.sync_all().await,
        SyncKind::Fdatasync => file.sync_data().await,
    }
}

async fn sync_row(ctx: &Ctx, kind: SyncKind, size: usize) -> io::Result<Outcome> {
    let mut file = OpenOptions::new()
        .write(true)
        .open(ctx.path("data"))
        .await?;
    let (buf, slots) = (random(size), ctx.file_bytes / size as u64);
    let (mut m, mut at) = (Meter::default(), 0);
    let w = Window::begin(ctx);
    while !w.over() {
        if at == slots {
            file.seek(SeekFrom::Start(0)).await?;
            at = 0;
        }
        let t = Instant::now();
        file.write_all(&buf).await?;
        sync(&file, kind).await?;
        m.record(t, size);
        at += 1;
    }
    Ok(w.finish(m))
}

async fn direct(ctx: &Ctx, size: usize, write: bool) -> io::Result<Outcome> {
    let file = Arc::new(
        std::fs::OpenOptions::new()
            .read(!write)
            .write(write)
            .custom_flags(libc::O_DIRECT)
            .open(ctx.path("data"))?,
    );
    let mut buf = Some(if write {
        AlignedBuf::random(size)
    } else {
        AlignedBuf::zeroed(size)
    });
    let (mut offs, mut m) = (Offsets::new(ctx, size), Meter::default());
    let w = Window::begin(ctx);
    while !w.over() {
        let (f, mut b, off) = (file.clone(), buf.take().expect("returned"), offs.pick());
        let t = Instant::now();
        let (r, b) = tokio::task::spawn_blocking(move || {
            let r = if write {
                f.write_all_at(b.as_slice(), off)
            } else {
                f.read_exact_at(b.as_mut_slice(), off)
            };
            (r, b)
        })
        .await?;
        r?;
        m.record(t, size);
        buf = Some(b);
    }
    let mut out = w.finish(m);
    out.note = Some(DIRECT_NOTE.into());
    Ok(out)
}

async fn append(ctx: &Ctx, kind: SyncKind, group: usize) -> io::Result<Outcome> {
    let mut file = File::create(ctx.path("log")).await?;
    let (buf, mut m, mut written) = (random(APPEND), Meter::default(), 0u64);
    let w = Window::begin(ctx);
    while !w.over() && written < LOG_MAX {
        let t = Instant::now();
        for _ in 0..group {
            file.write_all(&buf).await?;
        }
        sync(&file, kind).await?;
        m.record(t, group * APPEND);
        written += (group * APPEND) as u64;
    }
    Ok(w.finish(m))
}

async fn writers(ctx: &Ctx, many: bool) -> io::Result<Outcome> {
    let region = ctx.region();
    let mut files = Vec::with_capacity(WRITERS);
    for i in 0..WRITERS {
        let (path, base) = if many {
            (ctx.many(i), 0)
        } else {
            (ctx.path("data"), i as u64 * region)
        };
        let mut f = OpenOptions::new().write(true).open(path).await?;
        f.seek(SeekFrom::Start(base)).await?;
        files.push((f, base));
    }
    let slots = region / APPEND as u64;
    let w = Window::begin(ctx);
    let end = w.end;
    let tasks: Vec<_> = files
        .into_iter()
        .map(|(mut f, base)| {
            tokio::spawn(async move {
                let (buf, mut m, mut at) = (random(APPEND), Meter::default(), 0);
                while Instant::now() < end {
                    if at == slots {
                        f.seek(SeekFrom::Start(base)).await?;
                        at = 0;
                    }
                    let t = Instant::now();
                    f.write_all(&buf).await?;
                    f.sync_data().await?;
                    m.record(t, APPEND);
                    at += 1;
                }
                Ok::<Meter, io::Error>(m)
            })
        })
        .collect();
    let mut all = Meter::default();
    for t in tasks {
        all.merge(t.await??);
    }
    Ok(w.finish(all))
}

async fn busy(ctx: &Ctx, jobs: usize) -> io::Result<Outcome> {
    let (done, stop) = (
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicBool::new(false)),
    );
    let drivers: Vec<_> = (0..jobs)
        .map(|_| {
            let (done, stop) = (done.clone(), stop.clone());
            tokio::spawn(async move {
                while !stop.load(Ordering::Relaxed) {
                    if let Ok(v) = tokio::task::spawn_blocking(cpu_job).await {
                        std::hint::black_box(v);
                    }
                    done.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();
    let out = sync_row(ctx, SyncKind::Fdatasync, APPEND).await;
    stop.store(true, Ordering::Relaxed);
    for d in drivers {
        let _ = d.await;
    }
    let mut out = out?;
    out.jobs = done.load(Ordering::Relaxed);
    out.note = Some("perf counts include the CPU jobs".into());
    Ok(out)
}
