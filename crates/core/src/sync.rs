//! Locking a `Mutex` without panicking on poison. Google's Rust style guide says to panic on a poisoned mutex;
//! runnerd allows no panics outside tests (ADR 0016), so it takes the data a panicking holder left instead. It must
//! not be used on a mutex whose data a panic midway could leave inconsistent: each caller says why its data stays
//! usable.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Locks `mutex`, taking its data as a panicking holder left it.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
