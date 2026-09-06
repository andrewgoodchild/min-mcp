//! Poison-tolerant lock helpers.
//!
//! A panic while one request holds a lock must not take every other caller's
//! cache, breaker, or index down with it: the data behind these locks is
//! either regenerable (caches, buckets) or append-only (usage counts), so
//! continuing with the possibly-partial value beats poisoning the process.
//! One spelling, so a change of policy is one edit.

use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn read<T>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn write<T>(l: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(PoisonError::into_inner)
}
