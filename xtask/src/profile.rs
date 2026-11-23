//! `cargo xtask profile`: where a benchmark's time goes, from a sampling profiler.
//!
//! A benchmark gives the top-line time; this splits it by parkay module, without
//! timers in the code under test. `--breakdown` samples one benchmark with macOS
//! `sample`, demangles each sampled function, and groups the samples by module
//! (`parkay::thrift`, `parkay::metadata::template`, allocation, memory copies, the
//! harness). `--save` and `--diff` compare two breakdowns.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::bench::git::working_tree;
use crate::bench::machine::{KeepAwake, conditions};
use crate::bench::{
    BenchTarget, bench_command, bench_env, benchmark_names, build_bench, canonicalize_files,
    filter_regex,
};
use crate::{ChildGuard, Lock, lock, parse_name, rel_path, run, xtask_dir};

/// Categories and functions printed in a breakdown.
const CATEGORIES_SHOWN_MAX: usize = 14;
const FUNCTIONS_SHOWN_MAX: usize = 10;

/// Minimum `--seconds` with `--text`/`--breakdown` (1 s margin before Criterion ends).
const SAMPLE_SECONDS_MIN: u64 = 2;
const PROFILE_SECONDS_MAX: u64 = 3_600;

/// Minimum share delta (percentage points) printed in `profile --diff` (hides `0.0 pp`).
const DIFF_MIN_DELTA_PP: f64 = 0.05;

/// Scratch files each run rewrites in `target/xtask/profile/`, beside the `--save`
/// breakdowns (`NAME.txt`).
const BUILD_LOG: &str = "build.log";
const SAMPLE_REPORT: &str = "sample.txt";
const SAMPLY_PROFILE: &str = "profile.json.gz";
pub(crate) const SCRATCH_FILES: [&str; 3] = [BUILD_LOG, SAMPLE_REPORT, SAMPLY_PROFILE];

/// The `--save` name of `target/xtask/profile/FILENAME`, or `None` for scratch files.
pub(crate) fn saved_profile_name(filename: &str) -> Option<&str> {
    if SCRATCH_FILES.contains(&filename) {
        return None;
    }
    filename
        .strip_suffix(".txt")
        .filter(|name| !name.is_empty())
}

#[derive(clap::Args)]
pub struct Args {
    /// Corpus entry (benchmark group) name to profile.
    #[arg(long, value_name = "NAME")]
    pub entry: String,
    /// Workload name to profile.
    #[arg(long, value_name = "NAME")]
    pub workload: String,
    /// Bench target to profile.
    #[arg(long, value_enum, default_value_t = BenchTarget::Metadata)]
    pub bench: BenchTarget,
    /// Profiling duration in seconds.
    #[arg(
        long,
        default_value_t = 10,
        value_parser = clap::value_parser!(u64).range(1..=PROFILE_SECONDS_MAX)
    )]
    pub seconds: u64,
    /// Add footers and page indexes from every `.parquet` file in DIR.
    #[arg(long, value_name = "DIR")]
    pub files: Option<PathBuf>,
    /// Print macOS `sample`'s top-of-stack summary.
    #[arg(long, conflicts_with = "breakdown")]
    pub text: bool,
    /// Print the share of samples per parkay module and the busiest functions.
    #[arg(long)]
    pub breakdown: bool,
    /// Store the breakdown as NAME (requires --breakdown).
    #[arg(long, value_name = "NAME", value_parser = parse_name, requires = "breakdown")]
    pub save: Option<String>,
    /// Compare the breakdown against saved NAME (requires --breakdown).
    #[arg(long, value_name = "NAME", value_parser = parse_name, requires = "breakdown")]
    pub diff: Option<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ProfileProvenance {
    bench: Option<String>,
    entry: Option<String>,
    workload: Option<String>,
    tree: Option<String>,
    total_samples: Option<u32>,
}

struct Breakdown {
    total: u32,
    has_call_graph_total: bool,
    shares: Vec<(String, f64)>,
    top_functions: Vec<(String, u32)>,
}

