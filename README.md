# parkay

A ground-up Rust implementation of a Parquet reader and writer, aimed at maximum
performance. Early development: there is no functionality yet.

- Design: [`docs/design.md`](docs/design.md)
- Module boundaries and invariants: [`ARCHITECTURE.md`](ARCHITECTURE.md)
- Contributor and agent workflow: [`AGENTS.md`](AGENTS.md)

## Setup

```bash
git clone --recurse-submodules https://github.com/lgingerich/parkay
# or, in an existing clone:
git submodule update --init
```

Builds use [`kache`](https://github.com/kunobi-ninja/kache) as the `rustc-wrapper`
(set in `.cargo/config.toml`). Install it with `cargo install kache --locked`, or
build without it with `RUSTC_WRAPPER= cargo build`.

## License

MIT. See [LICENSE](./LICENSE). `format/parquet.thrift` and the
`testdata/parquet-testing` submodule are Apache-2.0, from the Apache Parquet project.
