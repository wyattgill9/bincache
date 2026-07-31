//! Thread-per-core serving shards.
//!
//! One shard per core, each pinned, each with its own `SO_REUSEPORT` listener and its own
//! Compio runtime. Nothing mutable is shared: the index, the store, and the token set are
//! read-only from here, and the counters are per-shard and padded.
//!
//! `SO_REUSEPORT` hashes a connection to a shard at accept time and never rebalances, so a
//! single machine pulling a large closure pins its stream to one shard while others idle.
//! `research/DESIGN_V2.md` accepts that skew as a known issue rather than mitigating it;
//! eBPF reuseport selection and a split accept queue for the payload plane are the named
//! fixes, to be revisited when it is measured.

use snafu::OptionExt as _;
use snafu::ResultExt as _;

/// Backlog handed to `listen`. Deep enough that a burst of closure resolution does not get
/// refused while a shard is mid-request.
const BACKLOG: i32 = 1024;

/// How often each shard proves its executor is still turning.
///
/// The heartbeat runs on a timer task rather than in the accept loop on purpose. A shard
/// parked in `accept` with no traffic is idle, not wedged, and beating from the accept loop
/// would report every quiet shard as stalled. A timer task only fails to run when something
/// on that shard is monopolizing the executor, which is exactly the condition worth
/// reporting.
const HEARTBEAT: core::time::Duration = core::time::Duration::from_secs(1);

#[derive(Debug, snafu::Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("creating the listening socket failed"))]
    Socket { source: std::io::Error },

    #[snafu(display("binding {address} failed"))]
    Bind { address: std::net::SocketAddr, source: std::io::Error },

    #[snafu(display("adopting the listener into the runtime failed"))]
    Adopt { source: std::io::Error },

    #[snafu(display("starting the shard runtime failed"))]
    Runtime { source: std::io::Error },

    #[snafu(display("shard {shard} panicked"))]
    Panicked { shard: usize },
}

/// Whether shards pin themselves to cores. Unpinned is the right answer on a shared
/// machine and in tests; pinned is the point of the architecture on a dedicated box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pinning {
    Pinned,
    Unpinned,
}

pub struct Config {
    pub address: std::net::SocketAddr,
    pub shards: core::num::NonZeroUsize,
    pub pinning: Pinning,
}

/// Runs until every shard thread exits, which under crash-only operation means until the
/// process is killed.
pub fn run(config: Config, cache: crate::handler::Cache) -> Result<(), Error> {
    let Config { address, shards, pinning } = config;
    let cores = core_affinity::get_core_ids().unwrap_or_default();

    let mut threads = Vec::with_capacity(shards.get());
    for shard in 0..shards.get() {
        // Each shard binds its own socket. Building it here rather than in the thread means
        // a bad address fails before any thread starts.
        let listener = bind(address)?;
        let cache = cache.clone();
        let core = cores.get(shard % cores.len().max(1)).copied();

        let thread = std::thread::Builder::new()
            .name(format!("bincache-shard-{shard}"))
            .spawn(move || {
                if pinning == Pinning::Pinned
                    && let Some(core) = core
                {
                    core_affinity::set_for_current(core);
                }
                serve(shard, listener, cache)
            })
            .context(RuntimeSnafu)?;
        threads.push(thread);
    }

    for (shard, thread) in threads.into_iter().enumerate() {
        thread.join().ok().context(PanickedSnafu { shard })??;
    }
    Ok(())
}

/// One `SO_REUSEPORT` listener. Every shard binds the same address; the kernel hashes
/// incoming connections across them.
fn bind(address: std::net::SocketAddr) -> Result<std::net::TcpListener, Error> {
    let domain = socket2::Domain::for_address(address);
    let socket = socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))
        .context(SocketSnafu)?;
    socket.set_reuse_address(true).context(SocketSnafu)?;
    socket.set_reuse_port(true).context(SocketSnafu)?;
    socket.set_tcp_nodelay(true).context(SocketSnafu)?;
    socket.bind(&address.into()).context(BindSnafu { address })?;
    socket.listen(BACKLOG).context(BindSnafu { address })?;
    Ok(socket.into())
}

fn serve(
    shard: usize,
    listener: std::net::TcpListener,
    cache: crate::handler::Cache,
) -> Result<(), Error> {
    let runtime = compio::runtime::Runtime::new().context(RuntimeSnafu)?;
    runtime.block_on(async move {
        let listener = compio::net::TcpListener::from_std(listener).context(AdoptSnafu)?;
        accept(shard, listener, cache).await;
        Ok(())
    })
}

/// Accepts forever on the current runtime.
///
/// Public so a test can run a real server, over a real socket, in-process: the difference
/// between asserting on this code and asserting on a copy of it is the whole value of a
/// conformance test.
pub async fn accept(
    shard: usize,
    listener: compio::net::TcpListener,
    cache: crate::handler::Cache,
) {
    tracing::info!(shard, "listening");

    let beating = cache.clone();
    compio::runtime::spawn(async move {
        loop {
            compio::time::sleep(HEARTBEAT).await;
            beating.stats().beat(shard);
        }
    })
    .detach();

    loop {
        let accepted = listener.accept().await;
        let (stream, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(error) => {
                // A failed accept is a per-connection fault, not a shard fault: a peer that
                // vanished between SYN and accept must not take the shard down.
                tracing::warn!(shard, error = ?error, "accept failed");
                continue;
            }
        };
        let cache = cache.clone();
        compio::runtime::spawn(async move {
            if let Err(error) = session(&cache, shard, stream).await {
                tracing::debug!(shard, %peer, error = ?error, "connection ended");
            }
        })
        .detach();
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
        let head = match connection.head().await {
            Ok(Some(head)) => head,
            Ok(None) => return Ok(()),
            Err(error) => return Err(crate::handler::Error::Connection { source: error }),
        };

        let Ok(request) = crate::http::parse(&head) else {
            let bytes = crate::http::response::bare(
                crate::http::response::Status::BadRequest,
                crate::http::KeepAlive::Close,
                0,
            );
            connection
                .write(bytes)
                .await
                .map_err(|source| crate::handler::Error::Connection { source })?;
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
