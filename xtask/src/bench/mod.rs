//! `bench`, `report`, and `snapshot`: measuring, and knowing what was measured.
//!
//! A benchmark result is only as good as two facts recorded beside it: which code ran
//! and what else the machine was doing. `bench` checks the machine before measuring,
//! records both facts in `target/xtask/runs/NAME.txt`, and runs the bench binaries
//! directly, so cargo's build lock is free while it measures. `--against` compares two
//! versions of the code in alternating rounds on the same machine, because drift
//! between runs minutes apart is as large as many of the changes being judged.

pub(crate) mod git;
pub(crate) mod machine;
mod report;

use std::fmt;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Instant, SystemTime};

use crate::{
    Lock, cargo, lock, output, output_checked, parse_name, rel_path, run_logged, tail, xtask_dir,
};

pub use git::{SnapshotArgs, snapshot};
pub use report::{Freshness, ReportArgs, reference_status, report};

use git::{Tree, export, git as run_git, resolve, working_tree};
use machine::{
    KeepAwake, conditions, local_time, machine as machine_id, refuse_if_noisy, settle,
    sleeps_between,
};
use report::{Record, table};

/// Default alternating rounds for `--against`: 6 rounds is the minimum for a two-sided
/// paired sign test to reach `p <= 0.05` (`2 / 2^6 = 0.031`).
const ROUNDS_DEFAULT: u32 = 6;
pub const ROUNDS_MAX: u32 = 20;

/// The baseline name plain `bench` runs (without `--save` or `--reference`) save under.
const LATEST: &str = "latest";

/// The baseline name the arrow-rs reference is saved under.
pub const REFERENCE: &str = "reference";

/// The compute-bound control workload run in every group to detect CPU clock drift.
pub const CONTROL: &str = "scan";

/// Submodule path holding external testdata fixtures, hashed when present.
const TESTDATA_PATH: &str = "testdata/parquet-testing";

/// Which Criterion bench target to build and run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
pub enum BenchTarget {
    /// Opening files and planning queries over the corpus (`benches/metadata.rs`).
    #[default]
    Metadata,
    /// Thrift compact-protocol reader kernel benchmarks (`benches/thrift.rs`).
    Thrift,
}

impl BenchTarget {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Metadata => "metadata",
            Self::Thrift => "thrift",
        }
    }

    pub const fn needs_internals(self) -> bool {
        matches!(self, Self::Thrift)
    }

    pub fn from_benchmark_name(name: &str) -> Self {
        if name.starts_with("thrift_") {
            Self::Thrift
        } else {
            Self::Metadata
        }
    }

    /// The command that saves this target's arrow-rs reference.
    pub const fn reference_command(self) -> &'static str {
        match self {
            Self::Metadata => "cargo xtask bench --reference",
            Self::Thrift => "cargo xtask bench --reference --bench thrift",
        }
    }
}

/// A name from a paired `--against` run: a round set (`NAME.a`, `NAME.b`) or one of its
/// rounds (`NAME.a3`, `NAME.b3`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RoundName<'a> {
    pub base: &'a str,
    pub side: char,
    /// The round number, or `None` for the round set itself.
    pub round: Option<u32>,
}

impl<'a> RoundName<'a> {
    pub fn parse(name: &'a str) -> Option<Self> {
        let (base, suffix) = name.rsplit_once('.')?;
        if base.is_empty() {
            return None;
        }
        let mut chars = suffix.chars();
        let side = chars.next().filter(|side| matches!(side, 'a' | 'b'))?;
        let digits = chars.as_str();
        let round = if digits.is_empty() {
            None
        } else if digits.bytes().all(|byte| byte.is_ascii_digit()) {
            Some(digits.parse().ok()?)
        } else {
            return None;
        };
        Some(Self { base, side, round })
    }

    /// True for a round set (`NAME.a`), false for one of its rounds (`NAME.a3`).
    pub const fn is_set(self) -> bool {
        self.round.is_none()
    }
}