pub fn profile(args: &Args, raw_args: &[String]) -> Result<(), String> {
    if (args.text || args.breakdown) && args.seconds < SAMPLE_SECONDS_MIN {
        return Err(format!(
            "--seconds must be at least {SAMPLE_SECONDS_MIN} with --text or --breakdown"
        ));
    }
    let filter = filter_regex(Some(&args.entry), Some(&args.workload), args.bench)?
        .ok_or("--entry and --workload must be non-empty")?;
    let files = canonicalize_files(args.files.as_deref())?;

    let _lock = lock(Lock::Measure, &format!("profile {}", raw_args.join(" ")))?;
    let profile_dir = xtask_dir("profile")?;
    let log = profile_dir.join(BUILD_LOG);
    drop(std::fs::remove_file(&log));
    let root = crate::root();
    let binary = build_bench(&root, None, Some("release-profiling"), args.bench, &log)?;
    let name = only_match(&root, &binary, &filter, files.as_deref())?;
    let seconds_str = args.seconds.to_string();
    let bench_args = ["--bench", "--profile-time", &seconds_str, filter.as_str()];

    let machine_conditions = conditions();
    for problem in &machine_conditions.problems {
        eprintln!("warning: machine hygiene: {problem}");
    }
    let _awake = KeepAwake::start();

    if !args.text && !args.breakdown {
        return run_samply(&root, &binary, &bench_args, files.as_deref(), &profile_dir);
    }
    let report = sample(&root, &binary, &bench_args, files.as_deref(), args.seconds)?;
    if !args.breakdown {
        print!("{}", top_of_stack(&report).unwrap_or(&report));
        return Ok(());
    }
    let samples = parse_top_of_stack(&report);
    if samples.is_empty() {
        return Err("the profile has no samples; try a longer --seconds".into());
    }
    let total_samples = parse_total_samples(&report);
    let breakdown = print_breakdown(&name, &samples, total_samples);
    let provenance = ProfileProvenance {
        bench: Some(args.bench.as_str().to_owned()),
        entry: Some(args.entry.clone()),
        workload: Some(args.workload.clone()),
        tree: working_tree().ok().map(|t| t.hash),
        total_samples: Some(breakdown.total),
    };
    // Diff before saving so `--save foo --diff foo` compares against the prior save.
    if let Some(saved) = args.diff.as_deref() {
        diff(saved, &provenance, &breakdown.shares)?;
    }
    if let Some(saved) = args.save.as_deref() {
        save_breakdown(saved, &provenance, &breakdown.shares)?;
    }
    Ok(())
}

fn run_samply(
    root: &Path,
    binary: &Path,
    bench_args: &[&str],
    files: Option<&Path>,
    profile_dir: &Path,
) -> Result<(), String> {
    let output_path = profile_dir.join(SAMPLY_PROFILE);
    let mut command = Command::new("samply");
    command
        .arg("record")
        .arg("-o")
        .arg(&output_path)
        .arg(binary)
        .args(bench_args)
        .current_dir(root);
    bench_env(&mut command, files);
    run(&mut command)
}

fn format_breakdown_file(provenance: &ProfileProvenance, shares: &[(String, f64)]) -> String {
    let mut text = String::new();
    if let Some(bench) = &provenance.bench {
        let _ = writeln!(text, "# bench={bench}");
    }
    if let Some(entry) = &provenance.entry {
        let _ = writeln!(text, "# entry={entry}");
    }
    if let Some(workload) = &provenance.workload {
        let _ = writeln!(text, "# workload={workload}");
    }
    if let Some(tree) = &provenance.tree {
        let _ = writeln!(text, "# tree={tree}");
    }
    if let Some(total) = provenance.total_samples {
        let _ = writeln!(text, "# total_samples={total}");
    }
    for (category, share) in shares {
        let _ = writeln!(text, "{share:.4}\t{category}");
    }
    text
}

fn parse_breakdown_file(text: &str) -> (ProfileProvenance, BTreeMap<String, f64>) {
    let mut provenance = ProfileProvenance::default();
    let mut shares = BTreeMap::new();
    for line in text.lines() {
        if let Some(header) = line.strip_prefix("# ") {
            if let Some((key, value)) = header.split_once('=') {
                let value = value.trim();
                match key.trim() {
                    "bench" => provenance.bench = Some(value.to_owned()),
                    "entry" => provenance.entry = Some(value.to_owned()),
                    "workload" => provenance.workload = Some(value.to_owned()),
                    "tree" => provenance.tree = Some(value.to_owned()),
                    "total_samples" => provenance.total_samples = value.parse().ok(),
                    _ => {}
                }
            }
            continue;
        }
        if let Some((share, category)) = line.split_once('\t')
            && let Ok(share) = share.parse::<f64>()
        {
            shares.insert(category.to_owned(), share);
        }
    }
    (provenance, shares)
}

