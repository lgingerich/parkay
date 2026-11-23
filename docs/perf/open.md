# Opening a file: sketch and measurements

> **Status: Historical** — M2 open-path measurements (2026-11-21–22) using the pre-corpus-refactor 6-fixture set and earlier workload names (`open_locations_narrow`, `open_statistics_narrow`, `arrow_no_statistics`, etc.).
> The current harness uses the 8-entry corpus in `bench-support/src/corpus.rs` across `benches/metadata.rs` (`open` / `open_narrow` / `open_all`) and `examples/metadata_memory.rs`; `cargo xtask report` commands below refer to local gitignored baselines from that run. See `docs/perf/README.md` to capture fresh measurements.

The open path of M2: the footer tail, the footer directory, the schema, and extraction
of column-chunk facts. This note starts with the sketch written before the code, as
`AGENTS.md` asks, and then logs what was measured.

## Sketch (written before the code, 2026-11-21)

Opening is sans-I/O, so of the four resources only memory and CPU are spent here.
Network and storage cost is the number of dependent requests, which the caller controls:
one when the tail it read holds the footer, two otherwise.

What the hardware allows, from the saved Thrift runs on the M3 Max (sketch rates in
decimal GB/s; Criterion throughput figures in the measurements below use binary GiB/s,
where 1 GiB/s ≈ 1.074 GB/s):

| Floor | What it stands for | Rate on `wide_10000x10` (10.6 MB) |
|---|---|---|
| `memcpy` | Memory bandwidth: the footer is copied once | about 70 GB/s (~65 GiB/s) |
| `scan` | One pass that touches every byte: the least any parser can do | about 18–20 GB/s (~17–18.5 GiB/s) |
| `skip` | Passing over every Thrift value: the least the lazy design can do | about 1.45 GB/s (~1.35 GiB/s) |

`skip` is far below `scan` because Thrift has no length prefixes. Each field's length
is known only after its header and, for varints, its bytes have been read, so the walk
is a chain of dependent steps of about ten cycles a field (about 3.5 bytes a field).

Expected cost of each part, as a hypothesis:

| Part | Expected cost | Why |
|---|---|---|
| Copy the footer | `memcpy` rate, a few percent of `skip` | One pass, no dependence between bytes |
| Build the directory | `skip` rate plus a little | One `skip` per column chunk and one 4-byte store; the chunks are nearly all of the footer |
| Decode the schema | Small next to the directory for wide tables | One element per column, against one chunk per column per row group |
| Extract a few columns | Negligible | A few short decodes per row group, reached through the directory |
| Extract every column's statistics | About as much again as opening | A second pass over every chunk, decoding where the first pass skipped |

So opening should cost a little more than `skip`, and the case to watch is the last
one, where two passes may lose to arrow-rs's single eager pass. `skip` itself is the
part that decides how close opening gets to `scan`.

## Measurements (2026-11-22)

M3 Max, release build, criterion, one benchmark process at a time with nothing else
running. `cargo xtask bench` saved each run as a baseline; the names are given so the
tables can be printed again with `cargo xtask report`.

Fixtures (pre-refactor set in `bench-support/src/fixtures.rs`):

| Fixture | Footer bytes | What it is |
|---|---|---|
| `parquet_testing` | 79 KB in 70 files | The real files of parquet-testing: small, mostly one row group |
| `narrow_20x1000` | 2.04 MB | 20 columns, 1000 row groups, the same values in each |
| `wide_1000x100` | 10.4 MB | 1000 columns, 100 row groups, the same values in each |
| `wide_10000x10` | 10.6 MB | 10,000 columns, 10 row groups, the same values in each |
| `varied_300x60` | 1.95 MB | Six column types, random values, strings of random lengths, random nulls |
| `alternating_300x60` | 1.76 MB | Every column chunk has other fields than the one before it in its column |

`varied` stands for real data and `alternating` for the worst case of anything that
learns from one row group about the next. The first four alone would have flattered the
change described below: they write the same values to every row group.

### First version: one skip per column chunk (`m2-first`)

