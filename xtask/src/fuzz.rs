//! `cargo xtask fuzz`: cargo-fuzz targets in parallel, one summary line each.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use crate::{
    ChildGuard, Lock, cargo, collect_panic_block, lock, output, output_checked, rel_path, run,
    run_logged, run_logged_fresh, tail, xtask_dir,
};

/// A day; longer runs belong in a fuzzing service, not a terminal.
const SECONDS_MAX: u64 = 86_400;

/// Corpus file count at or above which a passing fuzz run automatically runs `cmin` so
/// corpora stay compact without having to remember `--minimize`.
const AUTO_MINIMIZE_FILES: usize = 5_000;

#[expect(
    clippy::struct_excessive_bools,
    reason = "independent clap CLI flags for fuzz modes"
)]
#[derive(clap::Args)]
pub struct Args {
    /// Fuzz targets to run (see `cargo +nightly fuzz list`).
    #[arg(value_name = "TARGET", conflicts_with = "all")]
    pub targets: Vec<String>,
    /// Run every fuzz target.
    #[arg(long)]
    pub all: bool,
    /// Seconds to fuzz each target.
    #[arg(
        long,
        default_value_t = 60,
        value_parser = clap::value_parser!(u64).range(1..=SECONDS_MAX),
        conflicts_with_all = ["replay", "minimize", "coverage"]
    )]
    pub seconds: u64,
    /// Replay each target's saved `fuzz/artifacts` instead of fuzzing.
    #[arg(long, conflicts_with_all = ["minimize", "coverage"])]
    pub replay: bool,
    /// Minimize each target's saved `fuzz/corpus` with `cargo +nightly fuzz cmin`.
    #[arg(long, conflicts_with_all = ["replay", "coverage"])]
    pub minimize: bool,
    /// Collect merged `llvm-cov` line/region coverage across selected corpora.
    #[arg(long, conflicts_with_all = ["replay", "minimize"])]
    pub coverage: bool,
}

pub fn fuzz(args: &Args) -> Result<(), String> {
    let known = targets()?;
    let chosen = select_targets(args, &known)?;
    let _lock = lock(Lock::Build, "fuzz")?;
    let logs = xtask_dir("fuzz")?;

    if args.replay {
        return replay(&chosen, &logs);
    }
    if args.coverage {
        return coverage(&chosen, &logs);
    }

    // Build before launching runs so parallel fuzzing does not queue on cargo's lock.
    // Build only the requested target when a single one was named.
    let single = match chosen.as_slice() {
        [only] => Some(only.as_str()),
        _ => None,
    };
    build_fuzz_targets(single, &logs, true)?;
    if args.minimize {
        return minimize(&chosen, &logs);
    }
    seed_empty(&chosen)?;
    explore(&chosen, args.seconds, &logs)?;
    auto_minimize(&chosen, &logs)
}

fn build_fuzz_targets(target: Option<&str>, logs: &Path, truncate_log: bool) -> Result<(), String> {
    let build_log = logs.join("build.log");
    let mut build = nightly_fuzz();
    build.arg("build");
    if let Some(target) = target {
        build.arg(target);
    }
    let ok = if truncate_log {
        run_logged_fresh(&mut build, &build_log)?
    } else {
        run_logged(&mut build, &build_log)?
    };
    if !ok {
        let text = std::fs::read_to_string(&build_log).unwrap_or_default();
        let what = target.map_or_else(
            || "building the fuzz targets".to_owned(),
            |name| format!("building fuzz target `{name}`"),
        );
        return Err(format!(
            "{what} failed:\n{}\nfull output: {}",
            tail(&text, 20),
            rel_path(&build_log)
        ));
    }
    Ok(())
}

fn select_targets(args: &Args, known: &[String]) -> Result<Vec<String>, String> {
    let raw: Vec<String> = if args.all {
        known.to_vec()
    } else {
        args.targets.clone()
    };
    if raw.is_empty() {
        return Err(format!(
            "fuzz needs a TARGET or --all; targets: {}",
            known.join(", ")
        ));
    }
    let mut seen = HashSet::new();
    let mut chosen = Vec::with_capacity(raw.len());
    for target in raw {
        if !known.contains(&target) {
            return Err(format!(
                "unknown fuzz target `{target}`; targets: {}",
                known.join(", ")
            ));
        }
        if seen.insert(target.clone()) {
            chosen.push(target);
        }
    }
    Ok(chosen)
}

