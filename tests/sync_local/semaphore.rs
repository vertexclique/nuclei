use nuclei::sync::TryAcquireError;
use nuclei::sync::local::Semaphore;

use crate::sync_support::Probe;
use crate::util::{now, pending, poll, ready};

#[test]
fn permits_are_taken_and_returned() {
    let sem = Semaphore::new(3);
    let two = sem.try_acquire_many(2).unwrap();
    assert_eq!(two.num_permits(), 2);
    assert_eq!(sem.available_permits(), 1);
    drop(two);
    assert_eq!(sem.available_permits(), 3);
}

#[test]
fn try_acquire_reports_no_permits_then_closed() {
    let sem = Semaphore::new(1);
    let held = sem.try_acquire().unwrap();
    assert_eq!(sem.try_acquire().unwrap_err(), TryAcquireError::NoPermits);
    drop(held);
    sem.close();
    assert!(sem.is_closed());
    assert_eq!(sem.try_acquire().unwrap_err(), TryAcquireError::Closed);
}

#[test]
#[should_panic(expected = "MAX_PERMITS")]
fn new_over_the_maximum_panics() {
    let _ = Semaphore::new(Semaphore::MAX_PERMITS + 1);
}

#[test]
#[should_panic(expected = "MAX_PERMITS")]
fn add_permits_over_the_maximum_panics() {
    Semaphore::new(1).add_permits(Semaphore::MAX_PERMITS);
}

#[test]
fn an_uncontended_acquire_completes_at_once() {
    let sem = Semaphore::new(2);
    let permit = now(sem.acquire()).unwrap();
    assert_eq!(permit.num_permits(), 1);
    assert_eq!(now(sem.acquire_many(1)).unwrap().num_permits(), 1);
}

#[test]
fn a_big_request_at_the_front_is_not_bypassed() {
    let sem = Semaphore::new(2);
    let first = sem.try_acquire().unwrap();
    let second = sem.try_acquire().unwrap();
    let (mut big_probe, mut small_probe) = (Probe::new(), Probe::new());
    let mut big = sem.acquire_many(2);
    let mut small = sem.acquire();
    pending(&mut big_probe, &mut big);
    pending(&mut small_probe, &mut small);

    drop(first);
    assert_eq!(sem.available_permits(), 1);
    assert_eq!(sem.try_acquire().unwrap_err(), TryAcquireError::NoPermits);
    pending(&mut small_probe, &mut small);
    assert_eq!(small_probe.wakes(), 0);
    assert_eq!(big_probe.wakes(), 0);

    drop(second);
    assert_eq!(big_probe.wakes(), 1);
    assert_eq!(small_probe.wakes(), 0);
    let big_permit = ready(&mut big_probe, &mut big).unwrap();
    assert_eq!(big_permit.num_permits(), 2);
    pending(&mut small_probe, &mut small);

    drop(big_permit);
    assert_eq!(small_probe.wakes(), 1);
    drop(ready(&mut small_probe, &mut small).unwrap());
}

#[test]
fn waiters_are_served_in_arrival_order() {
    let sem = Semaphore::new(1);
    let held = sem.try_acquire().unwrap();
    let mut probes: Vec<Probe> = (0..3).map(|_| Probe::new()).collect();
    let mut waits: Vec<_> = (0..3).map(|_| sem.acquire()).collect();
    for (probe, wait) in probes.iter_mut().zip(&mut waits) {
        pending(probe, wait);
    }
    drop(held);
    for i in 0..3 {
        assert_eq!(probes[i].wakes(), 1, "waiter {i} is woken in its turn");
        assert!(probes[i + 1..].iter().all(|p| p.wakes() == 0));
        let permit = ready(&mut probes[i], &mut waits[i]).unwrap();
        drop(permit);
    }
    assert_eq!(sem.available_permits(), 1);
}

#[test]
fn add_permits_serves_a_waiter() {
    let sem = Semaphore::new(0);
    let mut probe = Probe::new();
    let mut wait = sem.acquire();
    pending(&mut probe, &mut wait);
    sem.add_permits(1);
    assert_eq!(probe.wakes(), 1);
    ready(&mut probe, &mut wait).unwrap().forget();
    assert_eq!(sem.available_permits(), 0);
}

