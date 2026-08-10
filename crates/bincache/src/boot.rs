//! The composition root: open the two durable artifacts, build the shared handles once,
//! and hand them to the server.
//!
//! Boot is deliberately short. There is no snapshot to validate, no log to replay, and no
//! O(n) filesystem rescan, because `redb` owns crash consistency and a commit is the
//! publish. What is left is sweeping staging files a crash may have left behind.

use snafu::OptionExt as _;
use snafu::ResultExt as _;

/// Subdirectory of the data directory holding the NAR tree.
const PAYLOAD: &str = "payload";

/// The index database file, under the data directory.
const INDEX: &str = "index.redb";

#[derive(Debug, snafu::Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("starting the runtime failed"))]
    Runtime { source: std::io::Error },

    #[snafu(display("reading {} failed", path.display()))]
    ReadFile { path: std::path::PathBuf, source: std::io::Error },

    #[snafu(display("the secret key file is malformed"))]
    SecretKey { source: bincache_core::sign::Error },

    #[snafu(display("the store directory is not usable"))]
    StoreDir { source: bincache_core::storepath::Error },

    #[snafu(display("{path:?} is not a store path hash"))]
    Path { path: String, source: bincache_core::storepath::Error },

    #[snafu(display(
        "zstd level {level} is outside {:?}..={:?}",
        bincache_ingest::upload::Level::RANGE.start(),
        bincache_ingest::upload::Level::RANGE.end()
    ))]
    ZstdLevel { level: i32 },

    #[snafu(display("the payload directory could not be opened"))]
    Store { source: bincache_store::nar::Error },

    #[snafu(display("the index could not be opened"))]
    Index { source: bincache_index::index::Error },

    #[snafu(display(
        "the index at {} is held by another process. redb allows one writer process at a \
         time, so maintenance subcommands need the server stopped.",
        path.display()
    ))]
    IndexLocked { path: std::path::PathBuf },

    #[snafu(display("a maintenance operation failed"))]
    Maintain { source: bincache_ingest::maintain::Error },

    #[snafu(display("the server failed"))]
    Serve { source: bincache_serve::serve::Error },
}

pub fn run(args: crate::args::Args) -> Result<(), Error> {
    match args.command {
        crate::args::Command::Serve(serve) => runtime()?.block_on(self::serve(*serve)),
        crate::args::Command::Keygen(keygen) => self::keygen(&keygen.name),
        crate::args::Command::Token => {
            println!("{}", bincache_ingest::auth::generate());
            Ok(())
        }
        crate::args::Command::Delete(delete) => runtime()?.block_on(self::delete(delete)),
        crate::args::Command::Reconcile(storage) => runtime()?.block_on(self::reconcile(storage)),
        crate::args::Command::Rotate(rotate) => runtime()?.block_on(self::rotate(rotate)),
    }
}

fn runtime() -> Result<tokio::runtime::Runtime, Error> {
    tokio::runtime::Runtime::new().context(RuntimeSnafu)
}

async fn serve(args: crate::args::Serve) -> Result<(), Error> {
    let store_dir = dir(&args.store)?;
    let key = secret_key(&args.secret_key_file)?;
    let level = bincache_ingest::upload::Level::new(args.zstd_level)
        .context(ZstdLevelSnafu { level: args.zstd_level })?;

    let artifacts = open(&args.storage).await?;
    let swept = artifacts.store.sweep_staging().await.context(StoreSnafu)?;
    if swept > 0 {
        tracing::warn!(swept, "removed staging files a previous run left behind");
    }

    let tokens = push_tokens(&args)?;
    if tokens.is_empty() {
        tracing::warn!("no push tokens configured; the cache is read-only");
    }
    tracing::info!(
        paths = artifacts.index.count().context(IndexSnafu)?,
        %store_dir,
        public_key = %key.public().render(),
        "bincache starting"
    );

    let ingest = bincache_ingest::ingest::Ingest::new(bincache_ingest::ingest::Parts {
        store: artifacts.store,
        index: artifacts.index,
        key,
        dir: store_dir.clone(),
        level,
    });

    let cache = bincache_serve::handler::Cache::new(bincache_serve::handler::Parts {
        ingest,
        tokens,
        info: bincache_core::cacheinfo::CacheInfo {
            store_dir,
            mass_query: if args.want_mass_query {
                bincache_core::cacheinfo::MassQuery::Wanted
            } else {
                bincache_core::cacheinfo::MassQuery::Unwanted
            },
            priority: bincache_core::cacheinfo::Priority(args.priority),
        },
        stats: bincache_serve::stats::Stats::default(),
    });

    bincache_serve::serve::run(args.listen, cache).await.context(ServeSnafu)
}

