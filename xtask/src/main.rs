//! Project tasks for parkay: benchmarking, assembly inspection, and profiling.
//!
//! Run `cargo xtask help` for usage. Commands shell out to cargo, `cargo-show-asm`,
//! `samply`, and (on macOS) `sample`, so their output stays the tools' own.

use std::env;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::Duration;

const USAGE: &str = "\
usage: cargo xtask <command> [options]

commands:
  bench [FILTER] [--quick] [--save NAME] [--baseline NAME] [--files DIR]
      Run the `thrift` criterion benchmarks. FILTER is a criterion regex such as
      `wide_1000x100/skip`. --quick shortens warm-up and measurement (1 s / 3 s).
      --save stores results as baseline NAME; --baseline compares against NAME.
      --files adds the footer and page index of every .parquet file in DIR.
      Runs one benchmark process at a time; do not run two at once.

  fuzz TARGET [--seconds N]
      Run a cargo-fuzz target (see fuzz/Cargo.toml) for N seconds (default 60) on
      the nightly toolchain, with a 1 s per-input timeout to catch runaway inputs.
      Seeds fuzz/corpus/TARGET from real footers first if it is empty.

  report BASELINE [BASELINE...]
      Print a table of saved criterion baselines: median time and throughput per
      benchmark, and each later baseline's change against the first.

  asm SYMBOL [--rust]
      Show the assembly (or, with --rust, interleaved source) of a parkay function,
      built with the release-profiling profile. SYMBOL is a cargo-show-asm path,
      such as `<parkay::thrift::CompactReader>::skip_nested`. Run with no
      SYMBOL to list candidates.

  profile FILTER [--seconds N] [--text]
      Profile one benchmark (FILTER should match exactly one) for N seconds
      (default 10) with the release-profiling profile. Opens samply's UI, or with
      --text prints macOS `sample`'s call tree and top-of-stack summary.
