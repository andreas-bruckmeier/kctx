# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project state

`kctx` is a Rust binary crate (no `src/lib.rs`) that switches Kubernetes contexts without writing to
your kubeconfig, plus an optional read-only `inspect`. Modules under `src/`: `app`, `cli`, `filter`,
`kubeconfig/`, `kubernetes/`, `logging`, `output/`, `overlay`, `paths`, `ui/`. Layering runs
`kubeconfig/` → `kubernetes/` → `ui/`; `ui/*` never imports `kube` and `kubernetes/*` never imports
`ratatui`. See README.md for the architecture and the design guarantees.

Two invariants are mechanically enforced and must not be worked around:

- **Read-only.** Only `src/kubernetes/read.rs` may construct a `kube::Api`, and only
  `src/kubernetes/client.rs` may name a `kube::Client`. `tests/readonly_guard.rs` greps every file
  under `src/` and fails the build otherwise — including on the mere *mention* of `PostParams`,
  `PatchParams`, `DeleteParams`, `Patch::`, `Method::POST`/`PUT`/`PATCH`/`DELETE` or the
  subject-review APIs, so avoid those identifiers even in unrelated code. If a guard test fails, the
  fix is not to relax the test.
- **Private on-disk state.** The overlay cache and log files are created `0700`/`0600`, pre-existing
  directories are verified rather than trusted, and temporary files are created with `O_EXCL`
  (`OpenOptions::create_new`). Keep new filesystem writes to that pattern.

Tests are offline and dependency-injected: env access is isolated behind seams (`overlay::prepare_in`,
`DiscoverySources::new`) that tests call directly — nothing calls `std::env::set_var`.
`tempfile::tempdir()` is the filesystem fixture; note it creates directories at the umask (`0755`),
so pass a path *below* it where kctx creates its own private directory.

## Commands

```bash
cargo run                 # build + run the binary
cargo build --release     # optimized build -> target/release/kctx
cargo check               # fast type-check, no codegen
cargo test                # run all tests
cargo test <name>         # run tests whose name matches <name>
cargo test -- --nocapture # show println! output from tests
cargo fmt                 # format
cargo clippy --all-targets -- -D warnings   # lint
```

## Notes

- `Cargo.toml` sets `edition = "2024"`, so `unsafe` in `extern` blocks must be marked and
  `gen` is a reserved keyword. README.md requires at least Rust 1.89; the pinned environment
  has 1.97.1.
- `Cargo.lock` is committed, as it should be for a binary crate. Keep it in sync when
  dependencies change.
- The crate is Unix-only in practice (`std::os::unix` is used ungated in `paths.rs`, `overlay.rs`
  and `logging.rs`) but has no `cfg(unix)` gating.
