//! The rows on glommio: BufferedFile for the buffered rows, DmaFile (O_DIRECT, its
//! own aligned buffers) for the direct ones, on an executor pinned to the first CPU
//! the process may use, with an unbound blocking pool. BufferedFile::write_at takes
//! a `Vec<u8>` and keeps it, so every write hands it a fresh copy.

use super::{
    APPEND, Ctx, LOG_MAX, Meter, Offsets, Outcome, Row, Run, SyncKind, WRITERS, Window, cpu_job,
    random,
};
use glommio::io::{BufferedFile, OpenOptions};
use glommio::{LocalExecutorBuilder, Placement, PoolPlacement};
use std::cell::Cell;
use std::io;
use std::rc::Rc;
use std::time::Instant;

const POOL_THREADS: usize = 8;
const NO_FSYNC: &str = "glommio 0.9 has no fsync: DmaFile::fdatasync and BufferedFile::fdatasync are its only sync calls";

pub fn run(row: Row, ctx: &Ctx) -> io::Result<Run> {
    if let Row::Sync {
        kind: SyncKind::Fsync,
        ..
    }
    | Row::Append {
        kind: SyncKind::Fsync,
        ..
    } = row
    {
        return Ok(Run::Skip(NO_FSYNC.into()));
    }
    let cpu = echo::allowed_cpus().first().copied().unwrap_or(0);
    let ctx = ctx.clone();
    LocalExecutorBuilder::new(Placement::Fixed(cpu))
        .blocking_thread_pool_placement(PoolPlacement::Unbound(POOL_THREADS))
        .spawn(move || async move {
            Ok(Run::Done(match row {
                Row::Sync { size, .. } => sync_row(&ctx, size).await?,
                Row::DirectWrite { size } => direct_write(&ctx, size).await?,
                Row::DirectRead { size } => direct_read(&ctx, size).await?,
                Row::Append { group, .. } => append(&ctx, group).await?,
                Row::WritersOne => writers(&ctx, false).await?,
                Row::WritersMany => writers(&ctx, true).await?,
                Row::BusyPool { jobs } => busy(&ctx, jobs).await?,
            }))
        })
        .map_err(io::Error::from)?
        .join()
        .map_err(io::Error::from)?
}

async fn write_all_at(file: &BufferedFile, data: &[u8], at: u64) -> io::Result<()> {
    let mut done = 0;
    while done < data.len() {
        match file
            .write_at(data[done..].to_vec(), at + done as u64)
            .await?
        {
            0 => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "write_at wrote nothing",
                ));
            }
            n => done += n,
        }
    }
    Ok(())
}

async fn sync_row(ctx: &Ctx, size: usize) -> io::Result<Outcome> {
    let file = OpenOptions::new()
        .write(true)
        .buffered_open(ctx.path("data"))
        .await?;
    let (buf, slots) = (random(size), ctx.file_bytes / size as u64);
    let (mut m, mut at) = (Meter::default(), 0);
    let w = Window::begin(ctx);
    while !w.over() {
        if at == slots {
            at = 0;
        }
        let t = Instant::now();
        write_all_at(&file, &buf, at * size as u64).await?;
        file.fdatasync().await?;
        m.record(t, size);
        at += 1;
    }
    Ok(w.finish(m))
}

async fn direct_write(ctx: &Ctx, size: usize) -> io::Result<Outcome> {
    let file = OpenOptions::new()
        .write(true)
        .dma_open(ctx.path("data"))
        .await?;
    let mut dma = file.alloc_dma_buffer(size);
    dma.as_bytes_mut().copy_from_slice(&random(size));
    let (buf, mut offs, mut m) = (Rc::new(dma), Offsets::new(ctx, size), Meter::default());
    let w = Window::begin(ctx);
    while !w.over() {
        let t = Instant::now();
        let n = file.write_rc_at(buf.clone(), offs.pick()).await?;
        if n != size {
            return Err(io::Error::other(format!(
                "direct write of {size} bytes wrote {n}"
            )));
        }
        m.record(t, size);
    }
    Ok(w.finish(m))
}

async fn direct_read(ctx: &Ctx, size: usize) -> io::Result<Outcome> {
    let file = OpenOptions::new()
        .read(true)
        .dma_open(ctx.path("data"))
        .await?;
    let (mut offs, mut m) = (Offsets::new(ctx, size), Meter::default());
    let w = Window::begin(ctx);
    while !w.over() {
        let t = Instant::now();
        let got = file.read_at_aligned(offs.pick(), size).await?;
        if got.len() != size {
            return Err(io::Error::other(format!(
                "direct read of {size} bytes got {}",
                got.len()
            )));
        }
        m.record(t, size);
    }
    Ok(w.finish(m))
}

async fn append(ctx: &Ctx, group: usize) -> io::Result<Outcome> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .buffered_open(ctx.path("log"))
        .await?;
    let (buf, mut m, mut end) = (random(APPEND), Meter::default(), 0u64);
    let w = Window::begin(ctx);
    while !w.over() && end < LOG_MAX {
        let t = Instant::now();
        for _ in 0..group {
            write_all_at(&file, &buf, end).await?;
            end += APPEND as u64;
        }
        file.fdatasync().await?;
        m.record(t, group * APPEND);
    }
    Ok(w.finish(m))
}

async fn writers(ctx: &Ctx, many: bool) -> io::Result<Outcome> {
    let region = ctx.region();
    let shared = if many {
        None
    } else {
        Some(Rc::new(
            OpenOptions::new()
                .write(true)
                .buffered_open(ctx.path("data"))
                .await?,
        ))
    };
    let mut files = Vec::with_capacity(WRITERS);
    for i in 0..WRITERS {
        files.push(match &shared {
            Some(f) => (f.clone(), i as u64 * region),
            None => (
                Rc::new(
                    OpenOptions::new()
                        .write(true)
                        .buffered_open(ctx.many(i))
                        .await?,
                ),
                0,
            ),
        });
    }
    let slots = region / APPEND as u64;
    let w = Window::begin(ctx);
    let end = w.end;
    let tasks: Vec<_> = files
        .into_iter()
        .map(|(f, base)| {
            glommio::spawn_local(async move {
                let (buf, mut m, mut at) = (random(APPEND), Meter::default(), 0);
                while Instant::now() < end {
                    if at == slots {
                        at = 0;
                    }
                    let t = Instant::now();
                    write_all_at(&f, &buf, base + at * APPEND as u64).await?;
                    f.fdatasync().await?;
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
    let (done, stop) = (Rc::new(Cell::new(0u64)), Rc::new(Cell::new(false)));
    let drivers: Vec<_> = (0..jobs)
        .map(|_| {
            let (done, stop) = (done.clone(), stop.clone());
            glommio::spawn_local(async move {
                while !stop.get() {
                    std::hint::black_box(glommio::executor().spawn_blocking(cpu_job).await);
                    done.set(done.get() + 1);
                }
            })
        })
        .collect();
    let out = sync_row(ctx, APPEND).await;
    stop.set(true);
    for d in drivers {
        d.await;
    }
    let mut out = out?;
    out.jobs = done.get();
    out.note = Some(format!(
        "unbound blocking pool of {POOL_THREADS}; perf counts include the CPU jobs"
    ));
    Ok(out)
}