/// Fuzz target names from `cargo +nightly fuzz list`.
fn targets() -> Result<Vec<String>, String> {
    let out = output_checked(nightly_fuzz().arg("list"), "`cargo +nightly fuzz list`")?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

fn nightly_fuzz() -> Command {
    let mut command = Command::new("cargo");
    command
        .current_dir(crate::root().join("fuzz"))
        // The nightly toolchain comes from `+nightly`; drop the pinned one cargo set.
        .env_remove("RUSTUP_TOOLCHAIN")
        .env("RUSTC_WRAPPER", "")
        .args(["+nightly", "fuzz"]);
    command
}

/// The entries of `dir` other than dotfiles (such as macOS `.DS_Store`), in directory
/// order; none if `dir` is missing.
pub(crate) fn visible_entries(dir: &Path) -> impl Iterator<Item = std::fs::DirEntry> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| !name.starts_with('.'))
        })
}

/// Counts the regular files in a corpus directory.
fn corpus_file_count(dir: &Path) -> usize {
    visible_entries(dir)
        .filter(|entry| entry.path().is_file())
        .count()
}

/// Returns the targets whose corpus under `corpus_root` has at least `threshold` files.
fn bloated_targets(corpus_root: &Path, targets: &[String], threshold: usize) -> Vec<String> {
    targets
        .iter()
        .filter(|target| corpus_file_count(&corpus_root.join(target)) >= threshold)
        .cloned()
        .collect()
}

/// Automatically minimizes any target whose corpus reached [`AUTO_MINIMIZE_FILES`].
fn auto_minimize(targets: &[String], logs: &Path) -> Result<(), String> {
    let bloated = bloated_targets(
        &crate::root().join("fuzz/corpus"),
        targets,
        AUTO_MINIMIZE_FILES,
    );
    if bloated.is_empty() {
        return Ok(());
    }
    println!(
        "auto-minimizing {} bloated corpora (>= {AUTO_MINIMIZE_FILES} files)",
        bloated.len()
    );
    minimize(&bloated, logs)
}

/// Seeds every empty `fuzz/corpus/TARGET` from real footers in a single invocation.
fn seed_empty(targets: &[String]) -> Result<(), String> {
    let to_seed: Vec<&str> = targets
        .iter()
        .filter(|target| corpus_file_count(&crate::root().join("fuzz/corpus").join(target)) == 0)
        .map(String::as_str)
        .collect();
    if to_seed.is_empty() {
        return Ok(());
    }
    // `fuzz_corpus` passes explicit sizes and does not depend on `--release`; dev mode
    // avoids a full LTO rebuild of `parquet` just to emit ~74 small seed files.
    run(cargo()
        .args([
            "run",
            "--locked",
            "--quiet",
            "--example",
            "fuzz_corpus",
            "--",
        ])
        .args(&to_seed))
}

/// Minimizes each target's saved corpus in place using `cargo +nightly fuzz cmin`.
fn minimize(targets: &[String], logs: &Path) -> Result<(), String> {
    let mut failed = Vec::new();
    for target in targets {
        let corpus = crate::root().join("fuzz/corpus").join(target);
        let before = corpus_file_count(&corpus);
        if before == 0 {
            println!("-    {target:<22} empty corpus");
            continue;
        }
        let log = logs.join(format!("{target}.cmin.log"));
        let mut command = nightly_fuzz();
        command.args(["cmin", target, "--", "-timeout=1"]);
        let ok = run_logged_fresh(&mut command, &log)?;
        if ok {
            let after = corpus_file_count(&corpus);
            println!("ok   {target:<22} {before} -> {after} files");
        } else {
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            println!("FAIL {target:<22} {}", failure(&text));
            println!("     log: {}", rel_path(&log));
            failed.push(target.as_str());
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "corpus minimization failed in {}",
            failed.join(", ")
        ))
    }
}

