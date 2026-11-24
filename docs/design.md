# Parkay Design

Parkay is a ground-up, full-featured Rust Parquet reader and writer built to approach hardware limits (memory, codec, and I/O bandwidth) without sacrificing safety. It is structured in layers: a **sans-I/O, engine-agnostic core** handles the Parquet format over caller-owned buffers, with a **reference scanner**, **Arrow adapter**, and **DataFusion integration** built on top—so Parkay can serve as a real alternative to `arrow-rs`'s `parquet` crate while remaining usable by non-Arrow engines and custom runtimes.

```text
+-----------------------------------------------------------+
|  Integrations: Arrow adapter, DataFusion, other engines   |
+-----------------------------------------------------------+
|  Reference scanner: local file I/O, thread pool, planning |
+-----------------------------------------------------------+
|  Core (sans-I/O, engine-agnostic)                         |
|    metadata | page framing | pruning | selection | R/W    |
+-----------------------------------------------------------+
|  Kernels: branchless slice/block loops & codec wrappers   |
+-----------------------------------------------------------+
```

> **Other docs:** current codebase map & invariants in [`ARCHITECTURE.md`](../ARCHITECTURE.md) · built subsystem rationales in [`docs/subsystems/`](subsystems/) · benchmarks & crate evaluations in [`docs/perf/`](perf/) · coding rules in [`AGENTS.md`](../AGENTS.md).
> Everything below is design intent; performance expectations are hypotheses until benchmarked in `docs/perf/`.

---

## Core Bets

- **Layered, engine-agnostic core.** The core reads and writes Parquet over bytes and caller-owned buffers—no I/O, no threads, no async runtime, and no Arrow dependency inside core. End-to-end scanning, Arrow interoperability, and DataFusion integration layer cleanly on top so Arrow is a first-class target without being the only one.
- **Do less work before doing work faster.** Lazy metadata, pruning, dictionary-preserved output, and selection-aware decoding save far more than micro-optimizing decode loops alone.
- **Hardware-limit target.** "Optimal" means bounded by memory bandwidth, codec throughput, or storage/network I/O—not framing, allocation, or scheduling.
- **Safety is non-negotiable.** Corrupt or untrusted input returns `Err`, never panics or UB. Memory and loops are bounded by caller-set `Limits`. Safe Rust by default (`deny(unsafe_code)`; no inline assembly; any future `unsafe` requires measurements and sign-off).
- **One canonical path first.** Start with one decode path, one selection format (bitmask), one buffering model, and clean scalar Rust kernels. Add fusion, explicit SIMD, or alternative representations only when profiles prove they close a real gap to the hardware floor.
- **Pragmatic dependencies.** Zero-dependency is not a goal. Hot-path crates (codecs, UTF-8, hashing, CRC) are selected by benchmarking on real Parquet data ([`docs/perf/dependencies.md`](perf/dependencies.md)); codecs sit behind one internal interface; engine integrations are feature-gated so core stays lightweight.

**Format scope:** Pinned to `apache-parquet-format-2.14.0` (`format/parquet.thrift`). The reader tolerates real-world legacy files; the writer emits modern subsets gated by compatibility profiles. **Out of scope for v1:** Variant/shredded Variant and geospatial types (rejected with explicit errors).

**Non-goals:** New file formats/encodings · a query engine (planner, joins, general expressions) · owning host threads/async/memory policy in core · internal API backwards compatibility.

---

## How It Works

### End-to-end read flow (sans-I/O)
Every core call is a pure step over bytes the caller fetched—no callbacks, no blocking, no hidden I/O:

1. **Open:** Caller reads a speculative tail of the file and passes `(tail, file_size, limits, Option<&Schema>)` to `open()`. Core returns `Metadata` + `Schema`, or the exact missing byte range if the tail was too short.
2. **Plan & prune:** Caller extracts statistics/locations for the needed columns into a reused buffer (`ChunkFacts`), evaluates single-column predicates against row-group stats → page index → Bloom filters, and gets back the exact byte ranges and row selection to read.
3. **Fetch:** The layer above (reference scanner or host engine) coalesces/splits those byte ranges and schedules I/O and worker threads however it wants—minimizing round trips on object storage or maximizing throughput on local disk.
4. **Decode:** On any worker thread, caller passes fetched page bytes plus reusable scratch/output buffers to the column reader, which decompresses one page at a time and decodes cache-resident blocks directly into the caller's output buffers.

