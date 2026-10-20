//! A chain of a million nested tokens, dropped on a small stack.

use super::*;

/// A chain of a million nested tokens, built and then dropped on a thread
/// with a small stack. Dropping the tokens, and the nodes the chain keeps
/// alive as relays, must not recurse once per level; cancelling the root must
/// not either.
#[test]
fn a_chain_of_a_million_nested_tokens_drops_without_overflowing_the_stack() {
    const DEPTH: usize = 1_000_000;
    let work = std::thread::Builder::new()
        .name("deep-chain".into())
        .stack_size(256 * 1024)
        .spawn(|| {
            let depth = if cfg!(miri) { 2_000 } else { DEPTH };

            // The root goes first, so the chain below it is all relays and
            // the leaf is the last owner of every node above it.
            let root = CancellationToken::new();
            let mut leaf = root.clone();
            for _ in 0..depth {
                leaf = leaf.child_token();
            }
            assert!(!leaf.is_cancelled());
            drop(root);
            drop(leaf);

            // Cancelled from the top, then dropped from the bottom: the root
            // is the last owner, and the cancel walks the whole chain.
            let root = CancellationToken::new();
            let mut leaf = root.clone();
            for _ in 0..depth {
                leaf = leaf.child_token();
            }
            root.cancel();
            assert!(
                leaf.is_cancelled(),
                "the cancel reached the bottom of the chain"
            );
            drop(leaf);
            drop(root);
        })
        .expect("spawn");
    work.join()
        .expect("the chain overflowed the stack or panicked");
}
