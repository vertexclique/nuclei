//! The guards that cancel a token when dropped.

use super::CancellationToken;

/// A wrapper for a [`CancellationToken`] which cancels it on drop, created by
/// [`CancellationToken::drop_guard`].
///
/// ```
/// use nuclei::sync::CancellationToken;
///
/// let token = CancellationToken::new();
/// let guard = token.clone().drop_guard();
/// let still_alive = guard.disarm();
/// assert!(!token.is_cancelled());
/// drop(still_alive);
/// assert!(!token.is_cancelled());
/// ```
#[derive(Debug)]
pub struct DropGuard {
    pub(super) token: CancellationToken,
    pub(super) armed: bool,
}

impl DropGuard {
    /// Returns a reference to the token this guard wraps.
    pub fn token(&self) -> &CancellationToken {
        &self.token
    }

    /// Returns the token and disarms this guard: it no longer cancels the
    /// token. Other guards of the same token are not affected.
    pub fn disarm(mut self) -> CancellationToken {
        self.armed = false;
        self.token.clone()
    }
}

impl Drop for DropGuard {
    fn drop(&mut self) {
        if self.armed {
            self.token.cancel();
        }
    }
}

/// A wrapper for a borrowed [`CancellationToken`] which cancels it on drop,
/// created by [`CancellationToken::drop_guard_ref`].
#[derive(Debug)]
pub struct DropGuardRef<'a> {
    pub(super) token: &'a CancellationToken,
    pub(super) armed: bool,
}

impl<'a> DropGuardRef<'a> {
    /// Returns a reference to the token this guard wraps.
    pub fn token(&self) -> &CancellationToken {
        self.token
    }

    /// Returns the token and disarms this guard: it no longer cancels the
    /// token. Other guards of the same token are not affected.
    pub fn disarm(mut self) -> &'a CancellationToken {
        self.armed = false;
        self.token
    }
}

impl Drop for DropGuardRef<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.token.cancel();
        }
    }
}
