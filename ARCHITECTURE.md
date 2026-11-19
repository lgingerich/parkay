# Architecture

Project-specific invariants live here; universal Rust workflow lives in `AGENTS.md`.

## Purpose

`parkay` is currently an unmodified copy of the strict Rust project template: `Cargo.toml` still names the package `rust-template` (description and repository are `TODO`), and the committed code is a single smoke-test function. It is a library crate (`src/lib.rs` only, no binary, no dependencies). It has no domain functionality yet; update this section when the real purpose is decided.

## Module Boundaries

- `src/lib.rs`: the whole crate. It exposes `crate_name()`, which returns `env!("CARGO_PKG_NAME")`, with one unit test. No other modules exist, so there are no dependency directions to enforce yet.

## Core Invariants

None beyond `AGENTS.md`. The lint and profile configuration in `Cargo.toml` and `clippy.toml` is enforced by CI.

## Forbidden Patterns / Dependencies

None beyond `AGENTS.md`. There are no runtime or dev dependencies, and `Cargo.toml` lints `unused_dependencies` as a warning that CI treats as an error.
