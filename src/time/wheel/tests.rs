use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Wake, Waker};

use proptest::prelude::*;

use super::{LEVELS, SLOT_BITS, SPAN, Wheel, level_by_windows, level_for, slot_for};

/// A waker that counts its wakes.
struct Count(AtomicUsize);

impl Wake for Count {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn waker() -> (Waker, Arc<Count>) {
    let count = Arc::new(Count(AtomicUsize::new(0)));
    (Waker::from(count.clone()), count)
}

/// Advances and wakes what fired; returns how many fired.
fn advance(wheel: &mut Wheel, now: u64) -> usize {
    let mut fired = Vec::new();
    wheel.advance(now, &mut fired);
    let n = fired.len();
    fired.into_iter().for_each(Waker::wake);
    n
}

#[test]
fn the_fast_level_matches_its_definition_at_every_boundary() {
    let distances = (0..=36u32).flat_map(|b| {
        let p = 1u64 << b;
        [p.saturating_sub(1), p, p + 1]
    });
    for elapsed in [
        0u64,
        1,
        63,
        64,
        4095,
        4096,
        1 << 30,
        (1 << 36) - 1,
        123_456_789,
    ] {
        for d in distances.clone() {
            let when = elapsed + d.min(SPAN - 1);
            assert_eq!(
                level_for(elapsed, when),
                level_by_windows(elapsed, when),
                "elapsed {elapsed} when {when}"
            );
        }
    }
}

proptest! {
    #[test]
    fn the_fast_level_matches_its_definition(elapsed in 0u64..(1 << 50), d in 0u64..SPAN) {
        let when = elapsed + d;
        prop_assert_eq!(level_for(elapsed, when), level_by_windows(elapsed, when));
    }
}

/// A deadline in the wheel's top window goes to a slot after the wheel's
/// position on its level, in the same window. One beyond the window goes to
/// the top level, where it wraps into the next window.
#[test]
fn a_timer_is_placed_after_the_wheels_position_or_wraps_on_the_top_level() {
    for elapsed in [0u64, 5, 64, 100, 5000, 1 << 30, (1 << 30) + 1, 1 << 33] {
        for d in [
            1u64,
            2,
            63,
            64,
            65,
            4095,
            4096,
            4097,
            1 << 20,
            (1 << 36) - 2,
            (1 << 36) - 1,
        ] {
            let when = elapsed + d;
            let level = level_for(elapsed, when);
            assert!(level < LEVELS);
            let width = SLOT_BITS * (level as u32 + 1);
            if elapsed >> width == when >> width {
                assert!(level == 0 || slot_for(when, level) > slot_for(elapsed, level));
            } else {
                assert_eq!(level, LEVELS - 1, "only the top level wraps");
                assert_eq!(elapsed >> width, (when >> width) - 1, "the next window");
            }
        }
    }
}

#[test]
fn a_timer_fires_when_the_wheel_reaches_its_deadline() {
    let mut wheel = Wheel::new();
    let (w, count) = waker();
    let key = wheel.insert(5, &w);
    assert_eq!(wheel.next_deadline(), Some(5));
    assert_eq!(advance(&mut wheel, 4), 0);
    assert!(!wheel.is_fired(key));
    assert_eq!(advance(&mut wheel, 5), 1);
    assert!(wheel.is_fired(key));
    assert_eq!(count.0.load(Ordering::SeqCst), 1);
    assert_eq!(wheel.armed(), 0);
    assert_eq!(wheel.next_deadline(), None);
}

#[test]
fn timers_at_every_level_fire_exactly_on_time() {
    let distances = [
        1u64,
        2,
        63,
        64,
        65,
        127,
        128,
        4095,
        4096,
        4097,
        262_143,
        262_144,
        262_145,
        16_777_215,
        16_777_216,
        1 << 30,
        (1 << 30) + 7,
        (1 << 36) - 1,
    ];
    for step in [1u64, 7, 64, 1000, 1 << 20] {
        let mut wheel = Wheel::new();
        let start = 1_000u64;
        advance(&mut wheel, start);
        let timers: Vec<_> = distances
            .iter()
            .map(|&d| {
                let (w, c) = waker();
                (start + d, wheel.insert(start + d, &w), c)
            })
            .collect();
        let end = start + (1 << 36);
        let mut now = start;
        while now < end {
            // Jump to the next deadline or by the step, whichever is first,
            // and check nothing fires early or late.
            let next = (now + step).min(end);
            advance(&mut wheel, next);
            for (when, key, count) in &timers {
                let fired = count.0.load(Ordering::SeqCst);
                if *when <= next {
                    assert_eq!(fired, 1, "step {step}: due at {when}, wheel at {next}");
                    assert!(wheel.is_fired(*key));
                } else {
                    assert_eq!(fired, 0, "step {step}: due at {when}, fired at {next}");
                }
            }
            now = match wheel.next_deadline() {
                // Skip ahead over empty stretches when the step is small.
                Some(d) if d > next + step => d - step,
                _ => next,
            };
        }
        assert_eq!(wheel.armed(), 0);
    }
}

#[test]
fn a_deadline_beyond_the_span_fires_on_time() {
    let mut wheel = Wheel::new();
    let far = 3 * SPAN + 12_345;
    let (w, count) = waker();
    let key = wheel.insert(far, &w);
    let mut fired_at = None;
    let mut now = 0;
    while fired_at.is_none() {
        let next = wheel.next_deadline().expect("armed");
        assert!(next <= far, "the wheel never waits past the deadline");
        assert!(next > now || next == wheel.elapsed());
        now = next;
        if advance(&mut wheel, now) > 0 {
            fired_at = Some(now);
        }
    }
    assert_eq!(fired_at, Some(far));
    assert!(wheel.is_fired(key));
    assert_eq!(count.0.load(Ordering::SeqCst), 1);
}

#[test]
fn a_removed_timer_never_fires_and_its_key_goes_stale() {
    let mut wheel = Wheel::new();
    let (w, count) = waker();
    let key = wheel.insert(10, &w);
    wheel.remove(key);
    assert_eq!(wheel.armed(), 0);
    assert!(wheel.is_fired(key), "a stale key reads as fired");
    // The place is reused; the old key does not reach the new timer.
    let (w2, count2) = waker();
    let key2 = wheel.insert(10, &w2);
    wheel.remove(key);
    wheel.reset(key, 3, &w);
    assert_eq!(wheel.armed(), 1);
    assert_eq!(advance(&mut wheel, 20), 1);
    assert_eq!(count.0.load(Ordering::SeqCst), 0);
    assert_eq!(count2.0.load(Ordering::SeqCst), 1);
    assert!(wheel.is_fired(key2));
}

#[test]
fn a_reset_moves_the_deadline_and_arms_a_fired_timer_again() {
    let mut wheel = Wheel::new();
    let (w, count) = waker();
    let key = wheel.insert(10, &w);
    wheel.reset(key, 100, &w);
    assert_eq!(advance(&mut wheel, 50), 0);
    assert_eq!(advance(&mut wheel, 100), 1);
    wheel.reset(key, 130, &w);
    assert!(!wheel.is_fired(key));
    assert_eq!(advance(&mut wheel, 130), 1);
    assert_eq!(count.0.load(Ordering::SeqCst), 2);
    // A deadline already passed fires at once, with no wake.
    wheel.reset(key, 5, &w);
    assert!(wheel.is_fired(key));
    assert_eq!(count.0.load(Ordering::SeqCst), 2);
}

#[test]
fn a_deadline_already_passed_is_fired_at_once() {
    let mut wheel = Wheel::new();
    advance(&mut wheel, 100);
    let (w, count) = waker();
    let key = wheel.insert(100, &w);
    assert!(wheel.is_fired(key));
    assert_eq!(wheel.armed(), 0);
    assert_eq!(
        count.0.load(Ordering::SeqCst),
        0,
        "the caller sees it fired; no wake"
    );
}

#[test]
fn set_waker_keeps_a_waker_of_the_same_task_and_replaces_another() {
    let mut wheel = Wheel::new();
    let (w1, c1) = waker();
    let (w2, c2) = waker();
    let key = wheel.insert(10, &w1);
    wheel.set_waker(key, &w1.clone());
    wheel.set_waker(key, &w2);
    advance(&mut wheel, 10);
    assert_eq!(c1.0.load(Ordering::SeqCst), 0);
    assert_eq!(c2.0.load(Ordering::SeqCst), 1);
}

#[test]
fn many_timers_in_one_slot_all_fire_and_the_slab_is_reused() {
    let mut wheel = Wheel::new();
    let (w, count) = waker();
    let keys: Vec<_> = (0..1000).map(|_| wheel.insert(70, &w)).collect();
    assert_eq!(advance(&mut wheel, 70), 1000);
    assert_eq!(count.0.load(Ordering::SeqCst), 1000);
    keys.into_iter().for_each(|k| wheel.remove(k));
    let before = wheel.entries.len();
    let keys: Vec<_> = (0..1000).map(|i| wheel.insert(200 + i, &w)).collect();
    assert_eq!(wheel.entries.len(), before, "removed entries are reused");
    assert_eq!(wheel.armed(), 1000);
    keys.into_iter().for_each(|k| wheel.remove(k));
    assert_eq!(wheel.armed(), 0);
    assert_eq!(wheel.next_deadline(), None);
}

/// One operation of the model test.
#[derive(Clone, Debug)]
enum Op {
    Insert(u64),
    Remove(usize),
    Reset(usize, u64),
    Advance(u64),
}

fn op() -> impl Strategy<Value = Op> {
    let delta = prop_oneof![0u64..70, 0u64..5000, 0u64..(1 << 24), 0u64..(2 * SPAN)];
    prop_oneof![
        delta.clone().prop_map(Op::Insert),
        (0usize..64).prop_map(Op::Remove),
        ((0usize..64), delta.clone()).prop_map(|(i, d)| Op::Reset(i, d)),
        prop_oneof![
            0u64..3,
            0u64..200,
            0u64..100_000,
            0u64..(1 << 30),
            0u64..(2 * SPAN)
        ]
        .prop_map(Op::Advance),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(if cfg!(miri) { 4 } else { 2048 }))]

