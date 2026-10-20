//! Equality and hashing, then the rest of the surface: `Default`, `Debug`, an idempotent
//! cancel, a dropped waiter, the drop guards and unwind safety.

use super::*;

#[test]
fn cloned_cancellation_tokens_are_considered_equal() {
    let token = CancellationToken::new();
    let token_clone = token.clone();
    assert_eq!(token, token_clone);
}

#[test]
fn child_cancellation_tokens_are_not_considered_equal() {
    let token = CancellationToken::new();
    let token_clone = token.child_token();
    assert_ne!(token, token_clone);
}

#[test]
fn independent_cancellation_tokens_are_not_considered_equal() {
    assert_ne!(CancellationToken::new(), CancellationToken::new());
}

fn hash_of(token: &CancellationToken) -> u64 {
    let mut state = DefaultHasher::default();
    token.hash(&mut state);
    state.finish()
}

#[test]
fn cloned_cancellation_tokens_have_same_hash() {
    let token1 = CancellationToken::new();
    let token2 = token1.clone();
    assert_eq!(hash_of(&token1), hash_of(&token2));
}

#[test]
fn different_cancellation_tokens_have_different_hash() {
    assert_ne!(
        hash_of(&CancellationToken::new()),
        hash_of(&CancellationToken::new())
    );
}

// ---- the rest of the surface ----

#[test]
fn default_debug_and_clone_share_one_state() {
    let token = CancellationToken::default();
    assert_eq!(
        format!("{token:?}"),
        "CancellationToken { is_cancelled: false }"
    );
    let clone = token.clone();
    clone.cancel();
    assert!(token.is_cancelled());
    assert_eq!(
        format!("{token:?}"),
        "CancellationToken { is_cancelled: true }"
    );
}

#[test]
fn cancel_is_idempotent_and_wakes_each_waiter_once() {
    let token = CancellationToken::new();
    let mut waiters: Vec<_> = (0..5)
        .map(|_| Task::new(token.clone().cancelled_owned()))
        .collect();
    for waiter in &mut waiters {
        waiter.pending();
    }
    token.cancel();
    token.cancel();
    token.child_token().cancel();
    for waiter in &mut waiters {
        assert_eq!(waiter.wakes(), 1);
        waiter.ready();
    }
}

#[test]
fn a_waiter_on_an_already_cancelled_token_completes_at_once() {
    let token = CancellationToken::new();
    token.cancel();
    assert!(Task::new(token.cancelled()).poll().is_ready());
    assert!(Task::new(token.clone().cancelled_owned()).poll().is_ready());
    assert!(
        Task::new(token.child_token().cancelled_owned())
            .poll()
            .is_ready()
    );
}

#[test]
fn dropping_a_pending_waiter_leaves_the_others_to_be_woken() {
    let token = CancellationToken::new();
    let mut kept = Task::new(token.cancelled());
    kept.pending();
    {
        let mut dropped = Task::new(token.cancelled());
        dropped.pending();
        let mut dropped_owned = Task::new(token.clone().cancelled_owned());
        dropped_owned.pending();
    }
    token.cancel();
    assert_eq!(kept.wakes(), 1);
    kept.ready();
}

#[test]
fn drop_guard_cancels_on_drop_unless_disarmed() {
    let token = CancellationToken::new();
    drop(token.clone().drop_guard());
    assert!(token.is_cancelled());

    let token = CancellationToken::new();
    let guard: DropGuard = token.clone().drop_guard();
    assert_eq!(guard.token(), &token);
    let back = guard.disarm();
    assert_eq!(back, token);
    assert!(!token.is_cancelled());
    drop(back);
    assert!(!token.is_cancelled());

    let token = CancellationToken::new();
    let child = token.child_token();
    let _guard = token.clone().drop_guard();
    drop(_guard);
    assert!(child.is_cancelled(), "a guard cancels the children too");
}

#[test]
fn a_disarmed_guard_does_not_affect_other_guards() {
    let token = CancellationToken::new();
    let one = token.clone().drop_guard();
    let two = token.clone().drop_guard();
    let _kept = one.disarm();
    drop(two);
    assert!(token.is_cancelled());
}

#[test]
fn drop_guard_ref_cancels_on_drop_unless_disarmed() {
    let token = CancellationToken::new();
    {
        let guard: DropGuardRef<'_> = token.drop_guard_ref();
        assert_eq!(guard.token(), &token);
    }
    assert!(token.is_cancelled());

    let token = CancellationToken::new();
    let guard = token.drop_guard_ref();
    let same = guard.disarm();
    assert_eq!(same, &token);
    assert!(!token.is_cancelled());
}

#[test]
fn the_guards_debug_print() {
    let token = CancellationToken::new();
    assert!(format!("{:?}", token.clone().drop_guard()).contains("DropGuard"));
    assert!(format!("{:?}", token.drop_guard_ref()).contains("DropGuardRef"));
    assert!(format!("{:?}", token.cancelled()).contains("WaitForCancellationFuture"));
    assert!(format!("{:?}", token.clone().cancelled_owned()).contains("Owned"));
}

#[test]
fn a_future_awaiting_cancellation_runs_in_an_async_block() {
    let token = CancellationToken::new();
    let waiting = token.clone();
    let mut task = Task::new(async move {
        waiting.cancelled().await;
        waiting.is_cancelled()
    });
    task.pending();
    token.cancel();
    assert!(task.ready());
}

#[test]
fn the_token_is_unwind_safe() {
    let token = CancellationToken::new();
    let result = std::panic::catch_unwind(|| token.cancel());
    assert!(result.is_ok());
    assert!(token.is_cancelled());
}
