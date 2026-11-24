# Performance sketches and measurement logs

This directory holds pre-implementation performance sketches, crate evaluations, and measurement logs.

## Current harness (8-entry corpus)

The benchmark harness uses the 8-entry corpus in `bench-support/src/corpus.rs`
(`small`, `medium`, `many_row_groups`, `wide`, `very_wide`, `nested`, `legacy`,
`many_files`) with `benches/metadata.rs` and `benches/thrift.rs`. No static
numbers for the current corpus are committed here yet; generate fresh measurements
on your machine with:

```sh
cargo xtask bench --reference                         # arrow-rs & scan floors (metadata)
cargo xtask bench --save baseline                     # parkay metadata suite
cargo xtask report baseline --markdown                # markdown table with reference ratios
cargo xtask bench --bench thrift --reference          # arrow-rs & scan floors (thrift)
cargo xtask bench --bench thrift --save thrift-base   # parkay thrift skip suite
cargo run --release --example metadata_memory         # retained heap footprint
cargo run --release --example footer_stats            # corpus footer shape & field counts
```

## Crate evaluations

- [`dependencies.md`](dependencies.md) — Benchmark-backed hot-path crate choices (`zrip`, `snap`, `lz4_flex`, `simdutf8`, `foldhash`/`hashbrown`, `crc32fast`, plus `fearless_simd` if explicit SIMD is ever needed) so later phases don't repeat crate selection.

## Historical logs (pre-corpus-refactor)

All three logs below predate the 8-entry corpus refactor (they used the earlier
6-fixture suite and older workload names) and are kept for the design decisions
and microarchitectural lessons they recorded:

- [`thrift-skip.md`](thrift-skip.md) — Thrift `CompactReader::skip` optimization log, rejected ideas, `xctrace` issue-slot breakdown, and `skip_marking` cost.
- [`footer-ownership.md`](footer-ownership.md) — Why `open()` copies the footer into exact-size owned storage rather than taking caller ownership.
- [`open.md`](open.md) — M2 open-path sketch, chunk-template speedup measurements, extraction tuning, and retained-memory check.
