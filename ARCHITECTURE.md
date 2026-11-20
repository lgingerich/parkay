# Architecture

Project-specific invariants live here; universal Rust workflow lives in `AGENTS.md`.
The full design, including what was decided against and deferred, is in
`docs/design.md`. This file records only what the code currently is and the rules it
must keep.

## Purpose

`parkay` is a ground-up Rust Parquet reader and writer aimed at maximum performance,
measured against the arrow-rs `parquet` crate as the baseline. It is a single library
crate whose only runtime dependency is `thiserror`. The only code so far is the Thrift
compact-protocol reader that M2's footer parser will build on.

## Format target

- Specification: Apache Parquet format `apache-parquet-format-2.14.0`.
- `format/parquet.thrift` is a verbatim copy of that tag's
  `src/main/thrift/parquet.thrift`. It is the reference for every metadata struct and
  is not compiled or code-generated. Changing the pinned version means replacing this
  file and updating this section.

## Test data

- `testdata/parquet-testing` is a git submodule of `apache/parquet-testing`, pinned to
  a commit. Tests build paths to it from `env!("CARGO_MANIFEST_DIR")`.
- Run `git submodule update --init` after cloning. CI checks out submodules.

## Module Boundaries

The planned layering (kernels, sans-I/O core, reference scanner, integrations) is in
`docs/design.md` section 3. Modules so far:

- `src/thrift/`: Thrift compact-protocol reader over a borrowed `&[u8]` (`mod.rs`), its
  error type (`errors.rs`), and its tests (`tests.rs`). Knows nothing about Parquet's
  structs. It must import nothing else from the crate, so it can move to its own crate
  unchanged.

Internal modules are crate-private by default. The `internals` Cargo feature makes them
public and hidden from docs, for `fuzz/` and benchmarks, which build as separate crates.
It is not a stable API. `src/thrift/` has no non-test caller until M2, so `src/lib.rs`
carries a temporary `dead_code` expectation on it, marked `TODO(M2)`.

`fuzz/` is a separate crate with its own workspace, so the main crate stays on the
pinned toolchain. Run a target with `cargo xtask fuzz <target> [--seconds N]` (nightly,
1 s per-input timeout; seeds the corpus from real footers via
`examples/fuzz_corpus.rs` when empty). Targets:

- `thrift_skip`: `skip` never panics, and the position stays within the input.
- `thrift_consistency`: whenever the `read_*` API decodes a value, `skip` succeeds and
  ends at the same byte.
- `thrift_arrow_diff`: when both accept an input, parkay's `skip` and arrow-rs's (the
  copy in `bench-support`) end at the same byte. Excludes bool list elements and
  maps: arrow-rs 60.1's `skip` consumes no bytes for bool list elements.

## Performance tooling

The repository is a workspace whose root package, `parkay`, is the only default member,
so plain cargo commands act on it alone. The other members are development tooling:

- `xtask/`: `cargo xtask <command>` (see `cargo xtask help`). `bench` runs the criterion
  benchmarks with named saved baselines, `report` tabulates saved baselines, `asm` shows
  a function's assembly (`cargo-show-asm`), and `profile` runs one benchmark under
  `samply`, or macOS `sample` with `--text`.
- `bench-support/`: shared footer fixtures (parquet-testing footers and synthetic
  narrow and wide footers written by arrow-rs), plus a copy of arrow-rs's Thrift reader
  (Apache-2.0, header kept) for like-for-like comparison. It does not use parkay's lint
  set.
- `benches/thrift.rs`: the Thrift reader benchmarks, with memcpy and scan floors.
- `examples/footer_stats.rs`: what the fixture footers are made of (value kinds,
  varint and binary lengths, list sizes), for choosing workloads and reading profiles.

Benchmark one process at a time. Any `#[inline]`, `#[inline(always)]`, or `#[cold]` must
be backed by a benchmark. Code comments stay short; what was measured, kept, and
rejected is logged in `docs/perf/` (for example `docs/perf/thrift-skip.md`).

## Core Invariants

- The core performs no I/O and spawns no threads (`docs/design.md` section 3).
- Corrupt or untrusted input produces an error, never a panic or undefined behavior.
- Every size the Thrift reader returns has been checked against the remaining input.
  Code built on it may allocate in proportion to a size, but must not pre-allocate
  `size * size_of::<T>()` for a decoded struct `T`: a 1-byte empty struct on the wire
  can decode into hundreds of bytes.
- Readers accept the legacy and ecosystem variants that real writers emit (such as
  `0` for false in Thrift bool lists), as long as accepting them costs no performance
  and keeps the design. Anything else malformed is an error.

## Forbidden Patterns / Dependencies

None beyond `AGENTS.md` and `docs/design.md` section 12. `Cargo.toml` lints `unused_dependencies` as a warning that CI treats as an error.