";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("bench") => bench(&args[1..]),
        Some("fuzz") => fuzz(&args[1..]),
        Some("report") => report(&args[1..]),
        Some("asm") => asm(&args[1..]),
        Some("profile") => profile(&args[1..]),
        Some("help" | "--help" | "-h") | None => {
            print!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(format!("unknown command `{other}`\n\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("xtask: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Parsed `[POSITIONAL] [--flag] [--key VALUE]` options.
struct Options {
    positional: Option<String>,
    flags: Vec<String>,
    values: Vec<(String, String)>,
}

impl Options {
    fn parse(args: &[String], flags: &[&str], keys: &[&str]) -> Result<Self, String> {
        let mut options = Self {
            positional: None,
            flags: Vec::new(),
            values: Vec::new(),
        };
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            if flags.contains(&arg.as_str()) {
                options.flags.push(arg.clone());
            } else if keys.contains(&arg.as_str()) {
                let value = iter.next().ok_or_else(|| format!("{arg} needs a value"))?;
                options.values.push((arg.clone(), value.clone()));
            } else if arg.starts_with("--") {
                return Err(format!("unknown option `{arg}`\n\n{USAGE}"));
            } else if options.positional.replace(arg.clone()).is_some() {
                return Err(format!("unexpected argument `{arg}`"));
            }
        }
        Ok(options)
    }

    fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f == name)
    }

    fn value(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives one level below the workspace root")
        .to_path_buf()
}

fn cargo() -> Command {
    let mut command = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    command.current_dir(root());
    command
}

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

fn bench(args: &[String]) -> Result<(), String> {
    let options = Options::parse(args, &["--quick"], &["--save", "--baseline", "--files"])?;
    let mut command = cargo();
    if let Some(dir) = options.value("--files") {
        let dir = std::fs::canonicalize(dir).map_err(|e| format!("--files {dir}: {e}"))?;
        command.env("PARKAY_BENCH_DIR", dir);
    }
    command.args([
        "bench",
        "--bench",
        "thrift",
        "--features",
        "internals",
        "--",
    ]);
    if let Some(filter) = &options.positional {
        command.arg(filter);
    }
    if options.flag("--quick") {
        command.args(["--warm-up-time", "1", "--measurement-time", "3"]);
    }
    if let Some(name) = options.value("--save") {
        command.args(["--save-baseline", name]);
    }
    if let Some(name) = options.value("--baseline") {
        command.args(["--baseline", name]);
    }
    run(&mut command)
}

fn fuzz(args: &[String]) -> Result<(), String> {
    let options = Options::parse(args, &[], &["--seconds"])?;
    let target = options.positional.clone().ok_or("fuzz needs a TARGET")?;
    let seconds = options.value("--seconds").unwrap_or("60");
    let corpus = root().join("fuzz/corpus").join(&target);
    let empty = std::fs::read_dir(&corpus).map_or(true, |mut entries| entries.next().is_none());
    if empty {
        run(cargo().args([
            "run",
            "--release",
            "--quiet",
            "--example",
            "fuzz_corpus",
            "--features",
            "internals",
            "--",
            target.as_str(),
        ]))?;
    }
    run(Command::new("cargo")
        .current_dir(root().join("fuzz"))
        // The nightly toolchain comes from `+nightly`; drop the pinned one cargo set.
        .env_remove("RUSTUP_TOOLCHAIN")
        .env("RUSTC_WRAPPER", "")
        .args(["+nightly", "fuzz", "run", target.as_str(), "--"])
        .arg(format!("-max_total_time={seconds}"))
        .args(["-timeout=1", "-print_final_stats=1"]))
}

/// One benchmark's saved median in nanoseconds and its throughput in bytes.
fn saved(dir: &std::path::Path, baseline: &str) -> Option<(f64, Option<f64>)> {
    let estimates = std::fs::read_to_string(dir.join(baseline).join("estimates.json")).ok()?;
    let median = json_number(
        &estimates[estimates.find("\"median\"")?..],
        "point_estimate",
    )?;
    let bytes = std::fs::read_to_string(dir.join(baseline).join("benchmark.json"))
        .ok()
        .and_then(|b| json_number(&b, "Bytes"));
    Some((median, bytes))
}

/// The first number after `"key":` in `json`. Criterion's files are flat enough that
/// this avoids a JSON dependency.
fn json_number(json: &str, key: &str) -> Option<f64> {
    let pattern = format!("\"{key}\":");
    let start = json.find(&pattern)? + pattern.len();
    let rest = json[start..].trim_start();
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | '-' | 'e' | 'E' | '+')))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn format_time(ns: f64) -> String {
    match ns {
        ns if ns >= 1e6 => format!("{:.3} ms", ns / 1e6),
        ns if ns >= 1e3 => format!("{:.2} µs", ns / 1e3),
        ns => format!("{ns:.1} ns"),
    }
}

fn format_rate(bytes: f64, ns: f64) -> String {
    let per_second = bytes / (ns / 1e9);
    if per_second >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} GiB/s", per_second / (1024.0 * 1024.0 * 1024.0))
    } else {
        format!("{:.0} MiB/s", per_second / (1024.0 * 1024.0))
    }
}

fn report(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err("report needs at least one BASELINE".into());
    }
    let criterion = root().join("target/criterion");
    let mut rows = Vec::new();
    let groups = std::fs::read_dir(&criterion).map_err(|e| {
        format!(
            "{}: {e} (run `cargo xtask bench --save NAME` first)",
            criterion.display()
        )
    })?;
    for group in groups.flatten().filter(|g| g.path().is_dir()) {
        for bench in std::fs::read_dir(group.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            let dir = bench.path();
            if !dir.join(&args[0]).is_dir() {
                continue;
            }
            let name = format!(
                "{}/{}",
                group.file_name().to_string_lossy(),
                bench.file_name().to_string_lossy()
            );
            rows.push((name, dir));
        }
    }
    rows.sort();
    let mut header = format!("{:<34}", "benchmark");
    for (i, baseline) in args.iter().enumerate() {
        header.push_str(&format!(" | {baseline:>23}"));
        if i > 0 {
            header.push_str(&format!(" {:>7}", "change"));
        }
    }
    println!("{header}");
    for (name, dir) in rows {
        let first = saved(&dir, &args[0]);
        let mut line = format!("{name:<34}");
        for (i, baseline) in args.iter().enumerate() {
            match saved(&dir, baseline) {
                Some((ns, bytes)) => {
                    let rate = bytes.map(|b| format_rate(b, ns)).unwrap_or_default();
                    line.push_str(&format!(" | {:>11} {:>11}", format_time(ns), rate));
                    if i > 0 {
                        let change =
                            first.map(|(base, _)| format!("{:+.1}%", (ns / base - 1.0) * 100.0));
                        line.push_str(&format!(" {:>7}", change.unwrap_or_default()));
                    }
                }
                None => line.push_str(&format!(" | {:>23}", "-")),
            }
        }
        println!("{line}");
    }
    Ok(())
}

