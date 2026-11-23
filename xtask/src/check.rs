//! `cargo xtask check`: the CI checks with compact output.

use std::collections::HashSet;
use std::path::Path;
use std::time::Instant;

use crate::{Lock, cargo, collect_panic_block, lock, rel_path, run_logged_fresh, xtask_dir};

/// Diagnostic lines printed for a failing step locally. The full output is in its log,
/// and printed in full when running under CI.
const FAILURE_LINES_MAX: usize = 40;

/// Directories skipped when scanning for crates' `Cargo.toml` manifests.
const SKIP_DIRS: &[&str] = &["target", ".git", "testdata", "node_modules", "fuzz"];

#[derive(clap::Args)]
pub struct Args {
    /// Run only clippy and the unit tests across workspace crates, for the edit loop.
    #[arg(long)]
    pub fast: bool,
    /// Run only nextest tests matching EXPR (a nextest filterset like `test(foo)` or a
    /// test-name substring), skipping unrelated checks.
    #[arg(long, value_name = "EXPR")]
    pub filter: Option<String>,
}

/// How a step runs, which sets its common cargo arguments and which lines of a failing
/// log it prints.
#[derive(Clone, Copy)]
enum Runner {
    /// `cargo clippy` with warnings denied; prints each diagnostic once.
    Clippy,
    /// `cargo nextest run`, quiet unless a test fails; prints failures. `--filter`
    /// narrows these steps and skips the rest.
    Nextest,
    /// Another cargo command that reports diagnostics; prints each once.
    Build,
    /// `cargo test`; prints failures.
    Test,
    /// A command that lists offending files; prints every line.
    Listing,
}

/// One check: a name, how it runs, its own cargo arguments, and a hint on failure.
struct Step {
    name: &'static str,
    runner: Runner,
    args: &'static [&'static str],
    hint: &'static str,
}

impl Step {
    /// The full cargo arguments: the runner's common ones around the step's own.
    fn command_args(&self) -> Vec<&'static str> {
        let (before, after): (&[&str], &[&str]) = match self.runner {
            Runner::Clippy => (
                &["clippy", "--locked"],
                &["--message-format=short", "--", "-D", "warnings"],
            ),
            Runner::Nextest => (
                &["nextest", "run", "--locked"],
                &[
                    "--no-tests",
                    "pass",
                    "--status-level",
                    "fail",
                    "--final-status-level",
                    "fail",
                ],
            ),
            Runner::Build | Runner::Test | Runner::Listing => (&[], &[]),
        };
        [before, self.args, after].concat()
    }

    fn filter(&self) -> fn(&str) -> bool {
        match self.runner {
            Runner::Clippy | Runner::Build => is_diagnostic,
            Runner::Nextest | Runner::Test => is_test_failure,
            Runner::Listing => |_| true,
        }
    }

    /// Clippy reports a lint once per target that includes the file; one copy is
    /// enough. Test output repeats lines that matter (`left:` in two failures).
    const fn dedup(&self) -> bool {
        match self.runner {
            Runner::Clippy | Runner::Build | Runner::Listing => true,
            Runner::Nextest | Runner::Test => false,
        }
    }

    const fn is_nextest(&self) -> bool {
        matches!(self.runner, Runner::Nextest)
    }
}

/// Every workspace crate (`parkay`, `bench-support`, `xtask`) in one build. Unifying
/// their features cannot hide a missing one: `bench-support` and `xtask` do not
/// depend on `parkay`, and `parkay` builds with all of its features here anyway.
const CLIPPY: Step = Step {
    name: "clippy",
    runner: Runner::Clippy,
    args: &["--workspace", "--all-targets", "--all-features"],
    hint: "",
};

