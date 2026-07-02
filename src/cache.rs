//! Warm-cache + stale-while-revalidate infrastructure for the heavy `/api` endpoints.
//!
//! The scanner / finder / regime / swing endpoints each loop the full ~1500-symbol
//! parquet archive and take 10–60s. Recomputing them on the request path is the
//! single biggest source of UI latency. This module fixes that with two cache
//! shapes that **serve instantly and refresh in the background**:
//!
//! - [`Cached<T>`]   — one global value (scanner, regime, swing). Inputs are fixed.
//! - [`KeyedCache<T>`] — a family keyed on quantized (capital, risk) for the finder
//!   and staging, whose results depend on the user's budget.
//!
//! Both follow the same discipline as the existing scanner handler: every method
//! takes a *synchronous* guard, clones/returns owned data, and drops the guard
//! before returning — so **no lock is ever held across an `.await`**. A lock-free
//! single-flight guard (`AtomicBool`) guarantees that however many requests pile
//! up on a cold/stale slot, **at most one recompute runs at a time** (no stampede).
//!
//! Honesty invariant: every cached value carries `built_ist` so the UI can show
//! "built <IST>" and a stale value is never presented as live.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// A cached value with freshness metadata.
#[derive(Clone)]
struct Entry<T> {
    value: T,
    built_at: Instant,
    /// Human/UI timestamp, e.g. "2026-06-28 09:31:04" (IST). Display only.
    built_ist: String,
    /// Out-of-band staleness (e.g. a symbol was onboarded): the value is still
    /// served, but the next lookup reports stale and kicks a refresh.
    stale_override: bool,
}

/// Result of a [`Cached::lookup`]: the current value (if any) and whether it is
/// older than the TTL and should trigger a background refresh.
pub struct Lookup<T> {
    pub value: Option<T>,
    pub stale: bool,
}

/// A single stale-while-revalidate cache slot with a single-flight refresh guard.
pub struct Cached<T> {
    inner: RwLock<Option<Entry<T>>>,
    /// Single-flight: at most one refresh in flight. Lock-free CAS.
    refreshing: AtomicBool,
    ttl: Duration,
}

impl<T: Clone> Cached<T> {
    pub fn new(ttl: Duration) -> Self {
        Cached {
            inner: RwLock::new(None),
            refreshing: AtomicBool::new(false),
            ttl,
        }
    }

    /// Clone out the current value plus a staleness flag. The read guard is
    /// dropped before returning, so this is never held across an `.await`.
    pub fn lookup(&self) -> Lookup<T> {
        let guard = self.inner.read().unwrap();
        match guard.as_ref() {
            Some(e) => Lookup {
                value: Some(e.value.clone()),
                stale: e.stale_override || e.built_at.elapsed() >= self.ttl,
            },
            None => Lookup { value: None, stale: true },
        }
    }

    /// Mark the cached value stale WITHOUT dropping it: it keeps being served
    /// (stale-while-revalidate) while the next lookup kicks a background
    /// refresh. For out-of-band data changes the TTL can't see — e.g. a symbol
    /// onboarded via /add_stock while the long-TTL scanner cache is warm.
    pub fn mark_stale(&self) {
        if let Some(e) = self.inner.write().unwrap().as_mut() {
            e.stale_override = true;
        }
    }

    /// Whether a value is present (regardless of staleness). Part of the cache's
    /// public surface; exercised by tests.
    #[allow(dead_code)]
    pub fn is_populated(&self) -> bool {
        self.inner.read().unwrap().is_some()
    }

    /// Try to claim the single-flight refresh slot. Returns `true` iff THIS caller
    /// won the race and is responsible for performing the refresh. Everyone else
    /// gets `false` and should serve the stale value.
    pub fn try_begin_refresh(&self) -> bool {
        self.refreshing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Store a freshly computed value and release the single-flight slot.
    pub fn store(&self, value: T, built_ist: String) {
        {
            let mut guard = self.inner.write().unwrap();
            *guard = Some(Entry {
                value,
                built_at: Instant::now(),
                built_ist,
                stale_override: false,
            });
        }
        self.refreshing.store(false, Ordering::Release);
    }

    /// Release the single-flight slot WITHOUT storing — the refresh failed, so a
    /// later request may retry. The previous value (if any) remains served.
    pub fn abort_refresh(&self) {
        self.refreshing.store(false, Ordering::Release);
    }
}

// ===========================================================================
// Keyed cache (finder / staging) — depends on (capital, risk)
// ===========================================================================

/// Quantized (capital, risk) cache key. Capital is bucketed to ₹1,000 and risk to
/// 1 basis point so trivially-different query params reuse a slot. The staging
/// tiers (`CAPITAL_POOL` × {0.5%, 1%, 2%}) map to stable keys here, letting the
/// desk's staging console share the finder page's cache.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CapRiskKey {
    cap_bucket: u64,
    risk_bp: u32,
}

impl CapRiskKey {
    pub fn new(capital: f64, risk_pct: f64) -> Self {
        CapRiskKey {
            cap_bucket: (capital / 1000.0).round().max(0.0) as u64,
            risk_bp: (risk_pct * 10_000.0).round().max(0.0) as u32,
        }
    }
}

