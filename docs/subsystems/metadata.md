# Metadata subsystem (`src/metadata/`)

Architectural rationale and contracts for Parkay's metadata layer. For module boundaries and repo-wide invariants, see `ARCHITECTURE.md`; for file layout, see `src/metadata/mod.rs`; for measurements, see `docs/perf/open.md` and `docs/perf/footer-ownership.md`.

## Lazy design and trade-offs

Opening a file makes one pass over the footer to decode file-level metadata and record the byte offset where each row group and column chunk starts—the **footer directory** (not called an "index" to avoid colliding with Parquet's page/column/offset indexes). Column-chunk details are decoded on demand via `Metadata::extract()`.

- **Reuse across scans and files:** Callers scanning one file repeatedly reuse the immutable `Metadata` directly. Across files of one dataset, `open()` accepts `Option<&Schema>` and skips schema decoding when the footer's leading schema bytes match identically (guaranteeing identical decode and end offset).
- **Saves CPU and memory, not round trips:** On object storage, dependent request count dominates (2 requests best case when the tail covers footer + page index; up to 4 worst case, plus 1 for a Bloom filter lacking `bloom_filter_length`). On local storage and cached metadata, parse time and retained footprint dominate.
- **Two structural bounds:** Thrift compact protocol has no length prefixes on structs, so `open()` must walk every footer byte to reach fields after `row_groups`. Chunk templates bring multi-row-group opens close to memory-scan speed; single-row-group footers remain bounded by Thrift `skip`. Conversely, reading every column of every row group touches the footer twice (open pass + extraction pass) instead of an eager parser's single pass.

## Opening and the late-error contract

`open(tail, file_size, limits, Option<&Schema>)` is a stateless, sans-I/O step. Passing `file_size` lets the core place `tail` and validate absolute offsets. A short tail returns `MissingBytes`; truncation inside a post-open requested range is an operating error.

- **Validated at `open()`:** Trailing magic (`PAR1`; `PARE` returns `OpenError::EncryptedFooter`), footer length against `file_size` and `Limits::footer_size_max`, file-level required fields, schema structure/repetition/depth (`Limits::schema_depth_max`), each row group's column count against schema leaf count, and `FileMetaData` encryption fields 8/9 (`Unsupported::Encryption`).
- **Deferred to `extract()` (late-error contract):** Column chunks are skipped structurally during `open()`, not value-validated, so malformed chunks surface when extracted. `ColumnChunk` checks—`file_path` (`Unsupported::ExternalFile`), required `meta_data`, and crypto fields 8/9 (`Unsupported::Encryption`)—run in `src/metadata/wire/column_chunk.rs`. Because `column_chunk_decode` stops once all requested fields are in hand, trailing `ColumnChunk` fields after `meta_data` are not read in that case (spec-compliant encrypted files are still rejected at `open()` via `PARE` or `FileMetaData` fields 8/9).
- **Field order and legacy offsets:** Ascending Thrift field-id order is never assumed. Legacy writers sometimes leave `dictionary_page_offset` unset/zero or emit unreliable `ColumnChunk.file_offset`, so chunk byte ranges are derived from validated data/dictionary offsets rather than trusting `file_offset` alone.
- **Directory shape:** One `u32` offset per column chunk (bounded to ≤ 4× footer size because every chunk occupies ≥ 1 byte). Per-chunk indexing keeps narrow extraction in microseconds.

### Decided malformed-footer rules
1. **Duplicate field id in a decoded struct is an error.** Real writers don't emit duplicates, readers disagree on which copy wins, and tracking seen bits catches repeats for free. Skipped structs remain unchecked under the late-error contract.
2. **Trailing bytes after the footer stop marker** (within the declared footer length) are ignored, matching other readers.
3. **Every non-root schema element must declare its repetition.** Required by the spec and real files; assuming a default causes level mismatches (caught by differential fuzzing against `arrow-rs`).

## Retained metadata: ownership and shape

`open()` copies the footer into exact-size owned storage rather than taking ownership of a caller buffer (`docs/perf/footer-ownership.md`):
1. Fresh memory faults on first touch regardless of allocator; taking ownership saves only one `memcpy`, not page faults.
2. Callers reusing a single fetch buffer across files cannot surrender it.
3. `open()` stays stateless across `MissingBytes` retries without returning buffers.
4. Speculative tail reads are often much larger than the footer and would stay pinned in a metadata cache.
5. Avoiding caller buffer ownership keeps `Metadata` non-generic and compatible with shared buffers or `mmap`.
6. Owning the copy lets Parkay drop or transform retained bytes later if needed.

Shape invariants: no self-references (offsets/lengths into the retained footer; schema names packed into one pre-validated `String` with ranges), no interior mutability or internal decode cache, and decoders copy out the few plan values they need instead of borrowing `Metadata`.

## Chunk templates (`src/metadata/template/`)

Across row groups, a column's chunks usually share identical Thrift structure with different values. Templates skip matching chunks without re-parsing them while guaranteeing exact equivalence with full parsing (soundness argument in `src/metadata/template/compare.rs`):

- **Masked comparison:** `CompactReader::skip_marking` records every bit that parsing inspects (headers, sizes, varint continuation bits—never binary payloads). A later chunk matching its column's template under that mask follows identical parse steps and ends at the same offset. Up to four varying integer/binary lengths are learned and stepped over without falling back.
- **Bounded worst-case cost and memory:** A credit budget grants one initial template per column and funds replacements only from prior match savings, so non-matching footers degrade by ~0%. Templates are transient working memory bounded to roughly the footer's size and are cross-checked against full parsing in debug, test, and fuzz builds.

## Extraction and wire decoders

- **Single extraction API:** File-level fields (`schema`, row-group counts/sizes) are decoded at open; `created_by`, `column_orders`, and `key_value_metadata` are located at open and decoded lazily on accessor call. All per-chunk data flows through `Metadata::extract()` into caller-owned struct-of-arrays `ChunkFacts` (`row_group * column_count + column`), reusing buffers across files with zero steady-state allocation and storing byte bounds as footer offsets. There is no parallel single-chunk accessor.
- **Direct wire decoding (`src/metadata/wire/`):** Decodes straight into Parkay's compact types (`Encodings` bitmask, byte offsets) with no intermediate Thrift AST. Seen/required fields use a small bitmask (`Seen`). A known field id with an unexpected wire type is skipped as unknown (matching Thrift-generated `parquet-java`/`parquet-cpp`, whereas `arrow-rs` reads it as the expected type).
- **Future extensions:** Page-index and Bloom-filter fact groups arrive with pruning; full-table `open_all` fusion or template-offset extraction remains an open optimization (`docs/design.md`); bidirectional metadata code generation from one table is deferred until the writer starts.
