//! The once-cell and the set-once cell.

use super::*;

// === OnceCell ===

#[test]
fn a_once_cell_is_built_every_way() {
    static EMPTY: OnceCell<u8> = OnceCell::const_new();
    static FULL: OnceCell<u8> = OnceCell::const_new_with(4);
    assert!(!EMPTY.initialized() && FULL.initialized());
    assert_eq!((EMPTY.get(), FULL.get()), (None, Some(&4)));
    assert_eq!(OnceCell::new_with(Some(1)).get(), Some(&1));
    assert_eq!(OnceCell::<u8>::new_with(None).get(), None);
    assert_eq!(OnceCell::<u8>::default().get(), None);
    assert_eq!(OnceCell::from(2).get(), Some(&2));
    assert_eq!(
        format!("{:?}", OnceCell::from(2)),
        "OnceCell { value: Some(2) }"
    );
    assert_eq!(
        format!("{:?}", OnceCell::<u8>::new()),
        "OnceCell { value: None }"
    );
}

#[test]
fn set_tells_a_full_cell_from_one_being_initialized() {
    let cell = OnceCell::new();
    assert_eq!(cell.set(1), Ok(()));
    assert_eq!(cell.set(2), Err(SetError::AlreadyInitializedError(2)));
    assert_eq!(FULL_AFTER.set(9), Err(SetError::AlreadyInitializedError(9)));

    let cell = OnceCell::new();
    let mut running = Polled::new(cell.get_or_init(std::future::pending::<u8>));
    running.pending();
    assert_eq!(cell.set(3), Err(SetError::InitializingError(3)));
    assert_eq!(cell.get(), None, "the failed set stored nothing");
    drop(running);
    assert_eq!(
        cell.set(3),
        Ok(()),
        "the dropped initializer gave the turn up"
    );
}

static FULL_AFTER: OnceCell<u8> = OnceCell::const_new_with(1);

#[test]
fn get_or_init_runs_one_initializer_and_calls_it_only_when_needed() {
    let cell = OnceCell::new();
    let mut calls = 0;
    let value = block_on(cell.get_or_init(|| {
        calls += 1;
        async { 5 }
    }));
    assert_eq!(*value, 5);
    let again = block_on(cell.get_or_init(|| {
        calls += 1;
        async { 6 }
    }));
    assert_eq!(*again, 5);
    assert_eq!(calls, 1, "the initializer is not called for a full cell");
}

#[test]
fn get_or_try_init_returns_the_error_and_lets_the_next_try() {
    let cell = OnceCell::new();
    let failed = block_on(cell.get_or_try_init(|| async { Err::<u8, &str>("no") }));
    assert_eq!(failed, Err("no"));
    assert_eq!(cell.get(), None);
    let worked = block_on(cell.get_or_try_init(|| async { Ok::<u8, &str>(3) }));
    assert_eq!(worked, Ok(&3));
    let full = block_on(cell.get_or_try_init(|| async { Err::<u8, &str>("unused") }));
    assert_eq!(full, Ok(&3));
}

#[test]
fn a_dropped_initializer_hands_the_turn_to_the_next_waiter() {
    let cell = OnceCell::new();
    let mut first = Polled::new(cell.get_or_init(std::future::pending::<u8>));
    first.pending();
    let mut second = Polled::new(cell.get_or_init(|| async { 2 }));
    second.pending();
    drop(first);
    assert_eq!(second.wakes(), 1);
    assert_eq!(*second.ready(), 2);
}

#[test]
fn a_waiter_sees_the_value_the_running_initializer_publishes() {
    let cell = OnceCell::new();
    let gate = SetOnce::<()>::new();
    let mut first = Polled::new(cell.get_or_init(|| async {
        gate.wait().await;
        1
    }));
    first.pending();
    let mut second = Polled::new(cell.get_or_init(|| async { 2 }));
    second.pending();
    gate.set(()).expect("first");
    assert_eq!(*first.ready(), 1);
    assert_eq!(second.wakes(), 1);
    assert_eq!(*second.ready(), 1);
}

