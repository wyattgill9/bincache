//! Entrypoint: allocator, tracing, CLI parse, one call into the library.

use snafu::ResultExt as _;

/// Default tracing filter. `info` covers boot and publishes; per-request work on the
/// serving path emits counters rather than spans and is silent here.
const FILTER_DEFAULT: &str = "info";

#[derive(Debug, snafu::Snafu)]
enum MainError {
    #[snafu(display("bincache failed"))]
    Run { source: bincache::boot::Error },
}

#[snafu::report]
fn main() -> Result<(), MainError> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(FILTER_DEFAULT));
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();

    let args: bincache::args::Args = clap::Parser::parse();
    bincache::boot::run(args).context(RunSnafu)
}