impl fmt::Display for BenchTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(clap::Args)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "clap CLI flags for bench modes"
)]
pub struct Args {
    /// Bench target to build and measure.
    #[arg(long, value_enum, default_value_t = BenchTarget::Metadata)]
    pub bench: BenchTarget,
    /// Comma-separated corpus entry (group) names, such as `medium,nested`.
    #[arg(long, value_name = "NAMES")]
    pub entry: Option<String>,
    /// Comma-separated workload names, such as `open_all`.
    #[arg(long, value_name = "NAMES")]
    pub workload: Option<String>,
    /// Save the run as baseline NAME.
    #[arg(
        long,
        value_name = "NAME",
        value_parser = parse_name,
        conflicts_with = "reference"
    )]
    pub save: Option<String>,
    /// Measure REF (commit, branch, or `snapshot` name) against the working tree in
    /// alternating rounds. Needs `--save NAME`.
    #[arg(
        long,
        value_name = "REF",
        requires = "save",
        conflicts_with = "reference"
    )]
    pub against: Option<String>,
    /// Alternating rounds for `--against` (>= 6 needed for `p <= 0.05` sign test).
    #[arg(
        long,
        default_value_t = ROUNDS_DEFAULT,
        value_parser = clap::value_parser!(u32).range(1..=i64::from(ROUNDS_MAX)),
        requires = "against"
    )]
    pub rounds: u32,
    /// Shorten warm-up and measurement (1 s / 3 s); explore with it, decide with a full run.
    #[arg(long)]
    pub quick: bool,
    /// Add the footer and page index of every `.parquet` file in DIR.
    #[arg(long, value_name = "DIR")]
    pub files: Option<PathBuf>,
    /// Measure even when machine hygiene checks report problems (recorded in provenance).
    #[arg(long)]
    pub allow_noisy: bool,
    /// Run only arrow-rs and floor benchmarks and save as `reference`.
    #[arg(long)]
    pub reference: bool,
    /// List the exact benchmarks that would run and exit without measuring.
    #[arg(
        long,
        conflicts_with_all = ["save", "against", "rounds", "quick", "allow_noisy"]
    )]
    pub list: bool,
}

fn validate_args(args: &Args) -> Result<(), String> {
    if args.save.as_deref() == Some(REFERENCE) {
        return Err(format!(
            "`{REFERENCE}` is reserved for `--reference`; choose another --save NAME"
        ));
    }
    if args.reference && (args.entry.is_some() || args.workload.is_some() || args.files.is_some()) {
        return Err(
            "`--reference` must run the full built-in suite and cannot be combined with \
             `--entry`, `--workload`, or `--files`, so reference freshness is never falsified"
                .into(),
        );
    }
    Ok(())
}

/// Lists `(group, workload, path)` for every benchmark directory under `criterion`,
/// skipping Criterion's `report/` directory and non-directories.
pub(crate) fn criterion_bench_dirs(criterion: &Path) -> Vec<(String, String, PathBuf)> {
    let mut dirs = Vec::new();
    for group in std::fs::read_dir(criterion).into_iter().flatten().flatten() {
        let group_path = group.path();
        if !group_path.is_dir() {
            continue;
        }
        let Ok(group_name) = group.file_name().into_string() else {
            continue;
        };
        if group_name == "report" {
            continue;
        }
        for bench in std::fs::read_dir(&group_path)
            .into_iter()
            .flatten()
            .flatten()
        {
            let bench_path = bench.path();
            if !bench_path.is_dir() {
                continue;
            }
            let Ok(workload) = bench.file_name().into_string() else {
                continue;
            };
            dirs.push((group_name.clone(), workload, bench_path));
        }
    }
    dirs
}

fn run_name(args: &Args) -> String {
    if args.reference {
        format!("{REFERENCE}-{}", args.bench)
    } else {
        args.save.as_deref().unwrap_or(LATEST).to_owned()
    }
}

pub fn canonicalize_files(files: Option<&Path>) -> Result<Option<PathBuf>, String> {
    files
        .map(|dir| {
            std::fs::canonicalize(dir).map_err(|e| format!("--files {}: {e}", rel_path(dir)))
        })
        .transpose()
}

fn list_benchmarks(
    bench: BenchTarget,
    filter: Option<&str>,
    files: Option<&Path>,
    reference: bool,
) -> Result<(), String> {
    let _lock = lock(Lock::Build, "bench --list")?;
    let log = fresh_log("list")?;
    let binary = build_bench(&crate::root(), None, None, bench, &log)?;
    let side = Side {
        label: "b",
        tree: working_tree()?,
        package: crate::root(),
        binary,
    };
    let (_, chosen) = select(&side, bench, filter, files, reference)?;
    for name in chosen {
        println!("{name}");
    }
    Ok(())
}

