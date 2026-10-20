//! A sequential model: whatever is created, cancelled and dropped in what order, a token's
//! state agrees with a plain model of the tree.

use super::*;
use proptest::prelude::*;

#[derive(Clone, Debug)]
enum Op {
    Child(usize),
    Cancel(usize),
    DropHandle(usize),
    CloneHandle(usize),
}

fn ops() -> impl Strategy<Value = Vec<Op>> {
    let op = prop_oneof![
        4 => (0..64usize).prop_map(Op::Child),
        2 => (0..64usize).prop_map(Op::Cancel),
        3 => (0..64usize).prop_map(Op::DropHandle),
        1 => (0..64usize).prop_map(Op::CloneHandle),
    ];
    proptest::collection::vec(op, 0..150)
}

proptest! {
    /// Whatever is created, cancelled and dropped in what order, a token
    /// is cancelled exactly when it or one of its ancestors was cancelled
    /// by a call, including ancestors whose last handle is long gone.
    #[test]
    fn a_token_is_cancelled_exactly_when_it_or_an_ancestor_was(ops in ops()) {
        // Node i has parent `parent[i]`; `handles[i]` are the live handles
        // to it; `explicit[i]` says a cancel was called on it.
        let mut handles: Vec<Vec<CancellationToken>> = vec![vec![CancellationToken::new()]];
        let mut parent: Vec<Option<usize>> = vec![None];
        let mut explicit = vec![false];

        let expected = |explicit: &[bool], parent: &[Option<usize>], mut i: usize| loop {
            if explicit[i] {
                return true;
            }
            match parent[i] {
                Some(p) => i = p,
                None => return false,
            }
        };

        for op in ops {
            let live: Vec<usize> =
                (0..handles.len()).filter(|i| !handles[*i].is_empty()).collect();
            if live.is_empty() {
                break;
            }
            let pick = |i: usize| live[i % live.len()];
            match op {
                Op::Child(i) => {
                    let at = pick(i);
                    let child = handles[at][0].child_token();
                    handles.push(vec![child]);
                    parent.push(Some(at));
                    explicit.push(false);
                }
                Op::Cancel(i) => {
                    let at = pick(i);
                    handles[at][0].cancel();
                    explicit[at] = true;
                }
                Op::DropHandle(i) => {
                    let at = pick(i);
                    handles[at].pop();
                }
                Op::CloneHandle(i) => {
                    let at = pick(i);
                    let copy = handles[at][0].clone();
                    handles[at].push(copy);
                }
            }
            for (i, node) in handles.iter().enumerate() {
                for token in node {
                    prop_assert_eq!(
                        token.is_cancelled(),
                        expected(&explicit, &parent, i),
                        "node {}", i
                    );
                }
            }
        }
    }
}