/// Single source of truth for CI (`.github/workflows/ci.yml` invokes `cargo xtask check`).
const FULL: &[Step] = &[
    Step {
        name: "fmt",
        runner: Runner::Listing,
        args: &["fmt", "--all", "--check", "--", "-l"],
        hint: "run `cargo fmt --all`",
    },
    Step {
        name: "fmt-fuzz",
        runner: Runner::Listing,
        args: &[
            "fmt",
            "--manifest-path",
            "fuzz/Cargo.toml",
            "--check",
            "--",
            "-l",
        ],
        hint: "run `cargo fmt --manifest-path fuzz/Cargo.toml`",
    },
    CLIPPY,
    Step {
        name: "fuzz-check",
        runner: Runner::Clippy,
        args: &["--manifest-path", "fuzz/Cargo.toml", "--bins"],
        hint: "",
    },
    Step {
        name: "doc",
        runner: Runner::Build,
        args: &[
            "doc",
            "--locked",
            "--no-deps",
            "--all-features",
            "--message-format=short",
        ],
        hint: "",
    },
    Step {
        name: "doctest",
        runner: Runner::Test,
        args: &["test", "--locked", "--doc"],
        hint: "",
    },
    // `--bins` holds xtask's tests. Benches run in `benches`, once, in test mode.
    Step {
        name: "nextest",
        runner: Runner::Nextest,
        args: &[
            "--workspace",
            "--lib",
            "--bins",
            "--tests",
            "--examples",
            "--all-features",
        ],
        hint: "",
    },
    Step {
        name: "benches",
        runner: Runner::Test,
        args: &["test", "--locked", "--benches", "--all-features"],
        hint: "",
    },
];

/// The edit loop: lint and unit-test every workspace crate (`parkay`, `bench-support`, `xtask`).
const FAST: &[Step] = &[
    CLIPPY,
    Step {
        name: "nextest-lib",
        runner: Runner::Nextest,
        args: &["--workspace", "--lib", "--bins", "--all-features"],
        hint: "",
    },
];

pub fn check(args: &Args) -> Result<(), String> {
    let _lock = lock(Lock::Build, "check")?;
    let logs = xtask_dir("check")?;
    let all_steps = if args.fast { FAST } else { FULL };
    if !args.fast && args.filter.is_none() {
        test_layout(&crate::root())?;
        println!("ok   layout");
    }
    let steps = select_steps(all_steps, args.filter.is_some());
    for step in steps {
        let log = logs.join(format!("{}.log", step.name));
        let started = Instant::now();
        let mut command = cargo();
        command
            .args(step.command_args())
            .env("CARGO_BUILD_WARNINGS", "deny")
            .env("RUSTDOCFLAGS", "-D warnings")
            .env("CARGO_TERM_COLOR", "never");
        if let Some(expr) = args.filter.as_deref()
            && step.is_nextest()
        {
            command.args(nextest_filter_args(expr));
        }
        let passed = run_logged_fresh(&mut command, &log)?;
        let seconds = started.elapsed().as_secs_f64();
        if passed {
            println!("ok   {:<20} {seconds:>6.1}s", step.name);
            continue;
        }
        println!("FAIL {:<20} {seconds:>6.1}s", step.name);
        print_failure(step, &log);
        return Err(format!("check failed at `{}`", step.name));
    }
    Ok(())
}

