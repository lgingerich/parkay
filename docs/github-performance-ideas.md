# Performance ideas from other projects

This file collects performance techniques from other Parquet readers and
writers, and ranks them by how useful they are likely to be to Parkay. Two
sweeps fed it, both on 2026-11-22:

- **GitHub threads.** Twelve agents read issues and pull requests in arrow-rs,
  Arrow C++, parquet-java, parquet-format, DuckDB, DataFusion, Polars (with
  arrow2 and parquet2), ClickHouse, Velox, Trino, Spark, StarRocks, Doris,
  Vortex, Lance, FastLanes, cuDF, Iceberg, iceberg-rust, arrow-go, and
  hyparquet.
- **Reader source code.** Five agents read the source of five readers, pinned
  to these commits:

| Reader | Language | Commit | Code |
|---|---|---|---|
| Apache Doris native reader | C++ | `f6b5f3d` | `be/src/format_v2/parquet/` |
| parquet2, and what Polars built from it | Rust | `b0e6545` (parquet2), `7ff0ffa` (Polars) | `crates/polars-parquet/src/` |
| Trino | Java | `010fed1` | `lib/trino-parquet/` |
| ClickHouse native reader v3 | C++ | `91b044d` | `src/Processors/Formats/Impl/Parquet/` |
| carquet (Vitruves/carquet) | C11 | `d61c669` | the whole repository |

