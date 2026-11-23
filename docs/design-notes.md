# Parkay design notes

`docs/design.md` holds the decisions that shape every phase. This file holds the
detail behind them, by area, in the order of design.md's sections.

The two kinds of section here are not equal:

- **Metadata** is the architectural rationale and trade-offs behind M2 (for current
  module layout, invariants, and fuzz targets, see `ARCHITECTURE.md` and
  `src/metadata/`).
- **Everything else** is working notes for phases not yet started. It records
  what was thought through so it is not lost, but it is not a commitment. When a
  phase starts, its notes are read again, checked against what the earlier
  phases taught, and then either taken into the design, changed, or dropped.

Performance techniques from other readers, ranked, are in
`docs/github-performance-ideas.md`. Measurements are in `docs/perf/`.

## Contents

- [Kernels (design.md section 4)](#kernels-designmd-section-4)
- [Metadata (design.md section 5, built in M2)](#metadata-designmd-section-5-built-in-m2)
- [Pruning: when statistics are trusted (design.md section 5)](#pruning-when-statistics-are-trusted-designmd-section-5)
- [Output forms (design.md section 5)](#output-forms-designmd-section-5)
- [Block-synchronous multi-column scan (design.md section 5)](#block-synchronous-multi-column-scan-designmd-section-5)
- [Nested data (design.md section 5)](#nested-data-designmd-section-5)
- [Strings (design.md section 5)](#strings-designmd-section-5)
- [Compression and page framing (design.md section 5)](#compression-and-page-framing-designmd-section-5)
- [Writer (design.md section 6)](#writer-designmd-section-6)
- [Reference scanner (design.md section 7)](#reference-scanner-designmd-section-7)
- [DataFusion (design.md section 8)](#datafusion-designmd-section-8)
- [Testing (design.md section 10)](#testing-designmd-section-10)
- [Benchmarking practice so far (design.md section 11)](#benchmarking-practice-so-far-designmd-section-11)

## Kernels (design.md section 4)

### Why the column reader holds the branching

When a block needs nothing between decoding and output (no nulls, no selection,
no conversion), the column reader has the decoder write straight into the
output buffer. Otherwise the decoder writes into the worker context's block-sized
scratch, and the kernels that follow write the output. Either way a value is
written to its final place once.

The hybrid decoder's runs and packed groups are the one encoded form the column
reader works on directly. A run is filled, or tested against a predicate, once for its
whole length. A packed group goes to an unpacking kernel or to a fused one. The
levels, dictionary indices, and run-length booleans all use this one decoder.

This shape was chosen over the alternative, in which each decoder pushes its
values into a generic consumer. That alternative relies on the compiler
specializing every decoder for every consumer and inlining across the two,
multiplies the generated code by the number of consumers, and spreads the
branching through the decoders. With one function holding the branching, each
kernel is compiled once, its speed does not depend on what consumes its output,
and it can be tested alone against its scalar reference. The cost is one pass through block-sized scratch
when short runs and packed groups alternate, which stays in the innermost cache.

### Kernels expected to need explicit SIMD

Kernels expected to need explicit SIMD include bit-unpacking and bit-packing
(Parquet's sequential packing layout vectorizes poorly on its own), compress and
expand operations for nulls and selections, delta prefix sums, conversion of
levels to bitmasks for nested columns (`max_def_level > 1`; flat nullable
columns copy bit-packed definition levels straight into the validity bitmap),
and possibly dictionary gathers and byte-stream-split.
Element-wise comparisons, simple copies, and integer statistics are expected to
autovectorize acceptably.

Not yet placed in that list: ALP, which the pinned format defines for `FLOAT`
and `DOUBLE` (section 9), so the reader must decode it.

### Fusion candidates

Candidates to evaluate first: unpack plus compare-to-bitmask, unpack plus
dictionary gather for fixed-width values, and encode plus statistics in the
writer.

## Metadata (design.md section 5, built in M2)

For current module layout, fuzz targets, and invariants, see `ARCHITECTURE.md`,
`src/metadata/`, and `src/thrift/`. This section records the rationale, contracts,
and trade-offs that shaped M2.

### Overview and lazy trade-offs

One pass over the footer records where each row group and column chunk starts —
the **footer directory** (not called an index, to avoid colliding with Parquet's
page/column/offset indexes) — and column-chunk details are decoded on demand.

- **Reuse across files and scans.** Callers that scan the same file repeatedly
  keep and reuse the `Metadata` value directly (plain, immutable, cacheable).
  Across files of one dataset, `open()` accepts `Option<&Schema>` (not previously
  obtained `Metadata`) and still reads each file's footer, skipping schema
  decoding when the footer's leading bytes match.
- **Lazy saves CPU and memory, not round trips.** On object storage, dependent
  request count dominates; on local storage and cached metadata, parse time and
  retained footprint remain.
- **Two structural bounds.** Thrift compact protocol has no length prefixes on
  structs, so opening must touch every footer byte (and fields after row groups
  are only reached at the end). Chunk templates move multi-row-group opens close
  to scan speed, while 1-row-group footers remain bounded by `skip`
  (`docs/perf/thrift-skip.md`). Conversely, reading every column of every row
  group touches the footer twice (open pass + extraction pass) instead of an
  eager decoder's single pass.

The M2 acceptance targets compare against arrow-rs configured for the minimum
work each case needs (not its default full decode):

| Case | arrow-rs configuration | Target |
|---|---|---|
| Open for a scan of a few columns, with no pruning | Column, encoding, and size statistics all skipped | Faster |
| Open and read statistics for one or two columns | Statistics kept for those columns only | Faster |
| Open and read every column's statistics | Statistics kept; encoding and size statistics skipped | No slower (fuse extraction into open if missed) |
| Open with the schema of an earlier file | Schema supplied | Faster, and cost does not grow with schema size |
| Retained metadata | Its decoded metadata held in memory | No larger; narrow extraction scales with requested columns and row groups, not table width |

All targets were met on the original 6-fixture M2 set (`docs/perf/open.md`);
reading every column from the worst-case `alternating` fixture initially missed
by 2% and passed after inlining hot extraction helpers. The benchmark harness
has since moved to the 8-entry corpus in `bench-support/src/corpus.rs`
(`small`, `medium`, `many_row_groups`, `wide`, `very_wide`, `nested`, `legacy`,
`many_files`), while uniform, varied, and alternating shapes remain covered by
unit tests in `src/metadata/tests.rs`. The external footer set (`PARKAY_BENCH_DIR`)
has not yet been measured on M2's open path on this machine. Extraction remains the dominant cost
of `open_all` (`docs/design.md` section 14).

### Opening a file

`open()` (`src/metadata/open.rs`) is one stateless step over `(tail, file_size,
limits, Option<&Schema>)`. Supplying `file_size` lets the core place the tail
and validate absolute file offsets.

- **Round trips and partial input.** On object storage the chain is two
  dependent requests at best (tail holds footer + page index, then data) and
  four at worst (tail, rest of footer, page index, data), plus one more for a
  Bloom filter lacking `bloom_filter_length`. The two follow-up range-planning
  steps (pruning ranges, then surviving data ranges) arrive with pruning and
  page framing. A short tail during `open()` returns `MissingBytes`; truncation
  inside a requested post-open range is an operating error.
- **Checked at open:**
  - Trailing magic (`PAR1`; encrypted footer magic `PARE` returns
    `OpenError::EncryptedFooter`) and footer length against `file_size`.
  - File-level required fields, schema structure/repetition, and each row
    group's column count against schema leaf count (from list headers alone).
  - `FileMetaData` fields 8 and 9 (`encryption_algorithm`,
    `footer_signing_key_metadata`), reported as `Unsupported::Encryption`.
- **Not checked at open (late-error contract):** Column chunks are skipped
  structurally during `open()`, not value-validated, so malformed chunks surface
  on `Metadata::extract()`. `ColumnChunk`-level checks happen at `extract()`
  (`src/metadata/wire/column_chunk.rs`): field 1 (`file_path` →
  `Unsupported::ExternalFile`), field 3 (`meta_data`, required), and fields 8
  and 9 (`crypto_metadata`, `encrypted_column_metadata` →
  `Unsupported::Encryption`). `column_chunk_decode` stops as soon as all
  requested fields are in hand (for example, once `meta_data` satisfies every
  wanted field on a chunk, or immediately after `meta_data` on an empty
  `Request`), so `ColumnChunk` fields after `meta_data` — including fields 8/9
  and trailing malformed fields — are not read or checked in that case.
  Spec-compliant encrypted files are still rejected at `open()` via `PARE` magic
  or `FileMetaData` fields 8 and 9.
- **Field order and legacy offsets.** Ascending field-id order is not assumed.
  Chunk offsets are validated when extracted; because older writers leave
  `dictionary_page_offset` zero/unset or emit unreliable `ColumnChunk.file_offset`,
  byte ranges are derived the way other readers derive them (see also
  `docs/github-performance-ideas.md`, section 1.1).
- **Directory shape.** One `u32` per column chunk (bounded to ≤ 4× footer size
  since every chunk is ≥ 1 byte). Per-chunk indexing keeps narrow extraction in
  microseconds, so a coarser per-row-group directory has not been needed.
- **Three decided malformed-footer rules:**
  1. *Duplicate field id in a decoded struct is an error.* Real writers do not
     emit duplicates, readers disagree on which copy wins (Thrift-generated
     readers keep last; arrow-rs keeps last for most fields but first for
     schema), and tracking seen bits checks repeats for free while eliminating
     reset paths. Skipped structs are unchecked under the late-error contract.
  2. *Trailing bytes after the footer stop marker (within declared footer
     length) are ignored*, matching other readers with no ambiguity.
  3. *Every non-root schema element must state its repetition.* The spec
     requires it, real files provide it, and assuming a default causes level
     mismatches (found by differential fuzzing against arrow-rs).

### Passing over column chunks without parsing them

Across row groups, a column's chunks usually share the same Thrift structure
with different values. `src/metadata/template/` exploits this while preserving
exact equivalence with full parsing (`compare.rs` states the soundness proof):

- **Masked comparison.** `skip_marking` records every bit parsing reads (headers,
  sizes, varint continuation bits — never binary payloads). A later chunk that
  matches its column's template under that mask follows identical parse steps
  and ends at the same offset. Up to four varying integer/binary lengths are
  learned and stepped over without falling back.
- **Bounded worst-case cost and memory.** A credit budget grants one initial
  template per column and only funds replacements from prior match savings, so
  non-matching footers degrade by ~0%. Templates are transient working memory
  bounded to roughly the footer's size and verified against full parsing in
  debug/test/fuzz builds (`ARCHITECTURE.md`, `docs/perf/open.md`).

### Retained metadata: ownership and shape

Parkay **copies the footer** out of `tail` into exact-size owned storage rather
than taking ownership of a caller buffer. Taking ownership was measured and
rejected (`docs/perf/footer-ownership.md`, with M2 open-path numbers in
`docs/perf/open.md`):

1. Fresh memory faults on first touch regardless of who allocates it; taking
   ownership saves only one `memcpy`, not page faults.
2. Callers that reuse a single fetch buffer across files cannot surrender it.
3. `open()` stays stateless on `MissingBytes` retries without handing buffers back.
4. Speculative tail reads are often far larger than the footer and would stay
   pinned in a metadata cache.
5. Avoiding `Vec`/`Box` ownership keeps `Metadata` non-generic and compatible
   with shared buffers or memory maps.
6. Copying lets Parkay drop or transform retained bytes later if needed.

Other shape choices: no self-references (offsets/lengths into the retained
footer, and schema names stored in one pre-validated `String` with ranges); no
interior mutability or decoded cache inside `Metadata`; decoders copy out the
few plan values they need instead of borrowing `Metadata`; and `Schema` prefix
reuse works by comparing leading footer bytes (identical bytes guarantee
identical decode and end offset).

### Reading column-chunk facts

File-level fields (`schema`, row-group row counts/sizes) are decoded at open;
`created_by`, `column_orders`, and `key_value_metadata` are located at open and
decoded lazily on accessor call. All per-chunk data flows through one API,
`Metadata::extract()`, into caller-owned struct-of-arrays (`row_group * column_count + column`) `ChunkFacts` buffers
(reusable across files with zero steady-state allocation, skipping unrequested
fields, storing byte bounds as footer offsets rather than copies). There is no
parallel single-chunk accessor — callers request one column and one row group.

M2 implements location/encoding and statistics fact groups; page-index and Bloom
filter groups arrive with pruning. The internal per-chunk decoder is shaped so
fusing extraction into `open()` remains possible if full-table extraction needs
it (`docs/design.md` section 14).

### The Thrift layer

Parkay uses its own zero-allocation compact-protocol reader (`src/thrift/`,
details and invariants in `ARCHITECTURE.md`) because the `thrift` crate decodes
eagerly into owned structs and arrow-rs's custom parser is crate-private.
`skip` validates structure (type codes, remaining-length bounds, depth) without
value-validating skipped fields, matching the late-error contract. M2 added
offset-positioned `CompactReader::new` (preserving footer-relative error
offsets), promoted the generic Thrift encoder to shared `test_support` for
out-of-order/malformed footer tests, and profiled/inlined the hot read helpers
during extraction tuning (`docs/perf/open.md`).

### Decoding metadata structures

Informed by arrow-rs's custom Thrift parser
([announcement](https://arrow.apache.org/blog/2025/10/23/rust-parquet-metadata/)),
where owned-struct allocation dominated parse time:

- Decode straight into Parkay's types with no intermediate Thrift AST; use
  compact representations (`Encodings` bitmask, byte-offset bounds).
- Track seen/required fields with a small bitmask (`Seen`) and keep
  field-header reading inlined on hot decoders.
- Match each field on `(id, wire_type)`. A known id with an unexpected wire
  type is skipped as unknown (matching Thrift-generated parquet-java and
  parquet-cpp; arrow-rs reads it as the expected type, so type-mismatched
  inputs are excluded from differential fuzzing).
- M2 hand-writes the decoders it needs (`FileMetaData`, `SchemaElement` and
  logical types, `RowGroup`, `ColumnChunk`, `ColumnMetaData`, `Statistics`,
  `ColumnOrder`, `KeyValue`, and later page headers, grouped across five files
  in `src/metadata/wire/`). The full bidirectional
  metadata surface will be generated from one table when the writer starts
  (`docs/design.md` section 12).

## Pruning: when statistics are trusted (design.md section 5)

Pruning on statistics that are wrong or misread drops rows that match, and the
result is silently wrong, not an error. Statistics are used only where they can
be trusted:

- The deprecated `min` and `max` fields were written with signed comparison
  whatever the type, so they are usable only for types where that is the correct
  order.
- `column_orders` decides what `min_value` and `max_value` mean. The pinned
  format defines three orders: the type-defined order, IEEE 754 total order for
  floating-point columns, and chronological order for `INT96`. Bounds are not
  used for a column whose order is missing or is one the reader does not
  support.
- Floating-point bounds follow the format's rules for each order. Under the
  type-defined order a NaN bound is ignored, a `+0` minimum may hide `-0` values
  and a `-0` maximum may hide `+0` values, and NaN values may be present unless
  `nan_count` says otherwise. Under IEEE 754 total order `nan_count` is always
  written, and a NaN bound in the page index means every non-null value on the
  page is NaN.
- `INT96` bounds are used only under the chronological order. Under the
  type-defined order the format says to ignore them.
- Truncated bounds are not exact (`is_min_value_exact`, `is_max_value_exact`),
  which matters for equality at a bound.
- Some writers, identified by `created_by`, wrote statistics that other readers
  have learned to ignore. Parkay follows the same list.
- Types whose order the format leaves undefined, such as intervals, lists, and
  maps, are not pruned on.

When in doubt the reader does not prune.

Working note, not decided: statistics are a claim the writer made, and nothing
ties them to the pages. Pruning has to trust them, because checking them would
mean reading what pruning exists to avoid. Decoding need not: where a statistic
would only save work that is cheap to do, such as skipping the definition levels
of a page because `null_count` is zero, the page's own bytes can prove the same
thing (`docs/github-performance-ideas.md`, section 1.4).

## Output forms (design.md section 5)

### Encoded blocks

The caller receives the data as the file stores it: a stream of blocks that are
each a run (a value or code and a length), a bit-packed group with its bit
width, or a slice of plain values, together with the dictionary where one
applies and the validity for the block. Nothing is expanded.

- This is a block-oriented interface, not a callback per run, because
  Parquet's hybrid encoding alternates short packed groups with runs.
- A block borrows from the reader and is valid only until the reader
  advances. The caller consumes or copies it before asking for the next one.
- Callers must expect the encoding to change between pages and dictionaries
  to differ between column chunks.
- Encodings with no useful compressed-domain form, such as the delta
  encodings and byte-stream-split, are delivered as blocks of decoded
  values.

Not yet defined: a run of values counts only non-null values, while validity
counts rows, so a block's extent has to be defined in both. How repetition and
definition levels of nested columns appear at this level is not defined either.
Doris's reader works in stored-value coordinates with validity attached in rows
(`docs/github-performance-ideas.md`, section 2.6), which is one of the two
choices.

### Dictionary fallback

A column chunk often starts with dictionary-encoded pages and continues with
plain pages once the writer gives up on the dictionary. The choices considered
were to report an error, to switch to materialized values partway through the
chunk (so the caller handles both forms), or to add the plain values to the
dictionary as they are read. design.md takes the second, decided at the start
of the chunk rather than partway, so a caller sees one form per chunk.

How the column reader knows before the first data page:

- `ColumnMetaData.encoding_stats`, when the writer recorded it, gives the
  encodings of the data pages.
- Otherwise the page headers, which page framing reads anyway. Without an offset
  index that means reading every header in the chunk before decoding, which
  costs nothing extra when the chunk was fetched whole.
- The `encodings` list alone is not enough: writers that encode the dictionary
  page itself as `PLAIN` list `PLAIN` even when every data page uses the
  dictionary.

Predicate evaluation against the dictionary still needs a path for the plain
pages of a mixed chunk: the predicate is evaluated on plain values there. Merging
dictionaries and remapping codes across a fallback, so a caller keeps codes for
the whole chunk, can be added later for a caller that asks for it.

### Later forms, added on evidence

- **Views into page bytes** for strings and binary values, with no copy. These
  conflict with scratch reuse: decompressing the next page into the same
  scratch buffer would overwrite the bytes earlier views point to. They require
  either that the caller finish with a page's views before the next page is
  decoded, or that each page's buffer be retained and reference-counted while
  any view exists.
- **Zero-copy value views** where the stored layout already matches what the
  caller needs. This applies cleanly to fixed-length byte arrays and to
  uncompressed, null-free pages whose values are suitably aligned in the buffer.
  It has the same buffer-lifetime requirement as views and is a narrow
  optimization.

## Block-synchronous multi-column scan (design.md section 5)

Columns have very different page sizes in rows, so every column decoder is
resumable at any row within a page, which block-at-a-time decoding already
requires. A lockstep scan over several columns keeps one decompressed page per
column alive at once. It is intended for small groups of columns, such as the
inputs to a filter expression, where those pages stay cache-resident together;
it is not the mechanism for decoding a wide projection.

## Nested data (design.md section 5)

- Level decoding is vectorized, and long runs of identical levels are handled in
  bulk.
- Sibling leaves under the same list or struct carry the same levels for that
  ancestor. The plan nominates one projected leaf per ancestor to produce its
  offsets and validity, and the other leaves skip that reconstruction.
- A row selection counts top-level records, not leaf values. Applying one to a
  repeated column means finding record boundaries from the repetition levels,
  and using each page's first row index from the offset index to skip whole
  pages.
- Pages are required to begin on a row boundary only when an offset index is
  present. Without one, an older writer may split one record's values across two
  pages, so per-page skipping and selection on a repeated column must carry a
  record across the page boundary.

## Strings (design.md section 5)

### Validating plain pages in place

The plan before 2026-11-23 had two forms: validate non-dictionary strings in bulk
over the contiguous output bytes after the copy, with a check that each value
starts on a character boundary; and, once views exist, validate a plain page in
place by overwriting each length prefix with ASCII spaces, which needs a mutable
page buffer. Polars's method replaces both (`docs/github-performance-ideas.md`,
section 2.5):

- A length prefix whose four bytes are all below `0x80` is itself valid UTF-8,
  so a span of plain values is validated prefixes and all, in one call, with no
  rewriting and no mutable buffer.
- The character-boundary check comes free. A value that ends partway through a
  character is followed by a prefix byte below `0x80`, which is an invalid
  continuation. A value that starts with a continuation byte follows an ASCII
  byte, which is also invalid.
- A prefix with any byte of `0x80` or more ends the span: any length whose
  low byte is 128 or more, such as 128 to 255. Such a value is at least 128
  bytes long, so validating it alone costs little next to its bytes.

Encodings that store value bytes contiguously are validated the same way, over
those bytes.

### Facts recorded while decoding a dictionary

The dictionary is already scanned once per column chunk to establish UTF-8
validity, so the longest value, whether every value is ASCII, and whether every
value has the same length could be recorded in that pass at little cost. A
consumer could then choose a cheaper layout per chunk: inline views when every
value fits in 12 bytes, a fixed-width byte column when every value is one byte,
and no character-boundary checks when every value is ASCII. The facts come from
the data, not from schema hints such as `char(1)` that Parquet does not carry,
so they apply to any file. What is recorded, and how a consumer receives it, is
to be decided when the dictionary decoder exists and a consumer asks for it.
The idea comes from DataFusion issue 18411, where engines expand short,
low-cardinality string columns that the reader could have described.

## Compression and page framing (design.md section 5)

- The codec interface accepts a batch of pages, so the common case is a batch of
  one, and parallel or offloaded decompression remains possible later without an
  API change.
- Page framing is one module. It is given the ordinals of the row group and the
  column, counts the pages it frames, and has one place that turns stored bytes
  into a readable page header and one that turns them into a readable page.
  Decryption needs exactly those ordinals and those two places, so it can be
  added later without changing the code around them.

## Writer (design.md section 6)

### Encoding path

- Null values are removed while encoding, without first building a list of
  non-null positions and gathering through it.
- For flat nullable columns, definition levels are produced directly from the
  input validity bitmap as runs.
- Minimum, maximum, and null count are computed while each block is already in
  cache for encoding.
- Bit-packing, run-length encoding, delta encoding, and byte-stream-split use
  the same kernel strategy as the reader.

### Encoding and compression selection

- The encoding for a column chunk is chosen by sampling values across the
  buffered row group, not from its first values only.
- Sampling across a row group means holding its values unencoded until the
  choice is made, which for wide tables can be a large amount of memory.
  Buffering is therefore governed by a byte budget, not only a row count. When
  the budget is reached before the row group is complete, encodings are chosen
  from what has been buffered so far and encoding begins.
- Dictionary encoding is abandoned early when the sampled cardinality makes it
  unprofitable. Other columns use a hash table that stores dictionary indices,
  created with a moderate initial capacity: growing the table was the largest
  single cost measured, and sizing it to the full value count hurt
  low-cardinality columns. A direct-address table for small integer domains is
  a measured but modest gain and is added only if profiles call for it.
- Compression may be skipped for data that will not benefit from it.

### Layout for fast reads

- Bloom filters are placed so they can be fetched on demand, not pulled in by
  the tail read.
- Pages use a moderate fixed target size with a row-count limit. The target is
  not tuned to the writing machine.
- Where the input is sorted, the writer records the sort order, and it writes
  statistics in a form that keeps pruning effective (for example truncated
  bounds for long values).
- Footer size affects how many requests it takes to open a file. Options that
  keep it small, such as omitting statistics for columns no reader filters on,
  are worth offering. Row group size trades a smaller footer against coarser
  pruning and less parallelism, so it is left to the caller.

## Reference scanner (design.md section 7)

- Scan planning covers projection, the closed predicate set, pruning, coalescing
  of nearby byte ranges into fewer reads, and splitting very large ranges into
  parallel reads. The thresholds differ between local and remote storage and
  are scanner policy, not core behavior.
- Parallelism is at the granularity of row group and column. Where a column
  chunk has an offset index, it can also be split into page ranges; many real
  files have none, and the planner then stays at row group and column
  granularity. Workers that share a dictionary-encoded chunk share one decoded
  dictionary, which is decoded before the page ranges are dispatched.
- Concerns that sit above a single file, such as pruning files from table-format
  manifests before opening them, opening many files concurrently, and
  overlapping one file's data reads with the next file's metadata, belong to the
  scanner or the engine. The core's part is to hold no global state, so many
  files can be in flight at once.
- Predicate order. Readers that order predicate columns by measured cost and
  selectivity (Doris, Polars, Trino) decode later predicate columns far less
  often, and a filter that keeps nearly every row can be turned off. The worker
  counters give the scanner rows in and rows out per predicate; time is measured
  by the scanner around its calls.

## DataFusion (design.md section 8)

The integration is a separate optional layer that:

- Presents Parkay as a file source the engine can scan.
- Translates the subset of the engine's filter expressions that fit the closed
  predicate set into Parkay predicates, and leaves the remainder for the engine
  to evaluate.
- Maps the engine's partitioning and scheduling onto the core's units of work,
  so the engine's runtime, not the reference scanner's thread pool, drives
  execution.

The engine's current interfaces do not constrain the core's lower-level APIs.
Encoded blocks and the block-synchronous scan are not expected to fit a
general-purpose engine without custom operators, and they are designed for what
the core can offer, not for what the first engine can consume.

## Testing (design.md section 10)

- A small corpus of real files from many writers (for example fastparquet,
  pyarrow, parquet-java, DuckDB, Spark, and Go implementations) guards the
  reader's tolerance of non-conforming but common output, such as fastparquet's
  empty lists with element type 0.
- Fuzz targets run under a nightly toolchain; the crate itself stays on the
  pinned toolchain. They are seeded from real footers and run with a short
  per-input timeout, so input that makes the work grow out of proportion is
  caught.
- Interpreter-based checking covers the scalar paths and framing code. It does
  not cover most SIMD intrinsics, which is why the scalar-reference tests carry
  that weight.

## Benchmarking practice so far (design.md section 11)

- Benchmarks run one at a time, never alongside builds or other benchmarks.
- The built-in corpus (`bench-support/src/corpus.rs`) covers eight shapes
  (`small`, `medium`, `many_row_groups`, `wide`, `very_wide`, `nested`, `legacy`,
  `many_files`), complemented by optional external sets such as
  [alamb/parquet_footer_parsing](https://github.com/alamb/parquet_footer_parsing)
  for comparison with published arrow-rs results. A change is judged across the
  full corpus so no single shape dominates.
- Profiling uses sampling (`samply`, macOS `sample`), the generated assembly, and
  hardware counters (Xcode's `xctrace` CPU Counters template on macOS), so a
  change is explained, not only timed.
- Every forced inline or cold annotation is backed by a measurement, and what was
  tried, kept, and rejected is logged in `docs/perf/`.
- Not yet covered: x86-64 measurements, tracking results over time, and request
  counting for object storage.
- Limits of the results so far: every number comes from one Apple Silicon
  machine, and several of the changes kept in the skip path were a few percent
  in quick mode, within run-to-run variation; they should be re-measured on
  x86-64 before being treated as settled. In M2, chunk templates bypassed `skip`
  on multi-row-group files (`docs/perf/open.md`), leaving `skip` as the bound
  only on 1-row-group and non-template footers.
- Baselines are run with settings that match. Parkay verifies page checksums by
  default and arrow-rs does not, so a comparison sets both the same way.
