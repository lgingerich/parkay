# parkay

A ground-up Rust implementation of a Parquet reader and writer aimed at maximum performance. Parkay is in early development.

- Design and big decisions: [`docs/design.md`](docs/design.md)
- Built subsystem rationales: [`docs/subsystems/`](docs/subsystems/)
- Module boundaries and invariants: [`ARCHITECTURE.md`](ARCHITECTURE.md)
- Contributor and agent workflow: [`AGENTS.md`](AGENTS.md)
- What has been measured: [`docs/perf/`](docs/perf/)

## Why

I'm building this for two reasons:

1. Parquet is the default storage format of the data world. Newer formats show real promise, but before the ecosystem moves on, I want to push a Parquet reader and writer as close to the hardware limits as they can go.
2. The engineer's job has already moved from writing code to building the environment agents write it in: the tests, measurements, and rules that keep their work correct and fast. This project is my attempt at doing that well, without a big lab's token budget.

### AI-driven development

**My job:** decide what parkay supports and what we trade off to support it, steer the architecture and API toward a design that's pleasant to use, and plan how it fits into the query engines and table formats that read Parquet. I also review where the agents are headed and pull them back when the code stops making sense.

**The agents' job:** write the code, and prove it's correct and fast with the tools below.

Parquet suits this kind of work because reading and writing it are stateless: bytes go in and values come out, or the reverse. Every change can be checked automatically, with no services, timing, or setup involved. The main pieces:

- **Measure, then verify**: project tasks are `cargo xtask` commands, written in Rust and run through cargo instead of shell scripts. Each prints a short summary and writes the full log to a file so agents spend few tokens reading results. An optimization goes through a loop:
  1. `profile` finds where the time goes.
  2. `asm` shows what the compiler made of a change.
  3. `bench --against` runs an A/B comparison on the same machine, and `report` calls it faster or slower only when it passes a statistical test.
  4. `fuzz` runs every target in parallel against arrow-rs to confirm the behavior didn't change.

  The measuring lets agents make aggressive changes, and the fuzzing makes those changes safe to keep.
- **[`AGENTS.md`](AGENTS.md)**: working rules written from reviewing past agent sessions, so each mistake is made once.
- **[`docs/perf/`](docs/perf/)**: what was tried, measured, kept, and rejected, so later sessions don't repeat dead ends.

## Setup

```bash
git clone --recurse-submodules https://github.com/lgingerich/parkay
# or, in an existing clone:
git submodule update --init
```

Builds use [`kache`](https://github.com/kunobi-ninja/kache) as the `rustc-wrapper` (set in `.cargo/config.toml`). Install it with `cargo install kache --locked`, or build without it with `RUSTC_WRAPPER= cargo build`.

## License

MIT. See [LICENSE](./LICENSE). `format/parquet.thrift` and the `testdata/parquet-testing` submodule are Apache-2.0, from the Apache Parquet project.