These are leads, not findings. Every number is as its source reported it, on
its own hardware, and none was reproduced. The source sweep's claims were read
in code by the agents. A sample was checked again by hand: Trino's selection
constants, Doris's fragmentation constants and PARQUET-816 padding, Trino's
single-run probe, Polars's UTF-8 masks, ClickHouse's run statistics and
footprint bound, and carquet's unpacking tables and benchmark loop. Source links
are pinned to commit and file (not line number) in [References](#references).

## Contents

- [How this list is ranked](#how-this-list-is-ranked)
- [Tier 1. Page framing and the first column reads (step 2)](#tier-1-page-framing-and-the-first-column-reads-step-2)
- [Tier 2. Interface decisions](#tier-2-interface-decisions)
- [Tier 3. The remaining flat encodings and codecs (step 4)](#tier-3-the-remaining-flat-encodings-and-codecs-step-4)
- [Tier 4. Strings and dictionary output (step 4; views deferred to step 9)](#tier-4-strings-and-dictionary-output-step-4-views-deferred-to-step-9)
- [Tier 5. The reference scanner: I/O, prefetch, and scheduling (step 4)](#tier-5-the-reference-scanner-io-prefetch-and-scheduling-step-4)
- [Tier 6. Selection-aware decoding and pruning (step 5)](#tier-6-selection-aware-decoding-and-pruning-step-5)
- [Tier 7. Nested data (step 7)](#tier-7-nested-data-step-7)
- [Tier 8. Metadata still to build](#tier-8-metadata-still-to-build)
- [Tier 9. Writer (step 8)](#tier-9-writer-step-8)
- [Low value, negative results, and patterns to avoid](#low-value-negative-results-and-patterns-to-avoid)
- [References](#references)

## How this list is ranked

The assessments were written on 2026-11-22 against `docs/design.md`. Ideas are
ranked by three things, in this order:

1. **How soon Parkay builds the code it affects.** The order of work (design.md
   section 13) is page framing, a scalar read of flat nullable integers, the
   first SIMD kernels on NEON and AVX2, dictionary columns, the fusion
   experiment, the remaining flat encodings, string/dictionary output, the
   scanner, selection-aware decoding, nested data, and then the writer. An idea
   that changes the next piece of work ranks above a larger one that waits.
2. **How much of the distance to the hardware floor it can close.** The measure
   is the memory-copy and scan floors, not arrow-rs.
3. **Fit with Parkay's constraints.** No `unsafe` without sign-off, no padding
   required on borrowed input, nothing that exists on only one of NEON and
   AVX2, one canonical path, and decoders that do not push values into a
   generic consumer. An idea that conflicts with a decision is marked
   **Conflicts**, and the entry says which decision.

"In design" means design.md, or the working notes in `docs/design-notes.md`,
already takes the idea; the entry stays for its technique or its numbers.
"Partly" means the design names the idea but not the form given here. Within
each section, the most useful idea comes first.

The list was pruned on 2026-11-23. Removed: what M2 already built, entries with
nothing to adopt, ideas outside the closed predicate set or the pinned format,
duplicates, x86-only tricks with a portable equivalent already listed, and
mistakes in other readers that Parkay's rules already forbid.

---

## Tier 1. Page framing and the first column reads (step 2)

These bear on the code that comes next: framing one column chunk,
decompressing its pages, and reading flat nullable integer and dictionary
columns, first in scalar code and then with SIMD.

### 1.1 Page framing and decompression

- **A column chunk can extend up to 100 bytes past the length its metadata
  gives.** parquet-mr before PARQUET-816 under-reported the chunk length. Doris
  identifies those writers from `created_by` and pads the range. It uses the
  same table to decide whether V2 pages are always compressed (the PARQUET-1053
  fix) and whether `null_count` can be trusted.
  ([doris/column_chunk_reader.cpp][doris-chunk])

  *Assessment.* This is correctness, not speed, and it belongs in page framing
  from the first version. `docs/design-notes.md` ("Opening a file") says a
  chunk's range "is derived the way other readers derive it"; this is one of
  the rules that sentence has to cover. The padded range must still end inside
  the file.

- **In V2 pages, decompress only the values.** The levels are stored
  uncompressed in front of them, and the page's `is_compressed` flag is
  respected. Doris also validates the declared sizes before reserving memory.
  ([doris/column_chunk_reader.cpp][doris-chunk],
  [parquet2/compression.rs][pq2-comp],
  [polars/read/compression.rs][polars-read-comp])

  *Assessment.* Correctness, plus a small saving. Doris and Polars copy the
  levels out; Parkay need not, because its kernels read borrowed input, so the
  levels can be decoded from the fetched bytes in place. Partly in design.

- **Decompress into memory that has not been zero-filled.** Polars estimates
  5–10%. The `snap` crate has no API that writes into uninitialized memory, so
  Polars vendored its decoder and changed it to write `[MaybeUninit<u8>]`. For
  zstd it decodes into spare capacity and zeroes only a shortfall.
  ([polars#29980][polars-29980], [arrow-rs#9579][arrow-rs-9579],
  [polars/snappy.rs][polars-snappy], [polars/compression.rs][polars-comp])

  *Assessment.* Worth having, but it runs straight into the `unsafe` rule.
  A column buffer that is reused is zero-filled only when it grows, which
  happens on the control plane and rarely. In steady state the fill already
  costs close to nothing, without any `unsafe`. So the first version can grow
  buffers by zero-filling and reuse them from then on. Only a workload that
  opens many small column chunks into fresh buffers pays the fill repeatedly,
  and that should be measured before any exception is requested. The gap in `snap`'s API is noted in `docs/design.md` under Chosen
  dependencies. In design for scratch.

- **Decompress zstd in one shot into the page buffer, and reuse contexts.**
  Streaming decompression allocated about 2.4 MiB per page; one-shot
  decompression was 5–12% faster (ClickHouse). Creating contexts was 11% of
  query CPU in one service. ClickHouse keeps one context per thread; in
  arrow-rs, per-thread contexts were disputed as a leak.
  ([clickhouse#133229][ch-133229], [arrow-rs#12294][arrow-rs-12294],
  [arrow-rs#12250][arrow-rs-12250], [clickhouse/Reader.cpp][ch-reader])

  *Assessment.* In design: contexts live in the worker context, and one-shot
  decompression follows from knowing the decompressed size. To check: that
  `zrip` offers a one-shot call into a caller's slice and a reusable context.
  If it does not, that is a reason to revisit the choice.

- **Bound what a page or dictionary will take from its header, before decoding
  it.** ClickHouse computes an upper bound on a dictionary's decoded footprint
  (twice its logical size plus 256 bytes of slack) and rejects the page before
  materializing anything. Doris checks a dictionary's entry count against its
  bytes before reserving, a DELTA_LENGTH_BYTE_ARRAY total against the bytes
  left, and each DELTA_BYTE_ARRAY prefix against the previous value. Velox,
  arrow-go, and parquet2 each fixed this class of bug.
  ([clickhouse/Decoding.cpp][ch-decoding],
  [doris/column_chunk_reader.cpp][doris-chunk], [velox#18259][velox-18259],
  [arrow-go#1473][arrow-go-1473], [parquet2#172][pq2-172])

  *Assessment.* In design under untrusted input and limits. The Doris list is
  a checklist for page framing and for each decoder as it is written.

- **Grow column buffers geometrically, cap the growth, and give memory back
  when a buffer is oversized.** carquet doubles a buffer but never past the
  chunk's value count. Doris rounds up to a power of two, and after a page
  transition frees the buffer only if it is oversized. Vortex shrank buffers
  left with spare capacity (up to 1.9x). StarRocks reserved the exact size for
  each page while appending, which made decoding quadratic (0.2 s to 46 s).
  parquet-java reused its RLE decoder's buffers for packed runs, and
  allocation fell from 13.5 to 0.002 bytes per value.
  ([carquet/page_reader.c][carquet-page], [parquet-java#3749][pqj-3749],
  [vortex#11434][vortex-11434], [starrocks#83259][sr-83259])

  *Assessment.* In design in principle: a buffer grows only when a page is
  larger than any before it. Two parts are not stated: growth must never be an
  exact reserve per page, and something must decide when to give memory back.
  The second is scanner policy, because the buffers belong to the caller.

- **Skip a page without decompressing it, and carry a skip that ends partway
  into a page over to the next page.** Trino advances its iterator over
  compressed pages and records the remainder as a deferred count. It looks
  ahead at most 8 compressed pages.
  ([trino/FlatColumnReader.java][trino-flat],
  [trino/PageReader.java][trino-page])

  *Assessment.* In design for pruned pages. The deferred count is a concrete
  form of the rule that every column decoder can resume at any row. Without an
  offset index, each header must still be parsed to find the next page.

- **Size a page-header read from a small guess that doubles**, not a fixed
  16 KB per page. StarRocks counted about 78 GB copied over 102 queries.
  ([starrocks#81535][sr-81535])

  *Assessment.* In a sans-I/O core this becomes: what range does framing
  report when a header is not in the bytes supplied? Headers usually sit inside
  chunks that were fetched whole, so this matters only to callers that fetch
  one page at a time.

### 1.2 The hybrid decoder and bit-unpacking

- **Unpack bit-packed groups without reading past the end of the input.**
  Eight values of width W take exactly W bytes. carquet loads a group as two
  overlapping pieces: one anchored at its start, the other ending at its last
  byte (at W−16, W−8, or W−4). It then applies a byte shuffle, a shift that
  differs per lane, and a mask, using tables built at compile time, for widths
  5 to 25. ClickHouse and parquet2 handle the end differently: they copy the
  last partial group into a zeroed buffer on the stack and unpack it from
  there. ([carquet/avx2_ops.c][carquet-avx2],
  [clickhouse/Decoding.cpp][ch-decoding], [parquet2/unpack.rs][pq2-unpack])

  *Assessment.* This ranks first in the list, because the first task of
  step 2 is this kernel. Both forms meet the rule that kernels require no
  padding on borrowed input. Copying the tail is simpler and costs one small
  copy per run, so it is the default. Overlapping loads matter when runs are
  short, because the body of a short run has no room for wide loads that run
  past the group. ClickHouse's code comment gives an average of about 39 values
  per bit-packed run in ClickBench's hits.parquet, which is five groups. Both
  forms map onto NEON: `tbl` does the shuffle, and `ushl` with a negative count
  does a right shift that differs per lane. Neither breaks the parity rule.

- **Choose one loop per bit width per page or run, not per block, and unroll
  each width with constant shifts.** ClickHouse gained 8–15%. Code comments in
  parquet2 credit the unrolled form with 4.5–5x and the jump table with
  2–2.5x. Velox's unpacker built on BMI2 `PDEP` reports 5.5–16x over Arrow, but
  needs a shift-and-mask fallback on ARM and on AMD Zen 1 and 2, where `PDEP`
  is microcoded. ([clickhouse#128262][ch-128262],
  [arrow-rs#11027][arrow-rs-11027], [velox#17993][velox-17993])

  *Assessment.* In design: kernels are monomorphized per width and chosen
  through a table. `PDEP` exists only on x86, so under the parity rule it can
  be an extra on the AVX2 tier but cannot be the design. The shuffle-and-shift
  form above runs on both targets.

- **Treat a run as one operation, and keep a single-value state.** An RLE run
  is filled or skipped in one step; parquet-java's skip that advances per run
  rather than per value was 76x faster on runs. In parquet2, most
  dictionary-index runs on one string column had length 1, and a state for a
  single value gained about 40%. ClickHouse accumulates counts in locals,
  because a store through an output pointer could alias and cost a load and a
  store per value. ([parquet-java#3773][pqj-3773], [parquet2#203][pq2-203])

  *Assessment.* In design: runs and packed groups are what the column reader
  works on. The hits.parquet averages, about 39 values per packed run and 91
  per RLE run, are a first check on block size. A block of a few hundred values
  spans several runs, so choosing a kernel per run must stay cheap next to 39
  values of work.

### 1.3 Dictionary decoding

- **Skip the bounds check on dictionary indices when the bit width already
  proves it.** If 2^bit_width is at most the dictionary's length, no index can
  be out of range. In arrow-rs about 40% of the dictionary path was
  validation. ([arrow-rs#11414][arrow-rs-11414],
  [arrow-rs#11462][arrow-rs-11462])

  *Assessment.* The cheapest win in this file, and safe, because the proof is
  one comparison per run. It applies whenever the dictionary's length is a
  power of two, or the writer chose a width that the dictionary fills; writers
  pick the smallest width that holds the dictionary. Otherwise the index is
  validated once per block, as designed. Not in design.

- **Otherwise, validate once per batch, then gather with no per-index
  check.** Doris takes a vector maximum over the batch and compares it once.
  ClickHouse unpacks 512 indices onto the stack, tracks their maximum, and
  checks once. Polars validates chunks of 32 indices. Doris and carquet then
  gather with AVX2 `vpgatherdd` and `vpgatherqq`.
  ([doris/decoder.h][doris-decoder],
  [doris/parquet_kernels.cpp][doris-kernels],
  [clickhouse/Decoding.cpp][ch-decoding])

  *Assessment.* Validating once per block is in design. Two points are not
  settled.
  - Whether to use the gather instruction. NEON has none. On Intel cores from
    Skylake to Ice Lake, the 2023 microcode fix for the "Downfall" (GDS) flaw
    made AVX2 gathers several times slower. That fact is general knowledge,
    not from these sources. So the portable kernel is an unrolled loop of
    scalar loads, and the hardware gather is measured against it on AVX2
    before it is written. This also bears on the open question of whether
    `fearless_simd` offers a safe gather: the scalar form needs none.
  - How to drop the per-index check in safe Rust. A validated maximum does not
    remove the compiler's bounds checks. A form that does: decode the
    dictionary into a buffer Parkay owns, sized up to a power of two, and index
    with `index & (capacity - 1)`. The mask makes every index provably in
    range, so the check disappears and the loop can vectorize. The mask changes
    nothing for indices that were validated. Owned buffers may carry padding,
    so this breaks no rule.

- **Turn RLE runs of indices into bulk fills, and never build the index
  array when values are materialized.** ClickHouse's `decodeAndIndex` makes an
  RLE run one repeated append, and gathers packed indices straight into the
  output. Doris fuses decoding and gathering, with one exception: when the
  dictionary is larger than L2 and the selection is sparse, it materializes
  the selected indices first, so the dictionary is never touched for rows
  already removed. ([clickhouse/Decoding.h][ch-decoding-h],
  [doris/column_chunk_reader.cpp][doris-chunk])

  *Assessment.* Unpacking plus a dictionary gather is already a named fusion
  candidate. Doris's rule about L2 is a good first rule for when not to fuse.
  It is decided once per page, from the dictionary's size, which the design
  allows.

- **Narrow dictionary index widths (`u8`/`u16`/`u32`) — decided (2026-11-23).**
  Dictionary-preserved output writes codes in the narrowest integer width that
  holds the dictionary (`u8`, `u16`, or `u32`), cutting output bandwidth by up
  to 4×. Adopted in `docs/design.md` (section 5, Output forms).
  ([polars/dictionary_encoded/mod.rs][polars-dict-mod],
  [polars/bitpacked/decode.rs][polars-bp-decode])

- **Convert the dictionary, not the rows.** Doris converts a dictionary once,
  for example INT96 to timestamps, and gathers from the converted entries.
  arrow-rs and Polars materialize and validate only the entries that rows
  reference; otherwise a dictionary of 100K entries costs as much for 2 rows as
  for 200K. ([doris/column_chunk_reader.cpp][doris-chunk],
  [arrow-rs#11303][arrow-rs-11303], [arrow-rs#11381][arrow-rs-11381],
  [polars#30248][polars-30248])

  *Assessment.* Converting through the dictionary extends the design's rule
  for predicates on dictionaries. Materializing only referenced entries needs
  a bitset of the codes used, which costs a pass over the codes. It pays only
  when far fewer rows are selected than the dictionary has entries.

### 1.4 Definition levels and nulls in flat columns

- **When the maximum definition level is 1, a bit-packed group of definition
  levels is already the validity bitmap.** Both hold one bit per value, least
  significant bit first. Trino reads packed groups as 64-bit words and counts
  non-null values with `bitCount`. Decoding levels straight into the bitmap
  gained 53–91% in Arrow C++ and 1.2–3.3x in ClickHouse.
  ([trino/NullsDecoders.java][trino-nulls], [arrow#50629][arrow-50629],
  [clickhouse#60361][ch-60361])

  *Assessment.* In design: levels are decoded directly into the bitmap. What
  is new is how little that takes. For packed groups it is a copy with a shift
  plus a popcount; for runs it is a fill of bits. Both run at memory speed, and
  the popcount gives the number of values to decode, which the column reader
  needs anyway. Noted in `docs/design-notes.md` (SIMD kernel list scoped to nested
  columns), so flat nullable columns need no kernel that converts levels to a
  bitmask. carquet's 16-bit
  compares with `movemask` work on levels already decoded to integers, which
  only nested columns need.

- **Prove from the page itself, cheaply, that it has no nulls.** Trino's
  `isSingleRleRun` parses only the first run header of the definition levels.
  If that one RLE run holds the maximum level for every value, the page has no
  nulls and level decoding is skipped. Trino also skips definition levels
  when the chunk's statistics give `null_count` 0. Doris trusts `null_count`
  only from writers known to write it correctly. DuckDB (about 15%), arrow-go
  (36–45%), and arrow-rs check for no nulls per batch.
  ([trino/RleBitPackingHybridDecoder.java][trino-rle],
  [trino#32222][trino-32222], [duckdb#16315][duckdb-16315],
  [arrow-go#1447][arrow-go-1447], [arrow-rs#11376][arrow-rs-11376])

  *Assessment.* Take the single-run probe; do not take the shortcut from
  statistics. Statistics are a claim nothing checks against the pages. If
  `null_count` is wrong, skipping the levels makes the reader expect more
  values than the page holds. With PLAIN values that usually surfaces as an
  error, because the values run out. With dictionary indices the padding of the
  last bit-packed group can supply the missing values, and every value after
  the first null lands in the wrong row: wrong data, which the safety
  constraint rules out. Trusting the statistic would also save little, since
  decoding levels of maximum 1 runs at memory speed (above). The probe gives the
  same saving from the page's own bytes, at the cost of reading one varint. The
  per-batch check for no nulls is in design.

- **Spread values into their rows using a table indexed by validity bits.**
  Doris and carquet index a 256-entry table with 8 validity bits to drive
  `vpermd`, and advance through the dense values by popcount. Doris writes
  backwards, so it can expand in place in the output buffer. arrow-rs works one
  mask word at a time: it copies words where every value is valid, and writes
  mostly valid words without branches (1.2–1.6x). cuDF picks its loop by null
  density. ([doris/parquet_kernels.cpp][doris-kernels],
  [carquet/avx2_ops.c][carquet-avx2], [arrow-rs#12275][arrow-rs-12275],
  [cudf#24221][cudf-24221])

  *Assessment.* The expand kernel is in design; this gives its form. NEON has
  no `vpermd`, so its table holds byte indices for `tbl`: one table indexed by
  4 validity bits covers four 32-bit lanes. That is the table-driven
  equivalent design.md requires. Expanding backwards in place lets the
  decoder write dense values straight into the output with no scratch pass.
  The bytes moved are the same, but the scratch footprint goes away. The
  choice by density is made per block, where the column reader already makes
  its choices.

- **Write the validity bitmap only once a null appears.** Trino allocates the
  bitmap but writes nothing to it until the first null, so a column with no
  nulls writes no bitmap. ([trino/FlatColumnReader.java][trino-flat])

  *Assessment.* An output-interface question for section 2.2. If a result can
  say "every row valid" (Arrow allows an absent null buffer), a page with no
  nulls writes no bitmap. That saves one bit per value of output: an eighth
  for one-byte values, a sixty-fourth for eight-byte values. Small, but free.

### 1.5 Benchmarking the first reads

- **carquet's uncompressed-read numbers do not measure a read.** Its
  benchmark touches only the first and last value of each column in each
  batch, with checksums off. For 10M rows the timed region is mostly the setup
  of views over a memory map. Its README reports 36–62x over Arrow C++ for
  these reads. ([carquet/benchmark_carquet.c][carquet-bench])

  *Assessment.* A warning when comparing against carquet, and a rule for
  Parkay's own benchmarks: the timed loop touches every output byte, for
  example with a checksum over the buffers, or a zero-copy path can win by
  doing nothing. carquet's compressed reads, 1.9–3.1x over Arrow C++ on an M3
  with the same zstd and lz4 libraries on both sides, are more meaningful.
  They were not checked.

- **Check every implementation against an oracle before timing it, and build
  fixtures outside the timed region.** Doris builds a legacy and a fused
  implementation into one binary and checks both against an oracle. It runs
  nested-selection cases in ABBA order on a pinned core. Its fused nullable
  path is enabled only for batches of at least 1,024 rows with at least 10%
  nulls and fragmented levels, and a matrix of negative controls checks that.
  Decoder and reader benchmarks are separate (152 and 137 cases).
  ([doris/benchmark/README.md][doris-bench-readme], [doris#65921][doris-65921])

  *Assessment.* The fusion experiment (step 3) needs exactly this: composed
  and fused kernels in one binary, checked equal, and measured both where
  fusion should help and where it should not.

- **Report decoded bytes per second**, not encoded bytes.
  ([arrow#38389][arrow-38389])

  *Assessment.* Needed for the comparison with the floor, because the memory
  copy floor applies to output bytes.

- **Benchmark the dense-null regime explicitly**; one suite never reached it.
  ([cudf#24263][cudf-24263])

---

## Tier 2. Interface decisions

These ideas bear on interfaces that should be fixed before the vertical slice
settles them by accident. Sections 2.1, 2.4, and 2.5 were decided on
2026-11-23; the rest remain open.

### 2.1 Dictionary fallback (decided)

**Decided (2026-11-23):** Decide once per column chunk before decoding —
produce dictionary-preserved output only when every data page is
dictionary-encoded (checked via `encoding_stats` or page headers, not
`encodings` alone) and materialize otherwise; merging dictionaries across
fallbacks is deferred. See `docs/design.md` (section 5, Output forms) and
`docs/design-notes.md` ("Dictionary fallback").
([trino/AbstractColumnReader.java][trino-abs-col], [duckdb#24957][duckdb-24957],
[polars/page/reader.rs][polars-page-reader], [arrow-rs#12195][arrow-rs-12195])

### 2.2 The output interface

- **Dictionary index width** (section 1.3) and an **"every row valid"
  result** (section 1.4) both change what the output interface reports.
- **Know a string page's total size before copying it.** Trino reads plain
  byte arrays in two passes. The first fills the offsets array with lengths and
  sums them; the second copies every value's bytes in one go.
  ([trino/PlainByteArrayDecoders.java][trino-plain-ba])

  *Assessment.* With output buffers the caller owns, the bytes buffer can be
  too small for a page. A first pass over the lengths gives the exact size, so
  the call can report "needs N bytes" as a control-plane result before writing
  anything, instead of stopping partway through a page. The pass is cheap, and
  the UTF-8 check in section 2.5 can share it.

- **Monomorphize kernels per physical byte width, not per Rust type.** Polars
  generates its kernels per aligned width (`AlignedBytes`). Flags say whether
  a value needs converting after the raw copy, and a conversion runs only
  where the representations differ.
  ([polars/primitive/plain/mod.rs][polars-prim-plain])

  *Assessment.* Adopted in `docs/design.md` (section 4): monomorphizing over
  physical byte width divides the generated code by the number of types that
  share a width (`i32`, `u32`, `f32`, and dates share one kernel), converting
  only where representations differ.

### 2.3 Selection representation and the choice per page

- **Trino chooses one of three modes per batch of selected positions:** full
  decode; by row, seeking and decoding only selected runs; or by page,
  skipping whole pages. Its constants: by row only if at most 5% of rows are
  selected for fixed-width columns (20% if nullable), 10% for dictionary
  columns, and 25% for binary columns. By page if up to 75% are selected. A
  skipped run must cover at least 300 positions, and a skipped page at least
  4 KB of estimated bytes. Booleans, and binary values of 1 KB or more with
  more than 4 projected columns, never use the by-row mode.
  ([trino/ParquetReader.java][trino-reader])
- **Doris decodes a whole span once when the selection is fragmented:** at
  least 8 ranges, at most 4 selected values per range on average, and no more
  than 8 decoded values per selected one. Otherwise it skips and decodes range
  by range. Its selections are u16 index lists plus ranges, and a dense
  bitmap built from them is cached and shared by every column in a batch.
  ([doris/decoder.h][doris-decoder],
  [doris/selection_vector.h][doris-selvec])
- **Choose the representation by shape.** In DataFusion, short selected runs
  between long skipped runs made the overhead per run dominate (ClickBench Q10
  25% slower with pushdown). Building the bitmap directly from filter results
  gave 26% end to end, but forcing a bitmap regressed sparse input by 108%.
  ClickHouse decodes densely across a scattered selection and filters once,
  up to 3x faster than seeking per run. Lance keeps full and contiguous
  selections as ranges and expands only the batch being consumed.
  ([datafusion#20325][df-20325], [arrow-rs#10852][arrow-rs-10852],
  [arrow-rs#8846][arrow-rs-8846], [arrow-rs#8565][arrow-rs-8565],
  [clickhouse#128139][ch-128139], [lance#11119][lance-11119])
- **parquet2 turns a list of intervals into Skipped, Repeated, and Bitmap
  runs**, but matches each page against every interval, which costs pages ×
  intervals; walking both lists together costs pages + intervals.
  ([parquet2/filtered_rle.rs][pq2-filt-rle],
  [parquet2/intervals.rs][pq2-intervals])

*Assessment.* The evidence says density alone does not choose well. Two pages
of the same density cost very differently when one has 10 selected runs and
the other has 10,000 (DataFusion, Doris). The design's choice per page can use
the number of runs as well as the density without any history. Both come from
the bitmask in one cheap pass (assuming unused high bits in the final word are
zero-padded): a popcount of each word gives the selected count, and with the
cross-word carry `carry = prev_word >> 63` (0 for the first word) and
`prev = (word << 1) | carry`, popcounting `word & !prev` counts exact 0→1
transitions (selected-run starts), while `!word & prev` counts 1→0 transitions
and `word ^ prev` counts total boundaries (adding the final `carry` if row
count is a multiple of 64 and the last bit is 1). Trino's constants are a
starting table by type. Its minimum of 300 skipped positions per run is the
same idea from the other side. The bitmask stays the one representation:
nothing here shows it costing more than ranges at the densities measured.
Doris's index lists are a third form, not evidence against the bitmask.
Adopted in `docs/design.md` (section 5: Selection-aware decoding).

### 2.4 Predicate order, and turning filters off (decided)

**Decided (2026-11-23):** Ordering predicate columns by measured selectivity /
cost and disabling unselective filters belong in the scanner or engine above
the core, keeping per-page decode strategy stateless. The core exposes
per-predicate `rows_in` and `rows_out` worker counters while the scanner times
its own calls. See `docs/design.md` (sections 3 and 5) and
`docs/design-notes.md` ("Reference scanner").
([doris/parquet_scan.cpp][doris-scan], [polars#29397][polars-29397],
[trino#24336][trino-24336], [datafusion#22883][df-22883],
[vortex#11215][vortex-11215], [clickhouse/Decoding.h][ch-decoding-h])

### 2.5 UTF-8 validation of plain string pages (decided)

**Decided (2026-11-23):** Adopted Polars's in-place method — a 4-byte length
prefix with `length & 0x8080_8080 == 0` is valid ASCII, so a span of plain
values is validated prefixes-and-all in one call (checking character boundaries
for free without mutating the page buffer); lengths with any byte ≥ `0x80`
(all ≥ 128 bytes, e.g. 128..=255) split the span. See
`docs/design.md` (section 5, Strings) and `docs/design-notes.md` ("Validating
plain pages in place"). ([polars/binview/mod.rs][polars-binview])

### 2.6 Encoded blocks with nulls

- **Doris decodes selected non-null values in "physical" coordinates, among
  the stored values, with one call per span of stored values, and then
  expands.** Its comment says working in row coordinates split one span into
  millions of calls. ([doris/column_chunk_reader.cpp][doris-chunk])

*Assessment.* Supports defining an encoded block's extent in stored values,
with validity attached in rows: one of the two definitions
`docs/design-notes.md` ("Encoded blocks") leaves open.

---

## Tier 3. The remaining flat encodings and codecs (step 4)

### 3.1 Encodings

- **ALP is in the pinned format.** `format/parquet.thrift` (2.14.0) defines
  `ALP = 10` for FLOAT and DOUBLE, so the reader must decode it. arrow-go fused
  its passes (48%), then added an AVX2 kernel (another 79%).
  ([arrow-go#1552][arrow-go-1552], [lance#10972][lance-10972])

  *Assessment.* Ranks higher than its position in the earlier list suggested,
  because it is in scope. design.md section 9 now says so.

- **BYTE_STREAM_SPLIT as a transpose.** Doris loads 32 rows from each stream
  and runs a tree of byte interleaves (three steps for 8-byte values, two for
  4-byte values), then fixes the lane crossing; it needs at least 32 values. A
  blocked scalar rewrite went from 2.8 to 4.1 GB/s, and Lance wrote SIMD
  transposes into uninitialized output (write 2.1 to 5.5 GiB/s). Measure each
  width: a form that helped f64 made f32 3.5x slower in arrow-rs, and batching
  by 8 helped nothing. Polars gathers one byte per stream per value, with no
  blocking. ([doris/parquet_kernels.cpp][doris-kernels],
  [arrow#38529][arrow-38529], [lance#10905][lance-10905],
  [arrow-rs#12278][arrow-rs-12278], [arrow-rs#9252][arrow-rs-9252],
  [parquet-java#3569][pqj-3569])

  *Assessment.* A candidate kernel in design. The interleave tree maps onto
  NEON's `zip1` and `zip2`, so it meets the parity rule. The pinned format
  allows BYTE_STREAM_SPLIT for INT32, INT64, and FIXED_LEN_BYTE_ARRAY as well
  as floats, so the kernel needs widths 4 and 8 and an arbitrary width.
  ([parquet-format#229][pqfmt-229])

- **DELTA_BINARY_PACKED.** Doris computes the prefix sum in registers, with a
  carry from one vector to the next. Trino unpacks into the output, then sums
  in the output's own width, so narrow types wrap as they should. Its
  `toByteExact` threw on a correct TINYINT wrap, a bug fixed in PR 32160. A
  miniblock of width zero is a constant delta: fill it, and skip it in O(1).
  Summing packed deltas skips them without materializing them (11x and 1.9x).
  Keep miniblock widths on the stack. Velox found dispatch through a switch
  6–25% slower than an inlined chain. Fusing unpacking, undelta, and gather
  means the deltas are never written. Polars sends deltas through a sink trait
  with callbacks for constant runs and for chunks of 64.
  ([doris/parquet_kernels.cpp][doris-kernels],
  [trino/DeltaBinaryPackedDecoders.java][trino-delta-bp],
  [trino#32160][trino-32160], [velox#17990][velox-17990],
  [arrow-rs#9783][arrow-rs-9783], [spark#55919][spark-55919],
  [vortex#11428][vortex-11428], [fastlanes#216][fastlanes-216],
  [polars/delta_bitpacked/decoder.rs][polars-delta-bp])

  *Assessment.* Prefix sums are a SIMD kernel in design. Worth taking: summing
  in the output width, and a test for the wrap case. **Conflicts:** the Polars
  sink is the generic consumer design.md decided against. What it shows is
  only that a constant miniblock deserves its own path, and in Parkay's shape
  that path is a run the column reader handles.

- **DELTA_LENGTH_BYTE_ARRAY and DELTA_BYTE_ARRAY.** When skipping
  DELTA_BYTE_ARRAY values, Trino walks the suffixes backwards and copies only
  the prefix bytes the next value needs. Polars decodes lengths in bulk and
  checks UTF-8 once per page (up to 9x). Doris finds where the payload starts
  with a full pass over the lengths, copies payload bytes one at a time, and
  copies the previous value into a `std::string` per batch.
  ([trino/DeltaByteArrayDecoders.java][trino-delta-ba],
  [polars#30632][polars-30632],
  [doris/delta_bit_pack_decoder.cpp][doris-delta])

  *Assessment.* Take Trino's skip and Polars's bulk lengths. Finding the
  payload does need a pass over the lengths stream, because its size is not
  stored. That pass can step over each miniblock by its width without
  unpacking it. Copying bytes one at a time is the part to avoid.

- **Width-specialized loops for FIXED_LEN_BYTE_ARRAY** turn a copy of runtime
  length into a move of constant size: 273 µs to 65 µs.
  ([arrow-rs#11910][arrow-rs-11910])

  *Assessment.* Fits monomorphizing per physical width (section 2.2), for the
  common widths such as 16 bytes for UUIDs and 128-bit decimals.

- **PLAIN BOOLEAN through a 256-entry table**, one 64-bit write per byte
  (3.6x), or a bulk bitmap copy. ([spark#54818][spark-54818],
  [arrow#39227][arrow-39227])

  *Assessment.* Parquet's PLAIN booleans are packed one bit per value, least
  significant bit first, as an Arrow bitmap is. So output as a bitmap is a
  copy with a shift, which is the floor. The table is for output with one byte
  per value.

- **Derive SIMD strides from the lane count**, not from 256-bit assumptions;
  wrong results appeared on NEON and SVE. ([velox#19130][velox-19130],
  [velox#18087][velox-18087])

  *Assessment.* `fearless_simd` is generic over width, which helps. The
  scalar-reference tests on NEON are what catch this.

### 3.2 Codecs

- **A faster Snappy decoder** (snipsnap) was reported at about 33% of scan
  time. carquet has its own Snappy and LZ4 decoders, with match copies
  specialized by offset: a broadcast for offset 1, a repeated pattern for
  offsets 2, 4, and 8, and 32-byte vector copies from offset 32 up.
  ([arrow-rs#12000][arrow-rs-12000], [carquet/avx2_ops.c][carquet-avx2])

  *Assessment.* design.md chose `snap` and plans to revisit it if profiles
  show that Snappy decoding matters. These are the alternatives to measure
  then.

---

## Tier 4. Strings and dictionary output (step 4; views deferred to step 9)

- **Keep dictionaries through to the engine.** In design (dictionary-preserved
  output). DataFusion ClickBench Q33 13.3 s to 2.5 s; Polars 1.2–3.4x.
  ([datafusion#26789][df-26789], [datafusion#27500][df-27500],
  [polars#30522][polars-30522], [duckdb#14194][duckdb-14194])
- **Cut short strings into inline 16-byte views from one 16-byte load.**
  Polars builds a view from the first 16 bytes of the page at the value's
  start, with a cold path near the end of the page. It points long values into
  the page buffer when non-inlined bytes fill at least three quarters of it,
  and otherwise copies them into one buffer of exact size.
  ([polars/binview/mod.rs][polars-binview], [polars#30632][polars-30632])

  *Assessment.* Views are deferred by decision until a benchmark asks for
  them (step 9). When they come, the 16-byte cut replaces a copy of varying
  length with one load and a mask per short string. The three-quarters rule is
  Polars's answer to the waste of retaining page buffers.

- **Enable `simdutf8`'s `std` feature.** Without it, generic x86 builds have no
  runtime dispatch and validate in scalar code.
  ([arrow-rs#11521][arrow-rs-11521])

  *Assessment.* A one-line check when the dependency is added.

---

## Tier 5. The reference scanner: I/O, prefetch, and scheduling (step 4)

All of this is scanner policy. The core's part, already in design, is to
report every range it can determine and to accept more bytes than it asked
for.

### 5.1 The scanner's memory model

- **Give each stage a share of a memory budget, and always admit the first
  unfinished row group.** ClickHouse's read manager has stages for Bloom
  filter headers and blocks, dictionaries, the column index, the offset index,
  and column data, each with a share. Tasks wait per row group and are queued
  only while their stage is under its share. The first unfinished row group is
  always admitted through the stages before column data, so the scan cannot
  stall. Read-ahead of column data starts before decoding, but only within
  that stage's share. Memory is charged in batches to avoid atomic contention.
  ([clickhouse/ReadManager.cpp][ch-readmgr], [clickhouse#82789][ch-82789])

  *Assessment.* The most complete design among these for what the scanner
  needs and design.md does not yet have: a memory model. design.md gives the
  scanner a thread pool and positioned reads. Per-stage shares fit the core's
  steps, which already separate pruning inputs from data.

- **Bounded read-ahead that overlaps I/O with decoding.** DataFusion with
  simulated object-store latency: 21.5 s to 12.4 s on TPC-H SF1. Unbounded
  versions were rejected for memory use. ([datafusion#23492][df-23492],
  [datafusion#18470][df-18470])
- **Pre-buffering costs memory**: it fetches whole column chunks, and buffered
  bytes must be released after decoding. A pre-buffer cache must not block
  scan threads waiting on a small I/O pool (StarRocks saw 512 of 512 workers
  blocked). Cancelling one task's shared I/O session cancelled its siblings'
  reads (Vortex). ([arrow#6744][arrow-6744], [arrow#36765][arrow-36765],
  [arrow#49855][arrow-49855], [starrocks#81771][sr-81771],
  [vortex#11226][vortex-11226])
- **Budget chunked reads including all per-page scratch.**
  ([cudf#25340][cudf-25340])

### 5.2 Reads and coalescing

- **Positioned reads instead of mmap**, with optional O_DIRECT: page faults
  block compute threads. Polars: RSS 8.1 GB to 2.4 GB; cold reads 10–15%
  faster with O_DIRECT. carquet maps the whole file with `MADV_SEQUENTIAL`,
  which works against pruned, random access. ([polars#29091][polars-29091],
  [carquet/mmap_reader.c][carquet-mmap])

  *Assessment.* Supports design.md's choice of positioned reads.

- **Choose the coalescing gap from measured latency × bandwidth**, not a fixed
  constant, both remote and local. DuckDB: S3 8 requests in 28 s versus 1
  request in 5 s; local cold cache 6.9 s to 4.2 s. For defaults: Trino merges
  across gaps of up to 1 MB into buffers of up to 8 MB, splits larger ranges
  into 8 MB pieces, and frees each buffer by reference count. ClickHouse uses a
  64 KiB gap (8 KiB for local files) and 4 MiB tasks.
  ([duckdb#23003][duckdb-23003], [duckdb#23290][duckdb-23290],
  [trino/AbstractParquetDataSource.java][trino-datasource],
  [clickhouse/Prefetcher.cpp][ch-prefetch])
- **Register every range before reading, and mark which ranges may be read
  as a side effect of another.** In ClickHouse's prefetcher, a flag on each
  range says whether it may be swept into a neighbour's read. Each task records
  its memory amplification, the ratio of bytes read to bytes needed, so that
  budget charges reflect what it really allocates.
  ([clickhouse/Prefetcher.cpp][ch-prefetch])

  *Assessment.* Fits the core's contract of reporting every range at once.
  The flag lets the scanner merge freely without pulling in a large range,
  such as a Bloom filter, that nothing needed.

- **Fetch filter columns first and projected columns only for surviving
  pages, but only when an offset index exists**; otherwise fetch the row group
  in one request. Coalesce across these rounds: predicate and projection
  reads issued separately raised DataFusion's request count from 583K to 894K.
  ([datafusion#26387][df-26387], [datafusion#26418][df-26418],
  [datafusion#24527][df-24527])
- **Pick a prefetch strategy per row group** (the whole group, by column, or
  filter columns first) from how selective the filters are.
  ([duckdb#22497][duckdb-22497])
- **Prefetch whole column chunks per leaf for nested projections.** Doris
  builds prefetch ranges from the physical leaf chunks, so a projected struct
  field fetches only its own leaf, using the same checked extent as the
  decoder, PARQUET-816 padding included. ([doris/parquet_scan.cpp][doris-scan])
- **Issue a blocked scan's reads concurrently**: DuckDB's S3 reads had started
  about 100 ms apart; 6.55 s to 2.65 s. ([duckdb#27690][duckdb-27690])
- **Grow the prefetch window until every thread has work.** Polars raised the
  minimum from 2 to 8 row groups. ([polars#30640][polars-30640])
- **When data is already in the page cache, read it in the decode task**
  rather than handing it off through the prefetch queue (futex calls 630K to
  34K). ([polars#29702][polars-29702])
- **Read without copying from the page cache.** When a task fits in one
  retained cache cell, ClickHouse keeps a reference to the cell and copies
  nothing. ([clickhouse/Prefetcher.cpp][ch-prefetch])

  *Assessment.* Fits the core: input bytes are borrowed for one call, from
  wherever the caller keeps them.

- **Count simulated GETs and bytes per query in the benchmark harness.**
  In design as an aim, not yet built. ([datafusion#23803][df-23803])

### 5.3 Batches and parallelism

- **Size batches by bytes, not only rows.** Trino starts a batch at 1 row,
  doubles it, caps it at 8,192 rows, and enforces a 16 MB budget from the
  largest bytes per cell seen per column. ClickHouse sizes row subgroups from
  the column metadata's bytes per row. Trino counts top-level rows, not leaf
  values, because leaf counts caused OOMs.
  ([trino/ParquetReader.java][trino-reader], [trino#31864][trino-31864])

  *Assessment.* Caller policy. The core's block size is fixed for cache
  reasons, and the size of the output the caller asks for is the caller's
  business.

- **Morsel-driven scans** instead of fixed row-group partitions: about 2x on
  some ClickBench queries. ([datafusion#20529][df-20529])
- **Parallelize columns inside large row groups** at about 1M values per task.
  ClickHouse v3 parallelizes by column as well as by row group, with one
  deterministic order of delivery. ([polars#30610][polars-30610])
- **Split one large file into row-group buckets handed out largest first.**
  ClickHouse kept only about 10 of 96 threads busy on one file.
  ([clickhouse#125738][ch-125738], [clickhouse#130740][ch-130740])
- **Prepare row groups outside the scan lock.**
  ([duckdb#26960][duckdb-26960])
- **Footer caches**: keyed by path and size, because ETags proved unreliable
  (ClickHouse); bounded, with peak RSS for 100 scans at 3.2 GB uncached,
  491 MB at 8 entries, and 271 MB at 1 (Polars); sized to the working set, or
  the LRU misses on every file (Vortex). A shared cached footer can have page
  indexes attached per query, so caching does not force a clone of the whole
  footer. ([clickhouse#89750][ch-89750], [polars#24513][polars-24513],
  [vortex#11613][vortex-11613], [trino#29684][trino-29684],
  [cudf#22662][cudf-22662], [datafusion#24288][df-24288],
  [arrow-rs#10653][arrow-rs-10653])

  *Assessment.* In design: retained metadata is plain data meant for a cache,
  and the page index is extracted separately.

---

## Tier 6. Selection-aware decoding and pruning (step 5)

### 6.1 Decoding under a selection

- **Walk the selection mask in 56-bit windows, popping set bits with
  `trailing_zeros`.** Polars trims leading and trailing zeros first, and sends
  an all-selected mask to the unfiltered kernel. It skips whole bit-packed
  chunks when a window is zero, and keeps dictionary indices in a ring buffer
  of 128 entries, filled 32 at a time and validated per chunk. A 56-bit window
  fits in one 64-bit load at any bit offset.
  ([polars/required_masked_dense.rs][polars-req-masked])

  *Assessment.* A good form for the sparse kernel. Partly in design: the
  bitmask is, the window walk and the skip of whole chunks are not.

- **Convert the filter to a bitmask once per decode call, and unpack only the
  selected values.** ClickHouse gathers bit-packed values with `ctz` loops and
  8-byte loads, counts RLE runs with popcount, and fills them. It also filters
  already decoded columns in place when at least half the rows pass.
  ([clickhouse#128287][ch-128287], [clickhouse#129188][ch-129188],
  [clickhouse/Decoding.cpp][ch-decoding])
- **Unpack only the 32-value groups that hold selected rows** (DuckDB: about
  30% faster when 1 row in 2 to 1 in 4 is selected).
  ([duckdb#28406][duckdb-28406])

  *Assessment.* Natural for Parquet's groups of 8: a group with no selected
  row is skipped.

- **Scan selection bitmaps a 64-bit word at a time** with popcount, skipping
  empty words: 2.4x. ([arrow-rs#11510][arrow-rs-11510],
  [arrow-rs#11639][arrow-rs-11639])
- **Decode predicates in two stages:** first those that need no decoding,
  then the remaining predicate columns under that mask, then the payload
  columns. ([polars#28531][polars-28531])
- **Do not fetch pages whose selection is empty.** ClickHouse queues only
  pages that intersect the row ranges surviving the column index, and pages
  whose span in the filter is all zero are never fetched.
  ([clickhouse/Reader.cpp][ch-reader])

  *Assessment.* Pages pruned by the index are dropped in design. Dropping a
  payload column's pages after the filter columns run is a second round of
  fetches, which is the trade-off DataFusion measured (section 5.2).

### 6.2 Predicates on encoded values

- **Evaluate a predicate inside the decode loop, straight into a bitmap.**
  Polars has kernels for equality, ranges, and membership in a set of up to
  eight values, which run over plain values in the page and push bits into a
  bitmap without copying values. For strings it compares the 16-byte view.
  Doris compares raw page bytes with AVX2 into a byte mask before any column
  is built, records which path ran per page, fuses type conversion into the
  comparison, and rebuilds its NaN ordering from masks.
  ([polars/primitive/plain/predicate.rs][polars-prim-pred],
  [doris/parquet_kernels.cpp][doris-kernels])

  *Assessment.* This is the fusion candidate "unpack plus compare to a bitmask",
  extended to plain pages, and it fits the closed predicate set exactly.
  Doris's NaN handling points at a decision design.md does not yet state:
  whether a predicate on floats compares as IEEE 754 or in total order. It
  must match the order pruning uses, or pruning and evaluation disagree.

- **Prefilter string predicates before copying** values into the output
  (2.3–2.6x). ([polars#25381][polars-25381])

### 6.3 Pruning

- **Prune with the dictionary only after statistics, and probe it as a
  sorted vector.** ClickHouse hashes a dictionary page into a plain vector of
  u64, sorts it only after max(8, log2 n) probes, and merges sorted probes
  against it: 18% fewer instructions and a 3–5x smaller footprint than a hash
  set. Its memory is charged to a pruning budget. Trino turns a dictionary
  page into a value domain and tests it against the predicate. In Arrow C++,
  reading every dictionary page before statistics pruned made pushdown slower
  than no pushdown. ([clickhouse#115744][ch-115744],
  [clickhouse/Reader.cpp][ch-reader],
  [trino/TupleDomainParquetPredicate.java][trino-tuple-pred],
  [arrow#38818][arrow-38818])

  *Assessment.* Partly in design: dictionary pages are a pruning input. The
  ordering is the important part. design.md's order of cost lists statistics,
  the page index, and Bloom filters, but not where dictionaries go; they
  should come after statistics, for the chunks that survive.

- **Bloom filters: read each in one request, only where statistics did not
  prune, and give up on large sets.** ClickHouse computes the block index with
  a multiply and a shift instead of a modulo, tests eight salted words with an
  early exit, and fetches only the blocks the queried values need. It skips
  `IN` sets of more than 100 values, because at a 1% false-positive rate 100
  values give about a 63% chance of a false positive. Reading a filter in one
  request needs `bloom_filter_length`. DuckDB coalesced Bloom reads across
  columns, from 453 GETs to 22. Evaluating column by column and stopping at
  the first rejection was up to 30x faster on slow stores. A branchless probe
  autovectorizes and matches a hand-written SIMD kernel on clang.
  ([clickhouse/Reader.cpp][ch-reader], [trino#32081][trino-32081],
  [duckdb#28095][duckdb-28095], [duckdb#28199][duckdb-28199],
  [datafusion#25811][df-25811], [hyparquet#190][hyparquet-190],
  [arrow#50030][arrow-50030])

  *Assessment.* In design: Bloom filters come after statistics, only for
  columns with a predicate. The cut-off on set size is cheap and worth taking,
  since the cost of probing grows with the set while the chance of pruning
  anything falls away.

- **Statistics-based skipping must be conservative.** Inverted min and max,
  NaN bounds, NULL in `IN` sets, and type reinterpretation each dropped rows
  somewhere. In design. ([clickhouse#128336][ch-128336],
  [clickhouse#121033][ch-121033], [clickhouse#128570][ch-128570],
  [iceberg-rust#3314][iceberg-3314], [parquet-java#3747][pqj-3747])
- **Decode the column index straight into its output layout**, without
  intermediate statistics structures: 3.5–5.1x.
  ([arrow-rs#11285][arrow-rs-11285])

  *Assessment.* Applies to the page-index decoder, the next metadata structure
  Parkay builds. It follows the M2 rule of extracting into arrays the caller
  owns.

- **A page-index pass that skips nothing must not install a selection**: it
  disabled later row-group pruning (DataFusion: 41.6 MB to 9.4 MB scanned).
  ([datafusion#25729][df-25729])
- **Prune `IN` against both bounds together.**
  ([iceberg-rust#3144][iceberg-3144])
- **Use the longer Parquet string statistics**, not an 8-byte truncation.
  ([duckdb#12928][duckdb-12928])
- **Visit row groups in min/max order for TopK and MIN/MAX** so dynamic filters
  tighten early. ClickHouse Q23 cold 12.6 s to 6.8 s; DataFusion MAX 5.0 s versus
  1.4 s for the equivalent ORDER BY LIMIT. ([clickhouse#115798][ch-115798],
  [datafusion#26809][df-26809], [polars#30006][polars-30006])

  *Assessment.* Scanner or engine policy. The core's part is to expose the
  statistics, which extraction does.

- **Push OFFSET into the scan using row counts**: 0.09 s versus 14.3 s.
  ([datafusion#19654][df-19654])

  *Assessment.* Scanner policy; the row counts are decoded at open.

---

## Tier 7. Nested data (step 7)

- **Bulk-decode runs of valid, non-empty list rows**; keep the per-value path
  for nulls and empties. Polars 1.3–3.6x. Its code writes a run's offsets into
  spare capacity and sets the length once, marks the push of outer rows
  `#[cold]` to keep the loop in registers, and checks wide fixed-size-list
  rows eight levels at a time with a fold that vectorizes.
  ([polars#30601][polars-30601], [polars/nested_utils.rs][polars-nested])
- **Produce offsets, the null map, and the count in one pass over the
  levels.** ClickHouse 14–19% less CPU, with branch-free stores, and counts of
  zeros taken while the levels decode. arrow-rs list building was 35–55% of
  instructions. ([clickhouse#117627][ch-117627],
  [arrow-rs#12017][arrow-rs-12017])
- **Read only levels for COUNT and array sizes.** ClickHouse picks the
  cheapest fixed-width leaf under an array that no other column reads, and
  decodes only its levels. ([clickhouse#86144][ch-86144],
  [clickhouse/SchemaConverter.cpp][ch-schema])

  *Assessment.* A natural extension of `docs/design-notes.md`'s ("Nested data")
  nominated leaf per ancestor: when no leaf under a list is projected, nominate
  the cheapest and read only its levels.

- **Skip the repetition-level buffer when the maximum repetition is 0**:
  Trino found 38 GB of all-zero buffers in one heap dump.
  ([trino#31381][trino-31381])
- **Carry a row across a page boundary explicitly.** Trino keeps flags for a
  last row that is unfinished or skipped.
  ([trino/NestedColumnReader.java][trino-nested])

  *Assessment.* In design: records can span pages when there is no offset
  index. These flags are the concrete state.

---

## Tier 8. Metadata still to build

M2 built the open path, and the ideas it took are gone from this list. These
remain for the structures still to come (the page header and the page index)
and for the deferred skip work.

- **Skip Thrift lists by counting bytes with the top bit clear instead of
  decoding each varint.** ([arrow-rs#11285][arrow-rs-11285],
  [arrow-rs#11911][arrow-rs-11911])

  *Assessment.* Deferred in design as the vectorized pass marking varint
  boundaries. It helps where comparing chunks with templates cannot: footers
  of one row group.

- **Planning costs that grow with the square of the column count** in wide
  schemas: Velox config lookup per column (30% on 10,000 columns), StarRocks
  statistics re-resolution (1.56 s to 0.08 s on 1,000 columns), and Spark work
  per record (15%). ([velox#18974][velox-18974],
  [starrocks#81889][sr-81889], [spark#50297][spark-50297])

  *Assessment.* A check for the per-leaf plan and projection resolution, which
  must stay linear in the number of leaves. The wide-schema benchmarks are
  where it would show.

---

## Tier 9. Writer (step 8)

- **Dictionary hash table**: keep fixed-width values inline in the entry (1.16x),
  store hashes so growth never rehashes, inline small strings, key short strings
  on their 16-byte view, and cache input-key → interned-key in a small
  direct-mapped table (up to 47%). carquet hashes keys of 9–16 bytes from two
  overlapping 8-byte loads mixed with the length.
  ([arrow-rs#12277][arrow-rs-12277], [arrow-rs#11524][arrow-rs-11524],
  [arrow-rs#10862][arrow-rs-10862], [vortex#11439][vortex-11439],
  [trino#30372][trino-30372], [carquet/dictionary.c][carquet-dict])
- **Keep dictionary and constant inputs as they are**, inserting each dictionary
  entry once: DuckDB 2.4–4.5x. ([duckdb#27990][duckdb-27990])
- **Statistics from referenced dictionary values only** (up to 2.8x), and copy
  min/max only when they change. ([arrow-rs#12282][arrow-rs-12282],
  [arrow-rs#10745][arrow-rs-10745])
- **Fill the Bloom filter from the dictionary** while a chunk stays
  dictionary-encoded (27%); fold Bloom filters to size them after the fact.
  ([arrow-rs#10966][arrow-rs-10966], [clickhouse#119230][ch-119230],
  [duckdb#28038][duckdb-28038])
- **Encode levels a group at a time** (up to 7.8x) and pack bit-packed groups as
  soon as they complete (2.1x). ([arrow-rs#12276][arrow-rs-12276],
  [arrow-rs#12281][arrow-rs-12281])
- **Delta encoding in the column's native width**: i32 min/max (3.8–4.1x).
  ([arrow-rs#12272][arrow-rs-12272])
- **Dispatch once per block, not per value** (up to 118%), and resolve per-column
  properties once. ([trino#30502][trino-30502], [arrow-rs#10880][arrow-rs-10880])
- **Write `ColumnMetaData.encoding_stats`**, so a reader knows a chunk is fully
  dictionary-encoded without reading page headers (section 2.1).
  ([duckdb#24957][duckdb-24957])
- **Place the page index before the footer** so the tail read brings it. In
  design. ([duckdb#27326][duckdb-27326])
- **Layout defaults affect reads**: one page per row group read up to 4x slower;
  row-group size trades local against S3 performance; a 20,000-row page limit
  made lookups up to 4x faster. ([arrow2#1303][arrow2-1303],
  [vortex#11720][vortex-11720], [arrow#15220][arrow-15220])
- **Skip compressing V2 pages that do not shrink**, so readers skip
  decompression. ([parquet-java#3344][pqj-3344],
  [arrow-rs#8257][arrow-rs-8257])
- **Writer-side type narrowing from per-file min/max**, so readers can fold
  predicates from metadata. ([datafusion#19337][df-19337])
- **Writer memory**: slab buffers capped at 1 MB, shared compression buffers, no
  state for absent Bloom filters or page indexes (about 40% less peak), lazily
  allocated encoder buffers. ([trino#32001][trino-32001],
  [arrow-rs#12073][arrow-rs-12073], [arrow-rs#11149][arrow-rs-11149])
- **Byte-accurate row-group size limits** for wide rows.
  ([duckdb#25060][duckdb-25060])
- **Estimate encoding sizes honestly**: bad estimates picked encodings that
  doubled sizes. ([duckdb#25785][duckdb-25785], [vortex#11721][vortex-11721])
- **Dictionary fallback by measured profit was tried and closed**: gains on
  ClickBench, losses on high-cardinality columns. Bears on early abandonment.
  ([arrow-rs#10775][arrow-rs-10775], [arrow-rs#10780][arrow-rs-10780],
  [arrow-rs#12291][arrow-rs-12291])
- **The spec's Bloom filter sizing table disagrees with the implementations.**
  Check the writer's sizing against them, not against the table.
  ([parquet-format#547][pqfmt-547])
- **Comparisons with carquet's writer are not like for like**: it writes
  dictionary and BYTE_STREAM_SPLIT encodings by default and Arrow does not,
  which its README states. ([carquet/README.md][carquet-readme])

---

## Low value, negative results, and patterns to avoid

### Negative or marginal results

- **Bulk-filling null runs gave nothing** (Spark). ([spark#56072][spark-56072])
- **Copying short values in fixed chunks was a mixed result**: 30–50% faster
  on macOS, neutral on Linux, and 2–2.7x slower on dictionaries of empty
  strings. ([arrow-rs#12046][arrow-rs-12046])
- **Aligning SIMD lookup tables to their load width** was a micro-effect
  (about 1.00x end to end). ([vortex#11488][vortex-11488])

### To check when a dependency is added

- **carquet's CPU dispatch checks XCR0 and requires BMI1, BMI2, and FMA
  alongside AVX2**, because its compiler may emit those anywhere in a file
  built with them. Rust's feature detection checks the operating system's
  support through XGETBV, and `target_feature` enables only the features named,
  so the second problem does not arise. To check: that `fearless_simd`'s AVX2
  level enables the features its kernels assume.
  ([carquet/detect.c][carquet-detect])

### Patterns to avoid, seen in the source

- **Padding must cover the widest unaligned load**: 11 bytes, not 8, for some
  delta widths in Velox. Relevant only to scratch Parkay owns, since borrowed
  input is never padded. ([velox#18779][velox-18779])
- **ClickHouse v3 has no SIMD intrinsics in its Parquet code.** Not a pattern
  to avoid, but a caution about where its speed comes from: scheduling,
  selections, and fusion, not hand-written kernels. Its byte-stream-split is a
  scalar loop. ([clickhouse/Decoding.cpp][ch-decoding])

---

## References

[carquet-avx2]: https://github.com/Vitruves/carquet/blob/d61c669606b480a7a1326971b852434e643e87c2/src/simd/x86/avx2_ops.c
[carquet-bench]: https://github.com/Vitruves/carquet/blob/d61c669606b480a7a1326971b852434e643e87c2/benchmark/benchmark_carquet.c
[carquet-detect]: https://github.com/Vitruves/carquet/blob/d61c669606b480a7a1326971b852434e643e87c2/src/simd/detect.c
[carquet-dict]: https://github.com/Vitruves/carquet/blob/d61c669606b480a7a1326971b852434e643e87c2/src/encoding/dictionary.c
[carquet-mmap]: https://github.com/Vitruves/carquet/blob/d61c669606b480a7a1326971b852434e643e87c2/src/reader/mmap_reader.c
[carquet-page]: https://github.com/Vitruves/carquet/blob/d61c669606b480a7a1326971b852434e643e87c2/src/reader/page_reader.c
[carquet-readme]: https://github.com/Vitruves/carquet/blob/d61c669606b480a7a1326971b852434e643e87c2/README.md
[ch-decoding]: https://github.com/ClickHouse/ClickHouse/blob/91b044d1980df254df04a749f9fc9bda078c1a18/src/Processors/Formats/Impl/Parquet/Decoding.cpp
[ch-decoding-h]: https://github.com/ClickHouse/ClickHouse/blob/91b044d1980df254df04a749f9fc9bda078c1a18/src/Processors/Formats/Impl/Parquet/Decoding.h
[ch-prefetch]: https://github.com/ClickHouse/ClickHouse/blob/91b044d1980df254df04a749f9fc9bda078c1a18/src/Processors/Formats/Impl/Parquet/Prefetcher.cpp
[ch-reader]: https://github.com/ClickHouse/ClickHouse/blob/91b044d1980df254df04a749f9fc9bda078c1a18/src/Processors/Formats/Impl/Parquet/Reader.cpp
[ch-readmgr]: https://github.com/ClickHouse/ClickHouse/blob/91b044d1980df254df04a749f9fc9bda078c1a18/src/Processors/Formats/Impl/Parquet/ReadManager.cpp
[ch-schema]: https://github.com/ClickHouse/ClickHouse/blob/91b044d1980df254df04a749f9fc9bda078c1a18/src/Processors/Formats/Impl/Parquet/SchemaConverter.cpp
[doris-bench-readme]: https://github.com/apache/doris/blob/f6b5f3da1649d46cf7e24a2d020bbd6e64f04354/be/benchmark/parquet/README.md
[doris-chunk]: https://github.com/apache/doris/blob/f6b5f3da1649d46cf7e24a2d020bbd6e64f04354/be/src/format_v2/parquet/reader/native/column_chunk_reader.cpp
[doris-decoder]: https://github.com/apache/doris/blob/f6b5f3da1649d46cf7e24a2d020bbd6e64f04354/be/src/format_v2/parquet/reader/native/decoder.h
[doris-delta]: https://github.com/apache/doris/blob/f6b5f3da1649d46cf7e24a2d020bbd6e64f04354/be/src/format_v2/parquet/reader/native/delta_bit_pack_decoder.cpp
[doris-kernels]: https://github.com/apache/doris/blob/f6b5f3da1649d46cf7e24a2d020bbd6e64f04354/be/src/util/simd/parquet_kernels.cpp
[doris-scan]: https://github.com/apache/doris/blob/f6b5f3da1649d46cf7e24a2d020bbd6e64f04354/be/src/format_v2/parquet/parquet_scan.cpp
[doris-selvec]: https://github.com/apache/doris/blob/f6b5f3da1649d46cf7e24a2d020bbd6e64f04354/be/src/format_v2/parquet/selection_vector.h
[polars-binview]: https://github.com/pola-rs/polars/blob/7ff0ffa2840489623c98a3d119d018543cd4b1a3/crates/polars-parquet/src/arrow/read/deserialize/binview/mod.rs
[polars-bp-decode]: https://github.com/pola-rs/polars/blob/7ff0ffa2840489623c98a3d119d018543cd4b1a3/crates/polars-parquet/src/parquet/encoding/bitpacked/decode.rs
[polars-comp]: https://github.com/pola-rs/polars/blob/7ff0ffa2840489623c98a3d119d018543cd4b1a3/crates/polars-parquet/src/parquet/compression.rs
[polars-delta-bp]: https://github.com/pola-rs/polars/blob/7ff0ffa2840489623c98a3d119d018543cd4b1a3/crates/polars-parquet/src/parquet/encoding/delta_bitpacked/decoder.rs
[polars-dict-mod]: https://github.com/pola-rs/polars/blob/7ff0ffa2840489623c98a3d119d018543cd4b1a3/crates/polars-parquet/src/arrow/read/deserialize/dictionary_encoded/mod.rs
[polars-nested]: https://github.com/pola-rs/polars/blob/7ff0ffa2840489623c98a3d119d018543cd4b1a3/crates/polars-parquet/src/arrow/read/deserialize/nested_utils.rs
[polars-page-reader]: https://github.com/pola-rs/polars/blob/7ff0ffa2840489623c98a3d119d018543cd4b1a3/crates/polars-parquet/src/parquet/read/page/reader.rs
[polars-prim-plain]: https://github.com/pola-rs/polars/blob/7ff0ffa2840489623c98a3d119d018543cd4b1a3/crates/polars-parquet/src/arrow/read/deserialize/primitive/plain/mod.rs
[polars-prim-pred]: https://github.com/pola-rs/polars/blob/7ff0ffa2840489623c98a3d119d018543cd4b1a3/crates/polars-parquet/src/arrow/read/deserialize/primitive/plain/predicate.rs
[polars-read-comp]: https://github.com/pola-rs/polars/blob/7ff0ffa2840489623c98a3d119d018543cd4b1a3/crates/polars-parquet/src/parquet/read/compression.rs
[polars-req-masked]: https://github.com/pola-rs/polars/blob/7ff0ffa2840489623c98a3d119d018543cd4b1a3/crates/polars-parquet/src/arrow/read/deserialize/dictionary_encoded/required_masked_dense.rs
[polars-snappy]: https://github.com/pola-rs/polars/blob/7ff0ffa2840489623c98a3d119d018543cd4b1a3/crates/polars-parquet/src/parquet/snappy.rs
[pq2-comp]: https://github.com/jorgecarleitao/parquet2/blob/b0e654526ce06bcb02b3e0e1f5aa3e1a10580350/src/read/compression.rs
[pq2-filt-rle]: https://github.com/jorgecarleitao/parquet2/blob/b0e654526ce06bcb02b3e0e1f5aa3e1a10580350/src/deserialize/filtered_rle.rs
[pq2-intervals]: https://github.com/jorgecarleitao/parquet2/blob/b0e654526ce06bcb02b3e0e1f5aa3e1a10580350/src/indexes/intervals.rs
[pq2-unpack]: https://github.com/jorgecarleitao/parquet2/blob/b0e654526ce06bcb02b3e0e1f5aa3e1a10580350/src/encoding/bitpacked/unpack.rs
[trino-abs-col]: https://github.com/trinodb/trino/blob/010fed1fa032582b78acf96e3517bf20c28d7a90/lib/trino-parquet/src/main/java/io/trino/parquet/reader/AbstractColumnReader.java
[trino-datasource]: https://github.com/trinodb/trino/blob/010fed1fa032582b78acf96e3517bf20c28d7a90/lib/trino-parquet/src/main/java/io/trino/parquet/AbstractParquetDataSource.java
[trino-delta-ba]: https://github.com/trinodb/trino/blob/010fed1fa032582b78acf96e3517bf20c28d7a90/lib/trino-parquet/src/main/java/io/trino/parquet/reader/decoders/DeltaByteArrayDecoders.java
[trino-delta-bp]: https://github.com/trinodb/trino/blob/010fed1fa032582b78acf96e3517bf20c28d7a90/lib/trino-parquet/src/main/java/io/trino/parquet/reader/decoders/DeltaBinaryPackedDecoders.java
[trino-flat]: https://github.com/trinodb/trino/blob/010fed1fa032582b78acf96e3517bf20c28d7a90/lib/trino-parquet/src/main/java/io/trino/parquet/reader/flat/FlatColumnReader.java
[trino-nested]: https://github.com/trinodb/trino/blob/010fed1fa032582b78acf96e3517bf20c28d7a90/lib/trino-parquet/src/main/java/io/trino/parquet/reader/NestedColumnReader.java
[trino-nulls]: https://github.com/trinodb/trino/blob/010fed1fa032582b78acf96e3517bf20c28d7a90/lib/trino-parquet/src/main/java/io/trino/parquet/reader/flat/NullsDecoders.java
[trino-page]: https://github.com/trinodb/trino/blob/010fed1fa032582b78acf96e3517bf20c28d7a90/lib/trino-parquet/src/main/java/io/trino/parquet/reader/PageReader.java
[trino-plain-ba]: https://github.com/trinodb/trino/blob/010fed1fa032582b78acf96e3517bf20c28d7a90/lib/trino-parquet/src/main/java/io/trino/parquet/reader/decoders/PlainByteArrayDecoders.java
[trino-reader]: https://github.com/trinodb/trino/blob/010fed1fa032582b78acf96e3517bf20c28d7a90/lib/trino-parquet/src/main/java/io/trino/parquet/reader/ParquetReader.java
[trino-rle]: https://github.com/trinodb/trino/blob/010fed1fa032582b78acf96e3517bf20c28d7a90/lib/trino-parquet/src/main/java/io/trino/parquet/reader/decoders/RleBitPackingHybridDecoder.java
[trino-tuple-pred]: https://github.com/trinodb/trino/blob/010fed1fa032582b78acf96e3517bf20c28d7a90/lib/trino-parquet/src/main/java/io/trino/parquet/predicate/TupleDomainParquetPredicate.java

[arrow-6744]: https://github.com/apache/arrow/pull/6744
[arrow-15220]: https://github.com/apache/arrow/issues/15220
[arrow-36765]: https://github.com/apache/arrow/issues/36765
[arrow-38389]: https://github.com/apache/arrow/issues/38389
[arrow-38529]: https://github.com/apache/arrow/pull/38529
[arrow-38818]: https://github.com/apache/arrow/issues/38818
[arrow-39227]: https://github.com/apache/arrow/issues/39227
[arrow-49855]: https://github.com/apache/arrow/pull/49855
[arrow-50030]: https://github.com/apache/arrow/pull/50030
[arrow-50629]: https://github.com/apache/arrow/pull/50629
[arrow-go-1447]: https://github.com/apache/arrow-go/pull/1447
[arrow-go-1473]: https://github.com/apache/arrow-go/pull/1473
[arrow-go-1552]: https://github.com/apache/arrow-go/pull/1552
[arrow-rs-8257]: https://github.com/apache/arrow-rs/pull/8257
[arrow-rs-8565]: https://github.com/apache/arrow-rs/issues/8565
[arrow-rs-8846]: https://github.com/apache/arrow-rs/issues/8846
[arrow-rs-9252]: https://github.com/apache/arrow-rs/issues/9252
[arrow-rs-9579]: https://github.com/apache/arrow-rs/issues/9579
[arrow-rs-9783]: https://github.com/apache/arrow-rs/issues/9783
[arrow-rs-10653]: https://github.com/apache/arrow-rs/pull/10653
[arrow-rs-10745]: https://github.com/apache/arrow-rs/pull/10745
[arrow-rs-10775]: https://github.com/apache/arrow-rs/pull/10775
[arrow-rs-10780]: https://github.com/apache/arrow-rs/pull/10780
[arrow-rs-10852]: https://github.com/apache/arrow-rs/pull/10852
[arrow-rs-10862]: https://github.com/apache/arrow-rs/pull/10862
[arrow-rs-10880]: https://github.com/apache/arrow-rs/pull/10880
[arrow-rs-10966]: https://github.com/apache/arrow-rs/pull/10966
[arrow-rs-11027]: https://github.com/apache/arrow-rs/pull/11027
[arrow-rs-11149]: https://github.com/apache/arrow-rs/pull/11149
[arrow-rs-11285]: https://github.com/apache/arrow-rs/pull/11285
[arrow-rs-11303]: https://github.com/apache/arrow-rs/issues/11303
[arrow-rs-11376]: https://github.com/apache/arrow-rs/pull/11376
[arrow-rs-11381]: https://github.com/apache/arrow-rs/pull/11381
[arrow-rs-11414]: https://github.com/apache/arrow-rs/issues/11414
[arrow-rs-11462]: https://github.com/apache/arrow-rs/pull/11462
[arrow-rs-11510]: https://github.com/apache/arrow-rs/pull/11510
[arrow-rs-11521]: https://github.com/apache/arrow-rs/pull/11521
[arrow-rs-11524]: https://github.com/apache/arrow-rs/pull/11524
[arrow-rs-11639]: https://github.com/apache/arrow-rs/pull/11639
[arrow-rs-11910]: https://github.com/apache/arrow-rs/pull/11910
[arrow-rs-11911]: https://github.com/apache/arrow-rs/pull/11911
[arrow-rs-12000]: https://github.com/apache/arrow-rs/pull/12000
[arrow-rs-12017]: https://github.com/apache/arrow-rs/issues/12017
[arrow-rs-12046]: https://github.com/apache/arrow-rs/pull/12046
[arrow-rs-12073]: https://github.com/apache/arrow-rs/pull/12073
[arrow-rs-12195]: https://github.com/apache/arrow-rs/issues/12195
[arrow-rs-12250]: https://github.com/apache/arrow-rs/issues/12250
[arrow-rs-12272]: https://github.com/apache/arrow-rs/pull/12272
[arrow-rs-12275]: https://github.com/apache/arrow-rs/pull/12275
[arrow-rs-12276]: https://github.com/apache/arrow-rs/pull/12276
[arrow-rs-12277]: https://github.com/apache/arrow-rs/pull/12277
[arrow-rs-12278]: https://github.com/apache/arrow-rs/pull/12278
[arrow-rs-12281]: https://github.com/apache/arrow-rs/pull/12281
[arrow-rs-12282]: https://github.com/apache/arrow-rs/pull/12282
[arrow-rs-12291]: https://github.com/apache/arrow-rs/pull/12291
[arrow-rs-12294]: https://github.com/apache/arrow-rs/pull/12294
[arrow2-1303]: https://github.com/jorgecarleitao/arrow2/pull/1303
[ch-60361]: https://github.com/ClickHouse/ClickHouse/pull/60361
[ch-82789]: https://github.com/ClickHouse/ClickHouse/pull/82789
[ch-86144]: https://github.com/ClickHouse/ClickHouse/pull/86144
[ch-89750]: https://github.com/ClickHouse/ClickHouse/pull/89750
[ch-115744]: https://github.com/ClickHouse/ClickHouse/pull/115744
[ch-115798]: https://github.com/ClickHouse/ClickHouse/pull/115798
[ch-117627]: https://github.com/ClickHouse/ClickHouse/pull/117627
[ch-119230]: https://github.com/ClickHouse/ClickHouse/pull/119230
[ch-121033]: https://github.com/ClickHouse/ClickHouse/pull/121033
[ch-125738]: https://github.com/ClickHouse/ClickHouse/pull/125738
[ch-128139]: https://github.com/ClickHouse/ClickHouse/pull/128139
[ch-128262]: https://github.com/ClickHouse/ClickHouse/pull/128262
[ch-128287]: https://github.com/ClickHouse/ClickHouse/pull/128287
[ch-128336]: https://github.com/ClickHouse/ClickHouse/pull/128336
[ch-128570]: https://github.com/ClickHouse/ClickHouse/pull/128570
[ch-129188]: https://github.com/ClickHouse/ClickHouse/pull/129188
[ch-130740]: https://github.com/ClickHouse/ClickHouse/pull/130740
[ch-133229]: https://github.com/ClickHouse/ClickHouse/pull/133229
[cudf-22662]: https://github.com/NVIDIA/cudf/issues/22662
[cudf-24221]: https://github.com/NVIDIA/cudf/pull/24221
[cudf-24263]: https://github.com/NVIDIA/cudf/pull/24263
[cudf-25340]: https://github.com/NVIDIA/cudf/pull/25340
[df-18470]: https://github.com/apache/datafusion/issues/18470
[df-19337]: https://github.com/apache/datafusion/issues/19337
[df-19654]: https://github.com/apache/datafusion/issues/19654
[df-20325]: https://github.com/apache/datafusion/issues/20325
[df-20529]: https://github.com/apache/datafusion/issues/20529
[df-22883]: https://github.com/apache/datafusion/issues/22883
[df-23492]: https://github.com/apache/datafusion/pull/23492
[df-23803]: https://github.com/apache/datafusion/pull/23803
[df-24288]: https://github.com/apache/datafusion/issues/24288
[df-24527]: https://github.com/apache/datafusion/issues/24527
[df-25729]: https://github.com/apache/datafusion/pull/25729
[df-25811]: https://github.com/apache/datafusion/pull/25811
[df-26387]: https://github.com/apache/datafusion/pull/26387
[df-26418]: https://github.com/apache/datafusion/pull/26418
[df-26789]: https://github.com/apache/datafusion/pull/26789
[df-26809]: https://github.com/apache/datafusion/issues/26809
[df-27500]: https://github.com/apache/datafusion/pull/27500
[doris-65921]: https://github.com/apache/doris/pull/65921
[duckdb-12928]: https://github.com/duckdb/duckdb/pull/12928
[duckdb-14194]: https://github.com/duckdb/duckdb/pull/14194
[duckdb-16315]: https://github.com/duckdb/duckdb/pull/16315
[duckdb-22497]: https://github.com/duckdb/duckdb/pull/22497
[duckdb-23003]: https://github.com/duckdb/duckdb/pull/23003
[duckdb-23290]: https://github.com/duckdb/duckdb/pull/23290
[duckdb-24957]: https://github.com/duckdb/duckdb/pull/24957
[duckdb-25060]: https://github.com/duckdb/duckdb/pull/25060
[duckdb-25785]: https://github.com/duckdb/duckdb/pull/25785
[duckdb-26960]: https://github.com/duckdb/duckdb/pull/26960
[duckdb-27326]: https://github.com/duckdb/duckdb/pull/27326
[duckdb-27690]: https://github.com/duckdb/duckdb/pull/27690
[duckdb-27990]: https://github.com/duckdb/duckdb/pull/27990
[duckdb-28038]: https://github.com/duckdb/duckdb/pull/28038
[duckdb-28095]: https://github.com/duckdb/duckdb/pull/28095
[duckdb-28199]: https://github.com/duckdb/duckdb/pull/28199
[duckdb-28406]: https://github.com/duckdb/duckdb/pull/28406
[fastlanes-216]: https://github.com/spiraldb/fastlanes/pull/216
[hyparquet-190]: https://github.com/hyparam/hyparquet/pull/190
[iceberg-3144]: https://github.com/apache/iceberg-rust/pull/3144
[iceberg-3314]: https://github.com/apache/iceberg-rust/pull/3314
[lance-10905]: https://github.com/lance-format/lance/pull/10905
[lance-10972]: https://github.com/lance-format/lance/pull/10972
[lance-11119]: https://github.com/lance-format/lance/pull/11119
[polars-24513]: https://github.com/pola-rs/polars/pull/24513
[polars-25381]: https://github.com/pola-rs/polars/pull/25381
[polars-28531]: https://github.com/pola-rs/polars/pull/28531
[polars-29091]: https://github.com/pola-rs/polars/pull/29091
[polars-29397]: https://github.com/pola-rs/polars/pull/29397
[polars-29702]: https://github.com/pola-rs/polars/pull/29702
[polars-29980]: https://github.com/pola-rs/polars/pull/29980
[polars-30006]: https://github.com/pola-rs/polars/pull/30006
[polars-30248]: https://github.com/pola-rs/polars/pull/30248
[polars-30522]: https://github.com/pola-rs/polars/pull/30522
[polars-30601]: https://github.com/pola-rs/polars/pull/30601
[polars-30610]: https://github.com/pola-rs/polars/pull/30610
[polars-30632]: https://github.com/pola-rs/polars/pull/30632
[polars-30640]: https://github.com/pola-rs/polars/pull/30640
[pq2-172]: https://github.com/jorgecarleitao/parquet2/pull/172
[pq2-203]: https://github.com/jorgecarleitao/parquet2/pull/203
[pqfmt-229]: https://github.com/apache/parquet-format/pull/229
[pqfmt-547]: https://github.com/apache/parquet-format/issues/547
[pqj-3344]: https://github.com/apache/parquet-java/issues/3344
[pqj-3569]: https://github.com/apache/parquet-java/pull/3569
[pqj-3747]: https://github.com/apache/parquet-java/issues/3747
[pqj-3749]: https://github.com/apache/parquet-java/pull/3749
[pqj-3773]: https://github.com/apache/parquet-java/pull/3773
[spark-50297]: https://github.com/apache/spark/pull/50297
[spark-54818]: https://github.com/apache/spark/pull/54818
[spark-55919]: https://github.com/apache/spark/pull/55919
[spark-56072]: https://github.com/apache/spark/pull/56072
[sr-81535]: https://github.com/StarRocks/starrocks/pull/81535
[sr-81771]: https://github.com/StarRocks/starrocks/pull/81771
[sr-81889]: https://github.com/StarRocks/starrocks/pull/81889
[sr-83259]: https://github.com/StarRocks/starrocks/pull/83259
[trino-24336]: https://github.com/trinodb/trino/pull/24336
[trino-29684]: https://github.com/trinodb/trino/pull/29684
[trino-30372]: https://github.com/trinodb/trino/pull/30372
[trino-30502]: https://github.com/trinodb/trino/pull/30502
[trino-31381]: https://github.com/trinodb/trino/pull/31381
[trino-31864]: https://github.com/trinodb/trino/pull/31864
[trino-32001]: https://github.com/trinodb/trino/pull/32001
[trino-32081]: https://github.com/trinodb/trino/pull/32081
[trino-32160]: https://github.com/trinodb/trino/pull/32160
[trino-32222]: https://github.com/trinodb/trino/pull/32222
[velox-17990]: https://github.com/facebookincubator/velox/pull/17990
[velox-17993]: https://github.com/facebookincubator/velox/pull/17993
[velox-18087]: https://github.com/facebookincubator/velox/pull/18087
[velox-18259]: https://github.com/facebookincubator/velox/pull/18259
[velox-18779]: https://github.com/facebookincubator/velox/pull/18779
[velox-18974]: https://github.com/facebookincubator/velox/pull/18974
[velox-19130]: https://github.com/facebookincubator/velox/pull/19130
[vortex-11215]: https://github.com/vortex-data/vortex/pull/11215
[vortex-11226]: https://github.com/vortex-data/vortex/pull/11226
[vortex-11428]: https://github.com/vortex-data/vortex/pull/11428
[vortex-11434]: https://github.com/vortex-data/vortex/pull/11434
[vortex-11439]: https://github.com/vortex-data/vortex/pull/11439
[vortex-11488]: https://github.com/vortex-data/vortex/pull/11488
[vortex-11613]: https://github.com/vortex-data/vortex/pull/11613
[vortex-11720]: https://github.com/vortex-data/vortex/pull/11720
[vortex-11721]: https://github.com/vortex-data/vortex/pull/11721
