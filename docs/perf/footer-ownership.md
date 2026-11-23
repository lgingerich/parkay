# Footer ownership: cost of copying the footer at open

> **Status: Historical** — Pre-M2 footer-copy measurement (2026-11-21) using pre-refactor fixture names and removed temporary `benches/thrift.rs` workloads.
> Pre-templates, `memcpy` was 2%–4% of `skip`; after M2 chunk templates accelerated multi-row-group opens, `memcpy` is ~13%–14% of `open` (`docs/perf/open.md`).

The measurement behind the decision to copy the footer at open (`docs/design-notes.md`,
"Retained metadata: ownership and shape"). It was taken once, on 2026-11-21, on one Apple M3 Max, with
two temporary workloads in `benches/thrift.rs` that were then removed. M2's open path
now exists (`docs/perf/open.md`); benchmark that path directly instead of restoring them.

- `box_copy`: copy each footer into a new `Box<[u8]>`, then free it.
- `box_copy_skip`: `box_copy`, then `skip` over the box. This stands in for opening
  a file.

There were two conditions. In the default run, macOS's allocator reuses the freed
block on every iteration, so the pages never fault, as in a long-running process.
With `MallocLargeCache=0`, every allocation faults on first touch, as on the first
open of a large file.

| Fixture | Footer size | `box_copy_skip` − `skip`, pages reused | `box_copy_skip` − `skip`, faults |
|---|---|---|---|
| parquet_testing | 79 KB (many small footers) | +8% | +10% |
| narrow_20x1000 | 2 MB | +5% to +8% | +8.5% |
| wide_1000x100 | 10.4 MB | within noise | +12% (about 0.85 ms) |
| wide_10000x10 | 10.6 MB | +3% to +4% | +11% (about 0.8 ms) |

- With pages reused, `box_copy` costs the same as `memcpy` into a buffer that is
  already allocated. The fresh allocation itself is nearly free.
- With faults, `box_copy` costs about 5× `memcpy`, roughly 50 µs per MB. The cost
  is first-touch faults and zero-filling, not the copy.
- Small footers pay a fixed malloc and free each time, about 2× `memcpy`.
- `skip` alone varied by up to 250 µs between runs, so the pages-reused figures for
  the 10 MB footers are at the level of the noise.
- Not measured: glibc's allocator on Linux, which hands large blocks back to the
  operating system more readily.

Conclusion: copying into fresh memory costs more than the copy when the pages
fault. This first suggested an entry point that takes an owned buffer and keeps
it. That was set aside in review (the reasons are in `docs/design-notes.md`,
"Retained metadata: ownership and shape"), because of what this measurement does not show:

- It compares against `skip` alone, the smallest possible open. At the time of this
  measurement, the real open path was expected to spend most of its time walking
  chunks, building the directory, and decoding the schema, making the copy a smaller
  share.
- It charges the faults to the copy. Fresh memory faults for whoever touches it
  first, so a caller that allocates a buffer per file in order to hand it over
  pays the same faults when it reads into it. They are saved only when the
  caller's buffer is used once and is exactly the size of the footer.
- A caller that reuses one fetch buffer is the pages-reused case, where the copy
  costs 3% to 8% of `skip`.

Decision (2026-11-21): parkay copies the footer from the caller's `&[u8]` into
its own exact-size storage. What an owned buffer would save is one `memcpy` of
the footer, not the faults. In the pre-template saved runs that `memcpy` was 2% to 4%
of `skip` over the same bytes:

| Fixture | `memcpy` | `skip` | `memcpy` as a share of `skip` |
|---|---|---|---|
| parquet_testing | 1.6 µs | 43.4 µs | 3.7% |
| narrow_20x1000 | 24.2 µs | 1.47 ms | 1.6% |
| wide_10000x10 | 152 µs | 7.34 ms | 2.1% |

Against that saving, an owned buffer cannot be reused by the caller from file to
file, pins the whole speculative tail when the tail is larger than the footer,
and has to be handed back when opening needs more bytes. M2's measurements on the real
open path (where chunk templates lowered `open` time so `memcpy` is now ~13%–14% on
multi-row-group files) are logged in `docs/perf/open.md`.
