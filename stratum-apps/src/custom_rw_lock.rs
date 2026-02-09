//! A custom read write lock safe implementation

use std::sync::{PoisonError, RwLock as InnerRwLock, RwLockReadGuard, RwLockWriteGuard};

/// A thin wrapper around [`std::sync::RwLock`] with an explicit locking policy.
///
/// This type exists to provide clearer, more ergonomic locking APIs while
/// preserving the semantics of `std::sync::RwLock`.
///
/// Higher-level methods on this type distinguish between:
/// - Scoped, closure-based access, which prevents lock guards from escaping
/// - Explicit guard-based access, for advanced use cases that require flexible control flow
#[derive(Debug)]
pub struct RwLock<T: ?Sized>(InnerRwLock<T>);

impl<T> RwLock<T> {
    /// Creates a new `RwLock` protecting `value`.
    pub fn new(value: T) -> Self {
        Self(InnerRwLock::new(value))
    }
}

impl<T: ?Sized> RwLock<T> {
    /// Executes `f` while holding a read lock.
    ///
    /// The lock guard cannot escape this method.
    /// Prefer this over [`read`] for small, self-contained operations.
    pub fn safe_read<F, R>(&self, f: F) -> Result<R, PoisonError<RwLockReadGuard<'_, T>>>
    where
        F: FnOnce(&T) -> R,
    {
        let guard = self.0.read()?;
        Ok(f(&*guard))
    }

    /// Executes `f` while holding a write lock.
    ///
    /// The lock guard cannot escape this method.
    /// Poisoning is propagated to the caller.
    pub fn safe_write<F, R>(&self, f: F) -> Result<R, PoisonError<RwLockWriteGuard<'_, T>>>
    where
        F: FnOnce(&mut T) -> R,
    {
        let mut guard = self.0.write()?;
        Ok(f(&mut *guard))
    }

    /// Acquires a read lock and returns the guard directly.
    ///
    /// This is an API intended for complex control flow where
    /// closure-based locking would harm readability.
    pub fn read(&self) -> Result<RwLockReadGuard<'_, T>, PoisonError<RwLockReadGuard<'_, T>>> {
        self.0.read()
    }

    /// Acquires a write lock and returns the guard directly.
    ///
    /// Callers are responsible for keeping the
    /// guard scope small and avoiding `.await` while holding it.
    pub fn write(&self) -> Result<RwLockWriteGuard<'_, T>, PoisonError<RwLockWriteGuard<'_, T>>> {
        self.0.write()
    }
}
