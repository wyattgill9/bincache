//! Thread-per-core serving shards.
//!
//! [`selene`] owns the pool: one thread per shard, each pinned, each with its own Compio
//! runtime, its own `SO_REUSEPORT` listener, its own connection registry, and the stall
//! watchdog that Compio itself does not provide. What is left in this module is the
//! [`selene::listener::Service`] impl, which is one connection's request loop, and the
//! translation from bincache's configuration into selene's.
//!
//! Nothing mutable is shared between shards: the index, the store, and the token set are
//! read-only from here, and the counters are per-shard and padded.
//!
//! `SO_REUSEPORT` hashes a connection to a shard at accept time and never rebalances, so a
//! single machine pulling a large closure pins its stream to one shard while others idle.
//! `research/DESIGN_V2.md` accepts that skew as a known issue rather than mitigating it;
//! eBPF reuseport selection and a split accept queue for the payload plane are the named
//! fixes, to be revisited when it is measured.

use snafu::ResultExt as _;

/// Backlog handed to `listen`. Deep enough that a burst of closure resolution does not get
/// refused while a shard is mid-request.
const BACKLOG: core::num::NonZeroU32 = core::num::NonZeroU32::new(1024).expect("nonzero");

/// How often each shard proves its executor is still turning, and how often the watchdog
/// samples that.
///
/// The heartbeat is a timer task rather than a mark in the accept loop on purpose. A shard
/// parked in `accept` with no traffic is idle, not wedged, and beating from the accept loop
/// would report every quiet shard as stalled. A timer task only fails to run when something
/// on that shard is monopolizing the executor, which is exactly the condition worth
/// reporting.
const SAMPLE: core::time::Duration = core::time::Duration::from_secs(1);

/// How long shutdown waits for in-flight requests to finish before giving up on them. A
/// large NAR is served in 256 KiB chunks, so a connection mid-stream reaches a chunk
/// boundary well inside this.
///
/// Selene's drain polls its connection registry rather than cancelling connections, so an
/// idle keep-alive connection holds its slot until the deadline: a client that pushed a
/// closure and kept its connection open costs the full ten seconds at every restart. The
/// named fix is cancelling registered connections in selene, not a shorter deadline here,
/// which would truncate a stream instead.
const DRAIN: core::time::Duration = core::time::Duration::from_secs(10);

#[derive(Debug, snafu::Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("the shard pool failed"))]
    Pool { source: selene::shard::Error },

    #[snafu(display("starting the shutdown runtime failed"))]
    Runtime { source: std::io::Error },

    #[snafu(display("waiting for a shutdown signal failed"))]
    Signal { source: std::io::Error },
}

pub struct Config {
    pub address: std::net::SocketAddr,
    pub shards: core::num::NonZeroU16,
    /// Whether shards pin themselves to cores. Unpinned is the right answer on a shared
    /// machine and in tests; pinned is the point of the architecture on a dedicated box.
    pub pin: selene::shard::Affinity,
    /// How long a shard may go without a heartbeat before the watchdog says so.
    pub stall: core::time::Duration,
}

/// Serves until the process is signalled, then drains open connections and returns.
pub fn run(config: Config, cache: crate::handler::Cache) -> Result<(), Error> {
    let Config { address, shards, pin, stall } = config;

    let listen = selene::listener::Config {
        addr: address,
        backlog: BACKLOG,
        nodelay: true,
        // A cache with no connection limit refuses nothing under load; it queues. The
        // shard's own accept backlog is the bound, and skew makes a per-shard cap the
        // wrong knob anyway.
        max_connections: selene::listener::MaxConnections::Unlimited,
    };
    let pool = selene::shard::Shards::start(
        selene::shard::Config {
            count: selene::shard::Count::Exactly(shards),
            pin,
            watchdog: selene::watchdog::Policy::On { stall_after: stall, sample_every: SAMPLE },
            drain: DRAIN,
        },
        listen,
        move |shard| Shard { cache: cache.clone(), shard: usize::from(shard.index()) },
    )
    .context(PoolSnafu)?;

    tracing::info!(shards = pool.shards_count(), %address, "listening");

    // The wait runs on its own runtime on the calling thread, which is a scheduling peer of
    // the shards rather than one of them: it is parked in a syscall until the signal.
    compio::runtime::Runtime::new().context(RuntimeSnafu)?.block_on(signalled())?;

    tracing::info!("signalled; draining");
    pool.shutdown().context(PoolSnafu)
}

/// Resolves on the first stop signal. Both are watched because they arrive from different
/// places and mean the same thing here: `SIGINT` from a terminal, `SIGTERM` from a service
/// manager stopping the unit.
async fn signalled() -> Result<(), Error> {
    // Exact by construction: `nix::sys::signal::Signal` is `repr(i32)` and these are its
    // discriminants, not a numeric conversion.
    let interrupt =
        core::pin::pin!(compio::signal::unix::signal(nix::sys::signal::Signal::SIGINT as i32));
    let terminate =
        core::pin::pin!(compio::signal::unix::signal(nix::sys::signal::Signal::SIGTERM as i32));

    match futures_util::future::select(interrupt, terminate).await {
        futures_util::future::Either::Left((delivered, _terminate)) => {
            delivered.context(SignalSnafu)
        }
        futures_util::future::Either::Right((delivered, _interrupt)) => {
            delivered.context(SignalSnafu)
        }
    }
}

/// One shard's half of the cache. Selene builds one per shard, on that shard's thread, and
/// hands it every connection the kernel gave that shard.
#[derive(Clone)]
pub struct Shard {
    cache: crate::handler::Cache,
    shard: usize,
}

impl Shard {
    /// Public so a test can serve real connections over a real socket without starting a
    /// pool: `SO_REUSEPORT` needs a concrete port, and parallel test binaries need an
    /// ephemeral one.
    #[must_use]
    pub const fn new(cache: crate::handler::Cache, shard: usize) -> Self {
        Self { cache, shard }
    }
}

impl selene::listener::Service for Shard {
    /// The typed error ends here. Selene's contract is `io::Result`, and a connection that
    /// failed is a per-peer fault the shard reports and moves past, so the chain is logged
    /// with its source rather than flattened into the return value.
    async fn serve(
        &self,
        conn: compio::net::TcpStream,
        peer: std::net::SocketAddr,
    ) -> std::io::Result<()> {
        if let Err(error) = session(&self.cache, self.shard, conn).await {
            tracing::debug!(shard = self.shard, %peer, error = ?error, "connection ended");
        }
        Ok(())
    }
}

/// One connection, for as long as it stays alive.
async fn session(
    cache: &crate::handler::Cache,
    shard: usize,
    stream: compio::net::TcpStream,
) -> Result<(), crate::handler::Error> {
    let mut connection = crate::connection::Connection::new(stream);
    loop {
        let Some(head) = connection.head().await.context(crate::handler::ConnectionSnafu)? else {
            return Ok(());
        };

        let Ok(request) = crate::http::parse(&head) else {
            let bytes = crate::http::response::bare(
                crate::http::response::Status::BadRequest,
                crate::http::KeepAlive::Close,
                0,
            );
            connection.write(bytes).await.context(crate::handler::ConnectionSnafu)?;
            return Ok(());
        };

        let mut exchange =
            crate::handler::Exchange { shard, connection: &mut connection, request: &request };
        let keep_alive = cache.handle(&mut exchange).await?;
        if keep_alive == crate::http::KeepAlive::Close {
            return Ok(());
        }
    }
}