fn keygen(name: &str) -> Result<(), Error> {
    let key = bincache_core::sign::SecretKey::generate(name.to_owned());
    // The secret goes to stdout so `bincache keygen --name x > key` is the whole workflow;
    // the public half goes to stderr so it stays visible in that same invocation.
    println!("{}", key.render());
    eprintln!("trusted-public-keys entry: {}", key.public().render());
    Ok(())
}

async fn delete(args: crate::args::Delete) -> Result<(), Error> {
    // Accept either the bare hash or a whole store path, since both are things an operator
    // has in front of them.
    let text = args.path.rsplit('/').next().unwrap_or(&args.path);
    let text = &text[..core::cmp::min(text.len(), bincache_core::storepath::HASH_TEXT_LEN)];
    let key = bincache_core::storepath::Hash::parse(text)
        .context(PathSnafu { path: args.path.clone() })?;

    let artifacts = open(&args.storage).await?;
    let deleted = bincache_ingest::maintain::delete(&artifacts.store, &artifacts.index, &key)
        .await
        .context(MaintainSnafu)?;
    println!("{key}: {deleted:?}");
    Ok(())
}

async fn reconcile(storage: crate::args::Storage) -> Result<(), Error> {
    let artifacts = open(&storage).await?;
    let found = bincache_ingest::maintain::reconcile(&artifacts.store, &artifacts.index)
        .context(MaintainSnafu)?;
    println!("orphan artifacts: {}", found.orphan_artifacts.len());
    for url in &found.orphan_artifacts {
        println!("  {}", url.name());
    }
    println!("records without artifacts: {}", found.records_without_artifacts.len());
    for path in &found.records_without_artifacts {
        println!("  {path}");
    }
    println!("unrecognized files: {}", found.unrecognized_files.len());
    for path in &found.unrecognized_files {
        println!("  {}", path.display());
    }
    Ok(())
}

async fn rotate(args: crate::args::Rotate) -> Result<(), Error> {
    let key = secret_key(&args.secret_key_file)?;
    let artifacts = open(&args.storage).await?;
    let rotated = bincache_ingest::maintain::rotate(&artifacts.index, &dir(&args.store)?, &key)
        .context(MaintainSnafu)?;
    println!("re-signed {rotated} records under {}", key.public().render());
    Ok(())
}

/// The two durable artifacts, opened together because nothing uses one without the other.
struct Artifacts {
    store: bincache_store::nar::Store,
    index: bincache_index::index::Index,
}

async fn open(storage: &crate::args::Storage) -> Result<Artifacts, Error> {
    let store = bincache_store::nar::Store::open(storage.data_dir.join(PAYLOAD))
        .await
        .context(StoreSnafu)?;
    let path = storage.data_dir.join(INDEX);
    let opened = bincache_index::index::Index::open(path.clone());
    if let Err(bincache_index::index::Error::Open {
        source: redb::DatabaseError::DatabaseAlreadyOpen,
        ..
    }) = &opened
    {
        return IndexLockedSnafu { path }.fail();
    }
    Ok(Artifacts { store, index: opened.context(IndexSnafu)? })
}

fn dir(store: &crate::args::Store) -> Result<bincache_core::storepath::Dir, Error> {
    bincache_core::storepath::Dir::new(store.store_dir.clone()).context(StoreDirSnafu)
}

fn secret_key(path: &std::path::Path) -> Result<bincache_core::sign::SecretKey, Error> {
    let text = std::fs::read_to_string(path).context(ReadFileSnafu { path })?;
    bincache_core::sign::SecretKey::parse(&text).context(SecretKeySnafu)
}

fn push_tokens(args: &crate::args::Serve) -> Result<bincache_ingest::auth::Tokens, Error> {
    let mut tokens: Vec<String> =
        args.push_token.iter().filter(|token| !token.is_empty()).cloned().collect();
    if let Some(path) = &args.push_token_file {
        let text = std::fs::read_to_string(path).context(ReadFileSnafu { path })?;
        tokens
            .extend(text.lines().map(str::trim).filter(|line| !line.is_empty()).map(str::to_owned));
    }
    Ok(bincache_ingest::auth::Tokens::new(tokens))
}