### Memory & plane rules
- **Caller owns reusable buffers:** In steady state, reads allocate nothing. Buffers for fetched bytes, per-column scratch (decompressed page + dictionary), per-thread codec/block state, and decoded output are owned and reused by the caller across pages and files.
- **One deliberate copy:** `open()` copies the footer into exact-size storage owned by `Metadata` so callers can reuse fetch buffers and cache `Metadata` cheaply ([`docs/perf/footer-ownership.md`](perf/footer-ownership.md)).
- **No hidden state:** No globals, no thread-local storage, no interior mutability, and no borrows outliving a single call (so every value moves freely across threads and async tasks).
- **Control plane vs. data plane:** Per-file/row-group/page code (control plane) validates, branches, and may allocate within `Limits`; per-block decode loops (data plane) run branchless, zero-allocation kernels on already-validated inputs.

---

## Subsystem Directions

### Kernels & column reader
- **Three roles:** *Decoders* hold minimal encoding state (skip, dense decode, hybrid runs); one *column reader* holds all branching (once per block/run, never per value); *kernels* are branchless free functions over slices/blocks. Values are written to their final place once.
- **Scalar first, SIMD on evidence:** Write clean, branchless, fixed-block scalar Rust so LLVM autovectorizes (checked with `cargo xtask asm`), and rely on existing crates for bulk hotspots (codecs, UTF-8, CRC). Only add explicit SIMD or multiversioning to an individual kernel if `profile --breakdown` shows scalar code is a real bottleneck far from the hardware floor (keeping scalar as fallback/oracle, supporting NEON and AVX2 cleanly).
- **Invisible fusion:** Composed block pipelines define semantics; fused kernels are internal substitutions added individually on benchmark evidence and differentially tested against the composed stages.

### Reader
- **Metadata *(built)*:** Lazy footer directory, exact-size owned footer, reusable `Schema`, batched `Metadata::extract()` into caller-owned `ChunkFacts`, standalone Thrift compact reader ([`metadata.md`](subsystems/metadata.md), [`thrift.md`](subsystems/thrift.md)).
- **Predicates & selection:** Closed single-column predicate set (compare, range, set, null) evaluated on dictionary codes once per dictionary and on runs once per run; multi-column expressions run in the caller (optionally via lockstep block-synchronous scan on small column groups). Filter columns emit a **bitmask** row selection; non-filter columns choose skip vs. decode-then-compact per page from density and run count.
- **Output levels (one decode path):**
  1. *Encoded blocks:* borrowed runs / bit-packed groups / plain slices (internal base layer; public exposure TBD).
  2. *Dictionary-preserved:* narrowest codes (`u8`/`u16`/`u32`) + decoded dictionary. Decided once per chunk—all-dictionary chunks stay dictionary-preserved; mixed fallback chunks materialize so callers see one form per chunk.
  3. *Materialized:* caller buffers + validity bitmap (strings as contiguous bytes + offsets).
- **Nulls, nested, strings, framing:** Flat nullable definition levels decode directly to a validity bitmap; nested columns use per-leaf ancestor thresholds (no record-assembly state machine) and handle records spanning pages when no offset index exists. Dictionary UTF-8 is validated once per chunk; plain string pages are validated in place. Hardware CRC page checksums are on by default; codec limits are passed explicitly; one page-framing module reserves the hook for future decryption.

### Writer
- Reuses the same kernel strategy, computing statistics while blocks are cache-resident.
- Chooses encodings by sampling across the buffered row group under a **byte budget**, constrained by a **compatibility profile**.
- Encodes/compresses column chunks as independent work units written in file order, with page index + footer contiguous at the tail and checksums/indexes enabled by default.

### Reference scanner & integrations
- **Reference scanner:** Local-file driver—thread pool, positioned reads, projection/pruning, range coalescing/splitting, and adaptive predicate ordering via worker counters. Object storage, async I/O, and direct I/O are later scanner additions, not core.
- **Integrations:** First-class (feature-gated) Arrow adapter wrapping caller buffers without copying, and DataFusion integration exercising pruning, selection-aware decoding, and dictionary-preserved output rather than a plain materialized stream.

---

## Safety & Correctness Guardrails

- **Untrusted input:** Validate all offsets/lengths before use; check decompressed length against declared page size for every codec; validate dictionary indices once per block (max check); never require padding on borrowed slices.
- **Limits:** One caller-supplied `Limits` value (currently 1 GiB footer, 128 schema depth; page-header, decompressed-page, and dictionary limits arrive with page framing).
- **Pruning statistics trust:** Silent row drops are worse than errors—only prune when guaranteed sound:
  - Honor `column_orders` (type-defined, IEEE 754 total order for floats, chronological only for `INT96`; ignore missing/undefined orders).
  - Under type-defined float order, ignore NaN bounds, account for `±0` hiding its opposite sign, and assume NaNs exist unless `nan_count` proves otherwise.
  - Only trust deprecated signed `min`/`max` where signed order matches the type; check `is_*_value_exact` on truncated bounds; ignore known buggy `created_by` writers. When in doubt, do not prune.
- **Differential oracles:** Reader/writer vs. `arrow-rs` and multi-writer real files · pruning vs. unpruned scan + filter · fused/SIMD kernels vs. composed/scalar references · continuous fuzzing with `debug_assert!` enabled.

