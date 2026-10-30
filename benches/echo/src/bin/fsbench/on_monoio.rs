//! The rows on monoio's io_uring fs: positional owned-buffer calls, with an aligned
//! buffer type for O_DIRECT, and its `DefaultThreadPool` for the busy-pool row.

use super::{
    APPEND, AlignedBuf, Ctx, LOG_MAX, Meter, Offsets, Outcome, Row, Run, SyncKind, WRITERS, Window,
    cpu_job, random,
};
use monoio::buf::{IoBuf, IoBufMut};
use monoio::fs::{File, OpenOptions};
use std::cell::Cell;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::rc::Rc;
use std::time::Instant;

const POOL_THREADS: usize = 8;

// SAFETY: the pointer stays valid while the buffer is owned by the operation, and
// `init` never exceeds the allocation.
unsafe impl IoBuf for AlignedBuf {
    fn read_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    fn bytes_init(&self) -> usize {
        self.init()
    }
}

// SAFETY: as for `IoBuf`; `bytes_total` is the allocation's length.
unsafe impl IoBufMut for AlignedBuf {
    fn write_ptr(&mut self) -> *mut u8 {
        self.as_mut_ptr()
    }

    fn bytes_total(&mut self) -> usize {
        self.capacity()
    }

    unsafe fn set_init(&mut self, pos: usize) {
        // SAFETY: the kernel initialized the first `pos` bytes.
        unsafe { AlignedBuf::set_init(self, pos) }
    }
}

pub fn run(row: Row, ctx: &Ctx) -> io::Result<Run> {
    let builder = monoio::RuntimeBuilder::<monoio::IoUringDriver>::new().enable_all();
    let mut rt = match row {
        Row::BusyPool { .. } => builder
            .attach_thread_pool(Box::new(monoio::blocking::DefaultThreadPool::new(
                POOL_THREADS,
            )))
            .build()?,
        _ => builder.build()?,
    };
    rt.block_on(async {
        Ok(Run::Done(match row {
            Row::Sync { kind, size } => sync_row(ctx, kind, size).await?,
            Row::DirectWrite { size } => direct_write(ctx, size).await?,
            Row::DirectRead { size } => direct_read(ctx, size).await?,
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

async fn write_at<T>(file: &File, buf: T, at: u64) -> io::Result<T>
where
    T: IoBuf,
{
    let (r, buf) = file.write_all_at(buf, at).await;
    r.map(|()| buf)
}

async fn sync_row(ctx: &Ctx, kind: SyncKind, size: usize) -> io::Result<Outcome> {
    let file = OpenOptions::new()
        .write(true)
        .open(ctx.path("data"))
        .await?;
    let (mut buf, slots) = (random(size), ctx.file_bytes / size as u64);
    let (mut m, mut at) = (Meter::default(), 0);
    let w = Window::begin(ctx);
    while !w.over() {
        if at == slots {
            at = 0;
        }
        let t = Instant::now();
        buf = write_at(&file, buf, at * size as u64).await?;
        sync(&file, kind).await?;
        m.record(t, size);
        at += 1;
    }
    Ok(w.finish(m))
}

async fn direct_write(ctx: &Ctx, size: usize) -> io::Result<Outcome> {
    let file = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_DIRECT)
        .open(ctx.path("data"))
        .await?;
    let (mut buf, mut offs, mut m) = (
        AlignedBuf::random(size),
        Offsets::new(ctx, size),
        Meter::default(),
    );
    let w = Window::begin(ctx);
    while !w.over() {
        let t = Instant::now();
        buf = write_at(&file, buf, offs.pick()).await?;
        m.record(t, size);
    }
    Ok(w.finish(m))
}

async fn direct_read(ctx: &Ctx, size: usize) -> io::Result<Outcome> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(ctx.path("data"))
        .await?;
    let (mut buf, mut offs, mut m) = (
        AlignedBuf::zeroed(size),
        Offsets::new(ctx, size),
        Meter::default(),
    );
    let w = Window::begin(ctx);
    while !w.over() {
        let t = Instant::now();
        let (r, b) = file.read_exact_at(buf, offs.pick()).await;
        buf = b;
        r?;
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
    let (mut buf, mut m, mut end) = (random(APPEND), Meter::default(), 0u64);
    let w = Window::begin(ctx);
    while !w.over() && end < LOG_MAX {
        let t = Instant::now();
        for _ in 0..group {
            buf = write_at(&file, buf, end).await?;
            end += APPEND as u64;
        }
        sync(&file, kind).await?;
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
                .open(ctx.path("data"))
                .await?,
        ))
    };
    let mut files = Vec::with_capacity(WRITERS);
    for i in 0..WRITERS {
        files.push(match &shared {
            Some(f) => (f.clone(), i as u64 * region),
            None => (
                Rc::new(OpenOptions::new().write(true).open(ctx.many(i)).await?),
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
            monoio::spawn(async move {
                let (mut buf, mut m, mut at) = (random(APPEND), Meter::default(), 0);
                while Instant::now() < end {
                    if at == slots {
                        at = 0;
                    }
                    let t = Instant::now();
                    buf = write_at(&f, buf, base + at * APPEND as u64).await?;
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
    let (done, stop) = (Rc::new(Cell::new(0u64)), Rc::new(Cell::new(false)));
    let drivers: Vec<_> = (0..jobs)
        .map(|_| {
            let (done, stop) = (done.clone(), stop.clone());
            monoio::spawn(async move {
                while !stop.get() {
                    if let Ok(v) = monoio::spawn_blocking(cpu_job).await {
                        std::hint::black_box(v);
                    }
                    done.set(done.get() + 1);
                }
            })
        })
        .collect();
    let out = sync_row(ctx, SyncKind::Fdatasync, APPEND).await;
    stop.set(true);
    for d in drivers {
        d.await;
    }
    let mut out = out?;
    out.jobs = done.get();
    out.note = Some(format!(
        "DefaultThreadPool of {POOL_THREADS}; perf counts include the CPU jobs"
    ));
    Ok(out)
}
