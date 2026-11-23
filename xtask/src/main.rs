//! Project tasks for parkay: checks, benchmarking, fuzzing, assembly, and profiling.
//!
//! Run `cargo xtask help` for usage. Commands shell out to cargo, `cargo-fuzz`,
//! `cargo-show-asm`, `samply`, `git`, and macOS tools. Long output goes to a log file
//! under `target/xtask/`, and the terminal gets a short summary, so a command's result
//! costs a few lines to read.

mod asm;
mod bench;
mod check;
mod clean;
mod fuzz;
mod profile;
mod status;

use std::env;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::OnceLock;

use clap::{Parser, Subcommand};

/// Maximum payload lines kept after a `panicked at` line before stopping.
pub(crate) const PANIC_BLOCK_LINES_MAX: usize = 12;

#[derive(Parser)]
#[command(
    name = "cargo xtask",
    about = "Project tasks for parkay: checks, benchmarking, fuzzing, assembly, and profiling."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the CI checks in order and stop at the first failure.
    Check(check::Args),
    /// Build and run Criterion benchmarks with machine checks and provenance.
    Bench(bench::Args),
    /// Print a comparison table of saved benchmark baselines.
    Report(bench::ReportArgs),
    /// Record the working tree (untracked files included) as `refs/xtask/NAME`.
    Snapshot(bench::SnapshotArgs),
    /// Run cargo-fuzz targets in parallel, replay saved artifacts, or minimize corpora.
    Fuzz(fuzz::Args),
    /// Show a function's assembly, or save it and diff against a later build.
    Asm(asm::Args),
    /// Profile one benchmark with samply or macOS `sample` (`--text` / `--breakdown`).
    Profile(profile::Args),
    /// Show saved baselines, snapshots, reference status, asm/profile saves, and fuzz state.
    Status,
    /// Remove transient xtask logs/caches or explicitly named saved artifacts.
    Clean(clean::Args),
}

