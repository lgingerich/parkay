# AGENTS.md

Read `ARCHITECTURE.md` before non-trivial changes.

## Workflow & Behavior
- Ask before assuming when requirements, architecture, or intent are unclear.
- Think before coding on design or debugging; surface tradeoffs first.
- Stay strictly in scope: only edit files required by the task. Mention unrelated follow-ups instead of making them.
- Ask before restructuring code, changing public APIs, weakening lints/CI/profiles, adding dependencies, or running destructive actions.
- Admit uncertainty instead of guessing.
- No filler openers ("Great question", "Certainly"). Start with the answer or action; end with what changed and how it was verified.

## Working Efficiently
These come from reviewing past sessions; each one cost real time or tokens.
- **Compile as you go.** Run `cargo xtask check --fast` after each file or logical step, not after writing several files: one batch of 110 KB came back with 17 errors. Run `cargo xtask check` before handing back.
- **Format once, at the end.** `cargo fmt --all` rewrites files and forces every one of them to be read again before the next edit. Run it once when a batch of edits is done (`check` runs `fmt --all --check` and `fmt-fuzz`).
- **Edit with the Edit tool.** Do not rewrite Rust or Markdown with Python, `sed`, or `perl` scripts: a scripted replace once dropped a test silently. After any change that touches many files, read `git diff --stat`.
- **Read narrowly.** Find with `grep -n`, then read the line range. Do not read whole large files (`docs/design.md`, big modules) for one fact, and do not re-read a file just edited.
- **Keep output small.** `cargo xtask` commands print a summary and a log path; read the part of the log you need. Do not `cat` whole logs, sources from other repositories, or generated files.
- **The shell is zsh.** Quote globs and regexes (`'--include=*.rs'`, `'^small/'`); an unquoted glob fails with "no matches found". Chain with `&&`. Never put `| tail` or `| head` after a command whose exit status matters.
- **Scratch goes outside the repo** (`/tmp/parkay-scratch/`). No temporary examples, tests, or debug prints in the tree; remove any before handing back.
- **Experiments use `cargo xtask snapshot` and `bench --against`**, not edits toggled by hand or file copies in `/tmp`.
- **Long jobs run in the background.** Benchmarks, fuzzing, and full checks over a minute run in the background and are waited on, not polled.
- **Keep threads short.** When a task is done, or a thread has grown long, write what the next session needs (decisions, state, open items) where it belongs (`docs/`, `// Note:` comments, or memory) and suggest a new thread.

## Minimum Code That Works
Understand the problem and trace the real flow first, then stop at the first rung that holds:
1. Does this need to be built at all? (YAGNI)
2. Does a helper or pattern already exist in this repo? Reuse it.
3. Does `std`, the platform, or an existing dependency cover it? Use it.
4. Can it be one line? Make it one line.
5. Otherwise, write the minimum code that works.

- Deletion over addition; boring over clever; fewest files possible.
- Fix root causes, not symptoms: grep all callers of any shared function you touch and fix it once.
- Question overbuilt requests ("Do you need X, or does Y cover it?").
- When two stdlib options are the same size, pick the edge-case-correct one.
- Mark deliberate simplifications that cut a corner with a known ceiling (e.g., global lock, O(n^2) scan) with a comment naming the ceiling and upgrade path.
- Never cut corners on trust-boundary validation, data-loss prevention, security, or explicitly requested behavior. Non-trivial logic must leave one minimal runnable check behind.

## Rust & API Rules
`Cargo.toml` and `clippy.toml` enforce formatting, `unsafe` bans, docs, panics, complexity, and common perf lints. Beyond those:
- **No internal back-compat**: when internal semantics change, update the method/type and migrate callers. Never add parallel `*_trusted`/`*_unchecked` helpers or duplicate constructors.
- **Conversions on the type**: implement `Display`/`FromStr`, `From`/`TryFrom`/`AsRef`/`Borrow` rather than one-off `to_*`/`from_*` free functions.
- **Public APIs**: private fields with `pub fn` accessors; accept borrowed inputs (`&str`, `&[T]`) and return owned values unless zero-copy is explicit; keep external/wire types at crate boundaries; use identical accessor names across sibling types.
- **Errors**: typed `thiserror` in libraries, `anyhow` in binaries/tests/examples, no `Box<dyn Error>`. Layer bottom-up with `#[from]` (`leaf -> domain -> boundary`); leaf errors name the bad input/constraint. Avoid `map_err` unless injecting boundary-only context or crossing orphan-rule limits.
- **Layout**: a module with more than one file is a directory with `mod.rs`; its `thiserror` types go in `<module>/errors.rs`.
- **Performance**: measure a release-with-symbols build before optimizing, and keep the benchmark justifying any `#[inline]`, `#[cold]`, or custom hasher. Keep hot paths zero-allocation, bounded, contiguous, and statically dispatched (`dyn Trait`/`Arc`/`Box` at boundaries). Never add `unsafe` for speed without measurements and sign-off.

