//! A value built on first use, without waiting.

use core::fmt;
use core::ops::Deref;

use super::once_cell::OnceCell;

/// A value built by `F` on first access.
///
/// Built on [`OnceCell::get_or_init_racy`]: threads that reach an
/// unbuilt `Lazy` at the same moment each run `F`, one result is kept and
/// the others are dropped, and nobody waits for anybody. `F` therefore
/// runs at least once and possibly more than once, so it is `Fn` and
/// should be free of side effects that must not repeat.
///
/// ```
/// use nuclei::sync::Lazy;
///
/// static TABLE: Lazy<Vec<u32>> = Lazy::new(|| (0..4).collect());
/// assert_eq!(TABLE.len(), 4);
/// ```
pub struct Lazy<T, F = fn() -> T> {
    cell: OnceCell<T>,
    init: F,
}

impl<T, F> Lazy<T, F> {
    loom_const_fn! {
        /// A `Lazy` that builds its value with `init`.
        pub fn new(init: F) -> Self {
            Self { cell: OnceCell::new(), init }
        }
    }
}

impl<T, F: Fn() -> T> Lazy<T, F> {
    /// The value, building it if nobody has yet.
    pub fn force(this: &Self) -> &T {
        this.cell.get_or_init_racy(|| (this.init)())
    }
}

impl<T, F: Fn() -> T> Deref for Lazy<T, F> {
    type Target = T;

    fn deref(&self) -> &T {
        Self::force(self)
    }
}

impl<T: fmt::Debug, F> fmt::Debug for Lazy<T, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Lazy").field(&self.cell.get()).finish()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn the_value_is_built_on_first_use_and_kept() {
        let calls = Cell::new(0);
        let lazy = Lazy::new(|| {
            calls.set(calls.get() + 1);
            vec![1, 2, 3]
        });
        assert_eq!(calls.get(), 0);
        assert_eq!(lazy.len(), 3);
        assert_eq!(*Lazy::force(&lazy), vec![1, 2, 3]);
        assert_eq!(calls.get(), 1);
    }
}