---

## Order of Work

| Step | Milestone | Focus |
|---|---|---|
| **1** | **Foundation & metadata** *(done)* | Pinned spec, Thrift reader, lazy open/extract, diff fuzzing, bench harness |
| **2** | **Page framing & first column slice** *(next)* | Headers, checksums, decompression + length checks, scalar flat nullable int & dict reads |
| **3** | **Fusion experiment** | Measure fused vs. composed block stages on the Step 2 slice |
| **4** | **Full flat reader & local scanner** | Remaining flat types/encodings (incl. ALP), output levels, local positioned-read scanner |
| **5** | **Pruning & selection-aware decode** | Page indexes, Bloom filters, bitmask selection, predicates on codes/runs |
| **6** | **Integrations** | Arrow adapter and first DataFusion integration |
| **7** | **Nested data** | Bulk level-to-validity/offset reconstruction and cross-page record selection |
| **8** | **Writer** | Byte-budgeted encoding selection, tail layout, stats/indexes, compatibility profiles |
| **9** | **Later extensions** *(on evidence)* | Zero-copy page views, encryption, object storage/async scanner, remaining spec |

When starting a step, check the matching tier in [`docs/github-performance-ideas.md`](github-performance-ideas.md) and settle concrete decisions in `docs/subsystems/<name>.md`.

---

## Decided Against & Deferred

### Decided against

| Idea | Why not |
|---|---|
| Runtime codegen (JIT) | Encodings/bit widths vary within chunks; high complexity/security cost for small gain over block pipelines |
| Ahead-of-time fusion of all stage combos | Combinatorial code size & test surface; cache-resident block pipelines capture most of the win |
| Push-based decoders into generic consumers | Multiplies monomorphization and scatters branching; column-reader + slice kernels compile once and test cleanly |
| Thread pool, executor, or polling loop in core | Scheduling and sequencing belong in the scanner or host engine |
| Arbitrary expression evaluation in reader | Engine territory; covered by block-synchronous multi-column scan |
| Record-assembly state machine for nested columnar | Columnar output only needs per-leaf ancestor thresholds |
| Generated Thrift decoders (`thrift` crate / codegen) | Eager owned-struct decoding conflicts with lazy skipping |
| Requiring input padding for wide loads | Incompatible with borrowed or memory-mapped input |
| Inline assembly | Prefer autovectorized Rust (or safe/portable SIMD crates if ever justified) |
| GPU / device-memory targets · vectorized dict building | Out of scope (GPU) / cache-miss bound at high cardinality where dicts should be abandoned |
| Planning around unratified format proposals | Target pinned spec and catch up later |

### Deferred

| Idea | Condition to revisit |
|---|---|
| Variant & geospatial types · encryption | Out of scope for v1 / later format staging (page framing reserves encryption hook) |
| Object storage, async I/O, direct/completion I/O | Scanner additions after local files work |
| Zero-copy page/value views | Requires retained page buffers (a second buffering model) |
| Range-based row selections | Only if bitmask proves costly on very sparse selections |
| Merging dictionaries across fallback chunks | When a caller asks to keep codes across mixed chunks |
| Separate single-chunk metadata accessor | Only if real consumers find batched `extract()` ergonomic friction |
| Open-pass extraction fusion / template-offset extraction | If `open_all` benchmarks show the second pass matters |
| Thrift encoder & unified bidirectional metadata table | Built when the writer starts |
| Accelerator codec backends · splitting crates | Only when a concrete need or dependency boundary appears |
| Row-struct derive macros · `no_std` · fuzzing in CI | Different trade-offs / no perf value / run by hand via `xtask` for now |

---

## Open Questions

- **Scoped `unsafe` policy:** Confirm keeping crate-wide `deny(unsafe_code)` with scoped exceptions only if uninitialized scratch or measured unchecked indexing requires it.
- **Engine integration timing:** Run a thin Arrow/DataFusion slice during Step 2 (to validate buffer/selection/string APIs early) or wait until Step 6?
- **Encoded blocks:** Keep internal in v1 until a consumer exists, or expose publicly? Also define block extent when runs count stored non-nulls while validity counts rows (and for nested levels).
- **Writer timing:** Keep at Step 8 (`arrow-rs` writes test fixtures) or build a minimal flat writer in Step 2?
- **Fusion extent:** Settled experimentally in Step 3.
- **Compatibility profiles:** Which writer profiles exist and which is default.
- **Full-table extraction (`open_all`):** Open-pass fusion vs. template-offset extraction.
- **x86-64 crate re-verification:** Re-confirm [`docs/perf/dependencies.md`](perf/dependencies.md) codec choices on x86-64 inside the repo harness when adopting them (and verify `fearless_simd` only if profiles later justify explicit SIMD).
