//! Threads creating, cancelling and dropping trees at once. The counts shrink under Miri
//! (`scale`).

use super::*;

fn scale(real: usize, miri: usize) -> usize {
    if cfg!(miri) { miri } else { real }
}

/// Children created while the parent is cancelled from another thread: every
/// child whose creation returned after the cancel returned is cancelled, and
/// every child created before is cancelled by the time both are done.
#[test]
fn a_cancel_racing_child_creation_leaves_no_uncancelled_child() {
    for _ in 0..scale(2_000, 3) {
        let root = CancellationToken::new();
        let go = Arc::new(Barrier::new(3));
        let creators: Vec<_> = (0..2)
            .map(|_| {
                let (root, go) = (root.clone(), Arc::clone(&go));
                std::thread::spawn(move || {
                    go.wait();
                    let mut made = Vec::new();
                    for _ in 0..20 {
                        let child = root.child_token();
                        let grandchild = child.child_token();
                        made.push(child);
                        made.push(grandchild);
                    }
                    made
                })
            })
            .collect();
        let canceller = {
            let (root, go) = (root.clone(), Arc::clone(&go));
            std::thread::spawn(move || {
                go.wait();
                root.cancel();
            })
        };
        canceller.join().expect("canceller");
        let made: Vec<_> = creators
            .into_iter()
            .flat_map(|c| c.join().expect("creator"))
            .collect();
        assert!(
            made.iter().all(CancellationToken::is_cancelled),
            "a descendant of a cancelled token was left running"
        );
        assert!(root.child_token().is_cancelled());
    }
}

/// Waiters on threads at every depth are woken by one cancel of the root.
#[test]
fn a_cancel_wakes_waiters_at_every_depth_on_other_threads() {
    for _ in 0..scale(500, 3) {
        let root = CancellationToken::new();
        let a = root.child_token();
        let b = a.child_token();
        let c = root.child_token();
        let tokens = [root.clone(), a, b, c, root.clone()];
        let ready = Arc::new(Barrier::new(tokens.len() + 1));
        let waiters: Vec<_> = tokens
            .into_iter()
            .enumerate()
            .map(|(i, token)| {
                let ready = Arc::clone(&ready);
                std::thread::spawn(move || {
                    ready.wait();
                    if i % 2 == 0 {
                        block_on(token.cancelled());
                    } else {
                        block_on(token.cancelled_owned());
                    }
                })
            })
            .collect();
        ready.wait();
        root.cancel();
        for waiter in waiters {
            waiter.join().expect("waiter");
        }
    }
}

