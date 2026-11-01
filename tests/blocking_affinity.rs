//! A blocking-pool thread runs on the mask the runtime was built with, not on
//! the core of the pinned worker that started it.

#![cfg(target_os = "linux")]

fn mask() -> Vec<usize> {
    // SAFETY: a zeroed set of our own, filled by the call.
    unsafe {
        let mut set: libc::cpu_set_t = core::mem::zeroed();
        assert_eq!(
            libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut set),
            0
        );
        (0..libc::CPU_SETSIZE as usize)
            .filter(|&cpu| libc::CPU_ISSET(cpu, &set))
            .collect()
    }
}

#[test]
fn pool_threads_started_by_pinned_workers_keep_the_runtime_mask() {
    let before = mask();
    if before.len() < 2 {
        eprintln!(
            "skipped: needs at least two allowed CPUs, have {}",
            before.len()
        );
        return;
    }
    let rt = nuclei::Builder::new()
        .worker_threads(2)
        .pin_workers(true)
        .build()
        .unwrap();
    let seen = rt.block_on(async {
        nuclei::spawn(async {
            let mut jobs = Vec::new();
            for _ in 0..4 {
                jobs.push(nuclei::spawn_blocking(|| {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    mask()
                }));
            }
            let mut seen = Vec::new();
            for job in jobs {
                seen.push(job.await);
            }
            seen
        })
        .await
    });
    for got in seen {
        assert_eq!(got, before, "a pool thread kept its creator's pinned mask");
    }
}
