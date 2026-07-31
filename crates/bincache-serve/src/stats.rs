//! Per-shard counters and the heartbeat the watchdog reads.
//!
//! Counters are written with relaxed stores by the owning shard only and harvested from
//! elsewhere, so the serving path never takes a lock and never contends a cache line.
//! Padding is 128 bytes rather than 64 because the adjacent-cache-line prefetcher makes
//! 64-byte padding insufficient.

use core::sync::atomic::Ordering;
use swrite::SWrite as _;

/// Written by the shard on every loop turn, read by a foreign thread. Compio has no
/// [glommio]-style stall detection, so this is what makes a wedged shard visible.
///
/// [glommio]: https://github.com/DataDog/glommio
#[derive(Debug, Default)]
pub struct Shard {
    pub requests: core::sync::atomic::AtomicU64,
    pub metadata_hits: core::sync::atomic::AtomicU64,
    pub metadata_misses: core::sync::atomic::AtomicU64,
    pub payload_bytes: core::sync::atomic::AtomicU64,
    pub uploads: core::sync::atomic::AtomicU64,
    pub rejections: core::sync::atomic::AtomicU64,
    /// Milliseconds since the counters were created, at the shard's last heartbeat.
    /// Milliseconds rather than seconds so a stall is visible well inside one scrape.
    pub heartbeat: core::sync::atomic::AtomicU64,
    /// How many times this shard has beaten. Zero means it has not started, which a
    /// timestamp alone cannot express: a shard that beat immediately also reads zero.
    pub heartbeats: core::sync::atomic::AtomicU64,
}

impl Shard {
    /// Relaxed because ordering against other counters buys nothing: a harvester reading a
    /// slightly stale count is reading a sample, and that is all it ever was.
    pub fn bump(counter: &core::sync::atomic::AtomicU64, by: u64) {
        counter.fetch_add(by, Ordering::Relaxed);
    }
}

/// One padded slot per shard, shared with the harvester and the watchdog.
#[derive(Clone)]
pub struct Shards {
    slots: std::sync::Arc<[crossbeam_utils::CachePadded<Shard>]>,
    /// The epoch heartbeats are measured against. Owned here so no caller has to carry an
    /// `Instant` alongside the counters and keep the two agreeing.
    started: std::time::Instant,
}

impl Shards {
    #[must_use]
    pub fn new(count: core::num::NonZeroUsize) -> Self {
        let slots: Vec<crossbeam_utils::CachePadded<Shard>> =
            (0..count.get()).map(|_| crossbeam_utils::CachePadded::new(Shard::default())).collect();
        Self { slots: slots.into(), started: std::time::Instant::now() }
    }

    /// Records that this shard's executor is still turning.
    pub fn beat(&self, shard: usize) {
        let elapsed = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.slots[shard].heartbeat.store(elapsed, Ordering::Relaxed);
        self.slots[shard].heartbeats.fetch_add(1, Ordering::Relaxed);
    }

    #[must_use]
    pub fn get(&self, shard: usize) -> &Shard {
        &self.slots[shard]
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Sums every shard's counters. Not atomic across shards, and does not need to be.
    #[must_use]
    pub fn total(&self) -> Totals {
        let mut totals = Totals::default();
        for slot in self.slots.iter() {
            totals.requests += slot.requests.load(Ordering::Relaxed);
            totals.metadata_hits += slot.metadata_hits.load(Ordering::Relaxed);
            totals.metadata_misses += slot.metadata_misses.load(Ordering::Relaxed);
            totals.payload_bytes += slot.payload_bytes.load(Ordering::Relaxed);
            totals.uploads += slot.uploads.load(Ordering::Relaxed);
            totals.rejections += slot.rejections.load(Ordering::Relaxed);
        }
        totals
    }

    /// Shards whose heartbeat is older than `stale`. A non-empty answer means a shard is
    /// wedged, which is exactly the failure a chunked-send discipline is meant to prevent
    /// and therefore the one worth watching for.
    ///
    /// A shard that has never beaten is not reported: it has not started, which is not the
    /// same as being stuck.
    #[must_use]
    pub fn stalled(&self, stale: core::time::Duration) -> Vec<usize> {
        let now = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let stale = u64::try_from(stale.as_millis()).unwrap_or(u64::MAX);
        let deadline = now.saturating_sub(stale);
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| {
                let started = slot.heartbeats.load(Ordering::Relaxed) > 0;
                started && slot.heartbeat.load(Ordering::Relaxed) < deadline
            })
            .map(|(shard, _)| shard)
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub requests: u64,
    pub metadata_hits: u64,
    pub metadata_misses: u64,
    pub payload_bytes: u64,
    pub uploads: u64,
    pub rejections: u64,
}

impl Totals {
    /// Prometheus text exposition, which is what `GET /metrics` answers with.
    #[must_use]
    pub fn render(&self, shards: usize, paths: u64) -> String {
        let mut body = String::with_capacity(512);
        for (name, value) in [
            ("bincache_requests_total", self.requests),
            ("bincache_metadata_hits_total", self.metadata_hits),
            ("bincache_metadata_misses_total", self.metadata_misses),
            ("bincache_payload_bytes_total", self.payload_bytes),
            ("bincache_uploads_total", self.uploads),
            ("bincache_rejections_total", self.rejections),
            ("bincache_shards", u64::try_from(shards).unwrap_or(u64::MAX)),
            ("bincache_paths", paths),
        ] {
            swrite::swriteln!(body, "{name} {value}");
        }
        body
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    fn shards(count: usize) -> crate::stats::Shards {
        crate::stats::Shards::new(core::num::NonZeroUsize::new(count).expect("nonzero"))
    }

    #[test]
    fn totals_sum_across_shards() {
        let shards = shards(4);
        for shard in 0..shards.len() {
            crate::stats::Shard::bump(&shards.get(shard).requests, 10);
            crate::stats::Shard::bump(&shards.get(shard).metadata_hits, 3);
        }
        let totals = shards.total();
        assert_eq!(totals.requests, 40);
        assert_eq!(totals.metadata_hits, 12);
        assert_eq!(totals.metadata_misses, 0);
    }

    /// A shard parked in `accept` with no traffic still beats, so silence means wedged.
    /// Shards 0 and 2 never beat: they have not started, which is a different thing and
    /// must not be reported.
    #[test]
    fn a_shard_that_stopped_beating_is_reported() {
        let shards = shards(3);
        shards.beat(1);
        std::thread::sleep(core::time::Duration::from_millis(60));
        assert_eq!(shards.stalled(core::time::Duration::from_millis(10)), vec![1]);
    }

    #[test]
    fn a_shard_that_kept_beating_is_not_reported() {
        let shards = shards(2);
        shards.beat(0);
        shards.beat(1);
        assert_eq!(shards.stalled(core::time::Duration::from_secs(30)), Vec::new());
    }

    #[test]
    fn renders_the_metrics_body() {
        let shards = shards(1);
        crate::stats::Shard::bump(&shards.get(0).requests, 7);
        expect_test::expect![[r#"
            bincache_requests_total 7
            bincache_metadata_hits_total 0
            bincache_metadata_misses_total 0
            bincache_payload_bytes_total 0
            bincache_uploads_total 0
            bincache_rejections_total 0
            bincache_shards 1
            bincache_paths 42
        "#]]
        .assert_eq(&shards.total().render(1, 42));
    }
}
