# bincache

The facade binary: configuration, boot, and wiring. The only crate that knows the others
exist together.

## Owns

- **Configuration.** A `clap` `Parser` struct with `#[arg(env = "...")]` for every
  env-driven input, and strict file config with unknown fields denied. No hidden runtime
  defaults.
- **The composition root.** Builds the shared runtime handles once and threads them:
  the index snapshot pointer, the store handles, the signing key, the credential set.
- **Boot.** Map the latest `rkyv` index snapshot, replay the publish log tail, then start
  listeners. A snapshot that fails validation degrades to a filesystem rescan, which is
  slow to boot and loses no data.
- **Process shape.** `mimalloc` as the global allocator, `panic = "abort"` licensed by
  crash-only design, shard and ingest core placement agreed with the NUMA node the arena
  is allocated from, the watchdog thread, the metrics endpoint, and a typed `MainError`
  reported through `snafu`.

## Does not own

Any behaviour. `main.rs` stays at entrypoint, CLI parse, and a single call into this
crate's `lib.rs`, which holds the boot sequence.

## Depends on

All five sibling crates.

## Design references

DESIGN.md: "Durability and Recovery", "Memory, Allocation, and Ordering Discipline",
"Build and Packaging".
