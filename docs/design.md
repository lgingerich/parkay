# Parkay design

Status: living document, last reviewed 2026-11-20. It records the design direction
agreed so far and what has been built (section 13). It describes intent, not
measured results: every performance expectation in it is a hypothesis until a
benchmark confirms it, and measurements are logged in `docs/perf/`, not here.

## 1. Purpose

Parkay is a ground-up Rust implementation of a Parquet reader and writer whose
primary goal is maximum performance. It is not a fork or wrapper of the arrow-rs
`parquet` crate, which serves as the baseline it is measured against.

The working definition of "optimal" is a hardware one. For any workload, some
physical limit applies: memory bandwidth, the throughput of the compression
codec, or the storage or network link. The implementation is optimal when that
limit, and not Parquet framing, decoding, allocation, or scheduling, is what
bounds throughput.

Three ideas follow from that definition and shape most of the design:

- **Do less work before doing work faster.** The largest gains come from never
  fetching, decompressing, or decoding data the caller does not need, and from
  handing data back in encoded form when the caller can use it that way.
- **Measure before believing.** No design choice in this document is justified
  by a speedup figure. Each one is to be validated against the baseline and the
  relevant hardware limit.
- **One canonical way first.** The first version offers a single way to do each
  thing: one decode path with the output levels layered on it, one selection
  representation, one buffering model. Alternatives described in this document are added only when
  a benchmark or a real integration shows they are needed. Internals are still
  shaped so that those alternatives can be added without a redesign.

## 2. Goals and non-goals

### Goals

- Read and write Parquet files that interoperate with the wider ecosystem.
- Make every stage of the read and write paths approach its hardware limit.
- Let callers skip I/O, decompression, and decoding wherever file metadata and
  predicates allow it.
- Expose encoded representations (runs, dictionaries, packed values) so engines
  can operate without full materialization.
- Treat nested data as a first-class performance target, not an afterthought.
- Produce files that are fast to read, not only fast to write.
- Remain safe on untrusted and corrupt input.

### Non-goals

- A new file format, or non-standard encodings or compression.
- A query engine: no planner, joins, aggregation, or general expression
  evaluation.
- Owning the host's threads, async runtime, or memory policy inside the core.
- Backwards compatibility for internal APIs while the design is still moving.

### Out of scope for the first version

- The Variant type and shredded Variant columns.
- Geospatial (geometry and geography) types.

Files containing these types should be rejected or have those columns reported
as unsupported with a clear error. They are not silently misread.

## 3. Architecture

Parkay is a core library plus a reference scanner. The core is engine-agnostic.
The scanner and the adapters sit on top of it.

```text
+-----------------------------------------------------------+
|  Integrations: Arrow adapter, DataFusion integration      |
+-----------------------------------------------------------+
|  Reference scanner: thread pool, file I/O, scan planning  |
+-----------------------------------------------------------+
|  Core (sans-I/O)                                          |
|    metadata | page framing | reader and writer state      |
|    machines | pruning | selection | encoded-form API      |
+-----------------------------------------------------------+
|  Kernels: unpack, pack, gather, compress, expand, delta,  |
|  byte-stream-split, levels, statistics, codec wrappers    |
+-----------------------------------------------------------+
```

### Packaging

The library is a single crate with modules for each layer. It is split into
several crates only if there is a concrete reason, such as optional heavy
dependencies. Keeping the kernels in the same crate as their callers avoids
inlining barriers across crate boundaries.

The repository is a workspace around that crate. The other members are
development tooling that never ships: a task runner (`xtask`) and benchmark
support (`bench-support`). Fuzz targets live in a separate crate with its own
workspace, so the library stays on the pinned stable toolchain. Internal modules
are exposed to the benchmarks and fuzz targets through an `internals` feature,
which is not a stable API.

Integrations that pull in external dependencies (Arrow, DataFusion) are optional
so the core stays dependency-light.

### Chosen dependencies

Third-party crates on the hot path were chosen by benchmarking the candidates
on real Parquet page data, on one Apple Silicon machine. The comparison code was
throwaway and is not in this repository. The rule applied: a C-backed library is
acceptable where its advantage is clear, and pure Rust is preferred where
performance is about the same. Each crate is added to the build when the code
that needs it is written; none of those in the table below is a dependency yet.