fn provenance_mismatch_warning(
    saved_name: &str,
    saved: &ProfileProvenance,
    current: &ProfileProvenance,
) -> Option<String> {
    let mut diffs = Vec::new();
    for (label, old, new) in [
        ("bench", saved.bench.as_deref(), current.bench.as_deref()),
        ("entry", saved.entry.as_deref(), current.entry.as_deref()),
        (
            "workload",
            saved.workload.as_deref(),
            current.workload.as_deref(),
        ),
    ] {
        if let (Some(old), Some(new)) = (old, new)
            && old != new
        {
            diffs.push(format!("{label}: {old} vs {new}"));
        }
    }
    (!diffs.is_empty()).then(|| {
        format!(
            "warning: comparing profiles from different benchmarks (`{saved_name}` vs current: {})",
            diffs.join(", ")
        )
    })
}

fn save_breakdown(
    saved: &str,
    provenance: &ProfileProvenance,
    shares: &[(String, f64)],
) -> Result<(), String> {
    let path = xtask_dir("profile")?.join(format!("{saved}.txt"));
    let text = format_breakdown_file(provenance, shares);
    std::fs::write(&path, text).map_err(|e| format!("{}: {e}", rel_path(&path)))?;
    println!("saved: {}", rel_path(&path));
    Ok(())
}

/// The one benchmark `filter` matches; profiling more than one mixes their profiles.
fn only_match(
    root: &Path,
    binary: &Path,
    filter: &str,
    files: Option<&Path>,
) -> Result<String, String> {
    let matches = benchmark_names(root, binary, files, Some(filter))?;
    match matches.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(format!("no benchmark matches `{filter}`")),
        many => Err(format!(
            "`{filter}` matches {} benchmarks; narrow --entry/--workload to one:\n  {}",
            many.len(),
            many[..many.len().min(10)].join("\n  ")
        )),
    }
}

/// Runs the benchmark for `seconds`, waits for Criterion to announce the profiling
/// phase on stderr (after corpus loading and setup finish), and samples it with
/// macOS `sample`. Returns the report text.
fn sample(
    root: &Path,
    binary: &Path,
    bench_args: &[&str],
    files: Option<&Path>,
    seconds: u64,
) -> Result<String, String> {
    if !cfg!(target_os = "macos") {
        // Simplification: Linux needs `perf record -g` and `perf report --stdio`
        // parsed the same way; add it with the first Linux benchmark machine.
        return Err("--text and --breakdown use macOS `sample`; on Linux use samply".into());
    }
    let out: PathBuf = xtask_dir("profile")?.join(SAMPLE_REPORT);
    let mut command = bench_command(root, binary, files);
    command
        .args(bench_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| format!("failed to start {}: {e}", rel_path(binary)))?;
    let stderr = child.stderr.take().ok_or("benchmark has no stderr")?;
    let guard = ChildGuard::new(child);
    let pid = guard.id().ok_or("spawned benchmark has no pid")?;

    // Criterion prints `Benchmarking <name>: Profiling for <N> s` to stderr once
    // corpus loading and benchmark setup finish.
    let mut reader = BufReader::new(stderr);
    let mut seen_stderr = String::new();
    let mut profiling_started = false;
    let mut line = String::new();
    while reader.read_line(&mut line).unwrap_or(0) > 0 {
        seen_stderr.push_str(&line);
        if line.contains("Profiling for") {
            profiling_started = true;
            break;
        }
        line.clear();
    }
    if !profiling_started {
        let status = guard.wait()?;
        let detail = crate::tail(seen_stderr.trim(), 10);
        return Err(format!(
            "benchmark exited before profiling started ({status}){}",
            if detail.is_empty() {
                String::new()
            } else {
                format!(":\n{detail}")
            }
        ));
    }

    // Leave a 1 s margin so `sample` finishes before Criterion exits.
    let sample_seconds = seconds.saturating_sub(1).max(1);
    let sampled = run(Command::new("sample")
        .arg(pid.to_string())
        .arg(sample_seconds.to_string())
        .arg("-file")
        .arg(&out)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null()));
    // Drain stderr while waiting so the child cannot block on a closed pipe.
    drop(std::io::copy(&mut reader, &mut std::io::sink()));
    let status = guard.wait()?;
    sampled?;
    if !status.success() {
        return Err(format!(
            "benchmark {} exited with {status}",
            rel_path(binary)
        ));
    }
    eprintln!("full report: {}", rel_path(&out));
    std::fs::read_to_string(&out).map_err(|e| format!("{}: {e}", rel_path(&out)))
}

