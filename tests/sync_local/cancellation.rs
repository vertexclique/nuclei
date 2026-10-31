use nuclei::sync::local::{Mutex, Semaphore, mpsc, oneshot};
use nuclei::sync::mpsc::error::{SendError, TryRecvError};

use crate::sync_support::{Drops, Probe, Task};
use crate::util::{now, pending, ready};

#[test]
fn a_dropped_waiting_acquire_leaves_the_line() {
    let sem = Semaphore::new(1);
    let held = sem.try_acquire().unwrap();
    let (mut a_probe, mut b_probe) = (Probe::new(), Probe::new());
    let (mut a, mut b) = (sem.acquire(), sem.acquire());
    pending(&mut a_probe, &mut a);
    pending(&mut b_probe, &mut b);
    drop(a);
    drop(held);
    assert_eq!((a_probe.wakes(), b_probe.wakes()), (0, 1));
    drop(ready(&mut b_probe, &mut b).unwrap());
    assert_eq!(sem.available_permits(), 1);
}

#[test]
fn dropping_the_front_waiter_serves_the_next_that_now_fits() {
    let sem = Semaphore::new(2);
    let (first, second) = (sem.try_acquire().unwrap(), sem.try_acquire().unwrap());
    let (mut big_probe, mut small_probe) = (Probe::new(), Probe::new());
    let (mut big, mut small) = (sem.acquire_many(2), sem.acquire());
    pending(&mut big_probe, &mut big);
    pending(&mut small_probe, &mut small);
    drop(first);
    assert_eq!(small_probe.wakes(), 0);
    drop(big);
    assert_eq!(small_probe.wakes(), 1);
    drop(ready(&mut small_probe, &mut small).unwrap());
    drop(second);
    assert_eq!(sem.available_permits(), 2);
}

#[test]
fn a_granted_acquire_dropped_unpolled_passes_its_permits_on() {
    let sem = Semaphore::new(1);
    let held = sem.try_acquire().unwrap();
    let (mut a_probe, mut b_probe) = (Probe::new(), Probe::new());
    let (mut a, mut b) = (sem.acquire(), sem.acquire());
    pending(&mut a_probe, &mut a);
    pending(&mut b_probe, &mut b);
    drop(held);
    assert_eq!((a_probe.wakes(), b_probe.wakes()), (1, 0));
    drop(a);
    assert_eq!(b_probe.wakes(), 1);
    drop(ready(&mut b_probe, &mut b).unwrap());
    assert_eq!(sem.available_permits(), 1);
}

#[test]
fn an_acquire_dropped_before_it_was_polled_is_harmless() {
    let sem = Semaphore::new(1);
    drop(sem.acquire());
    drop(sem.acquire_many(1));
    assert_eq!(sem.available_permits(), 1);
    assert!(sem.try_acquire().is_ok());
}

#[test]
fn a_dropped_pending_lock_leaves_the_line() {
    let mutex = Mutex::new(());
    let guard = mutex.try_lock().unwrap();
    let (mut a_probe, mut b_probe) = (Probe::new(), Probe::new());
    let (mut a, mut b) = (mutex.lock(), mutex.lock());
    pending(&mut a_probe, &mut a);
    pending(&mut b_probe, &mut b);
    drop(a);
    drop(guard);
    assert_eq!((a_probe.wakes(), b_probe.wakes()), (0, 1));
    drop(ready(&mut b_probe, &mut b));
    assert!(mutex.try_lock().is_ok());
}

#[test]
fn a_lock_handed_over_and_dropped_unpolled_passes_to_the_next() {
    let mutex = Mutex::new(());
    let guard = mutex.try_lock().unwrap();
    let (mut a_probe, mut b_probe) = (Probe::new(), Probe::new());
    let (mut a, mut b) = (mutex.lock(), mutex.lock());
    pending(&mut a_probe, &mut a);
    pending(&mut b_probe, &mut b);
    drop(guard);
    assert_eq!((a_probe.wakes(), b_probe.wakes()), (1, 0));
    drop(a);
    assert_eq!(b_probe.wakes(), 1);
    drop(ready(&mut b_probe, &mut b));
    assert!(mutex.try_lock().is_ok());
}

