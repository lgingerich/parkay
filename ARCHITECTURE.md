# Architecture

Module boundaries and repo-wide invariants.

> **Other docs:** vision, roadmap & big decisions in [`docs/design.md`](docs/design.md) · subsystem rationales in [`docs/subsystems/`](docs/subsystems/) · workflow & coding rules in [`AGENTS.md`](AGENTS.md) · measurements & crate evals in [`docs/perf/`](docs/perf/).

## Boundaries

`parkay` is the only default workspace member, so plain `cargo` commands target it alone.

| Path | Visibility / scope | Boundary rule |
|---|---|---|
| `src/limits.rs` | `pub` | Caller-configured bounds (`Limits`) with explicit defaults. |
| `src/metadata/` | `pub` | Sans-I/O file opening and chunk-fact extraction. Top-level files hold no Thrift; see `src/metadata/mod.rs` and [`docs/subsystems/metadata.md`](docs/subsystems/metadata.md). |
| `src/thrift/` | `pub(crate)` | Zero-alloc compact-protocol reader. Imports nothing else from `parkay`. Exposed (`#[doc(hidden)]`) via the unstable `internals` feature for benches/fuzz; error types re-exported from `metadata`. See [`docs/subsystems/thrift.md`](docs/subsystems/thrift.md). |
| `xtask/` | dev-only | Rust task runner (`cargo xtask help`); see [`.claude/skills/benchmarking/SKILL.md`](.claude/skills/benchmarking/SKILL.md). |
| `bench-support/` | dev-only | Benchmark corpus, fixture loaders, and arrow-rs Thrift reference copy. Exempt from `parkay`'s lint set. |
| `fuzz/` | separate workspace | Nightly `cargo-fuzz` targets (`fuzz/fuzz_targets/`), run via `cargo xtask fuzz`. Builds optimized with `debug_assert!` and overflow checks on. |

## Pinned references

- **Format spec:** `format/parquet.thrift` is a verbatim copy of `apache-parquet-format-2.14.0` (reference only; not code-generated).
- **Fixtures:** `testdata/parquet-testing` is a pinned submodule (`git submodule update --init`), resolved via `env!("CARGO_MANIFEST_DIR")`.

## Repo-wide invariants

Subsystem-specific invariants live in [`docs/subsystems/`](docs/subsystems/). Every change across `src/` must preserve:

- **Sans-I/O core:** No I/O and no thread spawning anywhere in `src/`.
- **Untrusted input never panics:** Corrupt input returns `Err`, never a panic or UB.
- **Bounded work:** Every loop over input consumes ≥ 1 byte per iteration or counts against a checked bound.
- **Permitted recursion (exhaustive list):** Thrift skipping (`src/thrift/`, bounded by `SKIP_DEPTH_MAX = 64`), plus `walk` in `benches/thrift.rs` (`SKIP_DEPTH_MAX`). Everything else—including schema building (`Limits::schema_depth_max`)—is iterative.
- **Ecosystem tolerance:** Accept real-world legacy quirks when doing so costs no performance and preserves the design; reject everything else malformed.