/// Runs `cargo +nightly fuzz coverage` over each selected non-empty corpus, merges the
/// `.profdata` files, and prints an `llvm-cov report` over `src/` (excluding tests).
fn coverage(targets: &[String], logs: &Path) -> Result<(), String> {
    let triple = host_triple().ok_or("`rustc +nightly -vV` did not report a host triple")?;
    let llvm_bin = nightly_llvm_bin(&triple)?;
    let profdata_tool = llvm_bin.join(format!("llvm-profdata{}", std::env::consts::EXE_SUFFIX));
    let cov_tool = llvm_bin.join(format!("llvm-cov{}", std::env::consts::EXE_SUFFIX));
    if !profdata_tool.is_file() || !cov_tool.is_file() {
        return Err(
            "nightly `llvm-tools` not installed; run: rustup component add llvm-tools --toolchain nightly"
                .to_owned(),
        );
    }

    let repo = crate::root();
    let mut profdatas = Vec::new();
    let mut binaries = Vec::new();
    for target in targets {
        let corpus = repo.join("fuzz/corpus").join(target);
        if corpus_file_count(&corpus) == 0 {
            println!("-    {target:<22} empty corpus");
            continue;
        }
        let log = logs.join(format!("{target}.cov.log"));
        let mut command = nightly_fuzz();
        command.args(["coverage", target]);
        let ok = run_logged_fresh(&mut command, &log)?;
        if !ok {
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            return Err(format!(
                "coverage failed for `{target}`:\n{}\nfull output: {}",
                tail(&text, 20),
                rel_path(&log)
            ));
        }
        let profdata = repo
            .join("fuzz/coverage")
            .join(target)
            .join("coverage.profdata");
        let binary = repo
            .join("fuzz/target")
            .join(&triple)
            .join("coverage")
            .join(&triple)
            .join("release")
            .join(format!("{target}{}", std::env::consts::EXE_SUFFIX));
        if profdata.is_file() && binary.is_file() {
            println!("ok   {target:<22} coverage collected");
            profdatas.push(profdata);
            binaries.push(binary);
        }
    }
    if binaries.is_empty() {
        return Err("no non-empty corpora to measure coverage on".to_owned());
    }

    let merged = logs.join("coverage.profdata");
    run(Command::new(&profdata_tool)
        .args(["merge", "-sparse"])
        .args(&profdatas)
        .arg("-o")
        .arg(&merged))?;

    let mut report = Command::new(&cov_tool);
    report
        .current_dir(&repo)
        .arg("report")
        .arg(format!("--instr-profile={}", merged.display()))
        .arg(&binaries[0]);
    for bin in &binaries[1..] {
        report.arg(format!("--object={}", bin.display()));
    }
    report
        .arg("--ignore-filename-regex=(tests\\.rs|test_support\\.rs)")
        .arg("--");
    let mut sources = Vec::new();
    collect_rs_files(&repo.join("src"), &mut sources);
    sources.sort();
    report.args(&sources);
    run(&mut report)
}

fn nightly_llvm_bin(triple: &str) -> Result<PathBuf, String> {
    let sysroot = output_checked(
        Command::new("rustc")
            .env_remove("RUSTUP_TOOLCHAIN")
            .args(["+nightly", "--print", "sysroot"]),
        "`rustc +nightly --print sysroot`",
    )?;
    Ok(PathBuf::from(sysroot.trim())
        .join("lib/rustlib")
        .join(triple)
        .join("bin"))
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in visible_entries(dir) {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// Runs every target at once for `seconds`. Each libFuzzer process is one thread, so
/// they use separate cores.
fn explore(targets: &[String], seconds: u64, logs: &Path) -> Result<(), String> {
    let mut running: Vec<(&String, ChildGuard, PathBuf)> = Vec::new();
    for target in targets {
        let log = logs.join(format!("{target}.log"));
        let file = std::fs::File::create(&log).map_err(|e| format!("{}: {e}", rel_path(&log)))?;
        let stderr = file.try_clone().map_err(|e| e.to_string())?;
        let child = nightly_fuzz()
            .args(["run", target, "--"])
            .arg(format!("-max_total_time={seconds}"))
            .args(["-timeout=1", "-print_final_stats=1"])
            .stdin(Stdio::null())
            .stdout(file)
            .stderr(stderr)
            .spawn()
            .map_err(|e| format!("failed to start fuzzing {target}: {e}"))?;
        running.push((target, ChildGuard::new(child), log));
    }
    println!("fuzzing {} targets for {seconds} s", running.len());
    let mut failed = Vec::new();
    for (target, child, log) in running {
        let status = child.wait()?;
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let done = text.lines().rev().find(|l| l.starts_with("Done "));
        if let (true, Some(done)) = (status.success(), done) {
            println!("ok   {target:<22} {done}");
        } else {
            println!("FAIL {target:<22} {}", failure(&text));
            println!("     log: {}", rel_path(&log));
            failed.push(target.as_str());
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("fuzzing found failures in {}", failed.join(", ")))
    }
}

/// Runs every saved artifact of each target individually so one failing artifact does
/// not hide later ones, and reports all failures. Builds each target once and invokes
/// its compiled binary directly to avoid per-artifact cargo overhead.
fn replay(targets: &[String], logs: &Path) -> Result<(), String> {
    drop(std::fs::remove_file(logs.join("build.log")));
    let triple = host_triple().ok_or("`rustc +nightly -vV` did not report a host triple")?;
    let repo = crate::root();
    let mut failed_targets = Vec::new();
    for target in targets {
        let inputs = target_artifacts(&repo, target);
        if inputs.is_empty() {
            println!("-    {target:<22} no artifacts");
            continue;
        }
        build_fuzz_targets(Some(target), logs, false)?;
        let binary = fuzz_binary(&triple, target);
        if !binary.is_file() {
            return Err(format!(
                "built fuzz target `{target}`, but {} does not exist",
                rel_path(&binary)
            ));
        }
        let log_path = logs.join(format!("{target}.replay.log"));
        let mut log_file = std::fs::File::create(&log_path)
            .map_err(|e| format!("{}: {e}", rel_path(&log_path)))?;
        let mut failures: Vec<(String, String)> = Vec::new();
        for input in &inputs {
            let out = run_replay_input(&binary, input)?;
            let combined = format!(
                "=== {} ===\n{}{}\n",
                rel_path(input),
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            drop(log_file.write_all(combined.as_bytes()));
            if !out.status.success() {
                let name = input
                    .file_name()
                    .map_or_else(|| rel_path(input), |n| n.to_string_lossy().into_owned());
                failures.push((name, failure(&combined)));
            }
        }
        if failures.is_empty() {
            println!("ok   {target:<22} {} artifacts pass", inputs.len());
        } else {
            println!(
                "FAIL {target:<22} {}/{} artifacts failed",
                failures.len(),
                inputs.len()
            );
            for (name, detail) in &failures {
                println!("     {name}: {detail}");
            }
            println!("     log: {}", rel_path(&log_path));
            failed_targets.push(target.as_str());
        }
    }
    if failed_targets.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "artifacts still fail in {}",
            failed_targets.join(", ")
        ))
    }
}

