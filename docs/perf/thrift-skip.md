# Thrift `skip`: performance log

What was measured on `CompactReader::skip`, so code comments can stay short. All
numbers are from `cargo xtask bench '/skip$'` on one Apple M3 Max (quick mode unless
noted), as changes in time against the version before. A negative change is faster.
Fixtures are in `bench-support`. Counters are from Xcode's `xctrace` CPU Counters
template.

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
| `#[inline(always)]` on `read_byte`, `skip_varint`, `take_array`, `read_size` | +25% to +30% | |
| Struct loop isolated with `#[inline(never)]` | +4% to +10% | With `cold_path` on top, still worse |

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