pub fn bench(args: &Args, raw_args: &[String]) -> Result<(), String> {
    validate_args(args)?;
    let filter = filter_regex(args.entry.as_deref(), args.workload.as_deref(), args.bench)?;
    let files = canonicalize_files(args.files.as_deref())?;

    if args.list {
        return list_benchmarks(
            args.bench,
            filter.as_deref(),
            files.as_deref(),
            args.reference,
        );
    }

    let label = format!("bench {}", raw_args.join(" "));
    let _lock = lock(Lock::Measure, label.trim_end())?;
    let name = run_name(args);
    let name = name.as_str();
    let log = fresh_log(name)?;

    // Refuse early, before a build that can take minutes, if the machine is unusable.
    refuse_if_noisy(&conditions(), args.allow_noisy)?;

    let sides = build_sides(args, &log)?;
    let active_side = &sides[sides.len() - 1];
    let (selected, _) = select(
        active_side,
        args.bench,
        filter.as_deref(),
        files.as_deref(),
        args.reference,
    )?;

    let measured = settle(args.allow_noisy)?;
    clear_previous_run(args, name)?;

    let started_at = SystemTime::now();
    let started = local_time();
    let _awake = KeepAwake::start();
    let run_settings = RunSettings {
        filter: &selected,
        bench: args.bench,
        quick: args.quick,
        files: files.as_deref(),
        log: &log,
    };
    if args.against.is_some() {
        alternate(&sides, args.rounds, name, &run_settings)?;
    } else {
        let baseline = if args.reference { REFERENCE } else { name };
        measure(&sides[0], baseline, &run_settings)?;
    }
    let finished = local_time();
    let slept = sleeps_between(&started, &finished);

    let hkey = harness_key(&active_side.tree, args.bench, files.as_deref())?;
    let rkey = args
        .reference
        .then(|| reference_key_from_harness(&hkey, &sides[0].tree))
        .transpose()?;
    let record_path = Record {
        name,
        bench: args.bench,
        args: raw_args,
        started: &started,
        finished: &finished,
        sides: &sides,
        quick: args.quick,
        rounds: args.against.as_ref().map(|_| args.rounds),
        conditions: &measured,
        allow_noisy: args.allow_noisy,
        slept: &slept,
        log: &log,
        harness_key: hkey,
        reference_key: rkey,
    }
    .write()?;
    println!("recorded: {}", rel_path(&record_path));

    check_slept(&slept)?;
    let sets = result_sets(args, name);
    println!();
    table(&sets, Some(started_at), false, false)
}

/// Removes what an earlier run saved under `name`. The provenance record goes first, so
/// a failed or interrupted run cannot leave an old record beside partial results.
fn clear_previous_run(args: &Args, name: &str) -> Result<(), String> {
    drop(std::fs::remove_file(
        xtask_dir("runs")?.join(format!("{name}.txt")),
    ));
    let stale = if args.reference {
        Baselines::Reference(args.bench)
    } else {
        Baselines::Named(name)
    };
    for dir in baseline_dirs(&crate::root().join("target/criterion"), stale) {
        drop(std::fs::remove_dir_all(dir));
    }
    Ok(())
}

fn check_slept(slept: &[String]) -> Result<(), String> {
    if !slept.is_empty() {
        return Err(format!(
            "the machine slept during the run ({}); its results are not valid. \
             Keep the lid open and the machine on AC power, and run it again.",
            slept.join(", ")
        ));
    }
    Ok(())
}

fn fresh_log(name: &str) -> Result<PathBuf, String> {
    let log = xtask_dir("bench")?.join(format!("{name}.log"));
    // A stale log from an earlier run with this name would be appended to.
    drop(std::fs::remove_file(&log));
    Ok(log)
}

fn build_sides(args: &Args, log: &Path) -> Result<Vec<Side>, String> {
    let working = working_tree()?;
    let against_tree = if let Some(reference) = args.against.as_deref() {
        let commit = resolve(reference)?;
        let tree = Tree {
            hash: run_git(&["rev-parse", &format!("{commit}^{{tree}}")])?,
            commit,
            dirty: false,
        };
        check_same_harness(&tree, &working, args.bench)?;
        if tree.hash != working.hash && same_codegen_inputs(&tree, &working) {
            println!(
                "warning: side a and side b have different trees, but `src/`, `Cargo.toml`, and \
                 `Cargo.lock` are identical (no codegen change expected in `{}`)",
                args.bench
            );
        }
        Some(tree)
    } else {
        None
    };

    println!("building benchmarks (log: {})", rel_path(log));
    let side_b = Side {
        label: "b",
        tree: working,
        package: crate::root(),
        binary: build_bench(&crate::root(), None, None, args.bench, log)?,
    };
    let Some(tree_a) = against_tree else {
        return Ok(vec![side_b]);
    };

    let package_a = export(&tree_a.commit)?;
    // Build side `a` in its own target directory so Cargo cannot collide with side `b`'s
    // executable path or reuse side `b`'s artifact.
    let target_a = xtask_dir("against-target")?;
    let binary_a = build_bench(&package_a, Some(&target_a), None, args.bench, log)?;
    if binary_a == side_b.binary {
        return Err(format!(
            "internal error: both --against sides resolved to the same binary ({})",
            rel_path(&binary_a)
        ));
    }
    let side_a = Side {
        label: "a",
        tree: tree_a,
        package: package_a,
        binary: binary_a,
    };
    Ok(vec![side_a, side_b])
}