## Tiger Style
Parkay takes the following from [Tiger Style](https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md), adapted to Rust. It is not adopted whole. Performance is the goal; safety is the constraint it may not break; developer experience comes after both. `docs/design.md` sections 3 and 10 define the control plane, the data plane, limits, and the assertion policy.
- **Zero technical debt**: fix a known problem when it is found. A marked simplification is allowed only where input cannot reach its ceiling.
- **Control flow**: simple and explicit. No recursion without a depth limit enforced in code and listed in `ARCHITECTURE.md`. Keep branching in the parent function and loops in the leaves; keep leaf functions pure. Few abstractions.
- **Limits**: every loop, size, and count has a bound that is checked or asserted.
- **Sized types**: `u32`/`u64` for anything stored, read from a file, or public; `usize` only where indexing needs it, converted at the point of use.
- **Assertions** are for programmer errors. Anything a file, I/O, or a limit can cause returns `Err`; no assertion may be reachable from input alone.
  - Assert the arguments, results, and invariants a function relies on. Use `debug_assert!` by default: it costs nothing in a release build and is live in tests and fuzzing.
  - Use `assert!` only for a check that runs once per page or less often, or on entry to a kernel where a benchmark shows it is free. Never inside a loop over values.
  - One condition per assertion. Assert what must hold and what must not. Check constant relationships and type sizes with `const _: () = assert!(..)`.
  - Check a property on two paths where possible, such as before writing and after reading.
- **Allocation**: none on the data plane. Buffers are caller-owned and reused.
- **Scope**: the smallest possible scope for every variable.
- **Conditions**: split compound conditions into nested branches. State them positively (`index < count`). Give every `if` its `else` or a reason. Match exhaustively on Parkay's own enums.
- **Errors**: handle every one, and test every error path.
- **Dependency options**: pass each option that affects safety or speed at the call site; never rely on a dependency's default.
- **Performance**: before building, sketch the expected cost against network, storage, memory, and CPU in `docs/perf/`; then measure. Batch. Write hot loops as free functions over slices and integers, without `self`.
- **Names**: no abbreviations. Units and qualifiers last (`page_size_max`, `timeout_ms`). Keep `index`, `count`, `size` (bytes), and `offset` distinct. One meaning per name. A helper carries its caller's name (`skip`, `skip_struct`).
- **Signatures**: no two interchangeable arguments of one primitive type; use a struct with named fields. Prefer returning `()` over `bool` over an integer over `Option` over `Result`.
- **Arithmetic**: show intent with `div_ceil`, `checked_*`, and `wrapping_*`.
- **Why**: comments and docs say why. Comments are sentences.
- **Tooling**: project tasks are `cargo xtask` commands written in Rust, not shell scripts.
- **Where Rust wins**: Rust's casing (`Uuid`, not `UUID`), and `usize` at indexing sites.
- **Adapted**: zero dependencies becomes the justified list in `docs/design.md`; static allocation of all memory becomes no allocation on the data plane, because a file sets page sizes.
- **Not taken**: always-on assertions, the assertion quota per function, and the 70-line and 100-column limits. `clippy.toml` and `cargo fmt --all` already bound function length and line width.

## Tests
- Group integration tests under `tests/integration/main.rs` (plus `tests/live/main.rs` only for network/credentials) with submodules. Never place `.rs` files directly in `tests/` and never set `autotests = false`. Build fixture paths from `env!("CARGO_MANIFEST_DIR")`.
- Test observable contracts, invariants, edge cases, and failure isolation—not implementation details or incidental formatting.
- Prefer real fixtures for formats/parsers and small local fakes over mocks. Write a behavior-named regression test before fixing a reproducible bug; use property tests for parsers and round-trips.
- No arbitrary sleeps in async tests; use explicit synchronization or time control.

## Verification
After code changes, run `cargo xtask check`. It runs the CI steps in `.github/workflows/ci.yml` (`layout`, `fmt`, `fmt-fuzz`, `clippy`, `fuzz-check`, `doc`, `doctest`, `nextest`, `benches`; `clippy` and `nextest` cover every workspace crate), stops at the first failure, and prints one line per step plus the failing step's errors; full output is in `target/xtask/check/STEP.log`. While editing, use `cargo xtask check --fast` (`clippy`, `nextest-lib`) or `cargo xtask check --filter <expr>` (filtered nextest run). Do not pipe checks through `head` or `tail`: the exit status is the result.

Other `cargo xtask` tasks (`cargo xtask help`): `cargo xtask status` (locks, snapshots, saved runs), `bench` (`--list` to preview selected workloads), `snapshot`, `report <paired-name> [--changed] [--markdown]` / `report <A> <B> --changed`, `fuzz`, `asm` (`--full` for raw output), `profile`, and `clean` (`--dry-run`, `--baseline`, `--snapshot`, `--asm`, `--profile`). For benchmarking, follow `.claude/skills/benchmarking/SKILL.md`.
