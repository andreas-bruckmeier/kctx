# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project state

`kctx` is a Rust binary crate that currently contains only the generated `fn main()` in
`src/main.rs`. There are no dependencies, no modules, no tests, and no CI. Treat any
structure as still to be decided — there is no established architecture to follow yet.

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
  `gen` is a reserved keyword. A toolchain of at least Rust 1.85 is required; the pinned
  environment has 1.97.1.
- There is no `Cargo.lock` committed yet. Since this is a binary crate, commit the lockfile
  once dependencies are added.