Opening cost what the sketch said: `skip` plus a little.

| Fixture | `memcpy` | `scan` | `skip` | `open` | `open_all` | `arrow_statistics_all` |
|---|---|---|---|---|---|---|
| `parquet_testing` | 1.38 µs | 4.17 µs | 40.20 µs | 94.90 µs | 160.94 µs | 219.95 µs |
| `narrow_20x1000` | 24.27 µs | 102.74 µs | 1.397 ms | 1.426 ms | 3.436 ms | 4.133 ms |
| `wide_1000x100` | 129.88 µs | 523.40 µs | 7.114 ms | 7.090 ms | 17.666 ms | 20.478 ms |
| `wide_10000x10` | 128.39 µs | 536.91 µs | 7.320 ms | 7.666 ms | 17.976 ms | 21.664 ms |

Every acceptance target was met, but opening ran at 1.3 GiB/s where a pass that only
touches the bytes runs at 18.5 GiB/s. All of that gap is the Thrift walk.

### Passing over column chunks by comparing them

The walk is slow because each field's length depends on the field before. It cannot be
made much faster one chunk at a time (`docs/perf/thrift-skip.md`). But a column's
chunks in successive row groups are usually alike, and a chunk that agrees with an
earlier chunk of its column on every bit the walk reads is walked by the same steps, so
it has the same length. `skip_marking` records which bits those are, and a masked
comparison then replaces the walk. The code and the argument that it is sound are in
`src/metadata/template/` (the argument is in `compare.rs`).

What was built, in order, and what each step measured. `open`, in µs unless marked:

| Fixture | Before | 1 | 2 | 3 | Final |
|---|---|---|---|---|---|
| `narrow_20x1000` | 1426 | 477 | 563 | 297 | 173 |
| `wide_1000x100` | 7090 | 2275 | 2502 | 1417 | 959 |
| `wide_10000x10` | 7666 | 4099 | 3691 | 2832 | 2883 |
| `varied_300x60` | 1359 | 541 | 585 | 353 | 272 |
| `alternating_300x60` | 1368 | 1517 | 1425 | 1400 | 1380 |
| `parquet_testing` | 94.9 | 87.5 | 90.6 | 91.2 | 90.6 |

"Before" for the last three fixtures is the same build with templates switched off.
Baselines: `m2-first`, `m2-templates`, `m2-templates2`, `m2-templates3`, `m2-final`.

1. **Exact comparison, a template rebuilt at every chunk that differs.** Two to three
   times faster on the regular fixtures, and two costs far above the design: marking a
   chunk cost 2.4 times a plain skip, and the worst case was 10% slower than no
   templates at all.
2. **Marking inlined; the comparison moved into its own function.** Marking fell to
   1.2 times a plain skip (`skip_marking` against `skip`: 1.73 ms against 1.44 ms on
   `narrow_20x1000`). Each mark had been a call, and the mask was resized and zeroed for
   every chunk. The marking hooks and `skip_varint` are now `#[inline(always)]`, with
   the rare cases in a `#[cold]` function, and the mask stays zero-padded between
   chunks. The plain skip is the same code with hooks that do nothing: its release
   assembly is the same 836 instructions as before the hooks existed, checked by
   diffing the two.
   The comparison loop had been inlined into the 4,400-line assembly pass, where its
   values lived on the stack: about 20 ns a chunk. As `Run::pass`, a function of its
   own over slices (`#[inline(never)]`), it takes about 6 ns for a 105-byte chunk, by
   the profile. This run was slower overall on three fixtures because of the next point.
3. **Templates that learn which lengths vary.** A file offset one byte longer than the
   template's put every later chunk of the column on a slow path that compares twice.
   A template now remembers up to four lengths that vary and steps over them in the
   fast loop. The test `written_row_groups_are_passed_over_by_comparing` asserts the
   slow path handles `<= 10%` of chunks (observed under 3% on these fixtures).
   The budget also changed here: credit granted up front pays only for a column's first
   template, and only what matches have earned pays for another. The worst case went
   from 10% slower to no slower.
