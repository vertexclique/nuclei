use std::cell::Cell;
use std::rc::Rc;

use nuclei::runtime::Runtime;
use nuclei::sync::local::Mutex;
use nuclei::task;

use crate::sync_support::Probe;
use crate::util::{now, pending, ready};

#[test]
fn a_guard_excludes_try_lock_and_unlocks_on_drop() {
    let mutex = Mutex::new(1);
    let mut guard = mutex.try_lock().unwrap();
    *guard += 1;
    assert!(mutex.try_lock().is_err());
    drop(guard);
    assert_eq!(*mutex.try_lock().unwrap(), 2);
}

#[test]
fn lock_completes_at_once_when_free() {
    let mutex = Mutex::new(String::from("a"));
    now(mutex.lock()).push('b');
    assert_eq!(*now(mutex.lock()), "ab");
}

#[test]
fn a_waiting_lock_is_handed_the_lock_by_the_unlock() {
    let mutex = Mutex::new(0);
    let guard = mutex.try_lock().unwrap();
    let mut probe = Probe::new();
    let mut lock = mutex.lock();
    pending(&mut probe, &mut lock);
    drop(guard);
    assert_eq!(probe.wakes(), 1);
    assert!(mutex.try_lock().is_err(), "the lock is owed to the waiter");
    drop(ready(&mut probe, &mut lock));
    assert!(mutex.try_lock().is_ok());
}

#[test]
fn locks_are_granted_in_arrival_order() {
    let mutex = Mutex::new(());
    let held = mutex.try_lock().unwrap();
    let mut probes: Vec<Probe> = (0..3).map(|_| Probe::new()).collect();
    let mut locks: Vec<_> = (0..3).map(|_| mutex.lock()).collect();
    for (probe, lock) in probes.iter_mut().zip(&mut locks) {
        pending(probe, lock);
    }
    drop(held);
    for i in 0..3 {
        assert_eq!(probes[i].wakes(), 1);
        assert!(probes[i + 1..].iter().all(|p| p.wakes() == 0));
        drop(ready(&mut probes[i], &mut locks[i]));
    }
}

#[test]
fn get_mut_into_inner_default_and_from() {
    let mut mutex = Mutex::from(5);
    *mutex.get_mut() += 1;
    assert_eq!(mutex.into_inner(), 6);
    assert_eq!(Mutex::<u8>::default().into_inner(), 0);
}

#[test]
fn debug_shows_the_value_or_that_it_is_locked() {
    let mutex = Mutex::new(7);
    assert_eq!(format!("{mutex:?}"), "Mutex { data: 7 }");
    let guard = mutex.try_lock().unwrap();
    assert_eq!(format!("{guard:?}"), "7");
    assert_eq!(format!("{mutex:?}"), "Mutex { data: <locked> }");
    assert!(format!("{:?}", mutex.lock()).contains("Lock"));
}

#[test]
fn try_lock_fails_while_a_waiter_is_queued() {
    let mutex = Mutex::new(());
    let guard = mutex.try_lock().unwrap();
    let mut probe = Probe::new();
    let mut lock = mutex.lock();
    pending(&mut probe, &mut lock);
    drop(guard);
    assert!(mutex.try_lock().is_err());
    drop(lock);
    assert!(mutex.try_lock().is_ok());
}

#[test]
fn many_tasks_never_hold_the_lock_together() {
    let rt = Runtime::builder()
        .worker_threads(1)
        .pin_workers(false)
        .build()
        .expect("the runtime starts");
    let total = rt.block_on(async {
        let mutex = Rc::new(Mutex::new(0u32));
        let inside = Rc::new(Cell::new(false));
        let tasks: Vec<_> = (0..50)
            .map(|_| {
                let (mutex, inside) = (Rc::clone(&mutex), Rc::clone(&inside));
                task::spawn_local(async move {
                    for _ in 0..5 {
                        let mut guard = mutex.lock().await;
                        assert!(!inside.replace(true), "two holders at once");
                        let seen = *guard;
                        task::yield_now().await;
                        *guard = seen + 1;
                        inside.set(false);
                    }
                })
            })
            .collect();
        for task in tasks {
            task.await;
        }
        *mutex.lock().await
    });
    assert_eq!(total, 250);
}
