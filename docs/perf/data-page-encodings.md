# Extracting data page encodings: measurements

`Request::with_data_page_encodings` reads the footer's `encoding_stats` for each column chunk into `Location::data_page_encodings`. This note records what it costs and why it is a request of its own. The design is in [`docs/subsystems/metadata.md`](../subsystems/metadata.md).

M3 Max, release build, 2026-11-25. Numbers are copied from `cargo xtask report`; the baselines are named so the tables can be printed again.

## Existing requests are unchanged

Paired run of the tree before the change against the tree with it, six rounds, over `open_narrow` and `open_all` on the whole corpus (baseline `es-noreg`): 15 of 16 comparisons within noise. The one verdict was `many_row_groups/open_narrow`, 879.16 µs to 885.60 µs (+0.7%). The new fields are specialized out of every request that does not ask for them.

## What the encodings cost

`open_locations` opens a file and extracts every column's location. `open_encodings` does the same and asks for the data page encodings. Both are in `benches/metadata.rs`; baseline `es-cost2`.

| Entry | `open` | `open_locations` | `open_encodings` | `open_all` |
|---|---|---|---|---|
| `medium` | 102.53 µs | 291.82 µs | 371.62 µs | 361.67 µs |
| `wide` | 362.36 µs | 1.074 ms | 1.435 ms | 1.354 ms |
| `many_row_groups` | 585.03 µs | 1.975 ms | 2.732 ms | 2.584 ms |
| `very_wide` | 7.045 ms | 10.548 ms | 12.496 ms | 11.978 ms |
| `many_files` | 1.273 ms | 2.970 ms | 3.791 ms | 3.665 ms |
| `nested` | 6.44 µs | 14.88 µs | 19.09 µs | 18.47 µs |

Asking for the encodings adds 18% to 38% to opening a file and extracting every location (`very_wide` and `many_row_groups`), and costs about as much as extracting every column's statistics.

The cost is structural. `encoding_stats` is field 13 of a chunk's metadata and its statistics are field 12, so reaching the first means passing over the second, and a request for locations alone stops before either. That is why the encodings are not part of every location: only a caller that wants dictionary codes pays for them.

## The decoder

The first version decoded each `PageEncodingStats` field by field (baseline `es-cost`). A profile of `medium/open_encodings` put 16.0% of the workload in that decoder and 13.9% in skipping statistics.

Writers emit the three fields of an entry in order, as one-byte values except for the page count. `CompactReader::try_fold_short_i32_triple_list` matches that shape directly and leaves anything else to the field-by-field decoder. The same profile then put 5.8% in the decoder (profiles `es-enc1` and the run after it).

| Entry | `open_encodings` before | after | change |
|---|---|---|---|
| `medium` | 448.38 µs | 371.62 µs | -17.1% |
| `wide` | 1.702 ms | 1.435 ms | -15.7% |
| `many_row_groups` | 3.289 ms | 2.732 ms | -16.9% |
| `very_wide` | 13.862 ms | 12.496 ms | -9.9% |
| `many_files` | 4.401 ms | 3.791 ms | -13.9% |
| `nested` | 22.37 µs | 19.09 µs | -14.6% |

`es-cost` and `es-cost2` are separate runs, not paired rounds. The control drifted by at most 0.2% between them.

What remains is passing over the statistics, which runs in the general Thrift skipper at the rate `docs/perf/thrift-skip.md` describes.