4. **Final: integers stepped over by a scan, and a template replaced once an integer
   has grown.** Stepping over a varint had gone through a reader and the general skip.
   And when an integer is longer than the template's and at least 32 row groups follow,
   the chunk replaces the template, so that later chunks are compared whole again.
   With fewer row groups the length is remembered as varying, which is why
   `wide_10000x10` did not move.

### Where opening stands (`m2-final`, with `m2-final-off` for the same build without templates)

| Fixture | `memcpy` | `scan` | `skip` | `open`, templates off | `open` | `open` ÷ `scan` |
|---|---|---|---|---|---|---|
| `parquet_testing` | 1.47 µs | 4.21 µs | 39.11 µs | 92.16 µs | 90.63 µs | 21.5 |
| `narrow_20x1000` | 24.05 µs | 102.46 µs | 1.440 ms | 1.498 ms | 172.63 µs | 1.7 |
| `wide_1000x100` | 124.73 µs | 522.84 µs | 7.411 ms | 7.450 ms | 959.19 µs | 1.8 |
| `wide_10000x10` | 134.11 µs | 535.22 µs | 7.257 ms | 7.839 ms | 2.883 ms | 5.4 |
| `varied_300x60` | 23.21 µs | 98.33 µs | 1.318 ms | 1.402 ms | 272.16 µs | 2.8 |
| `alternating_300x60` | 20.82 µs | 88.47 µs | 1.363 ms | 1.380 ms | 1.380 ms | 15.6 |

Opening is 5 to 9 times faster than the walk on files of many row groups, 2.7 times
on ten row groups, and unchanged where nothing matches or there is one row group.

What bounds each now. `varied_300x60/open` and `wide_10000x10/open` were profiled, at
step 3; the rest is read off the tables.

- **Many row groups** (`narrow`, `wide_1000x100`): opening runs at 10 to 11 GiB/s
  against 18.5 GiB/s for `scan`. What is left is the comparison, which reads three
  streams (the chunk, its template, and the mask), and the copy of the footer.
- **Ten row groups** (`wide_10000x10`): marking the first row group (a third of the
  time), the comparison (a third), decoding a schema of 10,000 columns (a fifth), and
  the copy.
- **Random data** (`varied`): the comparison (70% at step 3), which has to step over
  lengths in the columns that hold strings or nulls.
- **One row group, or small files** (`parquet_testing`): the walk, and allocation.
  Opening a small file costs about 1.3 µs where walking its footer costs 0.56 µs.
- **Nothing matches** (`alternating`): the walk. One template for each column is all
  that is wasted, and from the third row group on the pass does not look for more.

### Against arrow-rs

Each pair is Parkay against arrow-rs configured to do the least work that returns the
same thing. The number in brackets is how many times faster Parkay is.

| Fixture | Locations of 3 columns: `open_locations_narrow` / `arrow_no_statistics` | Statistics of 3 columns: `open_statistics_narrow` / `arrow_statistics_narrow` | Everything: `open_all` / `arrow_statistics_all` | Schema supplied: `open_schema_shared_locations_narrow` / `arrow_schema_shared` |
|---|---|---|---|---|
| `parquet_testing` | 106.4 µs / 209.3 µs (2.0) | 103.9 µs / 220.8 µs (2.1) | 153.3 µs / 221.8 µs (1.45) | 68.3 µs / 103.9 µs (1.52) |
| `narrow_20x1000` | 371 µs / 3.348 ms (9.0) | 478 µs / 3.582 ms (7.5) | 2.226 ms / 4.116 ms (1.85) | 370 µs / 3.343 ms (9.0) |
| `wide_1000x100` | 1.008 ms / 16.796 ms (16.7) | 1.003 ms / 17.097 ms (17.0) | 11.330 ms / 20.381 ms (1.80) | 953 µs / 16.706 ms (17.5) |
| `wide_10000x10` | 2.850 ms / 18.093 ms (6.3) | 2.851 ms / 18.383 ms (6.4) | 13.156 ms / 21.636 ms (1.64) | 2.506 ms / 16.927 ms (6.8) |
| `varied_300x60` | 290 µs / 3.091 ms (10.7) | 291 µs / 3.161 ms (10.9) | 2.253 ms / 3.335 ms (1.48) | 278 µs / 3.056 ms (11.0) |
| `alternating_300x60` | 1.405 ms / 3.014 ms (2.1) | 1.415 ms / 3.087 ms (2.2) | 3.075 ms / 3.014 ms (0.98) | 1.378 ms / 2.971 ms (2.2) |