fn main() -> ExitCode {
    let raw_args: Vec<String> = env::args().skip(2).collect();
    let cli = Cli::parse();
    let result = match cli.command {
        Commands::Check(args) => check::check(&args),
        Commands::Bench(args) => bench::bench(&args, &raw_args),
        Commands::Report(args) => bench::report(&args),
        Commands::Snapshot(args) => bench::snapshot(&args),
        Commands::Fuzz(args) => fuzz::fuzz(&args),
        Commands::Asm(args) => asm::asm(&args),
        Commands::Profile(args) => profile::profile(&args, &raw_args),
        Commands::Status => {
            status::status();
            Ok(())
        }
        Commands::Clean(args) => clean::clean(&args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("xtask: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Validates saved baseline, snapshot, assembly, and profile names, which become
/// filenames or git refs. Round names (`NAME.a`, `NAME.b1`) are reserved for paired
/// `--against` runs.
fn parse_name(name: &str) -> Result<String, String> {
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    if name.is_empty()
        || !name.chars().all(allowed)
        || name.starts_with(['.', '-'])
        || name.contains("..")
    {
        return Err(format!(
            "`{name}`: use letters, digits, `-`, `_`, and `.` (not first, not doubled)"
        ));
    }
    if bench::RoundName::parse(name).is_some() {
        return Err(format!(
            "`{name}`: suffixes `.a`, `.b`, `.aN`, and `.bN` are reserved for `--against` rounds"
        ));
    }
    Ok(name.to_owned())
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// Formats `path` relative to the repository root when possible, for concise terminal
/// output.
pub(crate) fn rel_path(path: &Path) -> String {
    match path.strip_prefix(root()) {
        Ok(rel) if rel.as_os_str().is_empty() => ".".to_owned(),
        Ok(rel) => rel.display().to_string(),
        Err(_) => path.display().to_string(),
    }
}

/// `target/xtask/<sub>`, created if missing.
fn xtask_dir(sub: &str) -> Result<PathBuf, String> {
    let dir = root().join("target/xtask").join(sub);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", rel_path(&dir)))?;
    Ok(dir)
}

/// kache is a local build cache; without it, `.cargo/config.toml`'s `rustc-wrapper`
/// setting fails. Probed once per xtask invocation.
fn has_kache() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        Command::new("kache")
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

fn cargo() -> Command {
    let mut command = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    command.current_dir(root());
    if !has_kache() {
        command.env("RUSTC_WRAPPER", "");
    }
    command
}

/// Runs `command` with inherited output.
fn run(command: &mut Command) -> Result<(), String> {
    let status = command
        .status()
        .map_err(|e| format!("failed to start {command:?}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{command:?} exited with {status}"))
    }
}

/// Runs `command` with stdout and stderr written to `log`, and returns whether it
/// succeeded. When `truncate` is true, any previous log at `log` is overwritten;
/// otherwise output is appended.
fn run_logged_mode(command: &mut Command, log: &Path, truncate: bool) -> Result<bool, String> {
    let mut options = File::options();
    options.create(true).write(true);
    if truncate {
        options.truncate(true);
    } else {
        options.append(true);
    }
    let file = options
        .open(log)
        .map_err(|e| format!("{}: {e}", rel_path(log)))?;
    let stderr = file.try_clone().map_err(|e| e.to_string())?;
    let status = command
        .stdin(Stdio::null())
        .stdout(file)
        .stderr(stderr)
        .status()
        .map_err(|e| format!("failed to start {command:?}: {e}"))?;
    Ok(status.success())
}

/// Runs `command` with stdout and stderr appended to `log`, and returns whether it
/// succeeded. Failing to start is an error; exiting non-zero is not.
fn run_logged(command: &mut Command, log: &Path) -> Result<bool, String> {
    run_logged_mode(command, log, false)
}

/// Runs `command` with stdout and stderr written to a fresh (truncated) `log`.
fn run_logged_fresh(command: &mut Command, log: &Path) -> Result<bool, String> {
    run_logged_mode(command, log, true)
}

/// Captures up to [`PANIC_BLOCK_LINES_MAX`] payload lines starting at `index` after a
/// `panicked at` line, stopping when `is_end` matches, and appends them to `out`.
/// Returns the next unconsumed line index.
pub(crate) fn collect_panic_block<'a>(
    all: &[&'a str],
    mut index: usize,
    is_end: fn(&str) -> bool,
    out: &mut Vec<&'a str>,
) -> usize {
    let mut taken = 0;
    while index < all.len() && taken < PANIC_BLOCK_LINES_MAX && !is_end(all[index]) {
        out.push(all[index]);
        index += 1;
        taken += 1;
    }
    index
}

/// Runs `command` and returns its stdout, or `None` if it could not start or failed.
/// For small probes (`pmset`, `ps`, `hostname`), whose failure is handled by the caller.
fn output(command: &mut Command) -> Option<String> {
    let out = command
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Runs `command` and returns its stdout, or an error including stderr on failure.
fn output_checked(command: &mut Command, what: &str) -> Result<String, String> {
    let out = command
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("failed to start {what}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let detail = tail(stderr.trim(), 5);
        if detail.is_empty() {
            Err(format!("{what} failed ({})", out.status))
        } else {
            Err(format!("{what} failed:\n{detail}"))
        }
    }
}

/// The last `count` lines of `text`.
fn tail(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(count)..].join("\n")
}

/// Kills and waits on a spawned child if dropped before [`ChildGuard::wait`], so an
/// early `?` cannot leave background processes running.
struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }

    fn id(&self) -> Option<u32> {
        self.0.as_ref().map(Child::id)
    }

    fn wait(mut self) -> Result<std::process::ExitStatus, String> {
        let mut child = self
            .0
            .take()
            .ok_or_else(|| "child already consumed".to_owned())?;
        child.wait().map_err(|e| e.to_string())
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            drop(child.kill());
            drop(child.wait());
        }
    }
}

/// Who may run beside whom. Measurement (bench, profile) needs the machine to itself;
/// builds and fuzzing (check, fuzz, asm) may run together but not during a measurement.
#[derive(Clone, Copy)]
enum Lock {
    Measure,
    Build,
}

/// Holds the OS lock file for the life of a command and removes `measure.owner` on drop
/// while the exclusive lock is still held.
pub(crate) struct LockGuard {
    _file: File,
    owner: Option<PathBuf>,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.take() {
            drop(std::fs::remove_file(owner));
        }
    }
}

