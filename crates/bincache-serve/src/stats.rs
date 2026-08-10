//! The counters `GET /metrics` reports.
//!
//! Relaxed throughout. A scrape reads a sample, which is all it ever was, and ordering
//! between counters would buy an accuracy no consumer of a Prometheus gauge can use.

use core::sync::atomic::Ordering;
use swrite::SWrite as _;

/// Cheap to clone: every handler holds one.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    counters: std::sync::Arc<Counters>,
}

#[derive(Debug, Default)]
struct Counters {
    requests: core::sync::atomic::AtomicU64,
    metadata_hits: core::sync::atomic::AtomicU64,
    metadata_misses: core::sync::atomic::AtomicU64,
    payload_bytes: core::sync::atomic::AtomicU64,
    uploads: core::sync::atomic::AtomicU64,
    rejections: core::sync::atomic::AtomicU64,
}

impl Stats {
    pub fn request(&self) {
        self.counters.requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn metadata_hit(&self) {
        self.counters.metadata_hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn metadata_miss(&self) {
        self.counters.metadata_misses.fetch_add(1, Ordering::Relaxed);
    }

    /// Counted when the response is framed, not as bytes leave the socket, so a client that
    /// hangs up mid-NAR still counts the whole span. This is what the payload plane was
    /// asked for rather than what it delivered.
    pub fn payload(&self, bytes: u64) {
        self.counters.payload_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn upload(&self) {
        self.counters.uploads.fetch_add(1, Ordering::Relaxed);
    }

    pub fn rejection(&self) {
        self.counters.rejections.fetch_add(1, Ordering::Relaxed);
    }

    #[must_use]
    pub fn total(&self) -> Totals {
        Totals {
            requests: self.counters.requests.load(Ordering::Relaxed),
            metadata_hits: self.counters.metadata_hits.load(Ordering::Relaxed),
            metadata_misses: self.counters.metadata_misses.load(Ordering::Relaxed),
            payload_bytes: self.counters.payload_bytes.load(Ordering::Relaxed),
            uploads: self.counters.uploads.load(Ordering::Relaxed),
            rejections: self.counters.rejections.load(Ordering::Relaxed),
        }
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
    pub fn render(&self, paths: u64) -> String {
        let mut body = String::with_capacity(512);
        for (name, value) in [
            ("bincache_requests_total", self.requests),
            ("bincache_metadata_hits_total", self.metadata_hits),
            ("bincache_metadata_misses_total", self.metadata_misses),
            ("bincache_payload_bytes_total", self.payload_bytes),
            ("bincache_uploads_total", self.uploads),
            ("bincache_rejections_total", self.rejections),
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

    #[test]
    fn counts_what_it_was_told() {
        let stats = crate::stats::Stats::default();
        for _ in 0..4 {
            stats.request();
        }
        stats.metadata_hit();
        stats.metadata_hit();
        stats.metadata_miss();
        stats.payload(2048);

        let totals = stats.total();
        assert_eq!(totals.requests, 4);
        assert_eq!(totals.metadata_hits, 2);
        assert_eq!(totals.metadata_misses, 1);
        assert_eq!(totals.payload_bytes, 2048);
        assert_eq!(totals.uploads, 0);
    }

    /// Clones share one set of counters, which is what lets every handler hold one.
    #[test]
    fn a_clone_counts_into_the_same_place() {
        let stats = crate::stats::Stats::default();
        let clone = stats.clone();
        clone.upload();
        assert_eq!(stats.total().uploads, 1);
    }

    #[test]
    fn renders_the_metrics_body() {
        let stats = crate::stats::Stats::default();
        stats.request();
        expect_test::expect![[r#"
            bincache_requests_total 1
            bincache_metadata_hits_total 0
            bincache_metadata_misses_total 0
            bincache_payload_bytes_total 0
            bincache_uploads_total 0
            bincache_rejections_total 0
            bincache_paths 42
        "#]]
        .assert_eq(&stats.total().render(42));
    }
}