The acceptance targets in `docs/design-notes.md` ("Metadata") are met on every fixture but one
cell (met later, by tuning the extraction decoder; see below). **Extracting everything
from `alternating_300x60` is 2% slower than arrow-rs.**
There the open pass saves nothing, extraction is a second full pass, and half the
chunks have no statistics for arrow-rs to decode. The target for that case is "no
slower", so this is a miss, on a fixture built to be the worst case.

Retained memory (`cargo run --release --example metadata_memory`), bytes:

| Fixture | Footer | Parkay | arrow-rs | arrow-rs ÷ Parkay |
|---|---|---|---|---|
| `parquet_testing` | 79,390 | 157,091 | 488,645 | 3.1 |
| `narrow_20x1000` | 2,039,343 | 2,136,945 | 8,682,226 | 4.1 |
| `wide_1000x100` | 10,377,964 | 10,846,860 | 43,206,726 | 4.0 |
| `wide_10000x10` | 10,637,860 | 12,049,141 | 45,889,086 | 3.8 |
| `varied_300x60` | 1,954,654 | 2,052,310 | 8,012,805 | 3.9 |
| `alternating_300x60` | 1,760,744 | 1,853,952 | 8,014,986 | 4.3 |

The templates are working memory of the open pass and are gone when it returns. They
are bounded to about the footer's own length (`Templates` in `template/mod.rs`).

### Extraction is now the slow part

`extract_all` runs at about 1 GiB/s on every fixture (10.3 ms for `wide_10000x10`), and
is unchanged by any of the above. (It was tuned afterwards: see the next section.)
With opening five times faster, it is three quarters or more of `open_all`. Narrow
extraction is cheap because the directory finds the chunks: 2 µs for three columns of
`wide_10000x10`.

### Tuning the extraction decoder (`m2-extract1` against `m2-final`)

The decoder had never been looked at. Its assembly showed a call for every field
header (`FieldCursor::next`), every integer, every binary, and every field it skipped,
each returning its result through memory. The design asked for field-header reading to
be inlined into the decoders, and it was not.

`FieldCursor::next`, `read_i32`, `read_i64`, `read_binary`, and `skip_field` are now
`#[inline]`, and `skip_field` passes over an integer itself instead of calling the
general skip. Nothing else changed.

| Fixture | `extract_all` before | after | `open_all` before | after | `arrow_statistics_all` |
|---|---|---|---|---|---|
| `parquet_testing` | 63.16 µs | 47.94 µs | 153.29 µs | 128.65 µs | 221.81 µs |
| `narrow_20x1000` | 2.038 ms | 1.526 ms | 2.226 ms | 1.767 ms | 4.116 ms |
| `wide_1000x100` | 10.359 ms | 7.828 ms | 11.330 ms | 8.948 ms | 20.381 ms |
| `wide_10000x10` | 10.263 ms | 7.906 ms | 13.156 ms | 10.828 ms | 21.636 ms |
| `varied_300x60` | 1.975 ms | 1.497 ms | 2.253 ms | 1.842 ms | 3.335 ms |
| `alternating_300x60` | 1.693 ms | 1.257 ms | 3.075 ms | 2.540 ms | 3.014 ms |

Extracting everything is 23% to 26% faster, extracting the locations of three columns
21% to 29%, and their statistics 34% to 39%. Opening and extracting everything is now
1.19 to 2.3 times faster than arrow-rs on every fixture, so the one missed target is
met: 2.540 ms against 3.014 ms on `alternating_300x60`. The arrow-rs column is from
`m2-final`; arrow-rs was not run again.