    /// Against a model that keeps every timer's deadline: a timer fires in the
    /// advance that first reaches its deadline, never earlier, never later,
    /// exactly once; the wheel's next deadline is never past the earliest
    /// armed one.
    #[test]
    fn the_wheel_agrees_with_a_model(ops in proptest::collection::vec(op(), 1..120)) {
        let mut wheel = Wheel::new();
        let mut now = 0u64;
        // id -> (key, deadline, its counter, fired in the model)
        let mut timers: Vec<(super::TimerKey, u64, Arc<Count>, bool, bool)> = Vec::new();
        for op in ops {
            match op {
                Op::Insert(d) => {
                    let (w, c) = waker();
                    let when = now + d;
                    let key = wheel.insert(when, &w);
                    timers.push((key, when, c, when <= now, false));
                }
                Op::Remove(i) => {
                    if let Some(t) = timers.get_mut(i) {
                        wheel.remove(t.0);
                        t.4 = true;
                    }
                }
                Op::Reset(i, d) => {
                    if let Some(t) = timers.get_mut(i)
                        && !t.4
                    {
                        let (w, c) = waker();
                        let when = now + d;
                        wheel.reset(t.0, when, &w);
                        t.1 = when;
                        t.2 = c;
                        t.3 = when <= now;
                    }
                }
                Op::Advance(d) => {
                    now += d;
                    advance(&mut wheel, now);
                    for t in timers.iter_mut().filter(|t| !t.4 && !t.3) {
                        if t.1 <= now {
                            t.3 = true;
                        }
                    }
                }
            }
            let mut earliest: BTreeMap<u64, usize> = BTreeMap::new();
            for (key, when, count, fired, removed) in &timers {
                if *removed {
                    continue;
                }
                let woken = count.0.load(Ordering::SeqCst);
                prop_assert_eq!(wheel.is_fired(*key), *fired, "deadline {} at {}", when, now);
                // Woken once if it fired by passing its deadline, never otherwise.
                prop_assert!(woken <= 1);
                if !*fired {
                    prop_assert_eq!(woken, 0);
                    *earliest.entry(*when).or_default() += 1;
                }
            }
            let armed: usize = earliest.values().sum();
            prop_assert_eq!(wheel.armed(), armed);
            if let Some((&first, _)) = earliest.iter().next() {
                let next = wheel.next_deadline().expect("armed timers");
                prop_assert!(next <= first, "the wheel would wait past {}", first);
                prop_assert!(next >= wheel.elapsed());
            } else {
                prop_assert_eq!(wheel.next_deadline(), None);
            }
        }
    }
}

/// A deadline in the next top window shares a top-level slot with the
/// wheel's position; it must not hide a nearer timer in a later slot of the
/// same level.
#[test]
fn a_timer_in_the_next_top_window_does_not_hide_a_nearer_one() {
    let mut wheel = Wheel::new();
    advance(&mut wheel, 1000);
    let (near, near_count) = waker();
    let (far, far_count) = waker();
    wheel.insert(1000 + (1 << 30), &near);
    wheel.insert(1000 + (1 << 36) - 1, &far);
    assert!(wheel.next_deadline().unwrap() <= 1000 + (1 << 30));
    let mut steps = 0;
    while let Some(d) = wheel.next_deadline() {
        advance(&mut wheel, d);
        steps += 1;
        if near_count.0.load(Ordering::SeqCst) == 1 {
            assert_eq!(
                wheel.elapsed(),
                1000 + (1 << 30),
                "the near timer fired on time"
            );
            assert_eq!(far_count.0.load(Ordering::SeqCst), 0);
            break;
        }
    }
    assert!(steps < 10);
}
