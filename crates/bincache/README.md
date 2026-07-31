# bincache

The facade binary: configuration, boot, and wiring. The only crate that knows the others
exist together.

## Owns

- **Configuration.** One `clap` `Parser` with `#[arg(env = ...)]` on every env-driven
  input, so `--help` is a complete list of what the process reads.
- **The composition root.** Opens the payload directory and the index once and threads them
  into the ingest handle and the serving cache.
- **Boot.** Sweep staging files a crash may have left, start the watchdog, start the
  shards. There is no snapshot to validate and no log to replay.
- **Operator subcommands.** `keygen`, `token`, `delete`, `reconcile`, `rotate`. The three
  that touch the index need the server stopped, because `redb` allows one writer process.
- **Process shape.** `mimalloc` as the global allocator, `panic = "abort"` licensed by
  crash-only design, a typed `MainError` reported through `snafu`, and the watchdog thread.

## Does not own

Any behaviour. `main.rs` is the allocator, tracing setup, a CLI parse, and one call.

## Depends on

All five sibling crates.

## Design references

DESIGN_V2.md: "Durability and recovery", "Memory, allocation, and ordering discipline",
"Build and packaging".