/// Takes the measurement lock for the life of the guard. The OS releases the lock when
/// the process exits, so a crashed run cannot leave it stuck.
fn lock(kind: Lock, what: &str) -> Result<LockGuard, String> {
    let dir = xtask_dir("")?;
    let path = dir.join("measure.lock");
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", rel_path(&path)))?;
    let owner_path = dir.join("measure.owner");
    let taken = match kind {
        Lock::Measure => file.try_lock(),
        Lock::Build => file.try_lock_shared(),
    };
    match taken {
        Ok(()) => {
            let owner = match kind {
                Lock::Measure => {
                    let note = format!("pid {}: cargo xtask {what}\n", std::process::id());
                    // The note only improves the message another command prints; losing it
                    // loses nothing, so a write failure is ignored.
                    drop(std::fs::write(&owner_path, note));
                    Some(owner_path)
                }
                Lock::Build => {
                    // Holding a shared lock proves no exclusive measurement lock is held,
                    // so any leftover `measure.owner` is stale from a crashed run.
                    drop(std::fs::remove_file(&owner_path));
                    None
                }
            };
            Ok(LockGuard { _file: file, owner })
        }
        Err(std::fs::TryLockError::WouldBlock) => {
            let holder = lock_holder_message(kind, read_lock_owner(&owner_path).as_deref());
            Err(format!(
                "{holder}; running `cargo xtask {what}` now would skew or be skewed by it. \
                 Wait for it to finish."
            ))
        }
        Err(std::fs::TryLockError::Error(e)) => Err(format!("{}: {e}", rel_path(&path))),
    }
}

fn read_lock_owner(owner_path: &Path) -> Option<String> {
    std::fs::read_to_string(owner_path)
        .ok()
        .map(|note| note.trim().to_owned())
        .filter(|note| !note.is_empty())
}

fn lock_holder_message(kind: Lock, active_owner: Option<&str>) -> String {
    match (kind, active_owner) {
        (_, Some(note)) => format!("a measurement is running ({note})"),
        (Lock::Build, None) => "a measurement is running".into(),
        (Lock::Measure, None) => "another cargo xtask check, fuzz, or asm run is running".into(),
    }
}

#[cfg(test)]
pub(crate) struct TempDir(PathBuf);

#[cfg(test)]
impl TempDir {
    pub(crate) fn new(label: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};

        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            env::temp_dir().join(format!("parkay-xtask-{label}-{}-{id}", std::process::id()));
        drop(std::fs::remove_dir_all(&path));
        std::fs::create_dir_all(&path)
            .unwrap_or_else(|e| panic!("failed to create {}: {e}", path.display()));
        Self(path)
    }
}

#[cfg(test)]
impl std::ops::Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

