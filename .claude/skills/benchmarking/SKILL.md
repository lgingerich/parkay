---
name: benchmarking
description: How to measure performance in parkay — running benchmarks, A/B comparisons, reading reports, and inspecting profiles or assembly. Use before any `cargo xtask bench`, `report`, `profile`, or `asm` run, and before claiming a change is faster or slower.
---

# Benchmarking parkay

Use `cargo xtask` for every measurement (`cargo xtask <cmd> --help` for flags).

The execution rules (sequential runs, background runs, no subagents, machine refusals) are in `AGENTS.md`. This skill covers the procedure.

## Before you start

- Before benchmarking third-party candidate crates (codecs, hashes, UTF-8, CRC), list the candidates and ask the user first.
- Do not run plain `cargo` builds or tests while measuring. `xtask` locks out concurrent `bench`/`profile`/`check`/`fuzz`/`asm`, but a build still adds noise.
- Wait for `bench` through the harness (`t3_background_wait`); never poll or sleep.
- Never use results from a run that slept.

## A/B workflow

Never toggle variants by editing source mid-run or copying files to `/tmp`.

1. Record the starting tree: `cargo xtask snapshot base` (or use a commit).
2. Make the change.
3. Run `cargo xtask bench --entry ... --workload ... --against base --save NAME` (alternates `ab`/`ba`, default 6 rounds; builds `a` in its own target dir and refuses if `bench-support/`, `benches/`, or `testdata/` differ between sides). Use `--quick` only to explore; decide with full runs across the whole corpus.
4. Use `--bench thrift` only when a profile points into `parkay::thrift`. Save `arrow-rs` once as the reference (`--reference`); refresh it only when `report` marks it `STALE`.

## Reading results, profiles, and codegen

- `cargo xtask report A B [--changed]` prints provenance and per-group `scan` (compute-bound) control drift. For paired `--against` rounds (`NAME.a` vs `NAME.b`), `faster`/`slower` requires the median paired `b/a` change to exceed the group's `scan` drift and pass an exact two-sided sign test at `p <= 0.05` (at least 6 rounds); `~` is noise; `?` means too few paired rounds or an unpaired comparison without a `scan` control. Heed `QUICK`, `NOISY`, `SLEPT`, same-tree, and harness-mismatch warnings.
- When logging numbers in `docs/perf/`, copy them from `report` output and name the baselines; do not retype or round by hand.
- Profile instead of adding micro-benchmarks: `cargo xtask profile --entry NAME --workload NAME --breakdown [--save A | --diff A]` (add `--bench thrift` for Thrift kernels).
- Check codegen with `cargo xtask asm [SYMBOL] --save before`, edit, then `--diff before` (still confirm with a benchmark before claiming a speedup).
