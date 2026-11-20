# Architecture

Project-specific invariants live here; universal Rust workflow lives in `AGENTS.md`.
The full design, including what was decided against and deferred, is in
`docs/design.md`. This file records only what the code currently is and the rules it
must keep.

## Purpose

`parkay` is a ground-up Rust Parquet reader and writer aimed at maximum performance,
measured against the arrow-rs `parquet` crate as the baseline. It is a single library
crate with no dependencies. No functionality exists yet: `src/lib.rs` holds only the
crate docs.

## Format target

- Specification: Apache Parquet format `apache-parquet-format-2.14.0`.
- `format/parquet.thrift` is a verbatim copy of that tag's
  `src/main/thrift/parquet.thrift`. It is the reference for every metadata struct and
  is not compiled or code-generated. Changing the pinned version means replacing this
  file and updating this section.

## Test data

- `testdata/parquet-testing` is a git submodule of `apache/parquet-testing`, pinned to
  a commit. Tests build paths to it from `env!("CARGO_MANIFEST_DIR")`.
- Run `git submodule update --init` after cloning. CI does not check out submodules
  yet; enable that in the same change that adds the first test using them.

## Module Boundaries

None yet. The planned layering (kernels, sans-I/O core, reference scanner,
integrations) is in `docs/design.md` section 3. Add each module here as it lands.

## Core Invariants

- The core performs no I/O and spawns no threads (`docs/design.md` section 3).
- Corrupt or untrusted input produces an error, never a panic or undefined behavior.

## Forbidden Patterns / Dependencies

None beyond `AGENTS.md` and `docs/design.md` section 12. There are no runtime or dev
dependencies, and `Cargo.toml` lints `unused_dependencies` as a warning that CI treats
as an error.