#[cfg(test)]
impl Drop for TempDir {
    fn drop(&mut self) {
        drop(std::fs::remove_dir_all(&self.0));
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clap::{CommandFactory, Parser};

    use super::{Cli, Lock, TempDir, lock_holder_message, parse_name, rel_path, root, tail};

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn cli_accepts_valid_modes_and_rejects_conflicts() {
        let ok_cases: &[&[&str]] = &[
            &["xtask", "check"],
            &["xtask", "check", "--fast"],
            &["xtask", "check", "--filter", "tail"],
            &["xtask", "check", "--fast", "--filter", "test(tail)"],
            &["xtask", "bench"],
            &["xtask", "bench", "--list"],
            &["xtask", "bench", "--save", "base"],
            &["xtask", "bench", "--against", "base", "--save", "exp"],
            &["xtask", "bench", "--bench", "thrift", "--reference"],
            &["xtask", "report", "base", "after", "--markdown"],
            &["xtask", "fuzz", "--all", "--replay"],
            &["xtask", "fuzz", "--all", "--minimize"],
            &[
                "xtask",
                "profile",
                "--entry",
                "medium",
                "--workload",
                "open",
                "--breakdown",
            ],
            &["xtask", "asm", "--save", "before"],
            &["xtask", "asm", "skip_nested", "--full"],
            &["xtask", "status"],
            &["xtask", "clean"],
            &["xtask", "clean", "--dry-run", "--baseline", "exp"],
            &["xtask", "clean", "--baseline", "reference"],
        ];
        for case in ok_cases {
            assert!(Cli::try_parse_from(*case).is_ok(), "should parse: {case:?}");
        }
        let bad_cases: &[&[&str]] = &[
            &["xtask", "bench", "--rounds", "4"],
            &["xtask", "bench", "--against", "base"],
            &["xtask", "bench", "--reference", "--save", "x"],
            &["xtask", "bench", "--list", "--save", "x"],
            &["xtask", "bench", "--baseline", "x"],
            &["xtask", "bench", "--kernels"],
            &["xtask", "bench", "^medium/"],
            &["xtask", "fuzz", "--all", "--replay", "--seconds", "10"],
            &["xtask", "fuzz", "--all", "--minimize", "--seconds", "10"],
            &["xtask", "fuzz", "--all", "--minimize", "--replay"],
            &["xtask", "profile", "--entry", "medium"],
            &["xtask", "asm", "--rust", "--save", "x"],
            &["xtask", "asm", "--full", "--diff", "x"],
        ];
        for case in bad_cases {
            assert!(
                Cli::try_parse_from(*case).is_err(),
                "should reject: {case:?}"
            );
        }
    }

    #[test]
    fn parse_name_blocks_traversal_bad_prefixes_and_reserved_paired_suffixes() {
        for ok in ["base", "v1.2_a-b", "foo.alpha", "foo.c", "foo.a1b"] {
            assert!(parse_name(ok).is_ok(), "{ok} should be valid");
        }
        for bad in [
            "", ".", "..", ".hidden", "-flag", "a..b", "a/b", "../x", "foo.a", "foo.b", "foo.a1",
            "foo.b12",
        ] {
            assert!(parse_name(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn tail_keeps_last_n_lines() {
        assert_eq!(tail("a\nb\nc\nd", 2), "c\nd");
        assert_eq!(tail("a\nb", 5), "a\nb");
    }

    #[test]
    fn rel_path_strips_repo_root_and_keeps_external_paths() {
        let inside = root().join("target/xtask/check/fmt.log");
        assert_eq!(rel_path(&inside), "target/xtask/check/fmt.log");
        assert_eq!(rel_path(&root()), ".");
        let outside = Path::new("/tmp/outside.log");
        assert_eq!(rel_path(outside), "/tmp/outside.log");
    }

    #[test]
    fn lock_holder_reports_active_measurement_or_accurate_fallback() {
        assert_eq!(
            lock_holder_message(Lock::Build, Some("pid 42: cargo xtask bench")),
            "a measurement is running (pid 42: cargo xtask bench)"
        );
        assert_eq!(
            lock_holder_message(Lock::Measure, Some("pid 42: cargo xtask bench")),
            "a measurement is running (pid 42: cargo xtask bench)"
        );
        assert_eq!(
            lock_holder_message(Lock::Build, None),
            "a measurement is running"
        );
        assert_eq!(
            lock_holder_message(Lock::Measure, None),
            "another cargo xtask check, fuzz, or asm run is running"
        );
    }

    #[test]
    fn temp_dir_creates_unique_dirs_and_cleans_up_on_drop() {
        let first = TempDir::new("probe");
        let second = TempDir::new("probe");
        assert!(first.is_dir());
        assert!(second.is_dir());
        assert_ne!(&*first, &*second);

        let first_path = first.to_path_buf();
        let second_path = second.to_path_buf();
        std::fs::write(first.join("marker"), b"ok").expect("write temp file");
        drop(first);
        assert!(!first_path.exists());
        assert!(second.is_dir());

        drop(second);
        assert!(!second_path.exists());
    }
}
