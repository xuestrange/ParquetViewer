//! Greedy per-window resources and bounded local display caches.
//!
//! Every Engine may use all available CPU workers and its own DataFusion managed
//! memory allowance. There is no process-wide operation gate, queue, CPU pool,
//! memory pool, or idle reservation. These allowances are not process RSS limits.
//!
//! Each Arrow result has independent accounting without a fixed total-size cap.
//! All imported typed buffers stay in RAM. Cloning a lease shares its charge
//! until the final clone drops; an old query snapshot cannot prevent a replacement
//! result from loading. Formatted caches
//! are limited per Client, with a smaller quota while its window is inactive.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use anyhow::{Result, ensure};

const MIB: usize = 1024 * 1024;
pub const ACTIVE_CACHE_BYTES: usize = 8 * MIB;
pub const INACTIVE_CACHE_BYTES: usize = MIB;

/// A window/Engine's local format-cache accounting. Clones share only this
/// Client's cache quota; other clients never consume or wait for its capacity.
#[derive(Clone, Debug)]
pub struct Client {
    inner: Arc<ClientInner>,
}

#[derive(Debug)]
struct ClientInner {
    active: AtomicBool,
    cache_bytes: AtomicUsize,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ClientInner {
                active: AtomicBool::new(true),
                cache_bytes: AtomicUsize::new(0),
            }),
        }
    }

    /// Only affects the local format-cache quota. CPU and query memory remain
    /// greedy in background windows too. Existing cache leases remain valid;
    /// callers should trim their cache after deactivation to meet the new quota.
    pub fn set_active(&self, active: bool) {
        self.inner.active.store(active, Ordering::Release);
    }

    pub fn cache_limit_bytes(&self) -> usize {
        self.inner.cache_limit_bytes()
    }

    /// Starts independent accounting for owned typed Arrow buffers. This records
    /// bytes, rather than limiting how many rows the user is allowed to import.
    /// Retained snapshots and other windows never compete for an allowance.
    pub fn reserve_display(&self, bytes: usize) -> Result<MemoryLease> {
        Ok(MemoryLease::display(bytes))
    }

    /// Lock-free, nonwaiting reservation from this Client's local cache quota.
    /// None means the current quota has insufficient room. A concurrent blur
    /// may lower the quota after admission; existing allocations are not revoked.
    pub fn try_reserve_cache(&self, bytes: usize) -> Option<MemoryLease> {
        self.inner.try_reserve_cache(bytes)?;
        Some(MemoryLease::new(
            MemoryPool::Cache(self.inner.clone()),
            bytes,
        ))
    }
}

impl ClientInner {
    fn cache_limit_bytes(&self) -> usize {
        if self.active.load(Ordering::Acquire) {
            ACTIVE_CACHE_BYTES
        } else {
            INACTIVE_CACHE_BYTES
        }
    }

    fn try_reserve_cache(&self, bytes: usize) -> Option<()> {
        self.cache_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                let next = current.checked_add(bytes)?;
                (next <= self.cache_limit_bytes()).then_some(next)
            })
            .ok()
            .map(|_| ())
    }
}

#[derive(Debug)]
enum MemoryPool {
    Display,
    Cache(Arc<ClientInner>),
}

/// An Arc-shared local allocation charge. A clone never charges the same bytes
/// twice. The final clone returns cache capacity even if its Engine is closed.
#[derive(Clone, Debug)]
#[must_use = "Keep the lease alive for as long as the associated buffers are retained"]
pub struct MemoryLease {
    inner: Arc<LeaseInner>,
}

#[derive(Debug)]
struct LeaseInner {
    pool: MemoryPool,
    bytes: AtomicUsize,
    growth: Mutex<()>,
}

impl MemoryLease {
    pub(crate) fn display(bytes: usize) -> Self {
        Self::new(MemoryPool::Display, bytes)
    }

    pub(crate) fn is_display(&self) -> bool {
        matches!(self.inner.pool, MemoryPool::Display)
    }

    fn new(pool: MemoryPool, bytes: usize) -> Self {
        Self {
            inner: Arc::new(LeaseInner {
                pool,
                bytes: AtomicUsize::new(bytes),
                growth: Mutex::new(()),
            }),
        }
    }

    pub fn bytes(&self) -> usize {
        self.inner.bytes.load(Ordering::Acquire)
    }

    /// Monotonic growth, serialized only with clones of this very lease. It
    /// never waits for another result/window to release capacity. Failed growth
    /// or cancellation leaves the previous charge intact. Shrinking cannot
    /// reclaim bytes because a cloned result may still retain the old buffers.
    pub fn grow_to(&self, total: usize, cancel: &AtomicBool) -> Result<()> {
        ensure!(!cancel.load(Ordering::Acquire), "Operation cancelled");
        let _growth = self
            .inner
            .growth
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        ensure!(!cancel.load(Ordering::Acquire), "Operation cancelled");
        let previous = self.bytes();
        if total <= previous {
            return Ok(());
        }
        match &self.inner.pool {
            MemoryPool::Display => {}
            MemoryPool::Cache(client) => ensure!(
                client.try_reserve_cache(total - previous).is_some(),
                "Window format cache exceeds its current local quota"
            ),
        }
        self.inner.bytes.store(total, Ordering::Release);
        Ok(())
    }
}