/// Parses the total sample count from the `Call graph:` section of a macOS `sample`
/// report by summing top-level `<count> Thread_...` lines.
fn parse_total_samples(report: &str) -> Option<u32> {
    let start = report.find("Call graph:")?;
    let section = &report[start..];
    let end = section
        .find("Total number in stack")
        .or_else(|| section.find("Sort by top of stack"))
        .or_else(|| section.find("Binary Images:"))
        .unwrap_or(section.len());
    let mut total = 0_u32;
    let mut found = false;
    for line in section[..end].lines().skip(1) {
        let mut words = line.split_whitespace();
        if let (Some(count_str), Some(thread)) = (words.next(), words.next())
            && thread.starts_with("Thread_")
            && let Ok(count) = count_str.parse::<u32>()
        {
            total = total.saturating_add(count);
            found = true;
        }
    }
    found.then_some(total)
}

/// The "Sort by top of stack" section of a `sample` report: self samples per function.
fn top_of_stack(report: &str) -> Option<&str> {
    let start = report.find("Sort by top of stack")?;
    let section = &report[start..];
    let end = section.find("Binary Images:").unwrap_or(section.len());
    Some(&section[..end])
}

/// `(symbol, library, samples)` for each line of the top-of-stack section.
fn parse_top_of_stack(report: &str) -> Vec<(String, String, u32)> {
    let Some(section) = top_of_stack(report) else {
        return Vec::new();
    };
    section
        .lines()
        .skip(1)
        .filter_map(|line| {
            let line = line.trim();
            let (rest, count) = line.rsplit_once(char::is_whitespace)?;
            let count: u32 = count.parse().ok()?;
            let (symbol, library) = rest.trim_end().rsplit_once(" (in ")?;
            let library = library.strip_suffix(')')?;
            Some((symbol.trim().to_owned(), library.to_owned(), count))
        })
        .collect()
}

fn compute_breakdown(samples: &[(String, String, u32)], total_samples: Option<u32>) -> Breakdown {
    let summed: u32 = samples
        .iter()
        .map(|(_, _, count)| *count)
        .fold(0_u32, u32::saturating_add);
    let total = total_samples.map_or(summed, |t| t.max(summed));
    let tail = total.saturating_sub(summed);
    let mut categories: BTreeMap<String, u32> = BTreeMap::new();
    let mut functions: BTreeMap<String, u32> = BTreeMap::new();
    for (symbol, library, count) in samples {
        let name = demangle(symbol);
        let path = name.as_deref().map(module_path);
        *categories
            .entry(category(path.as_deref(), symbol, library))
            .or_default() += count;
        *functions
            .entry(name.unwrap_or_else(|| symbol.clone()))
            .or_default() += count;
    }
    if tail > 0 {
        *categories
            .entry("other (<5 samples)".to_owned())
            .or_default() += tail;
    }
    let share = |count: u32| f64::from(count) / f64::from(total.max(1)) * 100.0;
    let mut shares: Vec<(String, f64)> = categories
        .into_iter()
        .map(|(cat, count)| (cat, share(count)))
        .collect();
    shares.sort_by(|x, y| y.1.total_cmp(&x.1));
    let mut top_functions: Vec<(String, u32)> = functions.into_iter().collect();
    top_functions.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    Breakdown {
        total,
        has_call_graph_total: total_samples.is_some(),
        shares,
        top_functions,
    }
}

/// Prints the share of samples per category and the busiest functions, and returns
/// the computed breakdown.
fn print_breakdown(
    name: &str,
    samples: &[(String, String, u32)],
    total_samples: Option<u32>,
) -> Breakdown {
    let breakdown = compute_breakdown(samples, total_samples);
    let share = |count: u32| f64::from(count) / f64::from(breakdown.total.max(1)) * 100.0;
    println!(
        "breakdown of {name} ({} samples, self time by module):",
        breakdown.total
    );
    for (category, pct) in breakdown.shares.iter().take(CATEGORIES_SHOWN_MAX) {
        println!("  {pct:>5.1}%  {category}");
    }
    println!("busiest functions:");
    for (function, count) in breakdown.top_functions.iter().take(FUNCTIONS_SHOWN_MAX) {
        println!("  {:>5.1}%  {function}", share(*count));
    }
    if breakdown.has_call_graph_total {
        println!(
            "note: time in inlined code counts toward the function it was inlined into; \
             functions under 5 samples are grouped into `other (<5 samples)`"
        );
    } else {
        println!(
            "note: time in inlined code counts toward the function it was inlined into; \
             samples of functions under 5 samples are left out"
        );
    }
    breakdown
}

