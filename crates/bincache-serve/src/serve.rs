//! Binding the listener and running the server on it.
//!
//! One listener, one work-stealing runtime. A slow NAR stream occupies a task rather than a
//! worker, so it cannot starve the metadata requests sharing the process, and there is
//! nothing to pin, rebalance, or watch for stalls.

use snafu::ResultExt as _;

#[derive(Debug, snafu::Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("binding {address} failed"))]
    Bind { address: std::net::SocketAddr, source: std::io::Error },

    #[snafu(display("serving failed"))]
    Serve { source: std::io::Error },
}

/// Runs until the process is killed. bincache is crash-only, so there is no orderly
/// shutdown for a caller to wait on.
pub async fn run(address: std::net::SocketAddr, cache: crate::handler::Cache) -> Result<(), Error> {
    let listener = tokio::net::TcpListener::bind(address).await.context(BindSnafu { address })?;
    tracing::info!(%address, "listening");
    axum::serve(listener, crate::handler::router(cache)).await.context(ServeSnafu)
}