/// True when `src/`, `Cargo.toml`, and `Cargo.lock` have identical git tree/blob hashes
/// between `a` and `b` (for example, when only docs, tests, or `xtask/` changed).
fn same_codegen_inputs(a: &Tree, b: &Tree) -> bool {
    ["src", "Cargo.toml", "Cargo.lock"].iter().all(|path| {
        let rev_a = run_git(&["rev-parse", &format!("{}:{path}", a.hash)]).ok();
        let rev_b = run_git(&["rev-parse", &format!("{}:{path}", b.hash)]).ok();
        rev_a.is_some() && rev_a == rev_b
    })
}

/// Required git tree paths that define the benchmark harness and locked corpus generator.
fn harness_required_paths(bench: BenchTarget) -> [String; 3] {
    [
        "Cargo.lock".to_owned(),
        "bench-support".to_owned(),
        format!("benches/{bench}.rs"),
    ]
}

/// Refuses `--against` when the two sides differ in the benchmark harness, locked
/// dependencies (`Cargo.lock`), corpus generator, or testdata submodule, because
/// comparing different inputs or workloads is invalid.
fn check_same_harness(a: &Tree, b: &Tree, bench: BenchTarget) -> Result<(), String> {
    let required = harness_required_paths(bench);
    let mut differing = Vec::new();
    for path in required
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(TESTDATA_PATH))
    {
        let rev_a = run_git(&["rev-parse", &format!("{}:{path}", a.hash)]).ok();
        let rev_b = run_git(&["rev-parse", &format!("{}:{path}", b.hash)]).ok();
        if rev_a != rev_b {
            differing.push(path);
        }
    }
    if differing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "--against sides differ in {}; cannot compare results across different harnesses \
             or corpora",
            differing.join(", ")
        ))
    }
}

fn result_sets(args: &Args, name: &str) -> Vec<String> {
    if args.against.is_some() {
        vec![format!("{name}.a"), format!("{name}.b")]
    } else if args.reference {
        vec![REFERENCE.to_owned()]
    } else {
        vec![name.to_owned()]
    }
}

/// Saved Criterion baseline directories, selected by what saved them.
#[derive(Clone, Copy)]
pub enum Baselines<'a> {
    /// `reference/` in every group of one bench target.
    Reference(BenchTarget),
    /// Plain `NAME/` and the paired rounds `NAME.a<N>/` and `NAME.b<N>/`.
    Named(&'a str),
}

/// The existing directories under `criterion` that `selection` names. Saving under a
/// name removes all of them first, so switching between plain and paired `--against`
/// runs under one name cannot leave orphaned baselines; `clean` removes the same set.
pub fn baseline_dirs(criterion: &Path, selection: Baselines<'_>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for (group_name, _, bench_dir) in criterion_bench_dirs(criterion) {
        match selection {
            Baselines::Reference(bench) => {
                let dir = bench_dir.join(REFERENCE);
                if BenchTarget::from_benchmark_name(&group_name) == bench && dir.is_dir() {
                    dirs.push(dir);
                }
            }
            Baselines::Named(name) => {
                for entry in std::fs::read_dir(&bench_dir)
                    .into_iter()
                    .flatten()
                    .flatten()
                {
                    let Ok(entry_name) = entry.file_name().into_string() else {
                        continue;
                    };
                    let is_member = entry_name == name
                        || RoundName::parse(&entry_name)
                            .is_some_and(|round| round.base == name && !round.is_set());
                    let path = entry.path();
                    if is_member && path.is_dir() {
                        dirs.push(path);
                    }
                }
            }
        }
    }
    dirs
}

/// Builds a Criterion regex from comma-separated `--entry` (group) and `--workload`
/// names, matched whole. Under `thrift`, `--entry medium` matches both `medium` and
/// `thrift_medium`.
pub fn filter_regex(
    entries: Option<&str>,
    workloads: Option<&str>,
    bench: BenchTarget,
) -> Result<Option<String>, String> {
    let alternatives = |names: &str| -> Result<String, String> {
        let names: Vec<String> = names
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(escape)
            .collect();
        if names.is_empty() {
            return Err("--entry and --workload need at least one name".into());
        }
        Ok(format!("(?:{})", names.join("|")))
    };
    let entry_prefix = if bench == BenchTarget::Thrift {
        "(?:thrift_)?"
    } else {
        ""
    };
    match (entries, workloads) {
        (Some(entries), Some(workloads)) => Ok(Some(format!(
            "^{entry_prefix}{}/{}$",
            alternatives(entries)?,
            alternatives(workloads)?
        ))),
        (Some(entries), None) => Ok(Some(format!("^{entry_prefix}{}/", alternatives(entries)?))),
        (None, Some(workloads)) => Ok(Some(format!("/{}$", alternatives(workloads)?))),
        (None, None) => Ok(None),
    }
}

/// Settings shared by every bench binary run of one `bench` invocation.
struct RunSettings<'a> {
    /// The criterion filter naming exactly the selected benchmarks.
    filter: &'a str,
    bench: BenchTarget,
    quick: bool,
    files: Option<&'a Path>,
    log: &'a Path,
}

