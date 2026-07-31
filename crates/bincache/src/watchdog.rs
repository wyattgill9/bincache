//! The stall detector.
//!
//! Compio has no [glommio]-style stall detection, and a shard that wedges inside a long
//! operation looks exactly like a shard with no traffic. A per-shard heartbeat written
//! with a relaxed store, read from here, tells the two apart. This covers Compio's biggest
//! operability gap against glommio, which `research/DESIGN_V2.md` names as its cost.
//!
//! v1 reports rather than acts: a wedged shard is a bug to go read a stack for, and
//! restarting the process would hide it. The design's `SIGUSR1` stack dump is the named
//! next step.
//!
//! [glommio]: https://github.com/DataDog/glommio

/// How often the watchdog looks. Frequent enough that a stall is noticed inside a scrape
/// interval, rare enough that the thread is free.
const INTERVAL: core::time::Duration = core::time::Duration::from_secs(5);

/// Starts the watchdog on its own thread and lets it run for the life of the process.
///
/// Detached on purpose: bincache is crash-only, so there is no orderly shutdown for this
/// thread to participate in.
pub fn spawn(stats: bincache_serve::stats::Shards, stale: core::time::Duration) {
    let started = std::time::Instant::now();
    let spawned = std::thread::Builder::new()
        .name("bincache-watchdog".to_owned())
        .spawn(move || watch(&stats, started, stale));

    if let Err(error) = spawned {
        // Serving without a watchdog is degraded, not broken: it costs visibility into a
        // failure that has not happened yet, so it is not worth refusing to start over.
        tracing::error!(error = ?error, "watchdog thread did not start; stalls will be silent");
    }
}

fn watch(
    stats: &bincache_serve::stats::Shards,
    started: std::time::Instant,
    stale: core::time::Duration,
) {
    loop {
        std::thread::sleep(INTERVAL);
        let stalled = stats.stalled(started.elapsed(), stale);
        if !stalled.is_empty() {
            tracing::error!(
                shards = ?stalled,
                stale_seconds = stale.as_secs(),
                "shards have not reached the top of their accept loop; go read a stack"
            );
        }
    }
}