/// One parent with thousands of children added from many threads (the
/// registry grows several times on the way), cancelled, then dropped by many
/// threads at once.
#[test]
fn a_wide_parent_is_cancelled_and_dropped_concurrently() {
    let per_thread = scale(3_000, 30);
    let root = CancellationToken::new();
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let root = root.clone();
            std::thread::spawn(move || {
                (0..per_thread)
                    .map(|_| root.child_token())
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let children: Vec<Vec<_>> = threads
        .into_iter()
        .map(|t| t.join().expect("creator"))
        .collect();
    assert!(children.iter().flatten().all(|c| !c.is_cancelled()));
    root.cancel();
    assert!(
        children
            .iter()
            .flatten()
            .all(CancellationToken::is_cancelled)
    );
    let droppers: Vec<_> = children
        .into_iter()
        .map(|chunk| std::thread::spawn(move || drop(chunk)))
        .collect();
    for dropper in droppers {
        dropper.join().expect("dropper");
    }
}

/// Random trees built by several threads under one root, with explicit
/// cancels of inner nodes and drops, and a cancel of the root from another
/// thread in half the rounds. At the end every token is cancelled exactly
/// when it, or one of its ancestors, was cancelled.
#[test]
fn concurrent_trees_end_with_exactly_the_expected_tokens_cancelled() {
    for round in 0..scale(300, 3) {
        let cancel_root = round % 2 == 0;
        let root = CancellationToken::new();
        let go = Arc::new(Barrier::new(5));
        let workers: Vec<_> = (0..4u64)
            .map(|seed| {
                let (root, go) = (root.clone(), Arc::clone(&go));
                std::thread::spawn(move || {
                    // (token, index of parent, cancelled explicitly)
                    let mut nodes: Vec<(CancellationToken, Option<usize>, bool)> =
                        vec![(root.child_token(), None, false)];
                    let mut state = 0x9E37_79B9_7F4A_7C15u64 ^ (seed + round as u64);
                    let mut next = || {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        state
                    };
                    go.wait();
                    for _ in 0..40 {
                        let at = (next() % nodes.len() as u64) as usize;
                        match next() % 4 {
                            0 | 1 => {
                                let child = nodes[at].0.child_token();
                                nodes.push((child, Some(at), false));
                            }
                            2 => {
                                nodes[at].0.cancel();
                                nodes[at].2 = true;
                            }
                            _ => {
                                // Replace a node by a clone: still one token.
                                let clone = nodes[at].0.clone();
                                nodes[at].0 = clone;
                            }
                        }
                    }
                    nodes
                })
            })
            .collect();
        let canceller = {
            let (root, go) = (root.clone(), Arc::clone(&go));
            std::thread::spawn(move || {
                go.wait();
                if cancel_root {
                    root.cancel();
                }
            })
        };
        canceller.join().expect("canceller");
        for worker in workers {
            let nodes = worker.join().expect("worker");
            let mut expected = vec![false; nodes.len()];
            for i in 0..nodes.len() {
                let inherited = match nodes[i].1 {
                    Some(parent) => expected[parent],
                    None => cancel_root,
                };
                expected[i] = inherited || nodes[i].2;
                assert_eq!(
                    nodes[i].0.is_cancelled(),
                    expected[i],
                    "token {i} (round {round}, cancel_root {cancel_root})"
                );
            }
        }
        assert_eq!(root.is_cancelled(), cancel_root);
    }
}

/// Cancelling and dropping while waiters poll on other threads: nothing hangs
/// and nobody is woken for a token that stays uncancelled.
#[test]
fn dropping_children_while_the_parent_is_cancelled_is_safe() {
    for _ in 0..scale(1_000, 3) {
        let root = CancellationToken::new();
        let woken = Arc::new(AtomicUsize::new(0));
        let children: Vec<_> = (0..16).map(|_| root.child_token()).collect();
        let watchers: Vec<_> = children
            .iter()
            .step_by(4)
            .map(|child| {
                let (child, woken) = (child.clone(), Arc::clone(&woken));
                std::thread::spawn(move || {
                    block_on(child.cancelled());
                    woken.fetch_add(1, Ordering::SeqCst);
                })
            })
            .collect();
        let dropper = std::thread::spawn(move || drop(children));
        let canceller = {
            let root = root.clone();
            std::thread::spawn(move || root.cancel())
        };
        dropper.join().expect("dropper");
        canceller.join().expect("canceller");
        for watcher in watchers {
            watcher.join().expect("watcher");
        }
        assert_eq!(woken.load(Ordering::SeqCst), 4);
    }
}

/// Every `cancel` call returns with its whole subtree cancelled, even while
/// other calls race it over the same nodes: a call that finds a node already
/// marked by a slower call still walks below it.
#[test]
fn every_cancel_returns_with_its_subtree_cancelled_while_others_race_it() {
    for _ in 0..scale(1_000, 3) {
        let root = CancellationToken::new();
        let children: Vec<_> = (0..4).map(|_| root.child_token()).collect();
        let grandchildren: Vec<Vec<_>> = children
            .iter()
            .map(|c| (0..4).map(|_| c.child_token()).collect())
            .collect();
        let go = Arc::new(Barrier::new(4));
        let spawn_canceller = |token: CancellationToken, subtree: Vec<CancellationToken>| {
            let go = Arc::clone(&go);
            std::thread::spawn(move || {
                go.wait();
                token.cancel();
                assert!(token.is_cancelled());
                assert!(
                    subtree.iter().all(CancellationToken::is_cancelled),
                    "a cancel returned with part of its subtree running"
                );
            })
        };
        let everything: Vec<_> = children
            .iter()
            .chain(grandchildren.iter().flatten())
            .cloned()
            .collect();
        let under_first: Vec<_> = grandchildren[0].clone();
        let threads = vec![
            spawn_canceller(root.clone(), everything.clone()),
            spawn_canceller(root.clone(), everything),
            spawn_canceller(children[0].clone(), under_first),
        ];
        go.wait();
        for thread in threads {
            thread.join().expect("canceller");
        }
        assert!(
            grandchildren
                .iter()
                .flatten()
                .all(CancellationToken::is_cancelled)
        );
    }
}