fn select_steps(steps: &'static [Step], filtered: bool) -> Vec<&'static Step> {
    steps
        .iter()
        .filter(|step| !filtered || step.is_nextest())
        .collect()
}

/// Formats `--filter EXPR` for `cargo nextest run`: filterset expressions (containing
/// `(`) go to `-E`, while plain substrings go as positional filters after `--`.
fn nextest_filter_args(expr: &str) -> [&str; 2] {
    if expr.contains('(') {
        ["-E", expr]
    } else {
        ["--", expr]
    }
}

fn print_failure(step: &Step, log: &Path) {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    if std::env::var_os("CI").is_some() {
        // CI runners are ephemeral, so print the entire failing step's log.
        print!("{text}");
        if !text.ends_with('\n') {
            println!();
        }
    } else {
        let lines = select_failure_lines(&text, step.filter(), step.dedup());
        if lines.is_empty() {
            // No line matched the filter: show the end, which holds cargo's own error.
            println!("{}", crate::tail(&text, 15));
        } else {
            for line in lines.iter().take(FAILURE_LINES_MAX) {
                println!("  {line}");
            }
            if lines.len() > FAILURE_LINES_MAX {
                println!("  ... {} more", lines.len() - FAILURE_LINES_MAX);
            }
        }
    }
    if !step.hint.is_empty() {
        println!("  hint: {}", step.hint);
    }
    println!("  full output: {}", rel_path(log));
}

fn select_failure_lines(text: &str, filter: fn(&str) -> bool, dedup: bool) -> Vec<&str> {
    let all: Vec<&str> = text.lines().collect();
    let mut lines = Vec::new();
    let mut index = 0;
    while index < all.len() {
        let line = all[index];
        if filter(line) {
            lines.push(line);
            index += 1;
            // Capture the full panic payload block (`assert_eq!` left/right, multi-line
            // messages, proptest details) without re-visiting those lines in the outer loop.
            if line.contains("panicked at") {
                index = collect_panic_block(&all, index, is_panic_block_end, &mut lines);
            }
        } else {
            index += 1;
        }
    }
    if dedup {
        let mut seen = HashSet::new();
        lines.retain(|line| seen.insert(*line));
    }
    lines
}

fn is_panic_block_end(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.is_empty()
        || trimmed.starts_with("note: run with")
        || trimmed.starts_with("stack backtrace:")
        || trimmed.starts_with("---- ")
        || trimmed.starts_with("test result:")
        || trimmed.contains("panicked at")
}

/// A `--message-format=short` diagnostic: `path:line:col: error: message`, or cargo's
/// own `error:` / `warning:` lines.
fn is_diagnostic(line: &str) -> bool {
    line.contains(": error")
        || line.contains(": warning")
        || line.starts_with("error")
        || line.starts_with("warning")
}

/// A test failure, its panic message, proptest minimal failing input, or the run's summary.
fn is_test_failure(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("FAIL")
        || trimmed.starts_with("Summary")
        || trimmed.starts_with("error")
        || trimmed.starts_with("left:")
        || trimmed.starts_with("right:")
        || trimmed.starts_with("---- ")
        || trimmed.contains("panicked at")
        || trimmed.starts_with("test result: FAILED")
        || trimmed.starts_with("minimal failing input:")
        || trimmed.starts_with("Test failed:")
}

/// Ensures integration tests are grouped: each `.rs` file directly in a crate's
/// `tests/` is its own test binary, so tests live in `tests/<name>/main.rs`.
fn test_layout(root: &Path) -> Result<(), String> {
    let mut stray = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if dir.join("Cargo.toml").is_file() {
            let tests = dir.join("tests");
            for entry in std::fs::read_dir(&tests).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "rs") {
                    stray.push(rel_path(&path));
                }
            }
        }
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir()
                && !path.is_symlink()
                && let Some(name) = path.file_name().and_then(std::ffi::OsStr::to_str)
                && !SKIP_DIRS.contains(&name)
            {
                stack.push(path);
            }
        }
    }
    stray.sort();
    if stray.is_empty() {
        Ok(())
    } else {
        println!("FAIL layout");
        for path in &stray {
            println!("  {path}");
        }
        println!("  hint: move these into tests/<name>/ as modules of its main.rs");
        Err("check failed at `layout`".into())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CLIPPY, FAST, FULL, SKIP_DIRS, is_diagnostic, is_test_failure, nextest_filter_args,
        select_failure_lines, select_steps, test_layout,
    };
    use crate::TempDir;

    #[test]
    fn dedup_keeps_repeated_test_lines_when_disabled() {
        let log = "\
---- test_a stdout ----
thread 'test_a' panicked at src/lib.rs:10:5:
assertion `left == right` failed
  left: 1
  right: 2
---- test_b stdout ----
thread 'test_b' panicked at src/lib.rs:20:5:
assertion `left == right` failed
  left: 1
  right: 2";
        let kept = select_failure_lines(log, is_test_failure, false);
        assert_eq!(kept.iter().filter(|l| l.trim() == "left: 1").count(), 2);
        assert_eq!(kept.iter().filter(|l| l.trim() == "right: 2").count(), 2);
    }

    #[test]
    fn dedup_collapses_duplicate_clippy_diagnostics() {
        let log = "\
src/lib.rs:1:1: warning: unused
src/lib.rs:1:1: warning: unused";
        let kept = select_failure_lines(log, is_diagnostic, true);
        assert_eq!(kept, vec!["src/lib.rs:1:1: warning: unused"]);
    }

    #[test]
    fn select_failure_lines_captures_single_line_and_multiline_assert_panics() {
        let single = "\
---- single stdout ----
thread 'single' panicked at src/lib.rs:5:9:
index out of bounds
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace";
        assert_eq!(
            select_failure_lines(single, is_test_failure, false),
            vec![
                "---- single stdout ----",
                "thread 'single' panicked at src/lib.rs:5:9:",
                "index out of bounds",
            ]
        );

        let assert_eq = "\
---- eq stdout ----
thread 'eq' panicked at src/metadata.rs:42:5:
assertion `left == right` failed: row group count mismatch
  left: 3
 right: 4
stack backtrace:
   0: std::panicking::begin_panic";
        assert_eq!(
            select_failure_lines(assert_eq, is_test_failure, false),
            vec![
                "---- eq stdout ----",
                "thread 'eq' panicked at src/metadata.rs:42:5:",
                "assertion `left == right` failed: row group count mismatch",
                "  left: 3",
                " right: 4",
            ]
        );
    }

    #[test]
    fn select_failure_lines_preserves_proptest_minimal_failing_input() {
        let log = "\
---- prop_roundtrip stdout ----
thread 'prop_roundtrip' panicked at src/thrift.rs:99:5:
Test failed: assertion `left == right` failed
  left: 10
  right: 20.
detail line 1
detail line 2
detail line 3
detail line 4
detail line 5
detail line 6
detail line 7
detail line 8
detail line 9
detail line 10
detail line 11

minimal failing input: bytes = [0x15, 0x00]
\tsuccesses: 12
note: run with `RUST_BACKTRACE=1`";
        let kept = select_failure_lines(log, is_test_failure, false);
        assert!(kept.contains(&"Test failed: assertion `left == right` failed"));
        assert!(kept.contains(&"  left: 10"));
        assert!(kept.contains(&"  right: 20."));
        assert!(kept.contains(&"minimal failing input: bytes = [0x15, 0x00]"));
        assert!(!kept.contains(&"detail line 11"));
    }

    #[test]
    fn test_layout_skips_fuzz_corpus_and_catches_stray_tests() {
        assert!(SKIP_DIRS.contains(&"fuzz"));

        let base = TempDir::new("layout");
        std::fs::create_dir_all(base.join("fuzz/corpus/target/tests")).expect("create fuzz corpus");
        std::fs::create_dir_all(base.join("fuzz/tests")).expect("create fuzz tests");
        std::fs::write(base.join("Cargo.toml"), "[package]\nname = \"demo\"\n")
            .expect("write root manifest");
        std::fs::write(base.join("fuzz/Cargo.toml"), "[package]\nname = \"fuzz\"\n")
            .expect("write fuzz manifest");
        std::fs::write(base.join("fuzz/tests/stray.rs"), "").expect("write fuzz stray");
        std::fs::write(base.join("fuzz/corpus/target/tests/stray.rs"), "")
            .expect("write corpus stray");

        test_layout(&base).expect("fuzz corpus should be skipped");

        std::fs::create_dir_all(base.join("tests")).expect("create root tests");
        std::fs::write(base.join("tests/stray.rs"), "").expect("write root stray");
        assert!(test_layout(&base).is_err());
    }

    #[test]
    fn filter_mode_selects_only_nextest_steps_and_formats_expr() {
        assert_eq!(nextest_filter_args("my_test"), ["--", "my_test"]);
        assert_eq!(
            nextest_filter_args("test(my_test)"),
            ["-E", "test(my_test)"]
        );

        let names = |steps, filtered| -> Vec<&str> {
            select_steps(steps, filtered)
                .iter()
                .map(|s| s.name)
                .collect()
        };
        assert_eq!(names(FAST, false), vec!["clippy", "nextest-lib"]);
        assert_eq!(names(FAST, true), vec!["nextest-lib"]);
        assert_eq!(names(FULL, true), vec!["nextest"]);
    }

    #[test]
    fn steps_cover_every_workspace_crate_and_wrap_runner_arguments() {
        for steps in [FAST, FULL] {
            for step in steps.iter().filter(|step| step.is_nextest()) {
                let args = step.command_args();
                assert!(args.contains(&"--workspace"), "{}", step.name);
                // `--bins` holds xtask's tests.
                assert!(args.contains(&"--bins"), "{}", step.name);
                assert_eq!(args[..3], ["nextest", "run", "--locked"], "{}", step.name);
                assert_eq!(args.last(), Some(&"fail"), "{}", step.name);
            }
        }
        assert!(CLIPPY.command_args().contains(&"--workspace"));
        assert_eq!(
            CLIPPY.command_args()[CLIPPY.command_args().len() - 3..],
            ["--", "-D", "warnings"]
        );
    }
}
