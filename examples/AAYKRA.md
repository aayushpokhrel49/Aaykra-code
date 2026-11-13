# Example project instructions

Aaykra Code loads this file automatically from `AAYKRA.md` at the repository
root. `CLAUDE.md` and `AGENTS.md` are accepted as fallbacks.

## Project

This repository contains a Rust web service and its integration tests.

## Build and test

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Run the service locally with `cargo run -- serve`.

## Layout

- `src/` — application code
- `tests/` — integration tests
- `migrations/` — database migrations

## Conventions

- Keep changes focused and add a test for behavior changes.
- Use structured errors; do not add `unwrap()` in request paths.
- Never edit generated files under `target/` or `vendor/`.

## Verification

Before reporting a task complete, run formatting, Clippy, and the relevant test
suite. Report any check you could not run and why.
