//! The rows on nuclei's fs: owned writes (`write_owned`, no copy) for the buffered
//! rows, and the copying `AsyncRead`/`AsyncWrite` path for O_DIRECT, whose pool
//! buffers are page-aligned: `write_owned` and `read_owned` take a `Vec<u8>`, whose
//! memory carries no block alignment.

use super::{
    APPEND, Ctx, LOG_MAX, Meter, Offsets, Outcome, Row, Run, SyncKind, WRITERS, Window, cpu_job,
    random,
};
use futures_lite::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use nuclei::fs::{File, OpenOptions};
use std::io::{self, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

pub fn run(row: Row, ctx: &Ctx) -> io::Result<Run> {
    let rt = nuclei::runtime::Builder::new().worker_threads(1).build()?;
    let ctx = ctx.clone();
    rt.block_on(rt.spawn(async move { body(row, &ctx).await.map(Run::Done) }))
}

async fn body(row: Row, ctx: &Ctx) -> io::Result<Outcome> {
    match row {
        Row::Sync { kind, size } => sync_row(ctx, kind, size).await,
        Row::DirectWrite { size } => direct_write(ctx, size).await,
        Row::DirectRead { size } => direct_read(ctx, size).await,
        Row::Append { kind, group } => append(ctx, kind, group).await,
        Row::WritersOne => writers(ctx, false).await,
        Row::WritersMany => writers(ctx, true).await,
        Row::BusyPool { jobs } => busy(ctx, jobs).await,
    }
}

async fn sync(file: &File, kind: SyncKind) -> io::Result<()> {
    match kind {
        SyncKind::Fsync => file.sync_all().await,
        SyncKind::Fdatasync => file.sync_data().await,
    }
}

async fn write(file: &File, buf: Vec<u8>) -> io::Result<Vec<u8>> {
    let (r, buf) = file.write_owned(buf).await;
    r.map(|_| buf)
}

async fn sync_row(ctx: &Ctx, kind: SyncKind, size: usize) -> io::Result<Outcome> {
    let mut file = OpenOptions::new()
        .write(true)
        .open(ctx.path("data"))
        .await?;
    let (mut buf, slots) = (random(size), ctx.file_bytes / size as u64);
    let (mut m, mut at) = (Meter::default(), 0);
    let w = Window::begin(ctx);
    while !w.over() {
        if at == slots {
            file.seek(SeekFrom::Start(0)).await?;
            at = 0;
        }
        let t = Instant::now();
        buf = write(&file, buf).await?;
        sync(&file, kind).await?;
        m.record(t, size);
        at += 1;
    }
    Ok(w.finish(m))
}

async fn direct_write(ctx: &Ctx, size: usize) -> io::Result<Outcome> {
    let mut file = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_DIRECT)
        .open(ctx.path("data"))
        .await?;
    let (buf, mut offs, mut m) = (random(size), Offsets::new(ctx, size), Meter::default());
    let w = Window::begin(ctx);
    while !w.over() {
        let t = Instant::now();
        file.seek(SeekFrom::Start(offs.pick())).await?;
        file.write_all(&buf).await?;
        file.flush().await?;
        m.record(t, size);
    }
    Ok(w.finish(m))
}

async fn direct_read(ctx: &Ctx, size: usize) -> io::Result<Outcome> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(ctx.path("data"))
        .await?;
    let (mut buf, mut offs, mut m) = (vec![0u8; size], Offsets::new(ctx, size), Meter::default());
    let w = Window::begin(ctx);
    while !w.over() {
        let t = Instant::now();
        file.seek(SeekFrom::Start(offs.pick())).await?;
        file.read_exact(&mut buf).await?;
        m.record(t, size);
    }
    Ok(w.finish(m))
}

async fn append(ctx: &Ctx, kind: SyncKind, group: usize) -> io::Result<Outcome> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(ctx.path("log"))
        .await?;
    let (mut buf, mut m, mut written) = (random(APPEND), Meter::default(), 0u64);
    let w = Window::begin(ctx);
    while !w.over() && written < LOG_MAX {
        let t = Instant::now();
        for _ in 0..group {
            buf = write(&file, buf).await?;
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
            nuclei::spawn(async move {
                let (mut buf, mut m, mut at) = (random(APPEND), Meter::default(), 0);
                while Instant::now() < end {
                    if at == slots {
                        f.seek(SeekFrom::Start(base)).await?;
                        at = 0;
                    }
                    let t = Instant::now();
                    buf = write(&f, buf).await?;
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
        all.merge(t.await?);
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
            nuclei::spawn(async move {
                while !stop.load(Ordering::Relaxed) {
                    std::hint::black_box(nuclei::spawn_blocking(cpu_job).await);
                    done.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();
    let out = sync_row(ctx, SyncKind::Fdatasync, APPEND).await;
    stop.store(true, Ordering::Relaxed);
    for d in drivers {
        d.await;
    }
    let mut out = out?;
    out.jobs = done.load(Ordering::Relaxed);
    out.note = Some("perf counts include the CPU jobs".into());
    Ok(out)
}
