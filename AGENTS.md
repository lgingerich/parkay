# AGENTS.md

Read `ARCHITECTURE.md` before non-trivial changes. Priority: performance, bounded by safety, then DX (`docs/design.md`).

## Workflow & Verification
- **Scope & sign-off:** Stay in scope (note follow-ups instead of making them). Ask when intent is unclear or before changing public APIs, restructuring modules, weakening lints/CI/profiles, adding dependencies or `unsafe`, or destructive actions.
- **Checks:** Run `cargo xtask check --fast` (or `--filter <expr>`) after each step, `cargo fmt --all` once at the end of an edit batch, and `cargo xtask check` before finishing. Never pipe exit-status commands into `| head`/`| tail`; read only failing lines from `target/xtask/check/<step>.log`.
- **Tooling & shell (zsh):** Project tasks are `cargo xtask` Rust commands (`cargo xtask help`), not shell scripts. Quote globs/regexes (`'--include=*.rs'`, `'^small/'`). Put scratch in `/tmp/parkay-scratch/`. Run >1m jobs (benches, fuzz, full checks) in the background and wait rather than polling.
- **Edits & reads:** Use file-edit tools (no `sed`/`perl`/Python rewrites); check `git diff --stat` after multi-file edits. Grep (`grep -n`) before reading large files (`docs/github-performance-ideas.md`); don't re-read just-edited files.
- **Perf tooling:** Follow `.claude/skills/benchmarking/SKILL.md` (`snapshot`, `bench --against`, `profile`, `asm`).

## Rust & API
- **No internal back-compat:** Update internal APIs in place and migrate all callers—no `*_trusted`/`*_unchecked` parallels or duplicate constructors. Fix known issues when found; mark any deliberate simplification (allowed only where input cannot reach its ceiling) with its ceiling and upgrade path.
- **Public APIs:** Private fields + `pub fn` accessors (matching names across siblings); borrowed inputs (`&str`, `&[T]`), owned returns unless zero-copy is explicit; wire/external types at boundaries only. Use `Display`/`FromStr`/`From`/`TryFrom`/`AsRef`/`Borrow` over `to_*`/`from_*` free functions.
- **Modules & errors:** Multi-file modules use `<module>/mod.rs` + `thiserror` in `<module>/errors.rs` (`anyhow` in bins/tests/examples; no `Box<dyn Error>`). Layer bottom-up via `#[from]`; leaf errors name the bad input/constraint; avoid `map_err` except for boundary context or orphan rules. Handle and test every error path.

## Tiger Style
- **Control flow & bounds:** Branch in parents, keep pure loops in leaves. No recursion without a code-enforced depth limit in `ARCHITECTURE.md`. Bound every loop, size, and count (checked or asserted).
- **Types & branches:** `u32`/`u64` for stored/wire/public values (`usize` only at indexing sites); explicit `div_ceil`/`checked_*`/`wrapping_*`. Minimal scope; split compound conditions into nested positive branches (`index < count`); explicit `else` (or reason); exhaustive `match` on Parkay enums.
- **Assertions (bugs only):** Input/file/I/O/limit errors return `Err`, never panic. Default `debug_assert!`; `assert!` only at page-or-coarser granularity or benchmark-free kernel entries, never in value loops. One condition per assertion; check write/read invariants on both paths where possible; `const _: () = assert!(..)` for constants/sizes.
- **Names & signatures:** No abbreviations; units/qualifiers last (`page_size_max`, `timeout_ms`); distinguish `index`, `count`, `size` (bytes), `offset`; helpers prefix with caller (`skip_struct`). No interchangeable same-primitive params (use a struct). Prefer `()` > `bool` > int > `Option` > `Result`. Comments explain *why* in sentences.
- **Performance & deps:** Zero allocation on the data plane (reused caller buffers); hot loops as free functions over slices/ints without `self`; hot paths bounded, contiguous, statically dispatched (`dyn`/`Arc`/`Box` at boundaries). Sketch cost (network/storage/memory/CPU) in `docs/perf/` before building, then measure release-with-symbols; justify any `#[inline]`, `#[cold]`, or custom hasher with a benchmark. Pass safety/speed dependency options explicitly at call sites.

## Tests
- Put integration tests in `tests/integration/main.rs` (`tests/live/main.rs` for network/creds) with submodules—never bare `tests/*.rs` or `autotests = false`. Build fixture paths via `env!("CARGO_MANIFEST_DIR")`.
- Use real format fixtures and local fakes over mocks; add a regression test before fixing a bug; proptest parsers and round-trips.