| Need | Choice | Basis |
|---|---|---|
| SIMD | `fearless_simd` | Chosen directly; covers dispatch, portable vectors, and safe intrinsics |
| Zstd | `zrip` | Faster than the C library on decode, roughly level on encode, and no `unsafe` in its own code. It is a young crate, so the C library stays in the differential tests as a reference |
| Snappy | `snap` | The C++ library was moderately faster, but needs a vendored build and puts untrusted input through C++. To be revisited if profiles show Snappy decode matters |
| LZ4 | `lz4_flex`, safe mode | Close to the C library, with no `unsafe` |
| UTF-8 validation | `simdutf8` | Level with the C++ library and several times faster than the standard library in bulk |
| Dictionary hashing | `foldhash` with `hashbrown` | Fastest or level on real columns, and robust on adversarial integer patterns where simpler hashes collapsed |
| CRC-32 | `crc32fast` | Fastest at typical page sizes; runs at about memory-copy speed |

Chosen without a comparison because they are off the hot path or rarely used:
`xxhash-rust` for Bloom filters (the format fixes the algorithm), `flate2` with
`zlib-rs` for gzip, and `brotli`.

Codecs sit behind one internal interface, so any of these can be replaced
without touching callers.

Other dependencies: `thiserror` for the library's error types. Development only:
`proptest`, `criterion`, the arrow-rs `parquet` crate (baseline, differential
tests, and fixture generation), and `libfuzzer-sys` in the fuzz crate.

### The core is sans-I/O

The core never performs I/O and never spawns threads. It is a set of state
machines over byte slices:

- The caller supplies bytes and asks what to do next.
- The core answers with either decoded output, a request for specific byte
  ranges, or a unit of work that can be executed on any thread.
- All policy about where bytes come from and which thread runs what belongs to
  the layer above.

This keeps the core usable from a synchronous program, an async runtime, or an
engine with its own scheduler, and it makes the core straightforward to fuzz.

### Access patterns: round trips over bytes

The cost of reading differs sharply between storage types. On object storage, each
request carries a large fixed latency, while extra bytes within a request are cheap,
and many independent requests can run in parallel for roughly the cost of one. What
matters is the length of the chain of requests that each depend on the previous one.
On local storage, requests are cheap, and the CPU cost of parsing metadata and
decoding is a larger share of the total.

The core does not choose between these; it is shaped so the layer above can make
either fast:

- When the core needs bytes, it reports every range it can already determine, not
  one range at a time, so independent ranges can be fetched together.
- It accepts more bytes than it asked for and uses them. A caller may read a large
  speculative piece of the file tail, and anything already present in it (the rest
  of the footer, the page index) needs no further request.
- Where the format allows it, the ranges for a scan are known before any of them is
  decoded, so fetching one page never waits on decoding another.
- Policy such as how much to read speculatively, which nearby ranges to merge, and
  which large ranges to split into parallel requests belongs to the layer above.

### Caller-owned memory

Decode and encode entry points write into buffers the caller provides. The core
keeps reusable scratch state (decompression buffers, codec contexts,
intermediate blocks) in an explicit per-worker object that the caller creates
once and passes in, not in thread-local storage. In steady state the read path
should perform no heap allocation.

Scratch buffers avoid needless zero-filling before being overwritten.

Scratch memory is reused from one page to the next, so nothing the caller keeps
may point into it. Output the caller keeps is always copied or decoded into
caller-owned buffers. The one thing that may reference scratch memory is an
encoded block, which is a short-lived borrow from the reader: the borrow must
end before the reader can advance, and the compiler enforces that. Output that
references decompressed page bytes and outlives the page would require keeping
each page's buffer alive for as long as the output exists, which is a different
buffering model; it is deferred (see "Output forms").

## 4. Kernels

### Block-at-a-time pipelines

Decoding and encoding work on small fixed-size blocks of values, sized so that a
block and its intermediates stay in the innermost cache. A page is processed as
a sequence of blocks flowing through composable stages, for example: unpack
dictionary codes, evaluate a predicate on the codes, gather surviving values,
place them in the output.

