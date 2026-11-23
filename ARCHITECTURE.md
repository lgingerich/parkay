# Architecture

Project-specific invariants live here; universal Rust workflow lives in `AGENTS.md`.
The design, including what was decided against and deferred, is in
`docs/design.md`; the detail behind it, by phase, is in `docs/design-notes.md`. This file records only what the code currently is and the rules it
must keep.

## Purpose

`parkay` is a ground-up Rust Parquet reader and writer aimed at maximum performance,
measured against the arrow-rs `parquet` crate as the baseline and against what the
hardware allows. It is a single library crate whose only runtime dependency is
`thiserror`. So far it opens a file: it reads the footer, the schema, and what the
footer records for each column chunk. It reads no pages yet.

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

- `src/limits.rs`: `Limits`, the bounds a caller sets on what a file may make the
  reader do, with their defaults. Public.
- `src/metadata/`: opening a file and reading its metadata. Public. It performs no I/O.
  `mod.rs` has a map of the files in the order a file goes through them. Two rules
  shape the layout:
  - The files at the top level are what callers see and hold no Thrift: `open.rs`
    (`open`), `file.rs` (`Metadata` and its accessors), `schema.rs` (`Schema`),
    `extract.rs` (`Metadata::extract`, its request, and `ChunkFacts`), `types.rs` (the
    format's plain definitions and the values the decoders fill in, so `wire/` depends
    on nothing above it), `errors.rs`, and `utils.rs` (what the decoders share).
  - `wire/` holds every decoder, as functions from bytes to those values, one file for
    each structure of `parquet.thrift`: `file_metadata.rs` (the one pass that opens a
    file), `schema_element.rs`, `column_chunk.rs`, `statistics.rs`, `key_value.rs`.
  - `template/` is how the pass finds where most column chunks end without parsing
    them. `compare.rs` is the comparison and has no state. `mod.rs` holds the
    templates, the budget that decides when one may be built, and the two loops that
    are kept as functions of their own for speed. Working memory of the pass
    only; nothing of it is retained.
- `src/thrift/`: Thrift compact-protocol reader over a borrowed `&[u8]` (`mod.rs`), its
  error type (`errors.rs`), a test encoder that the metadata tests also use
  (`test_support.rs`), and its tests (`tests.rs`). Knows nothing about Parquet's
  structs. It must import nothing else from the crate, so it can move to its own crate
  unchanged.

`src/thrift/` is crate-private. The `internals` Cargo feature makes it public and hidden
from docs, for `fuzz/` and benchmarks, which build as separate crates. It is not a
stable API. Its error types are re-exported from `metadata`, because a footer error can
hold one.

`fuzz/` is a separate crate with its own workspace, so the main crate stays on the
pinned toolchain. Run targets with `cargo xtask fuzz <target>... | --all [--seconds N]` (nightly, in
parallel, 1 s per-input timeout; seeds the corpus from real footers via
`examples/fuzz_corpus.rs` when empty), and replay saved crashes with `--replay`. cargo-fuzz builds targets optimized but with
debug assertions and overflow checks on, so every `debug_assert!` is live under fuzzing.
Targets:

- `thrift_skip`: `skip` never panics, and the position stays within the input.
- `thrift_consistency`: whenever the `read_*` API decodes a value, `skip` succeeds and
  ends at the same byte.
- `thrift_arrow_diff`: when both accept an input, parkay's `skip` and arrow-rs's (the
  copy in `bench-support`) end at the same byte. Excludes bool list elements and
  maps: arrow-rs 60.1's `skip` consumes no bytes for bool list elements.
- `thrift_marking`: `skip_marking` accepts what `skip` accepts and ends where it ends,
  and its marks are right: the value still skips to its end with every unmarked bit
  replaced, with a listed binary replaced by one of another length, and with an
  integer made longer.
- `metadata_open`: opening arbitrary footer bytes never panics, and what opens is
  consistent with itself: a column extracted alone has the same facts as among all, and
  the file opens the same with its own schema supplied.
- `metadata_arrow_diff`: when both accept a footer, parkay and arrow-rs agree on file
  metadata (version, row count, row-group counts/sizes, `created_by`, key-value
  metadata, column orders), schema (physical/logical types and levels), and every
  column chunk's facts (locations, codec, encodings, data/dictionary page offsets, and
  statistics). Three classes of inputs where the two readers' contracts differ are
  excluded: fields out of ascending id order (`common::fields_ascend`), fields with
  wrong wire types that arrow-rs reads as the expected type (`common::shapes`), and
  footers containing undecodable values inside skipped chunks (`common::walk`).

The open pass compares column chunks instead of parsing them where it can
(`src/metadata/template/`). In debug builds, and so in the tests and under fuzzing,
every chunk passed over that way is also parsed, and the two must agree on where it
ends.

## Performance tooling

The repository is a workspace whose root package, `parkay`, is the only default member,
so plain cargo commands act on it alone. The other members are development tooling:

- `xtask/`: `cargo xtask <command>` (see `cargo xtask help`). `check` runs the CI checks
  with compact output. `bench` checks the machine (AC power, no low power mode, no busy
  process, no sleep during the run), measures with criterion, and records the measured
  git tree and conditions in `target/xtask/runs/NAME.txt`; `--against REF` measures
  another version (a commit or a `snapshot`) in alternating rounds on the same machine
  using an isolated target directory for side `a`. `report` tabulates saved baselines
  with a verdict that counts a change only beyond the group's compute-bound `scan`
  control drift and (for paired `--against` rounds) an exact two-sided sign test
  (`p <= 0.05`, >= 6 rounds). A lock lets one bench or profile run at a time, and keeps
  check and fuzz from running during one. `asm` shows a function's assembly
  (`cargo-show-asm`) and diffs it against a saved build, and `profile` runs one
  benchmark under `samply`, or macOS `sample` with `--text` / `--breakdown`.
- `bench-support/`: the benchmark corpus and synthetic generators (`corpus.rs`: eight
  entries covering small, medium, many-row-group, wide, very wide, nested, legacy, and
  many-file cases, generated deterministically and cached in `target/bench-corpus/`),
  fixture loaders (`fixtures.rs`), the shared `scan` control workload (`lib.rs`), and a
  copy of arrow-rs's Thrift reader (Apache-2.0, header kept) for like-for-like
  comparison. It does not use parkay's lint set.
- `benches/metadata.rs`: the default benchmarks. Per corpus entry, `open`,
  `open_narrow` (a typical query's planning), and `open_all` (the lazy design's worst
  case), each beside arrow-rs doing the same work (`arrow_*`), plus the `scan` control.
  The arrow-rs results are a reference saved once (`cargo xtask bench --reference`).
  Where the time goes inside a workload comes from `cargo xtask profile --breakdown`,
  not from more benchmarks.
- `benches/thrift.rs`: kernel benchmarks for the Thrift reader (`skip`,
  `skip_marking`, `walk`, against arrow-rs's `skip`, with `scan` control and `memcpy`
  floor) over the corpus's footers and page indexes. Run with
  `cargo xtask bench --bench thrift`.
- `examples/footer_stats.rs`: what the fixture footers are made of (value kinds,
  varint and binary lengths, list sizes), for choosing workloads and reading profiles.
- `examples/metadata_memory.rs`: the memory `Metadata` and `Schema` retain for each
  fixture, against arrow-rs.

The synthetic corpus entries (`small`, `medium`, `many_row_groups`, `wide`,
`very_wide`, `nested`, `many_files`) use deterministic random values, varying string
lengths, changing null rates, and sparse/all-null row groups rather than uniform
chunks, alongside real files in `legacy`. Changes to the open pass are judged across
the full corpus; unit tests in `src/metadata/tests.rs` also exercise uniform, varied,
and worst-case alternating chunk shapes via helpers in `bench-support/src/corpus.rs`.

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
- Every loop over input either consumes at least one byte per iteration or counts
  against a size already checked against the remaining input, so work is bounded by the
  input's length.
- Recursion exists in one place in the library: `CompactReader::skip`, through `skip_nested`,
  `skip_struct_fields`, and `skip_map`, to at most `SKIP_DEPTH_MAX` (64) levels. The
  three compile into one function whose stack frame is 80 bytes on AArch64, so the
  deepest input uses about 5 KB of stack. `skip_marking` is the same code compiled a
  second time, with a frame of 144 bytes: about 9 KB at the deepest. The iterative
  form measured slower (`docs/perf/thrift-skip.md`). It is the one exception to the
  recursion rule in `AGENTS.md`. The schema is a tree but is built with an explicit
  stack, bounded by `Limits::schema_depth_max`. Outside the library, `walk` in
  `benches/thrift.rs` recurses to at most `SKIP_DEPTH_MAX` (64) levels.
- What opening a file allocates is bounded by the footer's length, whatever the footer
  claims: a few bytes per byte of footer for what is retained, and about the footer's
  length again for the open pass's working memory. The footer's length is itself
  bounded by `Limits::footer_size_max`.
- A column chunk that the open pass passes over by comparing is one that parsing would
  pass over by the same steps (`src/metadata/template/compare.rs` gives the argument). The pass
  accepts and rejects exactly what parsing every chunk would.

## Style

The code follows the parts of Tiger Style listed in `AGENTS.md`. `src/thrift/` was
brought in line with them in one pass that left the machine code of the benchmark binary
byte-for-byte unchanged (`docs/perf/thrift-skip.md`). Its assertions are compile-time
checks and `debug_assert!`, which fuzzing exercises. `src/metadata/` was written to
them. What `src/thrift/` still does differently:

- Two matches on `ElementType`, in `encoded_len_min` and in the list arm of
  `skip_nested`, end in a catch-all arm. Writing them exhaustively changes the machine
  code of the skip path, so they stay until that change is measured.
- `check_fits` and the cold error constructors `eof` and `size_exceeds_input` each take
  two integers of the same type, which could be swapped. Their shape is part of the
  measured skip path.
- The recursion in `skip`, described under Core Invariants.

## Forbidden Patterns / Dependencies

None beyond `AGENTS.md` and `docs/design.md` section 12. `Cargo.toml` lints `unused_dependencies` as a warning that CI treats as an error.