/// Computes diff rows with `|delta| >= 0.05 pp` and returns `(rows, hidden_count)`.
fn diff_rows(before: &BTreeMap<String, f64>, current: &[(String, f64)]) -> (Vec<String>, usize) {
    let mut rows = Vec::new();
    let mut hidden = 0_usize;
    let mut seen = Vec::new();
    for (category, share) in current {
        let old = before.get(category.as_str()).copied().unwrap_or(0.0);
        let delta = share - old;
        if delta.abs() >= DIFF_MIN_DELTA_PP {
            rows.push(format!(
                "  {old:>5.1}% -> {share:>5.1}%  {delta:>+5.1}  {category}"
            ));
        } else {
            hidden += 1;
        }
        seen.push(category.as_str());
    }
    for (category, &old) in before.iter().filter(|(c, _)| !seen.contains(&c.as_str())) {
        if old.abs() >= DIFF_MIN_DELTA_PP {
            rows.push(format!(
                "  {old:>5.1}% ->   0.0%  {:>+5.1}  {category}",
                -old
            ));
        } else {
            hidden += 1;
        }
    }
    (rows, hidden)
}

fn diff(
    saved: &str,
    current_provenance: &ProfileProvenance,
    current: &[(String, f64)],
) -> Result<(), String> {
    let path = xtask_dir("profile")?.join(format!("{saved}.txt"));
    let text = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "{}: {e} (save one first with --save {saved})",
            rel_path(&path)
        )
    })?;
    let (saved_provenance, before) = parse_breakdown_file(&text);
    if let Some(warning) = provenance_mismatch_warning(saved, &saved_provenance, current_provenance)
    {
        println!("{warning}");
    }
    println!("change against {saved} (share of samples, percentage points):");
    let (rows, hidden) = diff_rows(&before, current);
    if rows.is_empty() {
        println!("  (no category changed by >= 0.1 pp)");
    } else {
        for row in &rows {
            println!("{row}");
        }
        if hidden > 0 {
            let unit = if hidden == 1 {
                "category"
            } else {
                "categories"
            };
            println!("  ({hidden} unchanged {unit} hidden)");
        }
    }
    println!("note: shares, not times; a share can grow because everything else got faster");
    Ok(())
}

/// The module a sampled function belongs to: up to two module levels below `parkay`,
/// or what it is outside parkay.
fn category(segments: Option<&[String]>, symbol: &str, library: &str) -> String {
    if library.contains("libsystem_malloc")
        || symbol.contains("$$malloc")
        || symbol.contains("$$free")
    {
        return "allocator (malloc, free)".into();
    }
    if library.contains("libsystem_platform")
        || symbol.contains("$$memcpy")
        || symbol.contains("$$memmove")
    {
        return "libc memory (memmove, memset, memcmp)".into();
    }
    let Some(segments) = segments else {
        return format!("other ({library})");
    };
    match segments.first().map(String::as_str) {
        Some("parkay") => {
            // The last segment is the function or type; the modules sit between.
            let inner = &segments[1..segments.len().saturating_sub(1).max(1)];
            let modules: Vec<&str> = inner
                .iter()
                .map(String::as_str)
                .take_while(|s| {
                    s.chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                })
                .take(2)
                .collect();
            if modules.is_empty() {
                "parkay".into()
            } else {
                format!("parkay::{}", modules.join("::"))
            }
        }
        Some("alloc") => "alloc (Vec and Box growth)".into(),
        Some("parquet") => "arrow-rs".into(),
        Some("criterion") => "harness (criterion)".into(),
        Some(crate_name @ ("core" | "std")) => crate_name.to_owned(),
        Some(other) => format!("harness ({other})"),
        None => "other".into(),
    }
}