/// A bounded family of [`Cached`] slots keyed by [`CapRiskKey`]. When the number
/// of distinct keys exceeds `cap`, the slot with the oldest populated value is
/// evicted (simple, deterministic; the working set is a handful of tiers).
pub struct KeyedCache<T> {
    map: RwLock<HashMap<CapRiskKey, Arc<Cached<T>>>>,
    ttl: Duration,
    cap: usize,
}

impl<T: Clone> KeyedCache<T> {
    pub fn new(ttl: Duration, cap: usize) -> Self {
        KeyedCache { map: RwLock::new(HashMap::new()), ttl, cap }
    }

    /// Get (or create) the per-key cache slot. Returns an `Arc` so the caller can
    /// hold it past the map guard (which is dropped before returning).
    pub fn slot(&self, key: CapRiskKey) -> Arc<Cached<T>> {
        // Fast path: already present.
        if let Some(slot) = self.map.read().unwrap().get(&key) {
            return slot.clone();
        }
        // Slow path: create under the write lock (double-checked).
        let mut map = self.map.write().unwrap();
        if let Some(slot) = map.get(&key) {
            return slot.clone();
        }
        if map.len() >= self.cap {
            // Evict the least-recently-built populated slot (best effort).
            let victim = map
                .iter()
                .filter_map(|(k, v)| {
                    v.inner.read().unwrap().as_ref().map(|e| (*k, e.built_at))
                })
                .min_by_key(|(_, t)| *t)
                .map(|(k, _)| k);
            if let Some(k) = victim {
                map.remove(&k);
            } else if let Some(k) = map.keys().next().copied() {
                map.remove(&k);
            }
        }
        let slot = Arc::new(Cached::new(self.ttl));
        map.insert(key, slot.clone());
        slot
    }

    /// Snapshot the currently-cached keys. Part of the public surface; exercised
    /// by tests (the scheduler refreshes well-known keys explicitly).
    #[allow(dead_code)]
    pub fn keys(&self) -> Vec<CapRiskKey> {
        self.map.read().unwrap().keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_lookup_is_empty_and_stale() {
        let c: Cached<u32> = Cached::new(Duration::from_secs(60));
        let l = c.lookup();
        assert!(l.value.is_none());
        assert!(l.stale);
        assert!(!c.is_populated());
    }

    #[test]
    fn store_then_lookup_fresh() {
        let c: Cached<u32> = Cached::new(Duration::from_secs(60));
        assert!(c.try_begin_refresh());
        c.store(42, "2026-06-28 09:30:00".into());
        let l = c.lookup();
        assert_eq!(l.value, Some(42));
        assert!(!l.stale);
        assert!(c.is_populated());
    }

    #[test]
    fn single_flight_only_one_winner() {
        let c: Cached<u32> = Cached::new(Duration::from_secs(60));
        assert!(c.try_begin_refresh(), "first caller wins");
        assert!(!c.try_begin_refresh(), "second caller must lose");
        c.abort_refresh();
        assert!(c.try_begin_refresh(), "slot reopens after abort");
    }

    #[test]
    fn mark_stale_keeps_value_but_reports_stale() {
        let c: Cached<u32> = Cached::new(Duration::from_secs(3600));
        c.try_begin_refresh();
        c.store(9, "t".into());
        assert!(!c.lookup().stale, "fresh within a long TTL");
        c.mark_stale();
        let l = c.lookup();
        assert_eq!(l.value, Some(9), "stale value still served (SWR)");
        assert!(l.stale, "out-of-band staleness reported");
        // A refresh clears the override.
        c.try_begin_refresh();
        c.store(10, "t2".into());
        assert!(!c.lookup().stale);
    }

    #[test]
    fn zero_ttl_is_immediately_stale() {
        let c: Cached<u32> = Cached::new(Duration::from_secs(0));
        c.try_begin_refresh();
        c.store(7, "t".into());
        assert!(c.lookup().stale, "ttl=0 ⇒ always stale (always refresh)");
    }

    #[test]
    fn caprisk_key_quantization() {
        // Within the same ₹1k bucket and 1bp risk band ⇒ same key.
        assert_eq!(CapRiskKey::new(1_000_000.0, 0.01), CapRiskKey::new(1_000_400.0, 0.01004));
        // Different bucket ⇒ different key.
        assert_ne!(CapRiskKey::new(1_000_000.0, 0.01), CapRiskKey::new(1_002_000.0, 0.01));
    }

    #[test]
    fn keyed_cache_evicts_over_cap() {
        let kc: KeyedCache<u32> = KeyedCache::new(Duration::from_secs(60), 2);
        let k1 = CapRiskKey::new(100_000.0, 0.01);
        let k2 = CapRiskKey::new(200_000.0, 0.01);
        let k3 = CapRiskKey::new(300_000.0, 0.01);
        for (k, v) in [(k1, 1u32), (k2, 2)] {
            let s = kc.slot(k);
            s.try_begin_refresh();
            s.store(v, "t".into());
        }
        // Third distinct key forces eviction; cap stays at 2.
        let s3 = kc.slot(k3);
        s3.try_begin_refresh();
        s3.store(3, "t".into());
        assert!(kc.keys().len() <= 2, "cap enforced");
    }
}
