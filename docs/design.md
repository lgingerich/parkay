# Parkay design

Status: living document, last reviewed 2026-11-23. It records the decisions that
shape every phase and what has been built (section 13). Detail that belongs to
one phase is in `docs/design-notes.md`: the metadata design as built in M2, and
working notes for the phases not yet started. Those notes are settled only when
their phase starts, because building the earlier phases will change them. This
document describes intent, not measured results: every performance expectation
in it is a hypothesis until a benchmark confirms it, and measurements are logged
in `docs/perf/`, not here.

## Contents

- [1. Purpose](#1-purpose)
- [2. Goals and non-goals](#2-goals-and-non-goals)
- [3. Architecture](#3-architecture)
- [4. Kernels](#4-kernels)
- [5. Reader](#5-reader)
- [6. Writer](#6-writer)
- [7. Reference scanner](#7-reference-scanner)
- [8. Integrations](#8-integrations)
- [9. Format scope](#9-format-scope)
- [10. Safety and correctness](#10-safety-and-correctness)
- [11. Benchmarking](#11-benchmarking)
- [12. Decisions against and deferrals](#12-decisions-against-and-deferrals)
- [13. Order of work](#13-order-of-work)
- [14. Open questions](#14-open-questions)

## 1. Purpose

Parkay is a ground-up Rust implementation of a Parquet reader and writer whose
primary goal is maximum performance. It is not a fork or wrapper of the arrow-rs
`parquet` crate, which serves as the baseline it is measured against.

The working definition of "optimal" is a hardware one. For any workload, some
physical limit applies: memory bandwidth, the throughput of the compression
codec, or the storage or network link. The implementation is optimal when that
limit, and not Parquet framing, decoding, allocation, or scheduling, is what
bounds throughput.

Parkay borrows from Tiger Style, TigerBeetle's coding style; the parts it takes,
adapted to Rust, are in `AGENTS.md`. Performance is the goal, and it is what
Parkay will be judged on. Safety is the constraint performance may not break:
Parkay never returns or writes wrong data, never panics or exceeds its limits
because of what a file contains, and uses no `unsafe` without sign-off. Where
the two pull apart, the design looks for the form that costs nothing in a
release build, such as checks that run only in tests and fuzzing, before it
gives up either. Developer experience comes after both.

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
|    metadata | page framing | reader and writer steps      |
|    pruning | selection | encoded-form API                 |
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
| SIMD | `fearless_simd` | Chosen directly; intended to cover dispatch, portable vectors, and safe intrinsics (capabilities to be verified in step 2; see section 14) |
| Zstd | `zrip` | Faster than the C library on decode, roughly level on encode, and no `unsafe` in its own code. It is a young crate, so the C library stays in the differential tests as a reference |
| Snappy | `snap` | The C++ library was moderately faster, but needs a vendored build and puts untrusted input through C++. Note: `snap` has no `[MaybeUninit<u8>]` API (so decompression writes into a reused initialized slice). To be revisited if profiles show Snappy decode matters |
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

The core never performs I/O and never spawns threads. It is a set of steps over
plain data: opening a file, deciding which byte ranges and rows a scan needs,
and decoding. Each step is a function that takes the bytes and the state it
needs and returns either its result or the byte ranges it is missing.

- **Steps return; they do not wait or call back for bytes.** "More bytes
  needed" is an ordinary result that names every range the step can already
  determine. The caller fetches them and calls the step again. A step runs to
  completion each time it is called.
- **State is plain data.** What persists between calls (metadata, the outcome of
  pruning, the position of a column being decoded) holds no borrow of the
  caller's bytes, no shared pointer, and no interior mutability. It can be moved
  between threads, stored in a cache, or kept inside an async task without
  lifetime constraints.
- **Memory arrives as arguments.** Every call receives the input bytes, with
  their position in the file, and every buffer it reads or writes. Who owns that
  memory is set out under "Caller-owned memory".
- **Sequencing belongs to the layer above**: which row group comes next, how far
  ahead to fetch, what runs in parallel, and where bytes come from. The core has
  no driver loop of its own. A convenience driver for simple callers can be
  built on the steps; it is not part of the core.
- **A unit of work is a description**, not a closure: plain data naming what to
  decode. Whoever runs it supplies the bytes and buffers at that moment, on any
  thread.

This keeps the core usable from a synchronous program, an async runtime, or an
engine with its own scheduler. It also makes every step a function of its inputs
alone, which is what makes the core straightforward to test and to fuzz.

### Control plane and data plane

Work in the core is of two kinds, and the rules differ between them.

- The **control plane** decides. It opens a file, prunes, and sets up a column
  chunk or a page, and it runs once per file, row group, column chunk, or page.
- The **data plane** moves values. It is the kernels and the loops directly
  around them, and it runs once per block of values.

| | Control plane | Data plane |
|---|---|---|
| Allocation | Allowed, bounded by a limit or by the size of the input | None |
| Errors | Typed, naming the position and the structure concerned | None by construction, or a small `Copy` value |
| Assertions | In debug builds, tests, and fuzzing; in release builds only for a check that runs once per page or less often | In debug builds, tests, and fuzzing only, unless a measurement shows a check on entry to a kernel is free |
| Branching | Free to branch and to dispatch dynamically | Decided before the loop, not inside it |

The split is what lets the control plane check everything without the data
plane paying for it. A kernel receives input that has already been validated,
in batches large enough that the checks around it are amortized.

Errors follow the same split. A function called from a hot loop, whether per
value, per block, or per column chunk of a wide footer, returns no error where
its inputs can be validated beforehand. Where it must return one, the error is
`Copy` and stays within two machine words, held there by a compile-time
assertion, as the Thrift error is. The context a caller needs (the position in
the file, the row group, the column, the page) is added once, where the error
crosses into the control plane.

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

The core's own values own no large memory. Everything a decode or encode call
reads or writes is created by the caller and passed in. There are four kinds:

- **Input bytes.** The caller's fetch buffers, borrowed for the length of one
  call and addressed by their offset in the file. The caller can reuse them from
  file to file.
- **Column buffers.** One set for each column being read: the decompressed page
  and the decoded dictionary. A column that is part of the way through a page
  keeps that page between calls, so these belong to the column and not to the
  worker. They are reused from page to page and from one column chunk to the
  next.
- **The worker context.** One per thread, created once and passed explicitly,
  not kept in thread-local storage. It holds the codec contexts, the block-sized
  intermediates the kernels use, the kernels chosen for this CPU, and the
  counters described below. It has a fixed size, is fully allocated when it is
  created, and does not grow.
- **Output buffers.** Decode and encode entry points write into buffers the
  caller provides.

No call on the data plane allocates. A column buffer grows only on the control
plane, when a page or a dictionary is larger than any before it, and only up to
a limit (section 10). In steady state the read path performs no heap allocation
of its own. Where a dependency allocates internally, as a codec may, that is
recorded as an exception.

The worker context carries plain integer counters for the work done with it,
such as bytes decompressed, pages decoded and skipped, and rows decoded and
skipped, and rows in and out for each predicate. They are incremented where the
work happens, and the layer above reads and sums them across workers. There are
no global or atomic counters.

Scratch buffers avoid needless zero-filling before being overwritten.

The page buffer is reused from one page to the next, so nothing the caller keeps
may point into it. Output the caller keeps is always copied or decoded into
caller-owned buffers. The one thing that may reference the page buffer is an
encoded block, which is a short-lived borrow: the borrow must end before the
column is advanced again, and the compiler enforces that. Output that references
decompressed page bytes and outlives the page needs a different buffering model,
and is deferred (section 12).

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
matter, such as bit width and physical byte width (converting only where
representations differ), and selected through a dispatch table.
Nothing is generated at run time.

Pages are decompressed and decoded one at a time, so decompressed bytes are
consumed while still cache-resident, and the scratch buffer is reused for the
next page.

### Decoders, the column reader, and kernels

Decoding a column has three kinds of code, and the branching lives in one of
them.

- A **decoder** reads one encoding. It is plain state: a position in the page
  and whatever the encoding carries from one value to the next. It offers a
  small fixed set of operations: skip some values; decode some values densely
  into a slice; and, for the run-length and bit-packed hybrid only, report the
  next run or packed group without expanding it. A decoder knows nothing about
  nulls, selections, predicates, or output types.
- The **column reader** is one function per column, and it holds all the
  branching. For the next block of rows it reads the levels and the selection,
  works out how many values to skip and how many to decode, chooses the kernels,
  and calls them. Every decision about encoding, nulls, selection, and output form is made
  here, once per block or per run, never inside a loop over values.
- A **kernel** is a loop over slices with no decisions in it: unpack, gather,
  compare, compress, expand, convert. Kernels are free functions that take
  slices and integers. They are not methods on a decoder or on the column
  reader.

A value is written to its final place once. Why this shape was chosen over
decoders that push values into a generic consumer is in `docs/design-notes.md`.

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

SIMD code is intended to use `fearless_simd` to supply the three layers this
strategy needs: multiversioned autovectorization of scalar code with runtime
dispatch, portable vector types generic over the instruction set (for per-lane
shifts, byte shuffles, bitmask conversion, and compress/expand where supported),
and safe access to raw intrinsics for operations the portable API does not cover
(such as gathers, if measurement favors them over scalar loads). Which of these
operations `fearless_simd` exposes safely is to be verified when the first SIMD
kernels are written (section 14). Kernels use the highest layer that reaches the
hardware limit and drop to a lower one only on evidence.

UTF-8 validation, checksums, compression codecs, and hashing use existing
crates, each chosen by a benchmark comparison on Parquet-shaped inputs.

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

How far to take fusion is decided by experiment: once the composed pipeline
exists, a few representative fused kernels are written by hand and compared
against it.

## 5. Reader

### Metadata

Built in M2. The full design, with its acceptance targets and the rules for
malformed footers, is in `docs/design-notes.md`. The decisions that shape what
comes after it:

- **Lazy.** The footer is not deserialized into an owned tree. One pass records
  where each row group and column chunk begins, the **footer directory**, and
  details are decoded on demand for the columns and row groups requested.
  Thrift has no length prefixes on structs, so that pass still reads every
  footer byte; most column chunks are passed over by comparing them with an
  earlier chunk of the same column instead of parsing them.
- **Opening is one step.** It takes any tail of the file, the file's length,
  and the limits, and returns the metadata or the range that is missing. Lazy
  decoding saves CPU and memory, not round trips.
- **Late errors.** Column-chunk details are skipped at open, and skipping checks
  structure, not values, so a file can open and fail when a chunk is first
  decoded. That is the documented contract.
- **Parkay copies the footer** into exact-size storage it owns
  (`docs/perf/footer-ownership.md`). The retained metadata is plain, immutable,
  and shareable, borrows nothing from its own bytes, and caches nothing
  decoded. Decoders take the few values they need and do not borrow it.
- **The schema is a separate value**, reusable across files of one dataset.
- **Extraction is the one way to read column-chunk facts**, into arrays the
  caller owns, indexed by row group and column.
- **Parkay has its own Thrift reader**, over a borrowed slice, with no knowledge
  of Parquet's structures.

### Pruning and skipping

The cheapest page is the one never fetched. The reader uses, in order of cost:

1. Row-group statistics.
2. The page index (column index and offset index), to select individual pages.
3. Bloom filters, fetched only for columns that have an applicable predicate.

The result is a plan of byte ranges to fetch and a row selection to apply. Pages
excluded by the plan are neither read nor decompressed.

Pruning on statistics that are wrong or misread drops rows that match, and the
result is silently wrong, not an error. Statistics are used only where the
format and the writer make them trustworthy, and when in doubt the reader does
not prune. The rules are in `docs/design-notes.md`.

### Predicates

The core accepts a closed set of single-column predicates: comparison with a
constant, range, set membership, and null checks. These are the forms that can
be evaluated against statistics, Bloom filters, dictionaries, and encoded data.

Anything more general is evaluated by the caller, using either the
block-synchronous scan described below or its own machinery over the decoded
output. In what order predicates on different columns are evaluated is a choice
across the scan, so it belongs to the layer above; the core reports what that
choice needs in the worker counters (section 3).

### Selection-aware decoding

- Predicates on dictionary-encoded columns are evaluated once against the
  dictionary to produce the set of matching codes; rows are then tested by code
  without touching the values.
- Predicates on run-length-encoded data are evaluated once per run.
- Filter columns are decoded first and produce a row selection. Other columns
  are then decoded only at the selected positions.
- The decode strategy for each page is chosen from the selection over that page
  alone, using both selection density and run count (computed in one pass over
  the bitmask): sparse selections with few runs use skipping kernels, dense or
  highly fragmented selections use a full decode followed by compaction. No
  history or estimator is involved.
- Row selections have one in-memory representation in the first version, a
  bitmask. Whole pages excluded by pruning are dropped from the plan and never
  appear in it. A range-based representation for very sparse selections is
  added only if measurement shows the bitmask is a cost.

### Output forms

The reader is structured around three output levels — choices the column reader
makes over the same decoders and kernels (section 4), not three implementations,
and not layers that hand decoded blocks to one another. Dictionary-preserved and
materialized output are public from the first version; whether encoded blocks are
also exposed publicly in the first version or remain an internal base layer until
a consumer needs them is an open question (section 14).

1. **Encoded blocks**, the lowest level: runs, bit-packed groups, and slices of
   plain values as the file stores them, with the dictionary and the validity.
   A block borrows from the reader and is valid only until the reader advances.
   Encodings with no useful compressed-domain form are delivered as blocks of
   decoded values.
2. **Dictionary-preserved output.** One code per row, written into a
   caller-owned buffer, with the dictionary decoded once per column chunk into
   a caller-owned buffer. Runs and packed groups of codes are expanded; the
   dictionary lookup is not performed. Codes are written as `u8`, `u16`, or
   `u32`, the narrowest that holds the dictionary, because at this level writing
   the codes is the cost.
3. **Materialized values.** One value per row, written into caller-owned
   buffers with a validity bitmap. Strings and binary values are copied into
   one contiguous caller-owned bytes buffer with an offsets array. An adapter
   can present that buffer in a view layout without copying the bytes again.

Predicate evaluation on dictionary codes and on runs happens in the column
reader, on runs and packed groups, so callers at every level benefit from it.

**Dictionary fallback.** A column chunk often starts with dictionary-encoded
pages and continues with plain pages once the writer gives up on the
dictionary. The column reader decides once per chunk, before decoding it:
dictionary-preserved output is produced only for a chunk whose data pages are
all dictionary-encoded, and any other chunk is materialized. It knows this from
`encoding_stats` where the writer recorded it, and otherwise from the page
headers. A caller sees one form per chunk. Details are in
`docs/design-notes.md`.

### Block-synchronous multi-column scan

The reader can decode several columns in lockstep, one block of rows at a time,
and hand the aligned blocks to a caller-supplied function. This lets an engine
evaluate multi-column expressions while the data is cache-resident, without
Parkay owning an expression evaluator. It is meant for small groups of columns,
such as the inputs to a filter, not for decoding a wide projection.

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
- A row selection counts top-level records, not leaf values, and a record may
  span two pages when there is no offset index.

### Strings

- UTF-8 validity of a dictionary is established once; rows gathered from it need
  no further checking.
- A plain-encoded page is validated where it lies, before any value is copied.
  A length prefix whose bytes are all below `0x80` is itself valid UTF-8, so a
  span of values is validated prefixes and all, in one call, and the check that
  each value starts on a character boundary comes free. A prefix with a larger
  byte ends the span. The reasoning is in `docs/design-notes.md`.

### Compression and integrity

- Codec contexts are created once per worker and reused across pages.
- The fastest available implementation of each codec is used.
- Page checksums are verified using hardware-accelerated CRC when present, and
  verification is on by default.
- Every option of a codec that affects safety or speed is passed where the codec
  is called, never left to the codec's default. The limit on decompressed size,
  for example, is the size the page declares, not the library's own default.
- Page framing is one module, shaped so decryption can be added later without
  changing the code around it.

## 6. Writer

- The encoding path uses the same kernel strategy as the reader, and computes
  statistics while each block is in cache for encoding.
- The encoding for a column chunk is chosen by sampling values across the
  buffered row group, under a byte budget rather than only a row count.
- All encoding and compression choices are constrained by a **compatibility
  profile**. Being valid under the specification does not guarantee that older
  readers can read a file, so newer encodings and page formats are enabled only
  under profiles that allow them.
- Column chunks are encoded and compressed independently, so they can run as
  separate units of work. Completed chunks are written out in file order. As in
  the reader, the core produces the units of work and the layer above schedules
  them.
- Files are laid out for fast reads: the page index and footer are written
  contiguously at the end of the file, so that one speculative read of the tail
  opens it, and page indexes and page checksums are written by default.

## 7. Reference scanner

The scanner is the layer that turns the core into something that reads files end
to end. It exists to prove the core's API, to serve as the benchmark vehicle,
and to be the basis for engine integrations.

Its initial scope is deliberately small: a thread pool that executes the core's
units of work, positioned reads on local files, and scan planning (projection,
the closed predicate set, pruning, coalescing and splitting of byte ranges, and
the order in which predicates run).

Object storage, asynchronous I/O interfaces, and direct I/O are later additions
to the scanner, not to the core.

## 8. Integrations

### Arrow adapter

A thin, optional layer that wraps the core's output buffers as Arrow arrays,
including dictionary and view arrays, and accepts Arrow arrays as writer input.
The core itself does not depend on Arrow.

### DataFusion

DataFusion, or a similar Arrow-based engine, is the intended first real
consumer, as a separate optional layer. A plain stream of fully decoded record
batches would deliver only the kernel improvements. The integration therefore
needs to use pruning, selection-aware decoding, and dictionary-preserved output
to show the design's benefit, and how far the engine's interfaces allow that is
something to establish early. The engine's interfaces do not constrain the
core's lower-level APIs.

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
  from the first version. The pinned version includes the ALP encoding for
  floating-point columns, so the reader must decode it.

## 10. Safety and correctness

### Unsafe code

The repository denies `unsafe_code` crate-wide. `fearless_simd` is intended to
expose SIMD, including raw intrinsics, through a safe API (capabilities to be
verified in step 2; see section 14), so kernels are expected to need
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

### Limits

Everything has a limit. A size or a count read from a file is compared with its
limit before it is used to allocate, to ask for bytes, or to bound a loop.

- The limits are one plain value, given when a file is opened. Nothing after
  opening checks a limit yet. When page framing adds limits that later steps
  check, the value is kept with the metadata, so everything derived from a file
  uses the same ones.
- Each limit has a named default, defined with the code that checks it. A caller
  reading untrusted files sets lower ones.
- Exceeding a limit is an ordinary error that names the limit and the value
  found. It is not a panic.
- Where a bound follows from the size of the input, the code asserts that
  derivation and adds no separate limit. The footer directory is the example:
  its size is bounded by the footer's.
- What Parkay allocates for one file's metadata (the retained footer, the
  directory, the schema) is in each case bounded by a constant multiple of the
  footer's size, so the limit on the footer bounds them all. The working memory
  of the pass that opens a file is bounded the same way.
- Loops are bounded in the same way. A loop over input either consumes input on
  every iteration or counts against a value that was checked beforehand.
  Recursion is used only with a depth limit enforced in code.

The first limits are the size of the footer and the nesting depth of the schema.
The footer's default is 1 GiB, a few times the largest external reference footer
(~240 MB for a 100,000-column table; not part of the generated benchmark corpus),
and the schema's is 128 levels. Limits on the page header, the decompressed page,
and the dictionary are added with page framing.

### Assertions

Parkay separates two kinds of failure.

- An **operating error** is anything a file, an I/O result, or a limit can
  cause. It is expected, and it is returned as an error. This is the rule that
  corrupt input never panics.
- A **programmer error** is a broken invariant in Parkay's own code. It is not
  expected, and continuing would risk returning or writing wrong data, so it
  stops the program through an assertion.

No assertion may be reachable from input alone. An input that trips one has
found a bug, either a missing check or a wrong assertion. Fuzzing looks for
exactly that: fuzz targets are built with assertions and overflow checks on.

- Functions assert the arguments, results, and invariants they rely on. By
  default an assertion is compiled only into debug builds, tests, and fuzz
  targets, so it costs nothing in a release build. Adding such assertions
  throughout the Thrift reader left the machine code of its benchmark unchanged
  (`docs/perf/thrift-skip.md`).
- An assertion stays on in a release build only where it is certain to be
  cheap: a check that runs once per page or less often, or a check on entry to a
  kernel that a measurement shows is free. There a failed check is one compare
  and a branch that is never taken. Inside a loop over values the same check can
  stop the compiler from vectorizing the loop, so it is never left on there.
- The checks worth keeping on in release are the ones that stand between a bug
  and wrong data leaving Parkay: what the writer is about to put in a file, and
  what pruning is about to drop.
- A property is checked on two different paths where possible. The writer
  asserts what it is about to write, and the reader checks the same property
  when it reads, as an operating error. A position is checked when it is
  recorded and asserted when it is used.
- Relationships between constants, and the sizes of types that must stay small,
  are asserted at compile time.
- Tests cover what must not happen as well as what must: invalid input, valid
  input as it becomes invalid, and a test for every rejection.

### Testing

- Every SIMD kernel is tested against its scalar reference across all bit
  widths, lengths, and alignments.
- Fused kernels are tested against the composed stages they replace.
- The reader and writer are tested differentially against arrow-rs: files
  written by one are read by the other and compared, and real files from many
  writers guard the reader's tolerance of non-conforming output.
- Pruning has its own oracle, because reading the same file as arrow-rs does not
  exercise it: a scan with pruning must return exactly the rows of a scan
  without pruning followed by the same filter. This runs as a property test over
  generated data and predicates, and over the corpus of real files.
- Parsers and round trips have property tests: generated values are encoded and
  decoded back, and arbitrary bytes must never cause a panic.
- Fuzzing is part of the project from the start, covering the footer parser,
  page decoding, and each kernel. Targets check more than the absence of panics
  where an oracle exists: two paths through Parkay that must agree (skipping a
  value and decoding it), and arrow-rs where both accept the input. A fuzz
  oracle is only trusted after it has been shown to catch a deliberately planted
  bug.

## 11. Benchmarking

The harness grows with the code. It is run through the task runner
(`cargo xtask bench`, `snapshot`, `report`, `profile`, `asm`) and aims to provide:

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

Design comes before measurement. Before a component is built, its expected cost
is sketched in `docs/perf/` against the four resources (network, storage,
memory, CPU) and the latency and bandwidth of each, together with the hardware
limit it should approach. The slowest resource a workload touches is considered
first. The sketch is a hypothesis, and the benchmark that follows confirms it or
replaces it.

How the benchmarks are run so far, and the limits of the results, are in
`docs/design-notes.md`.

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
| Decoders that push values into a generic consumer | It relies on the compiler specializing every decoder for every consumer; one function per column that holds the branching and calls kernels keeps each loop compiled once and testable alone |
| A driver loop inside the core that the caller polls | Sequencing belongs to the layer above; the core is steps over plain data, and a convenience driver can sit on top of them |
| An accessor that returns one decoded column chunk, beside extraction | One way to read column-chunk facts; a caller that wants one chunk asks for one column and one row group |

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
| Merging dictionaries across a dictionary fallback | Lets a caller keep codes for a chunk whose later pages are plain; added when a caller asks |
| A Thrift encoder | Built with the writer, along with encode/decode round-trip fuzzing. Until then tests build footers with the generic test encoder |
| The complete set of metadata structures | Generated from one table, decoder and encoder together, when the writer starts |
| Decoding column-chunk facts during the pass that opens a file | Added only if the benchmark of reading every column's facts shows the second pass matters; the decoder is shaped to be reached from either place |
| A vectorized pass marking varint boundaries for Thrift skipping | An untried structural idea for skip speed. It would help where comparing column chunks with earlier ones cannot: footers of one row group, and the first row group of any footer |
| Vector instructions for the comparison of a column chunk with its template | The compiler leaves it scalar. A `fearless_simd` kernel is the candidate, when that dependency is first used |
| Reading a column chunk's facts at the offsets its template has them | A chunk that matches its template bit for bit has every field where the template has it. It needs the open pass to retain, for each chunk, which chunk it matched |
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

- Done (M0/M1): the specification pinned and vendored (M0); the Thrift
  compact-protocol reader, with property tests, fuzzing, and its skip path
  profiled and optimized; the first part of the benchmark harness (step 1),
  covering the Thrift layer (M1).
- Done (M2): opening a file without I/O, the footer directory, the schema as a
  separate value shared between files, and extraction of column-chunk facts into
  caller-owned storage, with differential tests and fuzzing against arrow-rs and
  benchmarks against the acceptance targets. The rule behind it, aimed at scans
  over many files: Parkay owns only what it keeps past the call that receives
  it, such as the footer, which it copies. Bytes it reads only while decoding
  are borrowed from the caller. What was measured is in `docs/perf/open.md`. The
  steps after opening (ranges for the page index, Bloom filters, and data) come
  with pruning and page framing.
- Then: page framing for one column chunk (headers, checksums, decompression with
  length checks), and a first scalar read of flat nullable integer columns.

When a step starts, its notes in `docs/design-notes.md` and its entries in
`docs/github-performance-ideas.md` are read again and settled.

## 14. Open questions

- **Unsafe policy.** Confirm the scoped approach to `unsafe` in section 10.
- **Engine interface fit.** How much of pruning, selection, and encoded output
  the first engine's extension points can actually consume.
- **Fusion extent.** Settled by the experiment in step 3.
- **Compatibility profiles.** Which profiles exist and which is the default.
- **How reading every column's facts gets faster.** Extraction parses each
  chunk it is asked for, at about the speed the open pass had before it compared
  chunks, and is now most of the cost of reading everything. Two candidates: decoding during the pass that
  opens the file when a caller wants everything, and reading a chunk's facts at
  the offsets its template has them (section 12). The second changes what the
  metadata retains. Neither is built.
- **The one recursive function (settled for M1/M2).** Thrift `skip` recurses to
  a fixed depth (`SKIP_DEPTH_MAX`) because the iterative form measured slower.
  `AGENTS.md` codifies the adapted rule (no recursion without a code-enforced
  depth limit listed in `ARCHITECTURE.md`), and `ARCHITECTURE.md` records the
  bounds for `CompactReader::skip`.
- **Two catch-all matches in the Thrift reader.** Writing them exhaustively, as
  the style asks, changes the machine code of the skip path. They stay as they
  are until that change is measured.
- **Encoded blocks with nulls and nesting.** A run counts stored values while
  validity counts rows, so a block's extent needs defining in both, and nested
  levels have no form at this level yet.
- **Whether encoded blocks are public in the first version.** Section 1 says
  alternatives are added when a benchmark or an integration needs them, and
  `docs/design-notes.md` ("DataFusion") notes that the first engine is not
  expected to consume encoded blocks without custom operators. They can remain
  the internal base layer and be exposed publicly when a consumer exists.
- **When the engine is first tried.** Section 8 says engine fit should be
  established early, and section 13 puts the Arrow adapter and DataFusion at
  step 6. A thin slice over flat integers during step 2 would test the shape of
  the API (caller-owned buffers, bitmask selections, string layout) before more
  is built on it.
- **Whether the writer belongs in step 2.** It doubles the surface before the
  reader's interfaces are proven, and arrow-rs already writes the fixtures.
- **`zrip` and `fearless_simd`.** Both were chosen on comparisons that are not in
  the repository and were run on one machine. How much of the kernel surface stays
  free of `unsafe` depends on what `fearless_simd` exposes safely for bit-unpacking,
  compress/expand, and (if measured faster than scalar loads) gathers, which has
  not been verified yet. The first SIMD task of step 2 (a bit-unpacking kernel on
  NEON and AVX2) tests `fearless_simd`, and the codec comparison is brought into
  the benchmark harness before `zrip` is depended on.