impl Drop for LeaseInner {
    fn drop(&mut self) {
        if let MemoryPool::Cache(client) = &self.pool {
            let bytes = self.bytes.load(Ordering::Relaxed);
            let previous = client.cache_bytes.fetch_sub(bytes, Ordering::AcqRel);
            debug_assert!(previous >= bytes, "Cache lease accounting underflow");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Barrier, thread};

    #[test]
    fn snapshots_and_windows_have_independent_arrow_result_budgets() {
        const LARGE_ARROW_BYTES: usize = 256 * MIB;
        let first = Client::new();
        let second = Client::new();
        let old = first.reserve_display(LARGE_ARROW_BYTES).unwrap();
        let replacement = first.reserve_display(0).unwrap();
        let other_window = second.reserve_display(LARGE_ARROW_BYTES).unwrap();
        replacement
            .grow_to(LARGE_ARROW_BYTES, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(old.bytes(), LARGE_ARROW_BYTES);
        assert_eq!(replacement.bytes(), LARGE_ARROW_BYTES);
        assert_eq!(other_window.bytes(), LARGE_ARROW_BYTES);
        old.grow_to(LARGE_ARROW_BYTES * 2, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(old.bytes(), LARGE_ARROW_BYTES * 2);
        assert_eq!(replacement.bytes(), LARGE_ARROW_BYTES);
    }

    #[test]
    fn cancelled_growth_retains_the_old_arrow_charge() {
        let lease = Client::new().reserve_display(100).unwrap();
        let clone = lease.clone();
        assert!(lease.grow_to(200, &AtomicBool::new(true)).is_err());
        assert_eq!(clone.bytes(), 100);
        clone.grow_to(200, &AtomicBool::new(false)).unwrap();
        lease.grow_to(1, &AtomicBool::new(false)).unwrap();
        assert_eq!(lease.bytes(), 200);
    }

    #[test]
    fn cache_quota_is_shared_only_by_clones_of_the_same_window() {
        let first = Client::new();
        let first_clone = first.clone();
        let second = Client::new();
        let held = first.try_reserve_cache(ACTIVE_CACHE_BYTES).unwrap();
        assert!(first_clone.try_reserve_cache(1).is_none());
        assert!(second.try_reserve_cache(ACTIVE_CACHE_BYTES).is_some());
        first.set_active(false);
        assert_eq!(first_clone.cache_limit_bytes(), INACTIVE_CACHE_BYTES);
        assert_eq!(second.cache_limit_bytes(), ACTIVE_CACHE_BYTES);
        assert!(first_clone.try_reserve_cache(1).is_none());
        drop(held);
        assert!(first.try_reserve_cache(INACTIVE_CACHE_BYTES).is_some());
    }

    #[test]
    fn final_cache_lease_clone_returns_capacity_after_owner_window_closes() {
        let client = Client::new();
        let observer = Arc::downgrade(&client.inner);
        let lease = client.try_reserve_cache(ACTIVE_CACHE_BYTES).unwrap();
        let retained = lease.clone();
        drop(client);
        drop(lease);
        let owner = observer.upgrade().unwrap();
        assert_eq!(
            owner.cache_bytes.load(Ordering::Acquire),
            ACTIVE_CACHE_BYTES
        );
        drop(retained);
        assert_eq!(owner.cache_bytes.load(Ordering::Acquire), 0);
        drop(owner);
        assert!(observer.upgrade().is_none());
    }

    #[test]
    fn concurrent_cache_admission_and_growth_cannot_overcommit_local_quota() {
        let client = Client::new();
        let barrier = Arc::new(Barrier::new(17));
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let client = client.clone();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    barrier.wait();
                    let lease = client.try_reserve_cache(MIB);
                    barrier.wait();
                    lease
                })
            })
            .collect();
        barrier.wait();
        barrier.wait();
        let leases: Vec<_> = threads
            .into_iter()
            .filter_map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(leases.len(), 8);
        assert_eq!(
            client.inner.cache_bytes.load(Ordering::Acquire),
            ACTIVE_CACHE_BYTES
        );
        assert!(leases[0].grow_to(2 * MIB, &AtomicBool::new(false)).is_err());
        drop(leases);
        assert_eq!(client.inner.cache_bytes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn cache_growth_errors_and_unwinding_release_exact_local_charges() {
        let client = Client::new();
        let lease = client.try_reserve_cache(100).unwrap();
        lease.grow_to(200, &AtomicBool::new(false)).unwrap();
        assert!(lease.grow_to(usize::MAX, &AtomicBool::new(false)).is_err());
        assert_eq!(client.inner.cache_bytes.load(Ordering::Acquire), 200);
        assert_eq!(lease.bytes(), 200);
        drop(lease);
        let result = std::panic::catch_unwind(|| {
            let _lease = client.try_reserve_cache(ACTIVE_CACHE_BYTES).unwrap();
            panic!("simulated native error cleanup");
        });
        assert!(result.is_err());
        assert_eq!(client.inner.cache_bytes.load(Ordering::Acquire), 0);
    }
}