At the start of each page (and at each change of run or miniblock inside a
page), the reader resolves the stages it needs into a pipeline of specialized
kernels. Kernels are monomorphized ahead of time over the properties that
matter, such as bit width and value type, and selected through a dispatch table.
Nothing is generated at run time.

Pages are decompressed and decoded one at a time, so decompressed bytes are
consumed while still cache-resident, and the scratch buffer is reused for the
next page.

### SIMD strategy

Parkay uses both autovectorization and explicit SIMD, under one rule:

1. Every kernel has a scalar reference implementation. It is the portable
   fallback and the oracle for differential tests.
2. The scalar code is compiled once per supported instruction-set level, behind
   target-feature wrappers with runtime dispatch where the baseline target does
   not already include the feature set. This scaffolding is needed regardless of
   how the kernel body is written.
3. Explicit intrinsics are written only for kernels where measurement shows the
   compiler's output falls meaningfully short of the hardware limit.

Kernels expected to need explicit SIMD include bit-unpacking and bit-packing
(Parquet's sequential packing layout vectorizes poorly on its own), compress and
expand operations for nulls and selections, delta prefix sums, conversion of
levels to bitmasks, and possibly dictionary gathers and byte-stream-split.
Element-wise comparisons, simple copies, and integer statistics are expected to
autovectorize acceptably.

SIMD code is written with the `fearless_simd` crate. It provides the three
layers this strategy needs: multiversioned autovectorization of scalar code with
runtime dispatch, portable vector types that are generic over the instruction
set (including compress, expand, per-lane shifts, byte shuffles, and bitmask
conversion), and safe access to raw intrinsics for operations the portable API
does not cover, such as gathers. Kernels use the highest layer that reaches the
hardware limit and drop to a lower one only on evidence.

UTF-8 validation, checksums, compression codecs, and hashing use existing
crates. Each is chosen by a benchmark comparison of the candidates on
Parquet-shaped inputs, not by assumption.

Supported instruction sets are NEON on AArch64 and AVX2 on x86-64 as first-class
targets. AVX-512 is an additional tier. No design element may depend on an
instruction that exists on only one of the first-class targets; where a
wider instruction set offers a direct operation (for example compress and
expand), a table-driven equivalent exists for the others.

### Fusion policy

A fused kernel performs several stages in one loop without storing an
intermediate block. The policy is:

- The composed block pipeline defines the semantics. Fusion is never visible in
  the public API.
- A fused kernel is a substitution the pipeline planner may make for a specific
  sequence of stages. It must produce output identical to the composed stages,
  which gives a differential test for free.
- Stages are written as inlinable generic functions over a per-vector interface
  so that a fused loop is an instantiation choice, not a hand-written kernel.
- Each fused instantiation costs code size, compile time, and test surface on
  every instruction set, and needs entry and exit paths for partial blocks at
  run and page boundaries. Instantiations are therefore added individually, on
  evidence.

Candidates to evaluate first: unpack plus compare-to-bitmask, unpack plus
dictionary gather for fixed-width values, and encode plus statistics in the
writer.

The decision on how far to take fusion is made by experiment: once the composed
pipeline exists, a few representative fused kernels are written by hand and
compared against it.

## 5. Reader

### Metadata

- The footer is not eagerly deserialized into an owned tree. One pass over the
  footer records where each row group and column chunk begins; details are
  decoded on demand for the columns and row groups actually requested.
- Parsed metadata can be retained and reused by the caller across reads of the
  same file, and the API allows a caller to supply previously obtained metadata
  so a file can be opened without re-reading the footer.
- Opening a file starts from whatever tail of the file the caller supplies. If the
  footer, and the page index in front of it, are already in those bytes, opening
  needs no further request. Otherwise the core reports exactly what is missing.
- Lazy decoding saves CPU and memory, not round trips. On object storage, the
  number of dependent requests to open a file matters more than footer parse time;
  on local storage and for cached metadata, parse time is what remains.
- Retained metadata is a natural unit for a cache in the layer above, for example
  keyed by path and object version, and possibly shared across concurrent scans.
  Its form (the retained footer bytes plus an index into them) should stay cheap
  to keep and share.

#### The Thrift layer

Footer and page-header metadata is Thrift compact protocol. Parkay has its own
reader for it, because the alternatives do not fit the lazy design: the general
`thrift` crate decodes everything into owned structures, and arrow-rs's custom
parser is private to that crate. The reader:

- Works on a borrowed byte slice, allocates nothing, and borrows binary values
  from the input.
- Knows nothing about Parquet's structures. It imports nothing else from the
  crate, so it could become its own crate unchanged.
- Offers two ways over a value: the read methods, which decode and validate it,
  and `skip`, which passes over it. Skipping validates structure (type codes,
  sizes against the remaining input, nesting depth) but not the values it
  discards, such as integer widths. A value skipped at open can still fail when
  it is later decoded.
- Accepts the legacy and non-conforming output that real writers produce, such as
  fastparquet's empty lists with element type 0 and the older `0` for false in
  bool lists, and rejects anything else malformed.
- Reports every error with the byte offset of the bad value, in an error type kept
  small because every read returns it.

Skipping is the main cost of opening a file in the lazy design, so its
performance is tracked in `docs/perf/thrift-skip.md`. Its hot loop has proved
sensitive to small changes and to the shape of the input, so changes there are
checked on more than one kind of footer and in the generated assembly.

#### Decoding metadata structures

Direction for the decoders that turn requested parts of the footer into Parkay's
types, informed by arrow-rs's custom Thrift parser (its
[announcement](https://arrow.apache.org/blog/2025/10/23/rust-parquet-metadata/) and
the pull requests it cites). There, building and freeing owned structures, not the
Thrift protocol itself, dominated parse time.

- Decode directly into the final types, with no intermediate layer of generic
  Thrift structs to convert from.
- Prefer compact representations over many small allocations: for example a
  bitmask for a column chunk's encodings, counts rather than lists for page
  encoding statistics, and strings borrowed from the retained footer bytes rather
  than copied.
- Track which required fields were seen with a small bitmask and fill structures
  in place, rather than collecting optional locals and checking them at the end.
- Keep field-header reading inlined into the hot decoders, as the skip path does.
- Match each field on both its id and its wire type, so a field with the expected
  id but an unexpected type is never misread.
- The hot structures (row group, column chunk, column metadata, page header) are
  hand-written. Whether the long tail of simpler structures is generated by a
  declarative macro is decided once a few decoders exist and the repetition is
  visible. A macro mainly helps keep decoders faithful to the pinned
  `parquet.thrift`.

### Pruning and skipping

The cheapest page is the one never fetched. The reader uses, in order of cost:

1. Row-group statistics.
2. The page index (column index and offset index), to select individual pages.
3. Bloom filters, fetched only for columns that have an applicable predicate.

The result is a plan of byte ranges to fetch and a row selection to apply. Pages
excluded by the plan are neither read nor decompressed.

### Predicates

The core accepts a closed set of single-column predicates: comparison with a
constant, range, set membership, and null checks. These are the forms that can
be evaluated against statistics, Bloom filters, dictionaries, and encoded data.

Anything more general is evaluated by the caller, using either the
block-synchronous scan described below or its own machinery over the decoded
output.

### Selection-aware decoding

- Predicates on dictionary-encoded columns are evaluated once against the
  dictionary to produce the set of matching codes; rows are then tested by code
  without touching the values.
- Predicates on run-length-encoded data are evaluated once per run.
- Filter columns are decoded first and produce a row selection. Other columns
  are then decoded only at the selected positions.
- The decode strategy for each page is chosen from the density of the selection
  over that page: sparse selections use skipping kernels, dense selections use a
  full decode followed by compaction. No history or estimator is involved.
- Row selections have one in-memory representation in the first version, a
  bitmask. Whole pages excluded by pruning are dropped from the plan and never
  appear in it. A range-based representation for very sparse selections is
  added only if measurement shows the bitmask is a cost.

### Output forms

The reader returns a column at three levels, all supported from the first
version. They are layers of one decode path, not three implementations: encoded
blocks are the base, and each higher level is built by consuming the one below.

1. **Encoded blocks**, the lowest level. The caller receives the data as the
   file stores it: a stream of blocks that are each a run (a value or code and
   a length), a bit-packed group with its bit width, or a slice of plain
   values, together with the dictionary where one applies and the validity for
   the block. Nothing is expanded.
   - This is a block-oriented interface, not a callback per run, because
     Parquet's hybrid encoding alternates short packed groups with runs.
   - A block borrows from the reader and is valid only until the reader
     advances. The caller consumes or copies it before asking for the next one.
   - Callers must expect the encoding to change between pages and dictionaries
     to differ between column chunks.
   - Encodings with no useful compressed-domain form, such as the delta
     encodings and byte-stream-split, are delivered as blocks of decoded
     values.
2. **Dictionary-preserved output.** One code per row, written into a
   caller-owned buffer, with the dictionary decoded once per column chunk into
   a caller-owned buffer. Runs and packed groups of codes are expanded; the
   dictionary lookup is not performed.
3. **Materialized values.** One value per row, written into caller-owned
   buffers with a validity bitmap. Strings and binary values are copied into
   one contiguous caller-owned bytes buffer with an offsets array. An adapter
   can present that buffer in a view layout without copying the bytes again.

Predicate evaluation on dictionary codes and on runs happens inside the reader
at the encoded-block level, so callers at every level benefit from it.

Later forms, added on evidence:

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

### Block-synchronous multi-column scan

The reader can decode several columns in lockstep, one block of rows at a time,
and hand the aligned blocks to a caller-supplied function. This lets an engine
evaluate multi-column expressions while the data is cache-resident, without
Parkay owning an expression evaluator.

Columns have very different page sizes in rows, so every column decoder is
resumable at any row within a page, which block-at-a-time decoding already
requires. A lockstep scan over several columns keeps one decompressed page per
column alive at once. It is intended for small groups of columns, such as the
inputs to a filter expression, where those pages stay cache-resident together;
it is not the mechanism for decoding a wide projection.

### Nulls

Definition levels for flat nullable columns are decoded directly to a validity
bitmap. Values are placed into their final positions as part of the block
pipeline, not decoded densely for a whole page and spread out afterwards.

### Nested data

Nested columns (lists, maps, structs) are a primary performance target.

- For columnar output there is no record-assembly state machine. Each leaf
  column is converted independently: validity at each nesting level is a
  comparison of definition levels against a threshold, and list offsets are
  derived from repetition levels by building masks and counting.
- At file open, the schema is turned into a per-leaf plan holding those
  thresholds for each ancestor.
- Level decoding is vectorized, and long runs of identical levels are handled in
  bulk.
- Sibling leaves under the same list or struct carry the same levels for that
  ancestor. The plan nominates one projected leaf per ancestor to produce its
  offsets and validity, and the other leaves skip that reconstruction.
- A row selection counts top-level records, not leaf values. Applying one to a
  repeated column means finding record boundaries from the repetition levels,
  and using each page's first row index from the offset index to skip whole
  pages.

### Strings

- UTF-8 validity of a dictionary is established once; rows gathered from it need
  no further checking.
- Plain-encoded string pages interleave a length prefix with each value, so the
  page itself cannot be validated as one buffer. Non-dictionary strings are
  validated in bulk over the contiguous output bytes after the values have been
  copied there, with the additional check that each value starts on a character
  boundary. Encodings that store value bytes contiguously can be validated the
  same way.
- When views into page bytes are added, a plain page can be validated in place
  by overwriting each length prefix with ASCII spaces and validating the whole
  page in one call. This needs a mutable page buffer, which a decompressed page
  is.

### Compression and integrity

- Codec contexts are created once per worker and reused across pages.
- The fastest available implementation of each codec is used.
- The codec interface accepts a batch of pages, so the common case is a batch of
  one, and parallel or offloaded decompression remains possible later without an
  API change.
- Page checksums are verified using hardware-accelerated CRC when present, and
  verification is on by default.

## 6. Writer

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
- All of these choices are constrained by a **compatibility profile**. Being
  valid under the specification does not guarantee that older readers can read
  a file, so newer encodings and page formats are enabled only under profiles
  that allow them.

### Parallelism

Column chunks are encoded and compressed independently, so they can run as
separate units of work. Completed chunks are written out in file order. As in
the reader, the core produces the units of work and the layer above schedules
them.

### Layout for fast reads

- The page index and footer are written contiguously at the end of the file so
  that one speculative read of the tail opens the file. Bloom filters are placed
  so they can be fetched on demand, not pulled in by that tail read.
- Page indexes and page checksums are written by default.
- Pages use a moderate fixed target size with a row-count limit. The target is
  not tuned to the writing machine.
- Where the input is sorted, the writer records the sort order, and it writes
  statistics in a form that keeps pruning effective (for example truncated
  bounds for long values).
- Footer size affects how many requests it takes to open a file. Options that
  keep it small, such as omitting statistics for columns no reader filters on,
  are worth offering. Row group size trades a smaller footer against coarser
  pruning and less parallelism, so it is left to the caller.

## 7. Reference scanner

The scanner is the layer that turns the core into something that reads files end
to end. It exists to prove the core's API, to serve as the benchmark vehicle,
and to be the basis for engine integrations.

Its initial scope is deliberately small:

- A thread pool that executes the core's units of work.
- Positioned reads on local files.
- Scan planning: projection, the closed predicate set, pruning, coalescing of
  nearby byte ranges into fewer reads, and splitting very large ranges into
  parallel reads. The thresholds differ between local and remote storage and are
  scanner policy, not core behavior.
- Parallelism at the granularity of row group and column. Where a column chunk
  has an offset index, it can also be split into page ranges; many real files
  have none, and the planner then stays at row group and column granularity.
  Workers that share a dictionary-encoded chunk share one decoded dictionary,
  which is decoded before the page ranges are dispatched.

Object storage, asynchronous I/O interfaces, and direct I/O are later additions
to the scanner, not to the core. Concerns that sit above a single file, such as
pruning files from table-format manifests before opening them, opening many files
concurrently, and overlapping one file's data reads with the next file's metadata,
belong to the scanner or the engine. The core's part is to hold no global state, so
many files can be in flight at once.

## 8. Integrations

### Arrow adapter

A thin, optional layer that wraps the core's output buffers as Arrow arrays,
including dictionary and view arrays, and accepts Arrow arrays as writer input.
The core itself does not depend on Arrow.

### DataFusion

DataFusion, or a similar Arrow-based engine, is the intended first real
consumer. The integration is a separate optional layer that:

- Presents Parkay as a file source the engine can scan.
- Translates the subset of the engine's filter expressions that fit the closed
  predicate set into Parkay predicates, and leaves the remainder for the engine
  to evaluate.
- Maps the engine's partitioning and scheduling onto the core's units of work,
  so the engine's runtime, not the reference scanner's thread pool, drives
  execution.

A plain stream of fully decoded record batches would deliver only the kernel
improvements. The integration therefore needs to use pruning, selection-aware
decoding, and dictionary-preserved output to show the design's benefit, and how
far the engine's interfaces allow that is something to establish early.

The engine's current interfaces do not constrain the core's lower-level APIs.
Encoded blocks and the block-synchronous scan are not expected to fit a
general-purpose engine without custom operators, and they are designed for what
the core can offer, not for what the first engine can consume.

## 9. Format scope

- Parkay targets one tagged release of the Parquet format specification:
  `apache-parquet-format-2.14.0`, whose `parquet.thrift` is vendored at
  `format/parquet.thrift`. Proposed or in-progress format changes are not
  planned around; the implementation catches up later if needed.
- The **writer** emits a modern subset of that version, governed by the
  compatibility profile.
- The **reader** accepts what exists in real files, which includes deprecated
  and legacy features such as the original data page format, older statistics
  fields, legacy list layouts, and legacy timestamp representations.
- Within the pinned specification, features are staged. Flat and nested columns
  of the standard physical and logical types come first. Encryption and the
  remaining format surface come later. Variant and geospatial types are excluded
  from the first version.

## 10. Safety and correctness

### Unsafe code

The repository denies `unsafe_code` crate-wide. `fearless_simd` exposes SIMD,
including raw intrinsics, through a safe API, so kernels are expected to need
little or no `unsafe` of their own. What may remain is uninitialized scratch
buffers and any unchecked indexing that measurement shows is necessary. The
intended approach is to keep the crate-wide default and confine any such
`unsafe` to the buffer and kernel modules behind scoped, documented lint
expectations, with every block carrying a safety comment as the existing lints
require. Introducing the first such exception needs explicit sign-off. Inline
assembly is not used anywhere. The Thrift layer is entirely safe code and stays
that way.

### Untrusted input

- All lengths and offsets from file metadata and page headers are validated
  before use.
- After decompression, the decoded length is checked against the size the page
  header declares, for every codec. Some decoders accept malformed input and
  return short output instead of an error.
- Dictionary indices are validated once per block (a maximum and a single
  check), not per value and not skipped.
- Kernels do not require padding on borrowed input. Wide loads are used for the
  body of a buffer and a safe path handles the tail. Scratch buffers the core
  owns may carry padding.
- Corrupt input produces an error, never undefined behavior or a panic.

### Testing

- Every SIMD kernel is tested against its scalar reference across all bit
  widths, lengths, and alignments.
- Fused kernels are tested against the composed stages they replace.
- The reader and writer are tested differentially against arrow-rs: files
  written by one are read by the other and compared.
- A small corpus of real files from many writers (for example fastparquet,
  pyarrow, parquet-java, DuckDB, Spark, and Go implementations) guards the
  reader's tolerance of non-conforming but common output, such as fastparquet's
  empty lists with element type 0.
- Parsers and round trips have property tests: generated values are encoded and
  decoded back, and arbitrary bytes must never cause a panic.
- Fuzzing is part of the project from the start, covering the footer parser,
  page decoding, and each kernel. Fuzz targets run under a nightly toolchain;
  the crate itself stays on the pinned toolchain. Targets check more than the
  absence of panics where an oracle exists: two paths through Parkay that must
  agree (skipping a value and decoding it), and arrow-rs where both accept the
  input. They are seeded from real footers and run with a short per-input
  timeout, so input that makes the work grow out of proportion is caught. A fuzz
  oracle is only trusted after it has been shown to catch a deliberately planted
  bug.
- Interpreter-based checking covers the scalar paths and framing code. It does
  not cover most SIMD intrinsics, which is why the scalar-reference tests carry
  that weight.

## 11. Benchmarking

The harness grows with the code. It is run through the task runner
(`cargo xtask bench`, `report`, `profile`, `asm`) and aims to provide:

- arrow-rs as the baseline on identical files and queries.
- The relevant hardware limit for each workload (memory copy rate, raw codec
  throughput, raw read throughput), so results are read as a distance from the
  limit and not only as a ratio to the baseline.
- Coverage across the regimes that behave differently: uncompressed and
  compressed data, flat and nested schemas, full scans and selective scans,
  narrow and very wide tables, files written by Parkay and files written by
  other implementations.
- For object storage, the number of dependent requests needed to open a file and
  to start a scan, counted alongside time, since it dominates there and a local
  benchmark does not show it.
- Tracking over time, so regressions (including autovectorization regressions
  from toolchain changes) are caught.

Practice so far:

- Benchmarks run one at a time, never alongside builds or other benchmarks.
- Fixtures include real files (parquet-testing), synthetic footers of chosen
  shapes, and external sets such as
  [alamb/parquet_footer_parsing](https://github.com/alamb/parquet_footer_parsing),
  which allows comparison with published arrow-rs results. A change is judged on
  more than one shape of input: synthetic fixtures alone have been shown to
  over-represent one shape.
- Profiling uses sampling (`samply`, macOS `sample`), the generated assembly, and
  hardware counters (Xcode's `xctrace` CPU Counters template on macOS), so a
  change is explained, not only timed.
- Every forced inline or cold annotation is backed by a measurement, and what was
  tried, kept, and rejected is logged in `docs/perf/`.
- Not yet covered: x86-64 measurements, tracking results over time, and request
  counting for object storage.

## 12. Decisions against and deferrals

### Decided against

| Idea | Reason |
|---|---|
| Run-time code generation (JIT), in any form | Bit width and encoding change within a column chunk, so specialized loops do not stay valid; expected gain over block pipelines is small relative to the cost in complexity, portability, testing, and security |
| Fusing every stage combination ahead of time | Combinatorial code size and test surface; composed pipelines over cache-resident blocks capture most of the benefit |
| GPU or device-memory decode targets | That is a GPU reader, a different project |
| Hardware-specific vectorized dictionary building | The real cost is cache misses at high cardinality, where dictionary encoding should be abandoned anyway |
| A thread pool, executor, or NUMA policy inside the core | Scheduling belongs to the host engine or the scanner |
| A compiled or table-driven record-assembly machine for nested columnar output | Columnar output only needs per-leaf thresholds |
| Arbitrary expression evaluation inside the reader | Engine territory; covered by the block-synchronous scan |
| Requiring input padding for SIMD loads | Not possible for borrowed or memory-mapped input |
| Planning around proposed format changes | Target the pinned specification and catch up later |
| Generated Thrift decoders (the `thrift` crate or a code generator) | They decode eagerly into owned structures; the lazy design needs a reader that can skip |
| Inline assembly | Not used anywhere; SIMD goes through `fearless_simd` |
| Bulk-skipping Thrift lists of fixed-width types | No Parquet structure contains such a list |

### Deferred

| Idea | Note |
|---|---|
| Variant and geospatial types | Excluded from the first version |
| Encryption | Later in the format staging |
| Object storage and async I/O drivers | Scanner additions after local files work |
| Direct I/O and completion-based I/O | Platform-specific and only helpful for some workloads |
| Accelerator codec backends | The batched codec interface leaves room; no backends are built |
| Derive macros for reading and writing typed row structs | A row-oriented product with different trade-offs |
| `no_std` support | No performance value |
| Splitting into several crates | Only when a concrete need appears |
| Adaptive sizing of the speculative tail read | A caller-supplied hint suffices |
| Views into page bytes and zero-copy value views | Need retained page buffers, a second buffering model |
| A range-based selection representation | Added only if the bitmask proves costly for sparse selections |
| A Thrift encoder | Built with the writer, along with encode/decode round-trip fuzzing |
| A vectorized pass marking varint boundaries for Thrift skipping | The one untried structural idea for skip speed; set aside for now |
| Fuzzing in CI | Fuzz targets are run by hand for now |

## 13. Order of work

The overall order:

1. Benchmark harness with the arrow-rs baseline and hardware-limit measurements.
2. A vertical slice: flat nullable integer and dictionary-encoded columns, read
   and write, on both first-class instruction sets, with fuzzing and
   differential tests in place.
3. The fusion experiment on that slice.
4. The sans-I/O core across the remaining flat types and encodings, with all
   three output levels (encoded blocks, dictionary-preserved, materialized),
   and the reference scanner on local files.
5. Selection-aware decoding with page-index and Bloom-filter skipping.
6. Arrow adapter and the first DataFusion integration.
7. Vectorized nested data.
8. Reader-aware writer features: encoding selection, tail layout,
   pruning-friendly pages and statistics, compatibility profiles.
9. Views into page bytes and the remaining format surface, each as evidence
   calls for it.

Step 2 is being built from the bottom up, in small pieces that each end with tests
passing:

- Done: the specification pinned and vendored; the Thrift compact-protocol reader,
  with property tests, fuzzing, and its skip path profiled and optimized; the
  first part of the benchmark harness (step 1), covering the Thrift layer.
- Next: opening a file without I/O (accepting any tail of the file and reporting
  what is missing) and the lazy footer index, with the first metadata decoders
  and their comparison against arrow-rs.
- Then: page framing for one column chunk (headers, checksums, decompression with
  length checks), and a first scalar read of flat nullable integer columns.

## 14. Open questions

- **Unsafe policy.** Confirm the scoped approach to `unsafe` described in
  section 10.
- **Engine interface fit.** How much of pruning, selection, and encoded output
  the first engine's extension points can actually consume.
- **Fusion extent.** Settled by the experiment in step 3.
- **Compatibility profiles.** Which profiles exist and which is the default.
- **Ownership of retained metadata.** Whether the footer bytes behind lazily
  decoded metadata are uniquely owned or shared (for metadata caches and concurrent
  scans), and with which type.
- **Mismatched field types in metadata.** What a decoder does with a field that
  has the expected id but an unexpected wire type: an error, or skipping the
  field.
- **Macros for metadata decoders.** Whether the long tail of simple structures is
  generated, decided once the first decoders exist.
