//! The command line, which is also the entire configuration surface.
//!
//! Every env-driven input is declared here with `#[arg(env = ...)]` rather than read from
//! `std::env` somewhere in the middle of the program, so `--help` is a complete list of
//! what this process reads. Defaults that appear below are printed by `--help`; nothing is
//! filled in silently at a call site.

/// A read-optimized Nix binary cache.
#[derive(Debug, clap::Parser)]
#[command(name = "bincache", version, about)]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Serve the cache.
    Serve(Box<Serve>),
    /// Generate a signing key. The secret goes to stdout, the public key to stderr.
    Keygen(Keygen),
    /// Generate a push token for one build node.
    Token,
    /// Delete one path: its record, then its artifact. Needs the server stopped.
    Delete(Delete),
    /// Compare the payload directory against the index, in both directions. Needs the
    /// server stopped.
    Reconcile(Storage),
    /// Re-sign every record under the current key. Needs the server stopped.
    Rotate(Rotate),
}

/// Where the two durable artifacts live. Shared by every subcommand that touches them.
#[derive(Debug, clap::Args)]
pub struct Storage {
    /// Directory holding the NAR tree and the index database.
    #[arg(long, env = "BINCACHE_DATA_DIR")]
    pub data_dir: std::path::PathBuf,
}

/// The store directory paths are printed under. Its default is the one Nix itself uses.
#[derive(Debug, clap::Args)]
pub struct Store {
    #[arg(long, env = "BINCACHE_STORE_DIR", default_value = bincache_core::storepath::DIR_DEFAULT)]
    pub store_dir: String,
}

#[derive(Debug, clap::Args)]
pub struct Serve {
    #[command(flatten)]
    pub storage: Storage,

    #[command(flatten)]
    pub store: Store,

    /// Address every shard binds with `SO_REUSEPORT`.
    #[arg(long, env = "BINCACHE_LISTEN", default_value = "0.0.0.0:5000")]
    pub listen: std::net::SocketAddr,

    /// File holding `<name>:<base64 keypair>`, as `nix-store
    /// --generate-binary-cache-key` writes it. Never passed as an env value itself.
    #[arg(long, env = "BINCACHE_SECRET_KEY_FILE")]
    pub secret_key_file: std::path::PathBuf,

    /// A push credential. Repeatable, or comma-separated in the environment. With none
    /// configured the cache is read-only, which is a legitimate way to run it.
    #[arg(long, env = "BINCACHE_PUSH_TOKENS", value_delimiter = ',')]
    pub push_token: Vec<String>,

    /// File holding one push credential per line, for keeping them out of the process
    /// table and the environment.
    #[arg(long, env = "BINCACHE_PUSH_TOKEN_FILE")]
    pub push_token_file: Option<std::path::PathBuf>,

    /// Serving shards. Defaults to the parallelism the machine reports.
    #[arg(long, env = "BINCACHE_SHARDS")]
    pub shards: Option<core::num::NonZeroUsize>,

    /// Pin each shard to a core. The point of the architecture on a dedicated box, and
    /// the wrong default on a shared one.
    #[arg(long, env = "BINCACHE_PIN")]
    pub pin: bool,

    /// zstd level applied at ingest.
    #[arg(long, env = "BINCACHE_ZSTD_LEVEL", default_value_t = 3)]
    pub zstd_level: i32,

    /// `Priority` in `nix-cache-info`. Lower wins when several caches hold a path;
    /// `cache.nixos.org` sits at 40.
    #[arg(long, env = "BINCACHE_PRIORITY", default_value_t = 30)]
    pub priority: u32,

    /// Whether `nix-cache-info` invites bulk narinfo queries.
    #[arg(long, env = "BINCACHE_WANT_MASS_QUERY", default_value_t = true)]
    pub want_mass_query: bool,

    /// How long a shard may go without a heartbeat before the watchdog says so.
    #[arg(long, env = "BINCACHE_STALL_SECONDS", default_value_t = 30)]
    pub stall_seconds: u64,
}

#[derive(Debug, clap::Args)]
pub struct Keygen {
    /// Key name. Convention is `<host>-<generation>`, so rotation is visible in the name.
    #[arg(long)]
    pub name: String,
}

#[derive(Debug, clap::Args)]
pub struct Delete {
    #[command(flatten)]
    pub storage: Storage,

    /// The 32-character store path hash, with or without the rest of the path.
    pub path: String,
}

#[derive(Debug, clap::Args)]
pub struct Rotate {
    #[command(flatten)]
    pub storage: Storage,

    #[command(flatten)]
    pub store: Store,

    #[arg(long, env = "BINCACHE_SECRET_KEY_FILE")]
    pub secret_key_file: std::path::PathBuf,
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_command_line_is_internally_consistent() {
        let command: clap::Command = <crate::args::Args as clap::CommandFactory>::command();
        command.debug_assert();
    }
}