/// One version of the code under measurement: `a` is `--against`, `b` the working tree.
pub struct Side {
    pub label: &'static str,
    pub tree: Tree,
    pub package: PathBuf,
    pub binary: PathBuf,
}

/// Runs both sides in alternating rounds, `ab` then `ba`, so slow drift in the
/// machine falls on both sides alike.
fn alternate(sides: &[Side], rounds: u32, name: &str, run: &RunSettings<'_>) -> Result<(), String> {
    let (a, b) = (&sides[0], &sides[1]);
    for round in 1..=rounds {
        let order = if round % 2 == 1 { [a, b] } else { [b, a] };
        for side in order {
            let saved = format!("{name}.{}{round}", side.label);
            measure(side, &saved, run)?;
        }
    }
    Ok(())
}

/// Runs one bench binary, saving its results under Criterion baseline `baseline`, with
/// output appended to the log.
fn measure(side: &Side, baseline: &str, run: &RunSettings<'_>) -> Result<(), String> {
    let mut command = bench_command(&side.package, &side.binary, run.files);
    command
        .arg("--bench")
        // HTML reports take time after each benchmark and nobody reads them here.
        .arg("--noplot")
        .arg(run.filter)
        .args(["--save-baseline", baseline]);
    if run.quick {
        command.args(["--warm-up-time", "1", "--measurement-time", "3"]);
    }
    let bench_name = run.bench.as_str();
    let started = Instant::now();
    let passed = run_logged(&mut command, run.log)?;
    println!(
        "  {baseline:<20} {bench_name:<10} {:>6.1}s",
        started.elapsed().as_secs_f64()
    );
    if passed {
        Ok(())
    } else {
        let text = std::fs::read_to_string(run.log).unwrap_or_default();
        Err(format!(
            "bench `{bench_name}` failed:\n{}\nfull output: {}",
            tail(&text, 20),
            rel_path(run.log)
        ))
    }
}

/// Sets the Criterion and corpus paths shared by bench and profile commands, and the
/// `--files DIR` whose footers join the corpus.
pub fn bench_env(command: &mut Command, files: Option<&Path>) {
    command
        .env("CRITERION_HOME", crate::root().join("target/criterion"))
        .env(
            "PARKAY_CORPUS_DIR",
            crate::root().join("target/bench-corpus"),
        );
    if let Some(files) = files {
        command.env("PARKAY_BENCH_DIR", files);
    }
}

/// A command running `side`'s bench binary as `cargo bench` would: from its package,
/// with results and the corpus in this checkout's `target/`, whichever side it is.
pub fn bench_command(package: &Path, binary: &Path, files: Option<&Path>) -> Command {
    let mut command = Command::new(binary);
    command.current_dir(package);
    bench_env(&mut command, files);
    command
}

/// The benchmarks in `binary` that the Criterion regex `filter` matches (all of them
/// without one).
pub fn benchmark_names(
    package: &Path,
    binary: &Path,
    files: Option<&Path>,
    filter: Option<&str>,
) -> Result<Vec<String>, String> {
    let mut command = bench_command(package, binary, files);
    command.args(["--bench", "--list"]).args(filter);
    let listing = output_checked(&mut command, "listing the benchmarks")?;
    Ok(listing
        .lines()
        .filter_map(|line| line.strip_suffix(": benchmark"))
        .map(str::to_owned)
        .collect())
}

