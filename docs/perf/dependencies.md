# Hot-path dependency evaluations

Third-party crates for hot-path primitives were chosen by benchmarking candidates on real Parquet page data on one Apple Silicon machine (the comparison harnesses were throwaway and are not in this repository). Rule applied: a C-backed library is acceptable where its advantage is clear; pure Rust is preferred where performance is roughly level.

Each crate is added to `Cargo.toml` only when the code that needs it is built. Codecs sit behind one internal interface so any choice can be swapped without touching callers.

| Need | Choice | Basis |
|---|---|---|
| Zstd | `zrip` | Faster than the C library on decode, roughly level on encode, and no `unsafe` in its own code. Because it is a young crate, keep the C library in differential tests as a reference. |
| Snappy | `snap` | The C++ library was moderately faster, but needs a vendored build and feeds untrusted input to C++. Note: `snap` has no `[MaybeUninit<u8>]` API (writes into a reused initialized slice). Revisit if profiles show Snappy decode matters. |
| LZ4 | `lz4_flex` (safe mode) | Close to the C library, with no `unsafe`. |
| UTF-8 validation | `simdutf8` | Level with the C++ library and several times faster than `std` in bulk. |
| Dictionary hashing | `foldhash` with `hashbrown` | Fastest or level on real columns, and robust on adversarial integer patterns where simpler hashes collapsed. |
| CRC-32 | `crc32fast` | Fastest at typical page sizes; runs at roughly memory-copy speed. |
| Explicit SIMD *(only if justified by profiles)* | `fearless_simd` | Chosen directly for safe dispatch, portable vectors, and safe intrinsics; verify exact operation coverage if/when explicit SIMD is needed. |

Chosen without a comparison (off the hot path or rarely used): `xxhash-rust` for Bloom filters (algorithm fixed by the Parquet spec), `flate2` with `zlib-rs` for gzip, and `brotli`.