fn asm(args: &[String]) -> Result<(), String> {
    let options = Options::parse(args, &["--rust"], &[])?;
    let mut command = cargo();
    command.args([
        "asm",
        "--lib",
        "-p",
        "parkay",
        "--features",
        "internals",
        "--profile",
        "release-profiling",
    ]);
    if options.flag("--rust") {
        command.arg("--rust");
    }
    if let Some(symbol) = &options.positional {
        command.arg(symbol);
    }
    run(&mut command)
}

fn profile(args: &[String]) -> Result<(), String> {
    let options = Options::parse(args, &["--text"], &["--seconds"])?;
    let filter = options
        .positional
        .clone()
        .ok_or("profile needs a benchmark FILTER")?;
    let seconds = options.value("--seconds").unwrap_or("10");
    let bench_binary = build_bench_for_profiling()?;
    let bench_args = ["--bench", "--profile-time", seconds, filter.as_str()];

    if !options.flag("--text") {
        return run(Command::new("samply")
            .arg("record")
            .arg(&bench_binary)
            .args(bench_args)
            .current_dir(root()));
    }

    // Text mode: run the benchmark and attach macOS `sample` to it.
    let mut child = Command::new(&bench_binary)
        .args(bench_args)
        .current_dir(root())
        .stdout(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to start {}: {e}", bench_binary.display()))?;
    // Let fixture setup finish before sampling.
    thread::sleep(Duration::from_secs(3));
    let sample_seconds = seconds
        .parse::<u64>()
        .map_err(|e| format!("--seconds: {e}"))?
        .saturating_sub(4)
        .max(1);
    let out = root().join("target/profile-sample.txt");
    let sampled = run(Command::new("sample")
        .arg(child.id().to_string())
        .arg(sample_seconds.to_string())
        .arg("-file")
        .arg(&out));
    let _ = child.wait();
    sampled?;
    eprintln!("full report: {}", out.display());
    let report = std::fs::read_to_string(&out).map_err(|e| e.to_string())?;
    if let Some(start) = report.find("Sort by top of stack") {
        let summary = &report[start..];
        let end = summary.find("Binary Images:").unwrap_or(summary.len());
        print!("{}", &summary[..end]);
    } else {
        print!("{report}");
    }
    Ok(())
}

/// Builds the `thrift` bench with the release-profiling profile (line tables kept) and
/// returns the path of its binary.
fn build_bench_for_profiling() -> Result<PathBuf, String> {
    let output = cargo()
        .args([
            "bench",
            "--bench",
            "thrift",
            "--features",
            "internals",
            "--profile",
            "release-profiling",
            "--no-run",
            "--message-format=json-render-diagnostics",
        ])
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("failed to run cargo: {e}"))?;
    if !output.status.success() {
        return Err("building the benchmark failed".into());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter(|line| {
            line.contains("\"kind\":[\"bench\"]") && line.contains("\"name\":\"thrift\"")
        })
        .find_map(|line| {
            let start = line.find("\"executable\":\"")? + "\"executable\":\"".len();
            let end = start + line[start..].find('"')?;
            Some(PathBuf::from(&line[start..end]))
        })
        .ok_or_else(|| "could not find the benchmark binary in cargo's output".into())
}
