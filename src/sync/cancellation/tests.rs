//! Unit tests of the token tree: the registry, the relay, deep chains and
//! concurrent cancellation.
use super::node::Children;
use super::*;
use crate::sync::test_support::Polled;

/// The nodes of `tokens`, to check later that all of them were freed.
fn watch(tokens: &[&CancellationToken]) -> Vec<Weak<Node>> {
    tokens.iter().map(|t| Arc::downgrade(&t.node)).collect()
}

fn all_freed(nodes: &[Weak<Node>]) -> bool {
    nodes.iter().all(|node| node.upgrade().is_none())
}

fn registered(token: &CancellationToken) -> usize {
    token.node.children.get().map_or(0, Children::len)
}

#[test]
fn a_dropped_child_leaves_its_parents_registry() {
    let parent = CancellationToken::new();
    assert_eq!(registered(&parent), 0, "no child, no registry entry");
    let children: Vec<_> = (0..10).map(|_| parent.child_token()).collect();
    assert_eq!(registered(&parent), 10);
    let nodes = watch(&children.iter().collect::<Vec<_>>());
    drop(children);
    assert_eq!(
        registered(&parent),
        0,
        "the registry holds only live children"
    );
    assert!(all_freed(&nodes));
}

#[test]
fn a_parent_without_handles_is_a_relay_while_a_child_lives() {
    let grandparent = CancellationToken::new();
    let parent = grandparent.child_token();
    let child = parent.child_token();
    let nodes = watch(&[&parent, &child]);
    drop(parent);
    assert!(
        nodes[0].upgrade().is_some(),
        "the child keeps the parent node as a relay"
    );
    assert_eq!(registered(&grandparent), 1);
    grandparent.cancel();
    assert!(child.is_cancelled(), "the relay passes the cancellation on");
    drop(child);
    assert!(all_freed(&nodes), "nothing outlives the last live token");
    assert_eq!(registered(&grandparent), 0);
}

#[test]
fn dropping_the_last_handle_of_a_deep_chain_frees_every_node() {
    let root = CancellationToken::new();
    let mut nodes = Vec::new();
    let mut leaf = root.child_token();
    for _ in 0..50_000 {
        nodes.push(Arc::downgrade(&leaf.node));
        leaf = leaf.child_token();
    }
    nodes.push(Arc::downgrade(&leaf.node));
    drop(leaf);
    assert!(all_freed(&nodes));
    assert_eq!(registered(&root), 0);
}

/// Another call marked a child and has not walked below it yet: this call
/// must not return before the grandchild is cancelled.
#[test]
fn a_cancel_descends_into_a_child_another_call_already_marked() {
    let root = CancellationToken::new();
    let child = root.child_token();
    let grandchild = child.child_token();
    let great_grandchild = grandchild.child_token();
    child.node.cancelled.set();
    assert!(!grandchild.is_cancelled());
    root.cancel();
    assert!(grandchild.is_cancelled() && great_grandchild.is_cancelled());
}

/// The root itself was marked by another call that has not walked yet.
#[test]
fn a_cancel_walks_below_a_root_another_call_already_marked() {
    let root = CancellationToken::new();
    let child = root.child_token();
    root.node.cancelled.set();
    assert!(
        !child.is_cancelled(),
        "setting the flag alone marks nothing below"
    );
    root.cancel();
    assert!(child.is_cancelled());
}

#[test]
fn cancelling_a_child_alone_leaves_the_parent_and_siblings() {
    let parent = CancellationToken::new();
    let a = parent.child_token();
    let b = parent.child_token();
    a.cancel();
    assert!(a.is_cancelled());
    assert!(!parent.is_cancelled() && !b.is_cancelled());
    parent.cancel();
    assert!(b.is_cancelled());
}

#[test]
fn the_owned_future_is_woken_and_may_move_while_pending() {
    let token = CancellationToken::new();
    let mut waiting = Polled::new(token.clone().cancelled_owned());
    waiting.pending();
    let mut moved = Polled::new(token.clone().cancelled_owned());
    moved.pending();
    token.cancel();
    assert_eq!((waiting.wakes(), moved.wakes()), (1, 1));
    waiting.ready();
    moved.ready();
}

#[test]
fn the_registry_is_empty_after_concurrent_trees_are_cancelled_and_dropped() {
    let rounds = if cfg!(miri) { 3 } else { 200 };
    for _ in 0..rounds {
        let root = CancellationToken::new();
        let threads: Vec<_> = (0..4)
            .map(|t| {
                let root = root.clone();
                std::thread::spawn(move || {
                    let mut kept = Vec::new();
                    for i in 0..50 {
                        let child = root.child_token();
                        let grandchild = child.child_token();
                        if (i + t) % 7 == 0 {
                            child.cancel();
                            assert!(grandchild.is_cancelled());
                        }
                        if i % 3 == 0 {
                            kept.push(grandchild);
                        }
                        if i % 5 == 0 {
                            root.cancel();
                        }
                    }
                    kept
                })
            })
            .collect();
        let kept: Vec<_> = threads
            .into_iter()
            .flat_map(|t| t.join().expect("tree thread"))
            .collect();
        root.cancel();
        assert!(kept.iter().all(CancellationToken::is_cancelled));
        drop(kept);
        assert_eq!(registered(&root), 0, "every child left the registry");
    }
}