#[test]
fn close_wakes_every_waiter_with_an_error() {
    let sem = Semaphore::new(0);
    let (mut a_probe, mut b_probe) = (Probe::new(), Probe::new());
    let (mut a, mut b) = (sem.acquire(), sem.acquire_many(3));
    pending(&mut a_probe, &mut a);
    pending(&mut b_probe, &mut b);
    sem.close();
    sem.close();
    assert_eq!((a_probe.wakes(), b_probe.wakes()), (1, 1));
    assert!(ready(&mut a_probe, &mut a).is_err());
    assert!(ready(&mut b_probe, &mut b).is_err());
    assert!(now(sem.acquire()).is_err());
    sem.add_permits(2);
    assert_eq!(sem.available_permits(), 2);
    assert!(now(sem.acquire()).is_err());
}

#[test]
fn a_grant_made_before_the_close_is_kept() {
    let sem = Semaphore::new(1);
    let held = sem.try_acquire().unwrap();
    let mut probe = Probe::new();
    let mut wait = sem.acquire();
    pending(&mut probe, &mut wait);
    drop(held);
    sem.close();
    assert_eq!(ready(&mut probe, &mut wait).unwrap().num_permits(), 1);
}

#[test]
fn forget_permits_removes_only_free_permits() {
    let sem = Semaphore::new(5);
    let held = sem.try_acquire().unwrap();
    assert_eq!(sem.forget_permits(2), 2);
    assert_eq!(sem.available_permits(), 2);
    assert_eq!(sem.forget_permits(10), 2);
    assert_eq!(sem.available_permits(), 0);
    drop(held);
    assert_eq!(sem.available_permits(), 1);
}

#[test]
fn a_forgotten_permit_is_not_returned() {
    let sem = Semaphore::new(2);
    sem.try_acquire().unwrap().forget();
    assert_eq!(sem.available_permits(), 1);
}

#[test]
fn merged_permits_return_together() {
    let sem = Semaphore::new(4);
    let mut one = sem.try_acquire().unwrap();
    let two = sem.try_acquire_many(2).unwrap();
    one.merge(two);
    assert_eq!(one.num_permits(), 3);
    assert_eq!(sem.available_permits(), 1);
    drop(one);
    assert_eq!(sem.available_permits(), 4);
}

#[test]
#[should_panic(expected = "different semaphore")]
fn merging_permits_of_two_semaphores_panics() {
    let (a, b) = (Semaphore::new(1), Semaphore::new(1));
    let mut from_a = a.try_acquire().unwrap();
    from_a.merge(b.try_acquire().unwrap());
}

#[test]
fn the_waiter_wakes_the_task_it_was_last_polled_by() {
    let sem = Semaphore::new(1);
    let held = sem.try_acquire().unwrap();
    let (mut old, mut new) = (Probe::new(), Probe::new());
    let mut wait = sem.acquire();
    pending(&mut old, &mut wait);
    pending(&mut new, &mut wait);
    drop(held);
    assert_eq!((old.wakes(), new.wakes()), (0, 1));
    drop(ready(&mut new, &mut wait).unwrap());
}

#[test]
fn a_release_wakes_its_waiter_once() {
    let sem = Semaphore::new(1);
    let held = sem.try_acquire().unwrap();
    let mut probe = Probe::new();
    let mut wait = sem.acquire();
    pending(&mut probe, &mut wait);
    drop(held);
    sem.add_permits(1);
    assert_eq!(probe.wakes(), 1);
    assert!(poll(&mut probe, &mut wait).is_ready());
}

#[test]
fn a_completed_acquire_polled_again_acquires_afresh() {
    let sem = Semaphore::new(2);
    let mut probe = Probe::new();
    let mut wait = sem.acquire();
    ready(&mut probe, &mut wait).unwrap().forget();
    ready(&mut probe, &mut wait).unwrap().forget();
    assert_eq!(sem.available_permits(), 0);
}

#[test]
fn debug_shows_the_state() {
    let sem = Semaphore::new(2);
    assert_eq!(
        format!("{sem:?}"),
        "Semaphore { available_permits: 2, closed: false }"
    );
    assert!(format!("{:?}", sem.acquire()).contains("Acquire"));
    assert!(format!("{:?}", sem.try_acquire().unwrap()).contains("SemaphorePermit"));
}