/// Builds bench target `bench` in `package` and returns its binary. `profile`
/// overrides cargo's `bench` profile. Only passes `--features internals` for targets
/// that require it (`thrift`), so `metadata` compiles against parkay's public API.
pub fn build_bench(
    package: &Path,
    target_dir: Option<&Path>,
    profile: Option<&str>,
    bench: BenchTarget,
    log: &Path,
) -> Result<PathBuf, String> {
    let bench_name = bench.as_str();
    let mut command = cargo();
    command
        .current_dir(package)
        .args(["bench", "--locked", "--no-run", "--bench", bench_name]);
    if let Some(profile) = profile {
        command.args(["--profile", profile]);
    }
    if bench.needs_internals() {
        command.args(["--features", "internals"]);
    }
    command.arg("--message-format=json-render-diagnostics");
    if let Some(target_dir) = target_dir {
        command.env("CARGO_TARGET_DIR", target_dir);
    }
    let out = command
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("failed to run cargo: {e}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    append(log, &stderr)?;
    if !out.status.success() {
        return Err(format!(
            "building the benchmarks in {} failed:\n{}\nfull output: {}",
            rel_path(package),
            tail(&stderr, 20),
            rel_path(log)
        ));
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(bench_artifact)
        .find_map(|(name, binary)| (name == bench_name).then_some(binary))
        .ok_or_else(|| format!("cargo built no binary for bench `{bench_name}`"))
}

/// What a benchmark is for, from its name's last part.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// parkay's code: what runs by default.
    Parkay,
    /// Another reader doing the same work (`arrow_*`): saved once with `--reference`.
    Reference,
    /// A hardware floor (`memcpy`): context, saved with the reference.
    Floor,
    /// `scan`: a compute-bound floor that runs in every run as the control for drift.
    Control,
}

impl Role {
    pub fn of(name: &str) -> Self {
        let workload = name.rsplit('/').next().unwrap_or(name);
        if workload.starts_with("arrow_") {
            Self::Reference
        } else if workload == CONTROL {
            Self::Control
        } else if workload == "memcpy" {
            Self::Floor
        } else {
            Self::Parkay
        }
    }
}

/// The criterion filter naming exactly the benchmarks to run (and the chosen list):
/// those matching `filter` in the roles the run is for, plus the control for every
/// selected group.
fn select(
    side: &Side,
    bench: BenchTarget,
    filter: Option<&str>,
    files: Option<&Path>,
    reference: bool,
) -> Result<(String, Vec<String>), String> {
    let list = |pattern| benchmark_names(&side.package, &side.binary, files, pattern);
    let all = list(filter)?;
    let mut chosen: Vec<&str> = all
        .iter()
        .map(String::as_str)
        .filter(|name| match Role::of(name) {
            Role::Parkay => !reference,
            Role::Reference | Role::Floor => reference,
            Role::Control => true,
        })
        .collect();
    // Every group measured gets its control, even when the filter names workloads only.
    let everything = list(None)?;
    for name in &everything {
        let group = name.rsplit_once('/').map_or("", |(group, _)| group);
        let in_chosen = chosen.iter().any(|c| c.starts_with(&format!("{group}/")));
        if Role::of(name) == Role::Control && in_chosen && !chosen.contains(&name.as_str()) {
            chosen.push(name);
        }
    }
    if chosen.iter().all(|name| Role::of(name) == Role::Control) {
        return Err(format!(
            "no {} benchmark matches {}",
            if reference { "reference" } else { "parkay" },
            filter.map_or_else(|| "the corpus".to_owned(), |f| format!("`{f}`"))
        ));
    }
    print_selection_note(&all, &chosen, bench, reference);
    let names: Vec<String> = chosen.iter().map(|name| escape(name)).collect();
    let chosen_owned: Vec<String> = chosen.into_iter().map(str::to_owned).collect();
    Ok((format!("^(?:{})$", names.join("|")), chosen_owned))
}

fn print_selection_note(all: &[String], chosen: &[&str], bench: BenchTarget, reference: bool) {
    let skipped: Vec<&str> = all
        .iter()
        .map(String::as_str)
        .filter(|name| !chosen.contains(name))
        .collect();
    // A skipped benchmark is covered only if its reference was saved.
    let criterion = crate::root().join("target/criterion");
    let missing = skipped
        .iter()
        .filter(|name| {
            !criterion
                .join(name)
                .join(REFERENCE)
                .join("estimates.json")
                .is_file()
        })
        .count();
    let note = if skipped.is_empty() || reference {
        String::new()
    } else if missing == 0 {
        format!(
            " ({} arrow-rs and floor benchmarks come from the saved reference)",
            skipped.len()
        )
    } else {
        format!(
            " (skipping {} arrow-rs and floor benchmarks; {missing} have no saved reference: \
             run `{}`)",
            skipped.len(),
            bench.reference_command()
        )
    };
    println!("running {} benchmarks{note}", chosen.len());
}

/// `name` with regex metacharacters escaped.
fn escape(name: &str) -> String {
    let mut escaped = String::with_capacity(name.len());
    for c in name.chars() {
        if r"\.+*?()|[]{}^$".contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

type FileEntryFingerprint = (String, u64, Option<(u64, u32)>);

/// Sorted `(filename, len, mtime)` fingerprint for all regular `.parquet` files in `--files DIR`.
fn files_fingerprint(files: Option<&Path>) -> Result<Option<Vec<FileEntryFingerprint>>, String> {
    let Some(dir) = files else {
        return Ok(None);
    };
    let read_dir = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", rel_path(dir)))?;
    let mut entries = Vec::new();
    for entry in read_dir {
        let entry = entry.map_err(|e| format!("{}: {e}", rel_path(dir)))?;
        let path = entry.path();
        if path.is_file() && path.extension().is_some_and(|ext| ext == "parquet") {
            let meta = entry
                .metadata()
                .map_err(|e| format!("{}: {e}", rel_path(&path)))?;
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| (d.as_secs(), d.subsec_nanos()));
            entries.push((
                entry.file_name().to_string_lossy().into_owned(),
                meta.len(),
                modified,
            ));
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(Some(entries))
}

/// Hash of the benchmark harness, locked dependencies (`Cargo.lock`), corpus generator,
/// testdata submodule, and optional `--files` directory for `bench`. Two runs with
/// different harness keys did not measure the same workload or inputs.
pub fn harness_key(
    tree: &Tree,
    bench: BenchTarget,
    files: Option<&Path>,
) -> Result<String, String> {
    let mut parts = vec![bench.as_str().to_owned()];
    for path in harness_required_paths(bench) {
        parts.push(run_git(&["rev-parse", &format!("{}:{path}", tree.hash)])?);
    }
    if let Ok(testdata) = run_git(&["rev-parse", &format!("{}:{TESTDATA_PATH}", tree.hash)]) {
        parts.push(testdata);
    }
    let extra_files = files_fingerprint(files)?;
    let mut hasher = std::hash::DefaultHasher::new();
    parts.as_slice().hash(&mut hasher);
    extra_files.as_deref().hash(&mut hasher);
    Ok(format!("{:016x}", hasher.finish()))
}

/// Derives the arrow-rs reference key from an already-computed [`harness_key`], adding
/// the machine, the toolchain, and `Cargo.toml`.
///
/// Simplification: all of `Cargo.toml` is hashed, though only its bench and release
/// profiles and the `parquet` dependency change the reference build, so a lint or
/// metadata edit also marks the reference stale. Parse those sections out if
/// re-measuring after such edits costs too much.
fn reference_key_from_harness(harness: &str, tree: &Tree) -> Result<String, String> {
    let rustc = output(Command::new("rustc").arg("-vV")).unwrap_or_default();
    let cargo_toml = run_git(&["rev-parse", &format!("{}:Cargo.toml", tree.hash)])?;
    let mut hasher = std::hash::DefaultHasher::new();
    (harness, machine_id(), rustc, cargo_toml).hash(&mut hasher);
    Ok(format!("{:016x}", hasher.finish()))
}

/// What the arrow-rs reference depends on: the harness key (bench, `Cargo.lock`,
/// `bench-support`, bench file, testdata, `--files`), the machine, the toolchain, and
/// `Cargo.toml`. A reference saved under another key measured something else.
pub fn reference_key(
    tree: &Tree,
    bench: BenchTarget,
    files: Option<&Path>,
) -> Result<String, String> {
    let harness = harness_key(tree, bench, files)?;
    reference_key_from_harness(&harness, tree)
}

/// A bench target's name and binary from one line of cargo's JSON messages.
fn bench_artifact(line: &str) -> Option<(String, PathBuf)> {
    let message: serde_json::Value = serde_json::from_str(line).ok()?;
    if message["reason"] != "compiler-artifact" {
        return None;
    }
    let target = &message["target"];
    if !target["kind"]
        .as_array()?
        .iter()
        .any(|kind| kind == "bench")
    {
        return None;
    }
    let name = target["name"].as_str()?.to_owned();
    let executable = message["executable"].as_str()?;
    Some((name, PathBuf::from(executable)))
}

fn append(log: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    std::fs::File::options()
        .create(true)
        .append(true)
        .open(log)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .map_err(|e| format!("{}: {e}", rel_path(log)))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        Args, Baselines, BenchTarget, REFERENCE, Role, RoundName, baseline_dirs, bench_artifact,
        escape, files_fingerprint, filter_regex, validate_args,
    };
    use crate::TempDir;

    fn base_args() -> Args {
        Args {
            bench: BenchTarget::Metadata,
            entry: None,
            workload: None,
            save: None,
            against: None,
            rounds: 6,
            quick: false,
            files: None,
            allow_noisy: false,
            reference: false,
            list: false,
        }
    }

    #[test]
    fn reference_rejects_entry_or_workload_subsets() {
        let mut with_entry = base_args();
        with_entry.reference = true;
        with_entry.entry = Some("medium".into());
        assert!(validate_args(&with_entry).is_err());

        let mut with_workload = base_args();
        with_workload.reference = true;
        with_workload.workload = Some("open".into());
        assert!(validate_args(&with_workload).is_err());

        let mut full_ref = base_args();
        full_ref.reference = true;
        assert_eq!(validate_args(&full_ref).err(), None);
    }

    #[test]
    fn round_name_parses_sets_and_rounds_only() {
        let parse = RoundName::parse;
        assert_eq!(
            parse("v1.2.a12"),
            Some(RoundName {
                base: "v1.2",
                side: 'a',
                round: Some(12)
            })
        );
        assert!(parse("exp.b").is_some_and(RoundName::is_set));
        assert!(parse("exp.a1").is_some_and(|name| !name.is_set()));
        for not_round in [
            "exp",
            ".a",
            "exp.c",
            "exp.alpha",
            "exp.a1b",
            "exp.a99999999999",
        ] {
            assert_eq!(parse(not_round), None, "{not_round}");
        }
    }

    #[test]
    fn baseline_dirs_select_both_run_modes_and_reference_groups() {
        let dir = TempDir::new("bench-clear");
        let meta_bench = dir.join("medium/open");
        let stale_meta_bench = dir.join("removed_group/open");
        let thrift_bench = dir.join("thrift_medium/skip");
        for p in [
            meta_bench.join("exp"),
            meta_bench.join("exp.a1"),
            meta_bench.join("exp.b1"),
            meta_bench.join("other"),
            meta_bench.join(REFERENCE),
            stale_meta_bench.join(REFERENCE),
            thrift_bench.join(REFERENCE),
        ] {
            assert_eq!(
                std::fs::create_dir_all(&p).err().map(|e| e.to_string()),
                None
            );
        }

        let sorted = |mut dirs: Vec<PathBuf>| {
            dirs.sort();
            dirs
        };
        // Saving plain or paired under `exp` selects BOTH plain `exp/` and paired `exp.{a,b}*`.
        assert_eq!(
            sorted(baseline_dirs(&dir, Baselines::Named("exp"))),
            vec![
                meta_bench.join("exp"),
                meta_bench.join("exp.a1"),
                meta_bench.join("exp.b1"),
            ]
        );

        // A metadata reference selects all metadata groups (including stale ones),
        // without touching thrift groups.
        assert_eq!(
            sorted(baseline_dirs(
                &dir,
                Baselines::Reference(BenchTarget::Metadata)
            )),
            vec![meta_bench.join(REFERENCE), stale_meta_bench.join(REFERENCE)]
        );
    }

    #[test]
    fn files_fingerprint_tracks_sorted_parquet_files_and_skips_dirs() {
        let dir = TempDir::new("bench-files");
        assert_eq!(
            std::fs::write(dir.join("b.parquet"), b"1234")
                .err()
                .map(|e| e.to_string()),
            None
        );
        assert_eq!(
            std::fs::write(dir.join("a.parquet"), b"12")
                .err()
                .map(|e| e.to_string()),
            None
        );
        assert_eq!(
            std::fs::create_dir(dir.join("spark_table.parquet"))
                .err()
                .map(|e| e.to_string()),
            None
        );
        assert_eq!(
            std::fs::write(dir.join("ignore.txt"), b"zzz")
                .err()
                .map(|e| e.to_string()),
            None
        );
        let Ok(Some(fp)) = files_fingerprint(Some(&dir)) else {
            panic!("expected fingerprint for temp dir");
        };
        let names: Vec<(&str, u64)> = fp.iter().map(|(n, len, _)| (n.as_str(), *len)).collect();
        assert_eq!(names, vec![("a.parquet", 2), ("b.parquet", 4)]);
        assert_eq!(files_fingerprint(None), Ok(None));
    }

    #[test]
    fn filter_supports_default_and_kernel_entries() {
        assert_eq!(
            filter_regex(
                Some("medium,nested"),
                Some("open_all"),
                BenchTarget::Metadata
            ),
            Ok(Some("^(?:medium|nested)/(?:open_all)$".to_owned()))
        );
        assert_eq!(
            filter_regex(Some("medium,nested"), Some("skip"), BenchTarget::Thrift),
            Ok(Some("^(?:thrift_)?(?:medium|nested)/(?:skip)$".to_owned()))
        );
        assert_eq!(filter_regex(None, None, BenchTarget::Metadata), Ok(None));
    }

    #[test]
    fn role_classifies_scan_as_control_and_memcpy_as_floor() {
        assert!(Role::of("medium/scan") == Role::Control);
        assert!(Role::of("thrift_medium/memcpy") == Role::Floor);
        assert!(Role::of("medium/arrow_open") == Role::Reference);
        assert!(Role::of("medium/open") == Role::Parkay);
    }

    #[test]
    fn bench_artifact_reads_bench_executables_only() {
        let non_artifact = r#"{"reason":"compiler-message","target":{"kind":["bench"],"name":"metadata"},"executable":"/a"}"#;
        assert!(bench_artifact(non_artifact).is_none());

        let artifact = r#"{"reason":"compiler-artifact","target":{"kind":["bench"],"name":"metadata"},"executable":"/tmp/my\\path\/\"q\"\t\u0041\uD83D\uDE00/metadata-123"}"#;
        assert_eq!(
            bench_artifact(artifact),
            Some((
                "metadata".to_owned(),
                PathBuf::from("/tmp/my\\path/\"q\"\tA\u{1F600}/metadata-123")
            ))
        );
    }

    #[test]
    fn escape_quotes_regex_metacharacters() {
        assert_eq!(escape("a.b+c(1)"), r"a\.b\+c\(1\)");
    }
}