#[test]
fn get_mut_take_and_into_inner() {
    let mut cell = OnceCell::new();
    assert_eq!(cell.get_mut(), None);
    assert_eq!(cell.take(), None);
    cell.set(vec![1]).expect("empty");
    cell.get_mut().expect("full").push(2);
    assert_eq!(cell.take(), Some(vec![1, 2]));
    assert_eq!(cell.get(), None, "taking empties the cell");
    cell.set(vec![3]).expect("empty again");
    assert_eq!(cell.into_inner(), Some(vec![3]));
    assert_eq!(OnceCell::<u8>::new().into_inner(), None);
}

#[test]
fn a_once_cell_clones_and_compares_by_value() {
    let full = OnceCell::from(String::from("a"));
    let copy = full.clone();
    assert_eq!(copy, full);
    assert_ne!(full, OnceCell::new());
    assert_eq!(OnceCell::<String>::new(), OnceCell::new());
    assert_eq!(OnceCell::<String>::new().clone().get(), None);
    assert_eq!(copy.get().map(String::as_str), Some("a"));
}

#[test]
fn get_or_init_racy_stays_and_never_waits() {
    let cell = OnceCell::new();
    assert_eq!(*cell.get_or_init_racy(|| 1), 1);
    assert_eq!(*cell.get_or_init_racy(|| 2), 1);
}

// === SetOnce ===

#[test]
fn a_set_once_is_built_every_way() {
    static EMPTY: SetOnce<u8> = SetOnce::const_new();
    static FULL: SetOnce<u8> = SetOnce::const_new_with(4);
    assert!(!EMPTY.initialized() && FULL.initialized());
    assert_eq!((EMPTY.get(), FULL.get()), (None, Some(&4)));
    assert_eq!(SetOnce::new_with(Some(1)).get(), Some(&1));
    assert_eq!(SetOnce::<u8>::new_with(None).get(), None);
    assert_eq!(SetOnce::<u8>::default().get(), None);
    assert_eq!(SetOnce::from(2).get(), Some(&2));
    assert_eq!(
        format!("{:?}", SetOnce::from(2)),
        "SetOnce { value: Some(2) }"
    );
    assert_eq!(FULL.set(9), Err(SetOnceError(9)));
    assert_eq!(*block_on(FULL.wait()), 4, "a full cell waits for nothing");
}

#[test]
fn a_set_once_sets_once_and_wakes_every_waiter() {
    let once = SetOnce::new();
    let mut first = Polled::new(once.wait());
    let mut second = Polled::new(once.wait());
    first.pending();
    second.pending();
    assert_eq!(once.set(7), Ok(()));
    assert_eq!((first.wakes(), second.wakes()), (1, 1));
    assert_eq!((first.ready(), second.ready()), (&7, &7));
    assert_eq!(once.set(8), Err(SetOnceError(8)));
    assert_eq!(once.get(), Some(&7));
    drop((first, second));
    assert_eq!(once.into_inner(), Some(7));
    assert_eq!(SetOnce::<u8>::new().into_inner(), None);
}

#[test]
fn a_dropped_wait_leaves_the_others_to_their_wake() {
    let once = SetOnce::new();
    let mut kept = Polled::new(once.wait());
    kept.pending();
    Polled::new(once.wait()).pending();
    once.set(5).expect("empty");
    assert_eq!(kept.ready(), &5);
}

#[test]
fn a_set_once_clones_and_compares_by_value() {
    let full = SetOnce::from(String::from("a"));
    assert_eq!(full.clone(), full);
    assert_ne!(full, SetOnce::new());
    assert_eq!(SetOnce::<String>::new(), SetOnce::new());
}

#[test]
fn a_thread_waits_for_a_set_once() {
    let once = Arc::new(SetOnce::new());
    let waiter = {
        let once = Arc::clone(&once);
        std::thread::spawn(move || *block_on(once.wait()))
    };
    assert_eq!(once.set(9), Ok(()));
    assert_eq!(waiter.join().expect("waiter"), 9);
    assert!(on_a_thread(move || once.set(10).is_err()));
}