`skip` and `walk` moved by a few percent either way between the two runs, with no
pattern. The plain skip's assembly is unchanged.

Extraction still runs at about 1.25 GiB/s. In the profile of
`wide_10000x10/extract_all`, 80% of the time is in the decoder itself and 11% in
reading integers, which is still a call.

Tried and not kept: forcing the integer read inline. Forcing `read_i64` only moved the
call one level down, to `read_varint`. Forcing `read_varint` removed it but changed the
machine code of the plain skip, which shares it, so it was dropped without being
measured.

### Allocation on the open path (`m2-item8a`, `m2-parse-a`, `m2-tidy2a`)

The profile of `parquet_testing/open` put 40% of the time in the walk over column
chunks, 23% in the allocator, and 5% in checking that schema names are UTF-8. With the
schema supplied the allocator still took 16%.

| Change | `parquet_testing/open` | `open_schema_shared` |
|---|---|---|
| Before (`m2-lazy`) | 84.6 µs | 49.9 µs |
| Exact reservations for the directory and the row groups; room for schema names made once | 74.7 µs | 45.3 µs |
| The walk over chunks that cannot be compared, as its own function | 69.3 µs | 42.7 µs |
| Schema room bounded by the input, template columns reserved once (no shrink) | 71.1 µs | 42.7 µs |

The writer's name and the column orders stopped being decoded and stored at open just
before the first row; that change was not measured alone.

Two things were learned the hard way:

- **The walk's speed depended on what else was in the pass.** Inlined into the pass,
  it got about 10% slower on `alternating_300x60` (1.34 ms to 1.49 to 1.53 ms over
  three runs) when unrelated code in the pass changed. As `chunks_parse`, a function of
  its own, it is back to 1.39 to 1.42 ms.
- **Giving memory back costs as much as taking it.** Shrinking the schema's name
  storage to its exact size made `parquet_testing/open` 6.6% slower (75.8 µs against
  71.1 µs, twice). The room left over is kept.

Runs of the same build differed by up to 3%, and one run had another process using a
whole core, so differences of a few percent here are not findings. `extract_all` came
out 1% to 6% slower in the last full run than in `m2-extract1` with no change to the
extraction code; that has not been explained and may be noise.

### Tried and not kept, and candidates not tried

- **Vector instructions for the comparison.** The compiler leaves the masked comparison
  scalar, at one 8-byte word a cycle, however it is written: a loop over words, over
  pairs of words, or over sixteen byte lanes (which it expanded into sixteen scalar
  chains). The word loop was kept. A kernel written with `fearless_simd` would compare
  16 bytes for the same three loads. Candidate for when that dependency arrives.
- **Room reserved for the schema's nodes** (kept): `parquet_testing/open` fell 4.5%.
- **Candidate: extraction guided by templates.** A chunk that agrees with its template
  bit for bit has every field where the template has it, so its facts could be read at
  known offsets without walking the fields between. This needs the pass to keep, for
  each chunk, which chunk it matched: about one more word per chunk of retained
  memory. Not built; it changes what `Metadata` retains.
- **Candidate: extracting while opening** when a caller wants everything, which would
  remove the second pass on `open_all` (where extraction remains the dominant cost,
  even though tuning the extraction decoder above brought `alternating_300x60` within
  its target).
- **Candidate: validating all the schema's names as UTF-8 at once**, with a check that
  each name starts on a character boundary. Name validation is about 5% of opening
  `wide_10000x10`.
- **Candidate: fewer allocations when opening a small file.** About a dozen now.

Notes on procedure (historical; current policy in `.claude/skills/benchmarking/SKILL.md`
requires running benchmarks directly rather than delegating to subagents): an Opus
subagent ran these benchmarks, one command at a time. In the third run two commands
were sent together by mistake; both were run again alone and the saved numbers are from
those runs.

Not measured: the external footer set (`PARKAY_BENCH_DIR`), which was not on the
machine. Every number here is from the six built-in fixtures.