/// Sorted regular non-dotfile artifact paths under `repo/fuzz/artifacts/<target>/`.
pub(crate) fn target_artifacts(repo: &Path, target: &str) -> Vec<PathBuf> {
    let dir = repo.join("fuzz/artifacts").join(target);
    let mut inputs: Vec<PathBuf> = visible_entries(&dir)
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();
    inputs.sort();
    inputs
}

/// The host triple of the nightly toolchain that `cargo +nightly fuzz` builds with.
fn host_triple() -> Option<String> {
    let version = output(
        Command::new("rustc")
            .env_remove("RUSTUP_TOOLCHAIN")
            .args(["+nightly", "-vV"]),
    )?;
    parse_host_triple(&version).map(str::to_owned)
}

fn parse_host_triple(rustc_vv: &str) -> Option<&str> {
    rustc_vv
        .lines()
        .find_map(|line| line.strip_prefix("host:"))
        .map(str::trim)
        .filter(|triple| !triple.is_empty())
}

/// Where `cargo +nightly fuzz build` puts `target`'s binary.
fn fuzz_binary(triple: &str, target: &str) -> PathBuf {
    crate::root()
        .join("fuzz/target")
        .join(triple)
        .join("release")
        .join(format!("{target}{}", std::env::consts::EXE_SUFFIX))
}

/// Runs the fuzz `binary` on one saved `input`; libFuzzer runs a file argument once
/// and exits, so this skips cargo's overhead per artifact.
fn run_replay_input(binary: &Path, input: &Path) -> Result<Output, String> {
    Command::new(binary)
        .current_dir(crate::root().join("fuzz"))
        .arg("-timeout=1")
        .arg(input)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("failed to replay {}: {e}", rel_path(input)))
}

/// The lines that say what failed: the panic block, libFuzzer's summary, and the input.
fn failure(text: &str) -> String {
    let all: Vec<&str> = text.lines().collect();
    let mut lines: Vec<&str> = Vec::new();
    let mut index = 0;
    while index < all.len() {
        let line = all[index];
        if line.contains("panicked at") {
            lines.push(line);
            index = collect_panic_block(&all, index + 1, is_panic_block_end, &mut lines);
        } else {
            if line.starts_with("SUMMARY:") || line.contains("Test unit written to") {
                lines.push(line);
            }
            index += 1;
        }
    }
    if lines.is_empty() {
        return tail(text, 5);
    }
    // In a replay, the last input started is the one that failed.
    if let Some(running) = all.iter().rev().find(|l| l.starts_with("Running: ")) {
        lines.push(*running);
    }
    lines.join("\n     ")
}