#[test]
fn a_dropped_pending_send_loses_only_its_own_value() {
    let drops = Drops::new();
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(drops.value()).unwrap();
    let mut send = Task::new(tx.send(drops.value()));
    send.pending();
    drop(send);
    assert_eq!(drops.count(), 1);
    assert_eq!(tx.capacity(), 0);
    drop(rx.try_recv().unwrap());
    assert_eq!(tx.capacity(), 1);
    tx.try_send(drops.value()).unwrap();
    drop(rx);
    assert_eq!(drops.count(), 3);
}

#[test]
fn a_send_granted_its_slot_and_dropped_unpolled_gives_the_slot_on() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(0).unwrap();
    let mut first = Task::new(tx.send(1));
    let mut second = Task::new(tx.send(2));
    first.pending();
    second.pending();
    assert_eq!(rx.try_recv(), Ok(0));
    assert_eq!((first.wakes(), second.wakes()), (1, 0));
    drop(first);
    assert_eq!(second.wakes(), 1);
    assert_eq!(second.ready(), Ok(()));
    assert_eq!(rx.try_recv(), Ok(2));
    assert_eq!(tx.capacity(), 1);
}

#[test]
fn a_dropped_pending_reserve_holds_no_slot() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(0).unwrap();
    let mut reserve = Task::new(tx.reserve());
    reserve.pending();
    drop(reserve);
    rx.try_recv().unwrap();
    assert_eq!(tx.capacity(), 1);
    assert!(tx.try_reserve().is_ok());
}

#[test]
fn a_dropped_pending_recv_loses_nothing() {
    let (tx, mut rx) = mpsc::channel(2);
    let mut recv = Task::new(rx.recv());
    recv.pending();
    drop(recv);
    tx.try_send(1).unwrap();
    tx.try_send(2).unwrap();
    assert_eq!(rx.try_recv(), Ok(1));
    assert_eq!(now(rx.recv()), Some(2));
}

#[test]
fn a_dropped_closed_wait_leaves_the_channel_usable() {
    let (tx, rx) = mpsc::channel::<u8>(1);
    let mut first = Task::new(tx.closed());
    let mut second = Task::new(tx.closed());
    first.pending();
    second.pending();
    drop(first);
    drop(rx);
    assert_eq!(second.wakes(), 1);
    second.ready();
}

#[test]
fn a_reserve_granted_before_the_close_and_dropped_lets_the_receiver_end() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(1).unwrap();
    let mut reserve = Task::new(tx.reserve());
    reserve.pending();
    assert_eq!(rx.try_recv(), Ok(1));
    rx.close();
    let mut probe = Probe::new();
    assert!(probe.enter(|cx| rx.poll_recv(cx)).is_pending());
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    drop(reserve);
    assert_eq!(probe.wakes(), 1);
    assert_eq!(
        probe.enter(|cx| rx.poll_recv(cx)),
        std::task::Poll::Ready(None)
    );
    assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
}

#[test]
fn a_reserve_granted_before_the_close_still_sends() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(1).unwrap();
    let mut reserve = Task::new(tx.reserve());
    reserve.pending();
    assert_eq!(rx.try_recv(), Ok(1));
    rx.close();
    reserve.ready().unwrap().send(2);
    assert_eq!(rx.try_recv(), Ok(2));
    assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
}

#[test]
fn a_pending_send_fails_with_its_value_at_the_close() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(1).unwrap();
    let mut send = Task::new(tx.send(2));
    send.pending();
    rx.close();
    assert_eq!(send.wakes(), 1);
    assert_eq!(send.ready(), Err(SendError(2)));
    assert_eq!(rx.try_recv(), Ok(1));
}

#[test]
fn a_oneshot_receiver_dropped_while_pending_makes_the_send_fail() {
    let (tx, rx) = oneshot::channel();
    let mut wait = Task::new(rx);
    wait.pending();
    drop(wait);
    assert_eq!(tx.send(4), Err(4));
}
