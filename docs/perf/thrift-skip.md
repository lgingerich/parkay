# Thrift `skip`: performance log

> **Status: Historical** — Pre-M2 Thrift `skip` measurements (2026-11-20–22).
> CLI flags and benchmark names have since changed: `cargo xtask bench --bench thrift --workload skip` replaces `'/skip$'`, `arrow_rs_decode` was removed in favor of `benches/metadata.rs`, and multi-row-group opens now use chunk templates (`docs/perf/open.md`).

What was measured on `CompactReader::skip`, so code comments can stay short. All
numbers are from the Thrift skip benchmark (now
`cargo xtask bench --bench thrift --workload skip --quick`, full-length where noted)
on one Apple M3 Max, as changes in time against the version before. A negative change
is faster. Fixtures are in `bench-support`. Counters are from Xcode's `xctrace` CPU
Counters template.

## Kept

| Change | Effect |
|---|---|
| Skip scalar fields inline in the struct loop, dispatching once on the raw type code; recurse only for containers | -37% to -43% |
| `skip_binary` (advances without building a slice); list loops dispatch on the element type once | about -5% |
| `#[inline(always)]` on `read_list_header` and `skip_binary` (plain `#[inline]` was 3.5-5% slower) | -6% to -11% |
| `#[cold]` error constructors on the byte readers | -5% to -7% |
| One-byte fast path in `skip_varint` | -2% to -4.5% |
| Struct loop on a local copy of the reader, so the position stays in a register | -14% to -15% |

Total, full-length run: 55-60% less time than the first version, and about 1.8x the
throughput of arrow-rs 60.1's `skip` on the same footers.

## Rejected

| Change | Effect | Note |
|---|---|---|
| Iterative skip with an explicit frame stack (no calls) | +12% | Calls are cheap on this CPU; frame bookkeeping cost more |
| 8-byte error type | +3% to +9% | |
| Shrinking-slice and slice-iterator reader layouts | within noise to +4% | Measured before the call-overhead fixes |
| `get(..)` instead of indexing at `input[pos..]` | +2% to +4% | |
| Word-at-a-time scan for multi-byte varints | +15% at first; mixed (-2% to +6.5%) after the other fixes | Doubled `skip_nested`'s size |
| Local reader copy in the scalar list loops too | about +3% against the struct loop alone | |
| Frequency-ordered type dispatch | +21% to +26% | |
| Fused fast path for "short header + one-byte integer" | +16% to +23% | Front-end delivery stalls rose to 42%; mispredictions did not rise |
| `std::hint::cold_path` on rare branches | +55% to +73% | Caused the small hot helpers to be outlined |
| `#[inline(always)]` on `read_byte`, `skip_varint`, `take_array`, `read_size` | +25% to +30% | Forcing all four together regressed; `skip_varint` alone was later marked `#[inline(always)]` for `skip_marking` (see bottom section) |
| Struct loop isolated with `#[inline(never)]` | +4% to +10% | With `cold_path` on top, still worse |

## Against arrow-rs's full decode

Before chunk templates were added in M2, `skip` was the floor for opening a file in
the lazy design, so its time against arrow-rs's eager decode (`arrow_rs_decode`, default
options — an early Thrift benchmark workload since removed and replaced by `arrow_*` in
`benches/metadata.rs`) capped what laziness alone could gain. Median times from the
saved runs of 2026-11-20 on the
[alamb/parquet_footer_parsing](https://github.com/alamb/parquet_footer_parsing)
footers:

| Footer | `skip` | `arrow_rs_decode` | Ratio | `scan` floor |
|---|---|---|---|---|
| Float32, 100 columns | 0.19 ms | 0.42 ms | 2.2x | 0.012 ms |
| Float32, 1000 columns | 1.86 ms | 4.17 ms | 2.2x | 0.12 ms |
| Float32, 100,000 columns | 178 ms | 420 ms | 2.4x | 13 ms |
| String, 100 columns | 0.18 ms | 0.49 ms | 2.7x | 0.012 ms |
| String, 1000 columns | 1.84 ms | 4.95 ms | 2.7x | 0.12 ms |
| String, 100,000 columns | 181 ms | 495 ms | 2.7x | 12 ms |

- Skipping every chunk costs 37% to 45% of arrow-rs's whole decode, so a pure-skip
  lazy open can be at most 2.2x to 2.7x faster than that decode, before the footer
  directory, the schema, and any requested column are paid for. That ceiling applies
  to 1-row-group and non-template footers; on multi-row-group files, M2's chunk
  templates bypass per-chunk skipping and reach up to 17x faster than arrow-rs
  (`docs/perf/open.md`).
- Not measured: arrow-rs with a supplied schema or with its statistics policies
  set to skip, which do less work than the default and narrow the gap.
- `skip` is 14x to 16x slower than `scan`, the one-pass floor over the same bytes.

## Tiger Style pass (2026-11-21)

The reader was brought in line with the style rules in `AGENTS.md`: names spelled out
and qualifiers last (`SKIP_DEPTH_MAX`, `encoded_len_min`, `position`, `field_type`),
compile-time assertions on its constants and on the width of `read_varint`, and
`debug_assert!` on its invariants (the position stays within the input, every loop
iteration consumes input, depth stays within the limit, every size returned fits the
input left).

No re-measurement was needed: the `__text` section of the release benchmark binary is
byte-for-byte identical before and after, and so are the code sections of the library's
release assembly. `debug_assert!` and the compile-time assertions add nothing to a
release build.

Two changes the style asks for were left out because they do change the machine code of
the skip path, and have not been measured:

| Change | What it did to the code |
|---|---|
| Exhaustive match in `encoded_len_min` instead of a catch-all arm | In `read_list_header`, the minimum length is read from its table later, indexed by the element type instead of the type code |
| Exhaustive match in the list arm of `skip_nested` | Different register allocation and block order through `skip_nested` |

## Where the time goes now

For the wide footer, the share of the CPU's issue slots is:

| Category | First version | Now |
|---|---|---|
| Useful | 51% | 56% |
| Instruction processing (back end) | 42% | 10% |
| Instruction delivery (front end) | 6% | 32% |
| Discarded (mispredicted branches) | 1% | 2% |

The loop is front-end bound: a dense chain of compares and taken branches per field
(about 3.5 bytes per field in these footers). Changes that add branches or move them
around have made it slower even when they removed instructions, and the compiler's
inlining in this loop is sensitive to small edits. Check the assembly
(`cargo xtask asm`) after any change here, not only the timings.

## A marking hook that costs the skip nothing (2026-11-22)

`skip_marking` records which bits of a value the skip reads, for the pass that opens a
file (`docs/perf/open.md`). It is the same code as `skip`: the skip functions take a
`Marker`, and the plain skip passes one whose methods are empty.

The release assembly of the plain skip was compared before and after: `skip_nested`
is the same 836 instructions, and only the numbers of its jump tables' labels differ.
`skip_varint` is now `#[inline(always)]`, which changes nothing for the plain skip (it
was inlined already) and removes a call for each integer from the marking one.

`skip_marking` costs 1.16 to 1.22 times `skip` on the large fixtures. Before the hooks
were forced inline, each mark was a call, and it cost 2.4 times.

The recursive function exists twice in the binary now, once for each marker. The
marking one has a larger stack frame: 144 bytes on AArch64 against 80.
