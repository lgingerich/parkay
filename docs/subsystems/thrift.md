# Thrift subsystem (`src/thrift/`)

Architectural rationale and contracts for Parkay's Thrift compact-protocol reader. For module boundaries and repo-wide invariants, see `ARCHITECTURE.md`; for optimization history and rejected experiments, see `docs/perf/thrift-skip.md`.

## Why a custom reader

Parquet uses Thrift compact protocol for file footers, page indexes, and page headers. Parkay implements its own reader over a borrowed `&[u8]` because:
- Generated Thrift crates decode eagerly into owned struct trees, whereas Parkay's lazy metadata design needs zero-allocation reading and fast structural skipping (`skip` and `skip_marking`).
- `arrow-rs`'s custom Thrift parser is crate-private.
- `src/thrift/` knows nothing about Parquet and imports nothing else from `parkay`, so it can be tested, fuzzed, benchmarked, or extracted as a standalone crate unchanged.

## Safety and bounds invariants

- **Zero allocation on read:** Reading and `skip` perform no heap allocation. Only `skip_marking` grows the caller-owned `Marks` scratch buffer.
- **Checked lengths before use:** Every container size and binary length is checked against the protocol maximum (`i32::MAX`) and against remaining input bytes (using minimum possible encoded element size) before returning. Callers may allocate proportionally to a returned size, though never `size * size_of::<DecodedStruct>()` since a 1-byte empty struct on the wire can expand to many bytes.
- **Bounded nesting:** The only attacker-controlled nesting comes from unknown containers during skipping. `SKIP_DEPTH_MAX` (64) caps descent depth, returning `ThriftError::DepthExceeded` rather than overflowing the stack.
- **Compact error type:** `ThriftError` is `Copy` and 16 bytes (saturating `u32` byte offsets relative to the buffer passed to `CompactReader::new`), keeping hot error-returning paths small.
- **Safe Rust only:** `src/thrift/` uses no `unsafe` code and stays that way.

## Structural skipping and marking

- **`skip` and `skip_field`:** Validate wire structure (type codes, varint bounds, length bounds against remaining input, container depth) without value-validating skipped fields—matching metadata's late-error contract.
- **`skip_marking`:** Runs the same structural walk while recording a bitmask of every inspected bit (field/container headers, sizes, varint continuation bits—never binary payloads) and variable-length positions into caller-owned `Marks`. `src/metadata/template/` relies on these marks to prove byte-level equivalence when skipping repeated column chunks.
- **Recursion exception:** `skip` / `skip_marking` is the sole recursive path in the library, bounded to `SKIP_DEPTH_MAX`. An explicit iterative frame stack measured ~12% slower (`docs/perf/thrift-skip.md`).

## Legacy compatibility and fuzzing

- **Ecosystem quirks accepted:** Both compact bool element type codes (`1` and `2`) are accepted, and `0` is accepted alongside `2` for `false` in bool lists (emitted by older Parquet writers).
- **Differential contracts:** Fuzzed for panic freedom (`thrift_skip`), agreement between `read_*` and `skip` (`thrift_consistency`), agreement between `skip` and `skip_marking` under arbitrary mutation of unmarked bits and variable lengths (`thrift_marking`), and byte-offset agreement with `arrow-rs`'s `skip` (`thrift_arrow_diff`, excluding bool list elements and maps where `arrow-rs` 60.1 does not consume bool list bytes).

## Deferred

- **Thrift encoder:** Built with the writer (alongside encode/decode round-trip fuzzing). Tests currently use `src/thrift/test_support.rs`.
- **Vectorized varint-boundary marking:** Untried structural optimization for single-row-group and non-template footers where chunk templates cannot bypass `skip`.