fn is_panic_block_end(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.is_empty()
        || trimmed.starts_with("note:")
        || trimmed.starts_with("stack backtrace:")
        || trimmed.starts_with("SUMMARY:")
        || trimmed.contains("Test unit written to")
        || trimmed.contains("panicked at")
        || (trimmed.starts_with("==") && trimmed.contains("== "))
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::{bloated_targets, corpus_file_count, failure, parse_host_triple};
    use crate::TempDir;

    #[test]
    fn corpus_file_count_ignores_dotfiles_and_subdirs() {
        let root = TempDir::new("corpus");
        let dir = root.join("corpus");
        assert_eq!(corpus_file_count(&dir), 0);

        std::fs::create_dir_all(dir.join("nested")).expect("create corpus dir");
        std::fs::write(dir.join(".DS_Store"), b"mac").expect("write dotfile");
        assert_eq!(corpus_file_count(&dir), 0);

        std::fs::write(dir.join("seed-000"), b"abc").expect("write seed");
        std::fs::write(dir.join("seed-001"), b"def").expect("write seed");
        assert_eq!(corpus_file_count(&dir), 2);
    }

    #[test]
    fn bloated_targets_selects_corpora_at_or_above_threshold() {
        let root = TempDir::new("bloated");
        let small = root.join("small");
        let exact = root.join("exact");
        std::fs::create_dir_all(&small).expect("create small");
        std::fs::create_dir_all(&exact).expect("create exact");
        std::fs::write(small.join("a"), b"1").expect("write");
        std::fs::write(exact.join("a"), b"1").expect("write");
        std::fs::write(exact.join("b"), b"2").expect("write");

        let names = vec!["small".to_owned(), "exact".to_owned(), "missing".to_owned()];
        assert_eq!(bloated_targets(&root, &names, 2), vec!["exact".to_owned()]);
    }

    #[test]
    fn failure_places_panic_message_after_panicked_at() {
        let log = "\
Running: fuzz/artifacts/x/crash-1
SUMMARY: libFuzzer: deadly signal
thread '<unnamed>' panicked at src/lib.rs:42:9:
index out of bounds
Test unit written to crash-2";
        let summary = failure(log);
        let lines: Vec<&str> = summary.lines().map(str::trim).collect();
        assert_eq!(
            lines,
            vec![
                "SUMMARY: libFuzzer: deadly signal",
                "thread '<unnamed>' panicked at src/lib.rs:42:9:",
                "index out of bounds",
                "Test unit written to crash-2",
                "Running: fuzz/artifacts/x/crash-1",
            ]
        );
    }

    #[test]
    fn failure_captures_multiline_assert_eq_panic_block() {
        let log = "\
Running: fuzz/artifacts/metadata_arrow_diff/crash-abc
thread '<unnamed>' panicked at fuzz/fuzz_targets/metadata_arrow_diff.rs:28:5:
assertion `left == right` failed: num_rows mismatch
  left: 100
 right: 99
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
==12345== ERROR: libFuzzer: deadly signal
SUMMARY: libFuzzer: deadly signal";
        let summary = failure(log);
        let lines: Vec<&str> = summary.lines().map(str::trim).collect();
        assert_eq!(
            lines,
            vec![
                "thread '<unnamed>' panicked at fuzz/fuzz_targets/metadata_arrow_diff.rs:28:5:",
                "assertion `left == right` failed: num_rows mismatch",
                "left: 100",
                "right: 99",
                "SUMMARY: libFuzzer: deadly signal",
                "Running: fuzz/artifacts/metadata_arrow_diff/crash-abc",
            ]
        );
    }

    #[test]
    fn failure_caps_long_panic_block_and_keeps_summary() {
        let mut log = String::from("thread '<unnamed>' panicked at src/lib.rs:1:1:\n");
        for i in 1..=20 {
            writeln!(log, "detail {i}").expect("write to String");
        }
        log.push_str("SUMMARY: libFuzzer: deadly signal\n");
        let summary = failure(&log);
        assert!(summary.contains("detail 12"));
        assert!(!summary.contains("detail 13"));
        assert!(summary.contains("SUMMARY: libFuzzer: deadly signal"));
    }

    #[test]
    fn parse_host_triple_reads_rustc_vv_output() {
        let vv = "\
rustc 1.95.0-nightly (abc123 2026-11-20)
binary: rustc
host: aarch64-apple-darwin
release: 1.95.0-nightly";
        assert_eq!(parse_host_triple(vv), Some("aarch64-apple-darwin"));
        assert_eq!(parse_host_triple("no host here"), None);
    }
}