/// The demangled name of a Rust symbol, without its hash, such as
/// `<parkay::thrift::CompactReader>::skip_nested::<parkay::thrift::Unmarked>`. `None`
/// for non-Rust or malformed symbols: `try_demangle` rejects trailing garbage.
fn demangle(symbol: &str) -> Option<String> {
    Some(format!("{:#}", rustc_demangle::try_demangle(symbol).ok()?))
}

/// The segments of the first path in a demangled `name`, which [`category`] reads the
/// module from. For an impl method that is the `Self` type (`<&'a parkay::Foo<T> as
/// Trait>::f` gives `parkay::Foo`); generics, closures, and references end a path.
fn module_path(name: &str) -> Vec<String> {
    name.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == ':'))
        .find(|token| token.contains("::"))
        .unwrap_or_default()
        .split("::")
        .filter(|segment| !segment.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        ProfileProvenance, category, compute_breakdown, demangle, diff_rows, format_breakdown_file,
        module_path, parse_breakdown_file, parse_top_of_stack, parse_total_samples,
        provenance_mismatch_warning,
    };

    #[test]
    fn diff_rows_hides_zero_pp_changes_and_counts_them() {
        let before: BTreeMap<String, f64> = [
            ("parkay::thrift".to_owned(), 50.0),
            ("core".to_owned(), 10.0),
            ("gone".to_owned(), 2.0),
            ("tiny".to_owned(), 0.02),
        ]
        .into_iter()
        .collect();
        let current = vec![
            ("parkay::thrift".to_owned(), 45.0),
            ("core".to_owned(), 10.02),
        ];
        let (rows, hidden) = diff_rows(&before, &current);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].contains("parkay::thrift"));
        assert!(rows[1].contains("gone"));
        assert_eq!(hidden, 2);
    }

    /// Symbols from a real `sample` report of the metadata benchmark.
    #[test]
    fn real_symbols_demangle_to_their_paths() {
        let cases = [
            (
                "__RINvMs3_NtCsjFy0BM16MAW_6parkay6thriftNtB6_13CompactReader11skip_nestedNtB6_8UnmarkedEB8_",
                "<parkay::thrift::CompactReader>::skip_nested::<parkay::thrift::Unmarked>",
                "parkay::thrift",
            ),
            (
                "_RINvMs3_NtCsjFy0BM16MAW_6parkay6thriftNtB6_13CompactReader11skip_nestedNtB6_8UnmarkedEB8_",
                "<parkay::thrift::CompactReader>::skip_nested::<parkay::thrift::Unmarked>",
                "parkay::thrift",
            ),
            (
                "_RNvNtNtCsjFy0BM16MAW_6parkay8metadata6footer6decode",
                "parkay::metadata::footer::decode",
                "parkay::metadata::footer",
            ),
            (
                "_RNvMs0_NtNtCsjFy0BM16MAW_6parkay8metadata8templateNtB5_3Run4pass",
                "<parkay::metadata::template::Run>::pass",
                "parkay::metadata::template",
            ),
            (
                "_RNvMs8_NtCs6eGy9mWPo3p_5alloc7raw_vecNtB5_11RawVecInner11finish_growCsjFy0BM16MAW_6parkay",
                "<alloc::raw_vec::RawVecInner>::finish_grow",
                "alloc (Vec and Box growth)",
            ),
        ];
        for (symbol, name, module) in cases {
            let Some(demangled) = demangle(symbol) else {
                panic!("symbol should demangle: {symbol}");
            };
            assert_eq!(demangled, name);
            let path = module_path(&demangled);
            assert_eq!(category(Some(&path), symbol, "metadata-x"), module);
        }
    }

    #[test]
    fn module_path_is_the_self_type_or_first_path() {
        let cases = [
            (
                "<parkay::thrift::CompactReader<'_> as core::fmt::Debug>::fmt",
                "parkay::thrift::CompactReader",
            ),
            (
                "<&'a mut parkay::Foo as core::fmt::Debug>::fmt",
                "parkay::Foo",
            ),
            (
                "<*const parkay::Bar as core::marker::Send>::send",
                "parkay::Bar",
            ),
            (
                "<[u8] as core::slice::cmp::SlicePartialEq<u8>>::equal",
                "core::slice::cmp::SlicePartialEq",
            ),
            (
                "<&'a [u8] as parkay::thrift::Read>::read",
                "parkay::thrift::Read",
            ),
            ("parkay::metadata::open::<u8>", "parkay::metadata::open"),
            ("main", ""),
        ];
        for (name, path) in cases {
            assert_eq!(module_path(name).join("::"), path, "{name}");
        }
    }

    #[test]
    fn closure_inside_free_function_stays_in_enclosing_module() {
        for closure in ["{closure}", "{{closure}}", "{closure#0}"] {
            let path = module_path(&format!("parkay::metadata::open::{closure}"));
            assert_eq!(
                category(Some(&path), "_R", "metadata-x"),
                "parkay::metadata"
            );
        }
    }

    #[test]
    fn non_rust_symbols_are_rejected() {
        assert!(demangle("_xzm_free").is_none());
        assert!(demangle("_ReadFile").is_none());
    }

    #[test]
    fn parse_top_of_stack_and_call_graph_total_samples() {
        let report = "\
Call graph:
    200 Thread_12345   DispatchQueue_1: com.apple.main-thread  (serial)
    + 200 start  (in dyld) + 2360

Total number in stack (recursive counted multiple, when >=5):

Sort by top of stack, same collapsed (when >= 5):
        _RNvC6parkay4open  (in metadata-abc123)        120
        _platform_memmove  (in libsystem_platform.dylib)        35

Binary Images:
    0x1000 foo
";
        assert_eq!(parse_total_samples(report), Some(200));
        let parsed = parse_top_of_stack(report);
        assert_eq!(
            parsed,
            vec![
                (
                    "_RNvC6parkay4open".to_owned(),
                    "metadata-abc123".to_owned(),
                    120
                ),
                (
                    "_platform_memmove".to_owned(),
                    "libsystem_platform.dylib".to_owned(),
                    35
                ),
            ]
        );

        let breakdown = compute_breakdown(&parsed, parse_total_samples(report));
        assert_eq!(breakdown.total, 200);
        let by_cat: BTreeMap<&str, f64> = breakdown
            .shares
            .iter()
            .map(|(cat, pct)| (cat.as_str(), *pct))
            .collect();
        assert!((by_cat["parkay"] - 60.0).abs() < 1e-9);
        assert!((by_cat["libc memory (memmove, memset, memcmp)"] - 17.5).abs() < 1e-9);
        assert!((by_cat["other (<5 samples)"] - 22.5).abs() < 1e-9);

        let fallback = compute_breakdown(&parsed, None);
        assert_eq!(fallback.total, 155);
        assert!(
            !fallback
                .shares
                .iter()
                .any(|(cat, _)| cat == "other (<5 samples)")
        );
    }

    #[test]
    fn breakdown_save_load_provenance_and_mismatch_warning() {
        let saved_prov = ProfileProvenance {
            bench: Some("metadata".into()),
            entry: Some("medium".into()),
            workload: Some("open".into()),
            tree: Some("abc123".into()),
            total_samples: Some(1000),
        };
        let shares = vec![
            ("parkay::thrift".to_owned(), 62.5),
            ("other (<5 samples)".to_owned(), 4.5),
        ];
        let text = format_breakdown_file(&saved_prov, &shares);
        let (parsed_prov, parsed_shares) = parse_breakdown_file(&text);
        assert_eq!(parsed_prov, saved_prov);
        assert!((parsed_shares["parkay::thrift"] - 62.5).abs() < 1e-9);
        assert!((parsed_shares["other (<5 samples)"] - 4.5).abs() < 1e-9);

        assert!(provenance_mismatch_warning("before", &parsed_prov, &saved_prov).is_none());

        let other_prov = ProfileProvenance {
            bench: Some("metadata".into()),
            entry: Some("nested".into()),
            workload: Some("open_all".into()),
            tree: Some("def456".into()),
            total_samples: Some(900),
        };
        let Some(warning) = provenance_mismatch_warning("before", &parsed_prov, &other_prov) else {
            panic!("expected provenance mismatch warning");
        };
        assert!(warning.contains("entry: medium vs nested"));
        assert!(warning.contains("workload: open vs open_all"));

        let legacy = "55.0000\tparkay::thrift\n10.0000\tcore\n";
        let (legacy_prov, legacy_shares) = parse_breakdown_file(legacy);
        assert_eq!(legacy_prov, ProfileProvenance::default());
        assert!((legacy_shares["parkay::thrift"] - 55.0).abs() < 1e-9);
        assert!(provenance_mismatch_warning("old", &legacy_prov, &other_prov).is_none());
    }
}
