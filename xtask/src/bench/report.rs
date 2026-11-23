//! Benchmark run records, provenance, and comparison tables (`cargo xtask report`).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::{rel_path, xtask_dir};

use super::git::{Tree, short, working_tree};
use super::machine::{Conditions, machine};
use super::{
    BenchTarget, CONTROL, REFERENCE, Role, RoundName, Side, criterion_bench_dirs, reference_key,
};

/// Minimum paired rounds for a two-sided binomial sign test to reach `p <= 0.05`
/// (`2 / 2^6 = 0.03125`).
const PAIRED_ROUNDS_MIN: usize = 6;

#[derive(clap::Args)]
pub struct ReportArgs {
    /// Saved baseline names to tabulate (e.g. `base after`, `exp.a exp.b`, or paired `exp`).
    #[arg(value_name = "BASELINE", required = true)]
    pub baselines: Vec<String>,
    /// Print only rows whose verdict is not noise (`~`).
    #[arg(long)]
    pub changed: bool,
    /// Format the comparison table as GitHub-Flavored Markdown.
    #[arg(long)]
    pub markdown: bool,
}

/// Everything needed to trust or distrust a saved run, written beside it.
pub struct Record<'a> {
    pub name: &'a str,
    pub bench: BenchTarget,
    pub args: &'a [String],
    pub started: &'a str,
    pub finished: &'a str,
    pub sides: &'a [Side],
    pub quick: bool,
    pub rounds: Option<u32>,
    pub conditions: &'a Conditions,
    pub allow_noisy: bool,
    pub slept: &'a [String],
    pub log: &'a Path,
    pub harness_key: String,
    pub reference_key: Option<String>,
}

impl Record<'_> {
    pub fn write(&self) -> Result<PathBuf, String> {
        let mut text = String::new();
        let _ = writeln!(text, "name: {}", self.name);
        let _ = writeln!(text, "bench: {}", self.bench);
        if self.args.is_empty() {
            let _ = writeln!(text, "command: cargo xtask bench");
        } else {
            let _ = writeln!(text, "command: cargo xtask bench {}", self.args.join(" "));
        }
        let _ = writeln!(text, "started: {}", self.started);
        let _ = writeln!(text, "finished: {}", self.finished);
        for side in self.sides {
            // A plain run has one side and unprefixed keys.
            let prefix = if self.sides.len() > 1 {
                format!("{}.", side.label)
            } else {
                String::new()
            };
            let _ = writeln!(text, "{prefix}commit: {}", side.tree.commit);
            let _ = writeln!(text, "{prefix}tree: {}", side.tree.hash);
            let _ = writeln!(text, "{prefix}dirty: {}", side.tree.dirty);
        }
        if let Some(rounds) = self.rounds {
            let _ = writeln!(text, "rounds: {rounds}");
        }
        let _ = writeln!(text, "quick: {}", self.quick);
        let _ = writeln!(text, "machine_id: {}", machine());
        let _ = writeln!(text, "harness_key: {}", self.harness_key);
        if let Some(key) = &self.reference_key {
            let _ = writeln!(text, "reference_key: {key}");
        }
        for note in &self.conditions.notes {
            let _ = writeln!(text, "machine: {note}");
        }
        let problems = if self.conditions.problems.is_empty() {
            "none".to_owned()
        } else {
            self.conditions.problems.join("; ")
        };
        let slept = if self.slept.is_empty() {
            "no".to_owned()
        } else {
            self.slept.join(", ")
        };
        let _ = writeln!(text, "problems: {problems}");
        let _ = writeln!(text, "allow_noisy: {}", self.allow_noisy);
        let _ = writeln!(text, "slept: {slept}");
        let _ = writeln!(text, "log: {}", rel_path(self.log));
        let path = xtask_dir("runs")?.join(format!("{}.txt", self.name));
        std::fs::write(&path, text).map_err(|e| format!("{}: {e}", rel_path(&path)))?;
        Ok(path)
    }
}

/// The recorded facts about a set, from `runs/NAME.txt`. For a round set `NAME.a`, the
/// keys of side `a`.
fn provenance(set: &str) -> Option<BTreeMap<String, String>> {
    let runs = crate::root().join("target/xtask/runs");
    let (file, side) = match RoundName::parse(set) {
        Some(name) if name.is_set() && runs.join(format!("{}.txt", name.base)).exists() => {
            (name.base, Some(name.side))
        }
        _ => (set, None),
    };
    let text = std::fs::read_to_string(runs.join(format!("{file}.txt"))).ok()?;
    Some(parse_record(&text, side))
}

/// Parses `key: value` lines from a run provenance record, optionally selecting one
/// side (`a` or `b`) of a paired `--against` record.
fn parse_record(text: &str, side: Option<char>) -> BTreeMap<String, String> {
    let mut keys = BTreeMap::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(": ") else {
            continue;
        };
        let key = match (side, key.split_once('.')) {
            (Some(side), Some((prefix, rest))) if prefix.chars().eq([side]) => rest,
            (Some(_), Some(_)) => continue,
            _ => key,
        };
        keys.entry(key.to_owned())
            .and_modify(|v: &mut String| {
                v.push_str("; ");
                v.push_str(value);
            })
            .or_insert_with(|| value.to_owned());
    }
    keys
}

/// Expands `cargo xtask report <NAME>` into `[<NAME>.a, <NAME>.b]` when `<NAME>` is not
/// `reference`, has no plain `<NAME>` baseline with `estimates.json`, and paired
/// `<NAME>.a*` / `<NAME>.b*` round baselines exist under `criterion`.
fn expand_paired_shorthand(baselines: &[String], criterion: &Path) -> Vec<String> {
    let [name] = baselines else {
        return baselines.to_vec();
    };
    if name == REFERENCE || RoundName::parse(name).is_some_and(RoundName::is_set) {
        return baselines.to_vec();
    }
    let a_set = format!("{name}.a");
    let b_set = format!("{name}.b");
    let mut has_plain = false;
    let mut has_a = false;
    let mut has_b = false;
    for (_, _, dir) in criterion_bench_dirs(criterion) {
        if dir.join(name).join("estimates.json").is_file() {
            has_plain = true;
        }
        if !members(&dir, &a_set).is_empty() {
            has_a = true;
        }
        if !members(&dir, &b_set).is_empty() {
            has_b = true;
        }
    }
    if !has_plain && has_a && has_b {
        vec![a_set, b_set]
    } else {
        baselines.to_vec()
    }
}

pub fn report(args: &ReportArgs) -> Result<(), String> {
    let criterion = crate::root().join("target/criterion");
    let sets = expand_paired_shorthand(&args.baselines, &criterion);
    if args.changed && sets.len() < 2 {
        return Err("--changed compares baselines; pass at least two BASELINEs".into());
    }
    table(&sets, None, args.changed, args.markdown)
}

/// One saved criterion estimate: the median and its 95% confidence interval, in
/// nanoseconds, and the bytes processed per iteration.
#[derive(Clone, Copy)]
struct Estimate {
    median: f64,
    lower: f64,
    upper: f64,
    bytes: Option<f64>,
}

fn saved(dir: &Path, baseline: &str) -> Option<Estimate> {
    let estimates = std::fs::read_to_string(dir.join(baseline).join("estimates.json")).ok()?;
    let benchmark = std::fs::read_to_string(dir.join(baseline).join("benchmark.json")).ok();
    parse_estimate(&estimates, benchmark.as_deref())
}

/// The median from Criterion's `estimates.json`, and the bytes per iteration from its
/// `benchmark.json` when the benchmark declares a throughput.
fn parse_estimate(estimates: &str, benchmark: Option<&str>) -> Option<Estimate> {
    let estimates: serde_json::Value = serde_json::from_str(estimates).ok()?;
    let median = &estimates["median"];
    let interval = &median["confidence_interval"];
    let bytes = benchmark
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
        .and_then(|benchmark| benchmark["throughput"]["Bytes"].as_f64());
    Some(Estimate {
        median: median["point_estimate"].as_f64()?,
        lower: interval["lower_bound"].as_f64()?,
        upper: interval["upper_bound"].as_f64()?,
        bytes,
    })
}

/// A set's summary for one benchmark: the median of its rounds' medians, its spread
/// (`[lower, upper]`), and the round medians in round-number order for paired tests.
struct Summary {
    median: f64,
    lower: f64,
    upper: f64,
    bytes: Option<f64>,
    /// `(round_number, median_ns)` sorted by round number; empty for non-round sets.
    rounds: Vec<(u32, f64)>,
}

fn summarize(estimates: &[(Option<u32>, Estimate)]) -> Option<Summary> {
    let &(_, first) = estimates.first()?;
    let rounds: Vec<(u32, f64)> = estimates
        .iter()
        .filter_map(|&(round, est)| round.map(|r| (r, est.median)))
        .collect();
    if estimates.len() == 1 {
        return Some(Summary {
            median: first.median,
            lower: first.lower,
            upper: first.upper,
            bytes: first.bytes,
            rounds,
        });
    }
    let mut sorted: Vec<f64> = estimates.iter().map(|(_, e)| e.median).collect();
    sorted.sort_by(f64::total_cmp);
    Some(Summary {
        median: median_sorted(&sorted),
        lower: sorted[0],
        upper: sorted[sorted.len() - 1],
        bytes: first.bytes,
        rounds,
    })
}

fn median_sorted(sorted: &[f64]) -> f64 {
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted[middle]
    } else {
        f64::midpoint(sorted[middle - 1], sorted[middle])
    }
}

/// True when `base` and `other` are the two alternating sides of the same `--against`
/// run (`NAME.a` and `NAME.b`, in either order).
fn is_paired_sets(base: &str, other: &str) -> bool {
    match (RoundName::parse(base), RoundName::parse(other)) {
        (Some(base), Some(other)) => {
            base.is_set() && other.is_set() && base.base == other.base && base.side != other.side
        }
        _ => false,
    }
}

/// Per-round `other / base` ratios for rounds present in both summaries.
fn paired_ratios(base: &Summary, other: &Summary) -> Vec<f64> {
    let mut ratios = Vec::new();
    let mut j = 0;
    for &(round_a, med_a) in &base.rounds {
        while j < other.rounds.len() && other.rounds[j].0 < round_a {
            j += 1;
        }
        if let Some(&(round_b, med_b)) = other.rounds.get(j)
            && round_b == round_a
            && med_a > 0.0
            && med_b > 0.0
        {
            ratios.push(med_b / med_a);
        }
    }
    ratios
}

/// Exact two-sided binomial sign test at `p <= 0.05` (`two_sided * 20 <= 2^n`).
/// `minor` is `n - major`, so exact ties count conservatively against significance.
fn sign_test_significant(n: usize, minor: usize) -> bool {
    if !(PAIRED_ROUNDS_MIN..=120).contains(&n) || minor * 2 >= n {
        return false;
    }
    let total = 1_u128 << n;
    let mut one_sided = 0_u128;
    let mut choose = 1_u128;
    for k in 0..=minor {
        if k > 0 {
            choose = choose * (n - k + 1) as u128 / k as u128;
        }
        one_sided += choose;
    }
    let two_sided = (2 * one_sided).min(total);
    two_sided.saturating_mul(20) <= total
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Faster,
    Slower,
    Noise,
    /// Missing `scan` control, too few paired rounds (`< 6`), or non-positive baseline median.
    Inconclusive,
}

impl Verdict {
    /// For paired `--against` rounds, requires a `scan` control, at least 6 paired rounds
    /// (checked before comparing against drift), a change exceeding the group's `scan`
    /// control drift, and an exact two-sided sign test (`p <= 0.05`).
    /// For unpaired sets, requires a `scan` control, change exceeding drift, and
    /// non-overlapping ranges.
    fn of(
        base: &Summary,
        other: &Summary,
        paired: bool,
        control_drift_percent: Option<f64>,
    ) -> Self {
        let Some(drift) = control_drift_percent else {
            return Self::Inconclusive;
        };
        let Some(change) = change_percent(base, other, paired) else {
            return Self::Inconclusive;
        };
        if paired {
            let ratios = paired_ratios(base, other);
            if ratios.len() < PAIRED_ROUNDS_MIN {
                return Self::Inconclusive;
            }
            if change.abs() <= drift {
                return Self::Noise;
            }
            let faster = ratios.iter().filter(|&&r| r < 1.0).count();
            let slower = ratios.iter().filter(|&&r| r > 1.0).count();
            let major = faster.max(slower);
            let minor = ratios.len() - major;
            if !sign_test_significant(ratios.len(), minor) {
                return Self::Noise;
            }
            if faster > slower && change < 0.0 {
                Self::Faster
            } else if slower > faster && change > 0.0 {
                Self::Slower
            } else {
                Self::Noise
            }
        } else {
            if change.abs() <= drift {
                return Self::Noise;
            }
            if other.upper < base.lower || other.lower > base.upper {
                if other.median < base.median {
                    Self::Faster
                } else {
                    Self::Slower
                }
            } else {
                Self::Noise
            }
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Faster => "faster",
            Self::Slower => "slower",
            Self::Noise => "~",
            Self::Inconclusive => "?",
        }
    }
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
struct VerdictCounts {
    faster: usize,
    slower: usize,
    noise: usize,
    inconclusive: usize,
}

impl VerdictCounts {
    const fn record(&mut self, verdict: Verdict) {
        match verdict {
            Verdict::Faster => self.faster += 1,
            Verdict::Slower => self.slower += 1,
            Verdict::Noise => self.noise += 1,
            Verdict::Inconclusive => self.inconclusive += 1,
        }
    }

    fn summary_line(&self, label: Option<&str>) -> String {
        let prefix = label.map_or_else(|| "summary".to_owned(), |name| format!("summary ({name})"));
        format!(
            "{prefix}: {} faster, {} slower, {} ~, {} ?",
            self.faster, self.slower, self.noise, self.inconclusive
        )
    }
}

/// Every saved baseline name in `dir` (containing `estimates.json`) that belongs to
/// `set`: the name itself, or, for round sets (`NAME.a` / `NAME.b`), its rounds (`set`
/// followed by digits), sorted numerically by round number.
fn members(dir: &Path, set: &str) -> Vec<(Option<u32>, String)> {
    if dir.join(set).join("estimates.json").is_file() {
        return vec![(None, set.to_owned())];
    }
    let Some(set) = RoundName::parse(set).filter(|name| name.is_set()) else {
        return Vec::new();
    };
    let mut rounds: Vec<(u32, String)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().join("estimates.json").is_file())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter_map(|name| {
            let member = RoundName::parse(&name)?;
            let round = member
                .round
                .filter(|_| member.base == set.base && member.side == set.side)?;
            Some((round, name))
        })
        .collect();
    rounds.sort_by_key(|(round, _)| *round);
    rounds
        .into_iter()
        .map(|(round, name)| (Some(round), name))
        .collect()
}

/// A benchmark's row: its name and one summary per set.
struct Row {
    name: String,
    sets: Vec<Option<Summary>>,
    /// For a parkay benchmark `W`, the saved reference `arrow_W` in the same group.
    reference: Option<Estimate>,
}

fn rows(sets: &[String], since: Option<SystemTime>) -> Result<Vec<Row>, String> {
    let criterion = crate::root().join("target/criterion");
    if !criterion.is_dir() {
        return Err(format!(
            "{}: not found (run `cargo xtask bench --save NAME` first)",
            rel_path(&criterion)
        ));
    }
    let mut rows = Vec::new();
    for (group_name, workload, dir) in criterion_bench_dirs(&criterion) {
        let summaries: Vec<Option<Summary>> = sets
            .iter()
            .map(|set| {
                let estimates: Vec<(Option<u32>, Estimate)> = members(&dir, set)
                    .iter()
                    .filter_map(|(round, member)| Some((*round, saved(&dir, member)?)))
                    .collect();
                summarize(&estimates)
            })
            .collect();
        if summaries.iter().all(Option::is_none) {
            continue;
        }
        if let Some(since) = since
            && !is_fresh(&dir, sets, since)
        {
            continue;
        }
        let name = format!("{group_name}/{workload}");
        let reference = (Role::of(&name) == Role::Parkay)
            .then(|| {
                dir.parent().and_then(|group_dir| {
                    saved(&group_dir.join(format!("arrow_{workload}")), REFERENCE)
                })
            })
            .flatten();
        rows.push(Row {
            name,
            sets: summaries,
            reference,
        });
    }
    rows.sort_by(|x, y| x.name.cmp(&y.name));
    Ok(rows)
}

fn is_fresh(dir: &Path, sets: &[String], since: SystemTime) -> bool {
    // Allow 1 s slack for filesystems with 1-second mtime granularity.
    let cutoff = since
        .checked_sub(Duration::from_secs(1))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let last = sets.last().map_or_default(|set| members(dir, set));
    last.iter().any(|(_, member)| {
        std::fs::metadata(dir.join(member).join("estimates.json"))
            .and_then(|m| m.modified())
            .is_ok_and(|modified| modified >= cutoff)
    })
}

/// Measured `scan` control drift for one comparison set relative to `sets[0]`,
/// tracked per benchmark group so noise in one tiny group does not mask real changes
/// in another. Falls back to `global_max` when a group has no `scan` row.
struct ControlDrift {
    per_group: BTreeMap<String, f64>,
    global_max: Option<f64>,
}

impl ControlDrift {
    fn for_row(&self, benchmark_name: &str) -> Option<f64> {
        let group = benchmark_name.rsplit_once('/').map_or("", |(g, _)| g);
        self.per_group.get(group).copied().or(self.global_max)
    }
}

fn control_drift(rows: &[Row], sets: &[String]) -> Vec<ControlDrift> {
    let control_suffix = format!("/{CONTROL}");
    sets.iter()
        .enumerate()
        .map(|(set_index, set_name)| {
            let paired = set_index > 0 && is_paired_sets(&sets[0], set_name);
            let mut per_group = BTreeMap::new();
            let mut global_max: Option<f64> = None;
            for row in rows
                .iter()
                .filter(|row| row.name.ends_with(&control_suffix))
            {
                if let (Some(base), Some(other)) =
                    (row.sets[0].as_ref(), row.sets[set_index].as_ref())
                    && let Some(change) = change_percent(base, other, paired).map(f64::abs)
                {
                    if let Some((group, _)) = row.name.rsplit_once('/') {
                        per_group.insert(group.to_owned(), change);
                    }
                    global_max = Some(global_max.map_or(change, |prev| prev.max(change)));
                }
            }
            ControlDrift {
                per_group,
                global_max,
            }
        })
        .collect()
}

fn spread_legend(rows: &[Row]) -> &'static str {
    let has_multi_round = rows
        .iter()
        .any(|r| r.sets.iter().flatten().any(|s| s.rounds.len() > 1));
    let has_single = rows
        .iter()
        .any(|r| r.sets.iter().flatten().any(|s| s.rounds.len() <= 1));
    match (has_single, has_multi_round) {
        (true, true) => "95% CI (single) / round half-range (multi-round)",
        (false, true) => "round half-range",
        _ => "95% CI half-width",
    }
}

/// Prints the comparison table of `sets`, with what is known about each set.
pub fn table(
    sets: &[String],
    since: Option<SystemTime>,
    changed_only: bool,
    markdown: bool,
) -> Result<(), String> {
    let rows = rows(sets, since)?;
    if rows.is_empty() {
        return Err(format!("no saved results for {}", sets.join(", ")));
    }
    print_provenance(sets, &rows);
    let drift = control_drift(&rows, sets);
    print_control_lines(sets, &drift, spread_legend(&rows));
    let has_reference = rows.iter().any(|row| row.reference.is_some());
    if has_reference {
        print_reference_status(&rows);
    }
    let width = rows
        .iter()
        .map(|row| row.name.len())
        .max()
        .unwrap_or(9)
        .max(9);
    let columns = columns(sets.len(), has_reference);
    print_header(sets, &columns, width, markdown);
    let mut counts: Vec<VerdictCounts> =
        vec![VerdictCounts::default(); sets.len().saturating_sub(1)];
    for row in &rows {
        let is_control = Role::of(&row.name) == Role::Control;
        let (cells, verdicts) = row_cells(row, sets, &drift, has_reference, markdown);
        let changed = verdicts
            .iter()
            .flatten()
            .any(|verdict| *verdict != Verdict::Noise);
        let line = render(&cells, &columns, width, markdown);
        if !is_control {
            for (slot, verdict) in counts.iter_mut().zip(verdicts) {
                if let Some(verdict) = verdict {
                    slot.record(verdict);
                }
            }
        }
        if (changed && !is_control) || !changed_only {
            println!("{line}");
        }
    }
    print_verdict_summaries(sets, &counts);
    Ok(())
}

fn print_control_lines(sets: &[String], drift: &[ControlDrift], legend: &str) {
    if sets.len() <= 1 {
        println!("spread: ± is {legend}");
        return;
    }
    for (set, set_drift) in sets.iter().zip(drift).skip(1) {
        let paired = is_paired_sets(&sets[0], set);
        if let Some(max) = set_drift.global_max {
            println!(
                "control: per-group {CONTROL} drift in {set} (up to {max:.1}%); \
                 changes within a group's {CONTROL} drift are noise{}; ± is {legend}",
                if paired {
                    "; paired sign test p <= 0.05 (needs >= 6 rounds)"
                } else {
                    ""
                }
            );
        } else {
            println!(
                "control: none for {set}; verdicts need `{CONTROL}` in the run{}; ± is {legend}",
                if paired {
                    " (paired sign test still requires >= 6 rounds)"
                } else {
                    ""
                }
            );
        }
    }
}

fn print_verdict_summaries(sets: &[String], counts: &[VerdictCounts]) {
    if counts.is_empty() {
        return;
    }
    if counts.len() == 1 {
        println!("{}", counts[0].summary_line(None));
    } else {
        for (set, count) in sets.iter().skip(1).zip(counts) {
            println!("{}", count.summary_line(Some(set)));
        }
    }
}

/// A table column: it sets a cell's width and separator in the terminal table and its
/// alignment in the markdown one.
#[derive(Clone, Copy)]
enum Column {
    Name,
    Set,
    Change,
    Verdict,
    Reference,
    Speedup,
}

impl Column {
    const fn markdown_alignment(self) -> &'static str {
        match self {
            Self::Name => "---",
            Self::Verdict => ":---:",
            Self::Set | Self::Change | Self::Reference | Self::Speedup => "---:",
        }
    }
}

/// The columns for `set_count` sets; each set after the first gets a change and verdict.
fn columns(set_count: usize, has_reference: bool) -> Vec<Column> {
    let mut columns = vec![Column::Name];
    for index in 0..set_count {
        columns.push(Column::Set);
        if index > 0 {
            columns.extend([Column::Change, Column::Verdict]);
        }
    }
    if has_reference {
        columns.extend([Column::Reference, Column::Speedup]);
    }
    columns
}

/// One table line: padded columns for the terminal, or a GitHub-Flavored Markdown row.
fn render(cells: &[String], columns: &[Column], name_width: usize, markdown: bool) -> String {
    debug_assert_eq!(cells.len(), columns.len());
    if markdown {
        return format!("| {} |", cells.join(" | "));
    }
    let mut line = String::new();
    for (cell, column) in cells.iter().zip(columns) {
        let _ = match column {
            Column::Name => write!(line, "{cell:<name_width$}"),
            Column::Set => write!(line, " | {cell:>30}"),
            Column::Change => write!(line, " {cell:>7}"),
            Column::Verdict => write!(line, " {cell:>6}"),
            Column::Reference => write!(line, " | {cell:>11}"),
            Column::Speedup => write!(line, " {cell:>8}"),
        };
    }
    line
}

fn print_header(sets: &[String], columns: &[Column], name_width: usize, markdown: bool) {
    let mut set_names = sets.iter();
    let cells: Vec<String> = columns
        .iter()
        .map(|column| match column {
            Column::Name => "benchmark".to_owned(),
            Column::Set => set_names.next().cloned().unwrap_or_default(),
            Column::Change => "change".to_owned(),
            Column::Verdict => "verdict".to_owned(),
            Column::Reference => "arrow-rs".to_owned(),
            Column::Speedup => "speedup".to_owned(),
        })
        .collect();
    println!("{}", render(&cells, columns, name_width, markdown));
    if markdown {
        let alignments: Vec<&str> = columns.iter().map(|c| c.markdown_alignment()).collect();
        println!("| {} |", alignments.join(" | "));
    }
}

/// A set's median, spread, and rate: aligned for the terminal, compact for markdown.
fn format_summary(summary: &Summary, markdown: bool) -> String {
    let spread = if summary.median > 0.0 {
        (summary.upper - summary.lower) / 2.0 / summary.median * 100.0
    } else {
        0.0
    };
    let time = format_time(summary.median);
    let rate = summary
        .bytes
        .map_or_default(|bytes| format_rate(bytes, summary.median));
    match (markdown, rate.is_empty()) {
        (false, _) => format!("{time:>11} \u{00b1}{spread:>4.1}% {rate:>11}"),
        (true, true) => format!("{time} \u{00b1}{spread:.1}%"),
        (true, false) => format!("{time} \u{00b1}{spread:.1}% ({rate})"),
    }
}

/// A row's cells, one per column of [`columns`], and its verdict for each set after
/// the first (`None` where either set has no result).
fn row_cells(
    row: &Row,
    sets: &[String],
    drift: &[ControlDrift],
    has_reference: bool,
    markdown: bool,
) -> (Vec<String>, Vec<Option<Verdict>>) {
    let name = if markdown {
        format!("`{}`", row.name)
    } else {
        row.name.clone()
    };
    let mut cells = vec![name];
    let mut verdicts = Vec::with_capacity(sets.len().saturating_sub(1));
    for (index, summary) in row.sets.iter().enumerate() {
        cells.push(summary.as_ref().map_or_else(
            || "-".to_owned(),
            |summary| format_summary(summary, markdown),
        ));
        if index == 0 {
            continue;
        }
        let Some((base, summary)) = row.sets[0].as_ref().zip(summary.as_ref()) else {
            cells.extend([String::new(), String::new()]);
            verdicts.push(None);
            continue;
        };
        let paired = is_paired_sets(&sets[0], &sets[index]);
        let verdict = Verdict::of(base, summary, paired, drift[index].for_row(&row.name));
        let change = change_percent(base, summary, paired)
            .map_or_else(|| "-".to_owned(), |percent| format!("{percent:+.1}%"));
        cells.extend([change, verdict.label().to_owned()]);
        verdicts.push(Some(verdict));
    }
    if has_reference {
        let last = row.sets.last().and_then(Option::as_ref);
        let (time, speedup) = match (&row.reference, last) {
            (Some(reference), Some(last)) if last.median > 0.0 => (
                format_time(reference.median),
                format!("{:.2}x", reference.median / last.median),
            ),
            (Some(reference), Some(_)) => (format_time(reference.median), "-".to_owned()),
            _ => ("-".to_owned(), String::new()),
        };
        cells.extend([time, speedup]);
    }
    (cells, verdicts)
}

/// Says when the arrow-rs reference beside the table was saved, and whether it still
/// measures what this checkout would: same machine, toolchain, locked dependencies,
/// and harness.
fn print_reference_status(rows: &[Row]) {
    let mut benches: Vec<BenchTarget> = rows
        .iter()
        .filter(|row| row.reference.is_some())
        .map(|row| BenchTarget::from_benchmark_name(&row.name))
        .collect();
    benches.sort_unstable();
    benches.dedup();
    let tree = working_tree().ok();
    for bench in benches {
        let status = reference_status(bench, tree.as_ref());
        let started = status
            .as_ref()
            .and_then(|(record, _)| record.get("started"))
            .map_or("an unrecorded time", String::as_str);
        let command = bench.reference_command();
        match status.as_ref().map(|(_, freshness)| *freshness) {
            Some(Freshness::Current) => {
                println!("reference ({bench}): arrow-rs saved {started}, current");
            }
            Some(Freshness::Stale) => println!(
                "reference ({bench}): STALE (saved {started}; the machine, toolchain, \
                 Cargo.toml, Cargo.lock, or harness changed). Refresh: {command}"
            ),
            Some(Freshness::Unverifiable) | None => println!(
                "reference ({bench}): no record of how it was measured. Refresh: {command}"
            ),
        }
    }
}

/// Whether a saved arrow-rs reference still measures what this checkout would.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    Current,
    /// The machine, toolchain, `Cargo.toml`, `Cargo.lock`, or harness changed since it
    /// was saved.
    Stale,
    /// The record has no reference key, or the working tree's cannot be computed.
    Unverifiable,
}

/// The run record of `bench`'s saved arrow-rs reference, and its [`Freshness`] against
/// `tree`. `None` when no record was kept.
pub fn reference_status(
    bench: BenchTarget,
    tree: Option<&Tree>,
) -> Option<(BTreeMap<String, String>, Freshness)> {
    let record = provenance(&format!("{REFERENCE}-{bench}"))?;
    let current = tree.and_then(|tree| reference_key(tree, bench, None).ok());
    let freshness = match (record.get("reference_key"), current) {
        (Some(saved), Some(current)) if *saved == current => Freshness::Current,
        (Some(_), Some(_)) => Freshness::Stale,
        _ => Freshness::Unverifiable,
    };
    Some((record, freshness))
}

/// Percentage change from `base` to `other`. For paired `--against` rounds, takes the
/// median of per-round `other / base` ratios so slow drift cancels across rounds.
/// Returns `None` if `base.median <= 0.0` (and no valid paired ratios exist).
fn change_percent(base: &Summary, other: &Summary, paired: bool) -> Option<f64> {
    if paired {
        let mut ratios = paired_ratios(base, other);
        if !ratios.is_empty() {
            ratios.sort_by(f64::total_cmp);
            return Some((median_sorted(&ratios) - 1.0) * 100.0);
        }
    }
    (base.median > 0.0).then(|| (other.median / base.median - 1.0) * 100.0)
}

/// The bench targets that actually appear in `rows` for set `set_index`.
fn benches_in_set(rows: &[Row], set_index: usize) -> Vec<BenchTarget> {
    let mut benches: Vec<BenchTarget> = rows
        .iter()
        .filter(|row| row.sets.get(set_index).is_some_and(Option::is_some))
        .map(|row| BenchTarget::from_benchmark_name(&row.name))
        .collect();
    benches.sort_unstable();
    benches.dedup();
    benches
}

struct ProvenanceEntry {
    label: String,
    bench_kind: String,
    tree: String,
    harness_key: Option<String>,
}

/// Cross-baseline provenance notes and warnings, grouped by `bench_kind` so
/// `reference (metadata)` is never compared against `reference (thrift)`.
fn provenance_diagnostics(entries: &[ProvenanceEntry]) -> Vec<String> {
    let mut by_bench: BTreeMap<&str, Vec<&ProvenanceEntry>> = BTreeMap::new();
    for entry in entries {
        by_bench
            .entry(entry.bench_kind.as_str())
            .or_default()
            .push(entry);
    }
    let mut messages = Vec::new();
    for group in by_bench.values() {
        for (index, entry) in group.iter().enumerate() {
            for other in &group[index + 1..] {
                if entry.tree == other.tree && entry.tree != "?" {
                    messages.push(format!(
                        "note: {} and {} measured the same code; any difference is noise",
                        entry.label, other.label
                    ));
                }
            }
        }
        for (index, entry) in group.iter().enumerate() {
            let Some(harness) = entry.harness_key.as_deref() else {
                continue;
            };
            for other in &group[index + 1..] {
                let Some(other_harness) = other.harness_key.as_deref() else {
                    continue;
                };
                if harness != other_harness {
                    messages.push(format!(
                        "warning: {} and {} used different bench harnesses or corpora; \
                         their results may not be directly comparable",
                        entry.label, other.label
                    ));
                }
            }
        }
    }
    messages
}

/// One line per set saying what code it measured and under what conditions, and
/// warnings for comparisons that cannot show what they seem to.
fn print_provenance(sets: &[String], rows: &[Row]) {
    let mut entries: Vec<ProvenanceEntry> = Vec::new();
    for (set_index, set) in sets.iter().enumerate() {
        let benches = benches_in_set(rows, set_index);
        let lookup_names: Vec<(String, String, Option<BenchTarget>)> = if set == REFERENCE {
            benches
                .into_iter()
                .map(|bench| {
                    (
                        format!("{REFERENCE} ({bench})"),
                        format!("{REFERENCE}-{bench}"),
                        Some(bench),
                    )
                })
                .collect()
        } else {
            let hint = (benches.len() == 1).then(|| benches[0]);
            vec![(set.clone(), set.clone(), hint)]
        };
        for (label, record_name, bench_hint) in lookup_names {
            let Some(keys) = provenance(&record_name) else {
                println!(
                    "{label}: no record (saved before xtask kept records, or by plain criterion)"
                );
                continue;
            };
            let get = |key: &str| keys.get(key).map_or("?", String::as_str);
            let commit = get("commit");
            let tree = get("tree");
            let bench_kind = keys.get("bench").cloned().unwrap_or_else(|| {
                bench_hint.map_or_else(|| "unknown".to_owned(), |b| b.as_str().to_owned())
            });
            entries.push(ProvenanceEntry {
                label: label.clone(),
                bench_kind,
                tree: tree.to_owned(),
                harness_key: keys.get("harness_key").cloned(),
            });
            let mut line = format!(
                "{label}: commit {}{}, tree {}",
                short(commit),
                if get("dirty") == "true" {
                    " + uncommitted changes"
                } else {
                    ""
                },
                short(tree)
            );
            if let Some(rounds) = keys.get("rounds") {
                let _ = write!(line, ", {rounds} rounds");
            }
            if get("quick") == "true" {
                line.push_str(", QUICK (explore with it; decide with a full run)");
            }
            if get("problems") != "none" {
                let _ = write!(line, ", NOISY: {}", get("problems"));
            }
            if get("slept") != "no" {
                let _ = write!(line, ", SLEPT at {}", get("slept"));
            }
            let _ = write!(line, ", {}", get("started"));
            println!("{line}");
        }
    }
    for message in provenance_diagnostics(&entries) {
        println!("{message}");
    }
}

fn format_time(ns: f64) -> String {
    match ns {
        ns if ns >= 1e6 => format!("{:.3} ms", ns / 1e6),
        ns if ns >= 1e3 => format!("{:.2} \u{00b5}s", ns / 1e3),
        ns => format!("{ns:.1} ns"),
    }
}

fn format_rate(bytes: f64, ns: f64) -> String {
    if ns <= 0.0 || !ns.is_finite() {
        return String::new();
    }
    let per_second = bytes / (ns / 1e9);
    if per_second >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} GiB/s", per_second / (1024.0 * 1024.0 * 1024.0))
    } else {
        format!("{:.0} MiB/s", per_second / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BenchTarget, Estimate, ProvenanceEntry, REFERENCE, Row, Summary, Verdict, VerdictCounts,
        benches_in_set, change_percent, columns, control_drift, expand_paired_shorthand,
        is_paired_sets, parse_estimate, parse_record, provenance_diagnostics, render, row_cells,
        sign_test_significant, spread_legend, summarize,
    };
    use crate::TempDir;

    fn single(median: f64, lower: f64, upper: f64) -> Summary {
        let Some(summary) = summarize(&[(
            None,
            Estimate {
                median,
                lower,
                upper,
                bytes: None,
            },
        )]) else {
            panic!("single summary");
        };
        summary
    }

    fn rounds(medians: &[f64]) -> Summary {
        let estimates: Vec<(Option<u32>, Estimate)> = medians
            .iter()
            .enumerate()
            .map(|(idx, &m)| {
                let Ok(round) = u32::try_from(idx + 1) else {
                    panic!("round fits u32");
                };
                (
                    Some(round),
                    Estimate {
                        median: m,
                        lower: m * 0.99,
                        upper: m * 1.01,
                        bytes: None,
                    },
                )
            })
            .collect();
        let Some(summary) = summarize(&estimates) else {
            panic!("round summary");
        };
        summary
    }

    #[test]
    fn unpaired_verdict_distinguishes_inconclusive_noise_faster_and_slower() {
        let base = single(100.0, 99.0, 101.0);
        let faster = single(90.0, 89.0, 91.0);
        let slower = single(110.0, 109.0, 111.0);
        assert_eq!(
            Verdict::of(&base, &faster, false, None),
            Verdict::Inconclusive
        );
        assert_eq!(
            Verdict::of(&base, &faster, false, Some(2.0)),
            Verdict::Faster
        );
        assert_eq!(
            Verdict::of(&base, &slower, false, Some(2.0)),
            Verdict::Slower
        );
        assert_eq!(
            Verdict::of(&base, &faster, false, Some(12.0)),
            Verdict::Noise
        );
    }

    #[test]
    fn paired_verdict_requires_scan_control_and_checks_min_rounds_before_drift() {
        let a6 = rounds(&[100.0, 105.0, 110.0, 115.0, 120.0, 125.0]);
        let b6 = rounds(&[95.0, 99.75, 104.5, 109.25, 114.0, 118.75]);
        assert_eq!(Verdict::of(&a6, &b6, true, None), Verdict::Inconclusive);

        let a4 = rounds(&[100.0, 100.0, 100.0, 100.0]);
        let b4 = rounds(&[99.5, 99.5, 99.5, 99.5]);
        assert_eq!(
            Verdict::of(&a4, &b4, true, Some(5.0)),
            Verdict::Inconclusive
        );

        let a5 = rounds(&[100.0, 101.0, 102.0, 103.0, 104.0]);
        let b5 = rounds(&[95.0, 96.0, 97.0, 98.0, 99.0]);
        assert_eq!(
            Verdict::of(&a5, &b5, true, Some(1.0)),
            Verdict::Inconclusive
        );

        let Some(pct) = change_percent(&a6, &b6, true) else {
            panic!("expected paired change");
        };
        assert!((pct - (-5.0)).abs() < 1e-9);
        assert_eq!(Verdict::of(&a6, &b6, true, Some(1.0)), Verdict::Faster);
        assert_eq!(Verdict::of(&b6, &a6, true, Some(1.0)), Verdict::Slower);
        assert_eq!(Verdict::of(&a6, &b6, true, Some(6.0)), Verdict::Noise);

        let b6_mixed = rounds(&[95.0, 99.75, 104.5, 109.25, 114.0, 126.0]);
        assert_eq!(Verdict::of(&a6, &b6_mixed, true, Some(1.0)), Verdict::Noise);

        let a9 = rounds(&[100.0; 9]);
        let b9 = rounds(&[95.0, 95.0, 95.0, 95.0, 95.0, 95.0, 95.0, 95.0, 101.0]);
        assert_eq!(Verdict::of(&a9, &b9, true, Some(1.0)), Verdict::Faster);
    }

    #[test]
    fn provenance_groups_harness_checks_by_bench_kind() {
        let ref_only = vec![
            ProvenanceEntry {
                label: "reference (metadata)".into(),
                bench_kind: "metadata".into(),
                tree: "tree111".into(),
                harness_key: Some("meta-key".into()),
            },
            ProvenanceEntry {
                label: "reference (thrift)".into(),
                bench_kind: "thrift".into(),
                tree: "tree111".into(),
                harness_key: Some("thrift-key".into()),
            },
        ];
        assert_eq!(provenance_diagnostics(&ref_only), Vec::<String>::new());

        let mismatched = vec![
            ProvenanceEntry {
                label: "base".into(),
                bench_kind: "metadata".into(),
                tree: "tree111".into(),
                harness_key: Some("meta-key-1".into()),
            },
            ProvenanceEntry {
                label: "after".into(),
                bench_kind: "metadata".into(),
                tree: "tree222".into(),
                harness_key: Some("meta-key-2".into()),
            },
        ];
        let msgs = provenance_diagnostics(&mismatched);
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].contains("different bench harnesses"));
    }

    #[test]
    fn zero_median_never_produces_nan_or_inf() {
        let zero = Summary {
            median: 0.0,
            lower: 0.0,
            upper: 0.0,
            bytes: Some(1024.0),
            rounds: Vec::new(),
        };
        let nonzero = single(50.0, 49.0, 51.0);
        let row = Row {
            name: "tiny/open".to_owned(),
            sets: vec![Some(zero), Some(nonzero)],
            reference: Some(Estimate {
                median: 100.0,
                lower: 99.0,
                upper: 101.0,
                bytes: None,
            }),
        };
        let sets = vec!["a".to_owned(), "b".to_owned()];
        let drift = control_drift(&[], &sets);
        let columns = columns(sets.len(), true);
        let format_row = |tested: &Row| {
            let (cells, verdicts) = row_cells(tested, &sets, &drift, true, false);
            (render(&cells, &columns, 10, false), verdicts)
        };
        let (line, verdicts) = format_row(&row);
        assert!(!line.contains("NaN"), "unexpected NaN in: {line}");
        assert!(!line.contains("inf"), "unexpected inf in: {line}");
        assert_eq!(verdicts, vec![Some(Verdict::Inconclusive)]);

        let row_zero_last = Row {
            name: "tiny/open".to_owned(),
            sets: vec![
                Some(single(50.0, 49.0, 51.0)),
                Some(Summary {
                    median: 0.0,
                    lower: 0.0,
                    upper: 0.0,
                    bytes: Some(1024.0),
                    rounds: Vec::new(),
                }),
            ],
            reference: Some(Estimate {
                median: 100.0,
                lower: 99.0,
                upper: 101.0,
                bytes: None,
            }),
        };
        let (line2, _) = format_row(&row_zero_last);
        assert!(!line2.contains("NaN"), "unexpected NaN in: {line2}");
        assert!(!line2.contains("inf"), "unexpected inf in: {line2}");
    }

    #[test]
    fn paired_shorthand_requires_estimates_json_and_ignores_empty_dirs() {
        let dir = TempDir::new("report-shorthand");
        let bench_dir = dir.join("medium/open");
        for sub in [
            "exp.a1",
            "exp.b1",
            "both",
            "both.a1",
            "both.b1",
            "only_a.a1",
            "empty_plain.a1",
            "empty_plain.b1",
        ] {
            let sub_dir = bench_dir.join(sub);
            std::fs::create_dir_all(&sub_dir).expect("create sub dir");
            std::fs::write(sub_dir.join("estimates.json"), "{}").expect("write estimates");
        }
        // An empty plain directory without `estimates.json` must not block paired expansion.
        std::fs::create_dir_all(bench_dir.join("empty_plain")).expect("create empty plain dir");
        for side in ["a1", "b1"] {
            let ref_dir = bench_dir.join(format!("{REFERENCE}.{side}"));
            std::fs::create_dir_all(&ref_dir).expect("create ref dir");
            std::fs::write(ref_dir.join("estimates.json"), "{}").expect("write ref estimates");
        }

        assert_eq!(
            expand_paired_shorthand(&["exp".to_owned()], &dir),
            vec!["exp.a".to_owned(), "exp.b".to_owned()]
        );
        assert_eq!(
            expand_paired_shorthand(&["empty_plain".to_owned()], &dir),
            vec!["empty_plain.a".to_owned(), "empty_plain.b".to_owned()]
        );
        assert_eq!(
            expand_paired_shorthand(&[REFERENCE.to_owned()], &dir),
            vec![REFERENCE.to_owned()]
        );
        assert_eq!(
            expand_paired_shorthand(&["both".to_owned()], &dir),
            vec!["both".to_owned()]
        );
        assert_eq!(
            expand_paired_shorthand(&["only_a".to_owned()], &dir),
            vec!["only_a".to_owned()]
        );
        assert_eq!(
            expand_paired_shorthand(&["a".to_owned(), "b".to_owned()], &dir),
            vec!["a".to_owned(), "b".to_owned()]
        );
    }

    #[test]
    fn verdict_counts_summary_formats_single_and_labeled_multi_column() {
        let mut counts = VerdictCounts::default();
        counts.record(Verdict::Faster);
        counts.record(Verdict::Faster);
        counts.record(Verdict::Slower);
        counts.record(Verdict::Noise);
        counts.record(Verdict::Inconclusive);
        assert_eq!(
            counts.summary_line(None),
            "summary: 2 faster, 1 slower, 1 ~, 1 ?"
        );
        assert_eq!(
            counts.summary_line(Some("after")),
            "summary (after): 2 faster, 1 slower, 1 ~, 1 ?"
        );
    }

    #[test]
    fn markdown_row_formats_pipe_separated_cells_and_per_column_verdicts() {
        let row = Row {
            name: "medium/open".to_owned(),
            sets: vec![
                Some(single(1000.0, 990.0, 1010.0)),
                Some(single(800.0, 790.0, 810.0)),
                None,
            ],
            reference: Some(Estimate {
                median: 4000.0,
                lower: 3990.0,
                upper: 4010.0,
                bytes: None,
            }),
        };
        let scan = Row {
            name: "medium/scan".to_owned(),
            sets: vec![
                Some(single(100.0, 99.0, 101.0)),
                Some(single(100.5, 99.5, 101.5)),
                None,
            ],
            reference: None,
        };
        let sets = vec!["base".to_owned(), "after".to_owned(), "missing".to_owned()];
        let drift = control_drift(&[scan], &sets);
        let columns = columns(sets.len(), true);
        let (cells, verdicts) = row_cells(&row, &sets, &drift, true, true);
        let md = render(&cells, &columns, 0, true);
        assert_eq!(verdicts, vec![Some(Verdict::Faster), None]);
        assert!(md.starts_with("| `medium/open` | "));
        assert!(md.contains("| -20.0% | faster |"));
        assert!(md.ends_with(" | 4.00 \u{00b5}s | - |") || md.ends_with(" | - |  |"));
        assert_eq!(spread_legend(&[row]), "95% CI half-width");
    }

    #[test]
    fn sign_test_thresholds_match_exact_binomial() {
        assert!(!sign_test_significant(5, 0));
        assert!(sign_test_significant(6, 0));
        assert!(!sign_test_significant(6, 1));
        assert!(!sign_test_significant(8, 1));
        assert!(sign_test_significant(9, 1));
        assert!(sign_test_significant(10, 1));
    }

    #[test]
    fn control_drift_uses_scan_per_group_and_per_set() {
        let rows = vec![
            Row {
                name: "small/scan".to_owned(),
                sets: vec![
                    Some(single(100.0, 99.0, 101.0)),
                    Some(single(105.0, 104.0, 106.0)),
                    None,
                ],
                reference: None,
            },
            Row {
                name: "wide/scan".to_owned(),
                sets: vec![
                    Some(single(1000.0, 999.0, 1001.0)),
                    Some(single(1002.0, 1001.0, 1003.0)),
                    None,
                ],
                reference: None,
            },
        ];
        let sets = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let drift = control_drift(&rows, &sets);
        let Some(small) = drift[1].for_row("small/open") else {
            panic!("small drift");
        };
        let Some(wide) = drift[1].for_row("wide/open") else {
            panic!("wide drift");
        };
        let Some(other) = drift[1].for_row("other/open") else {
            panic!("fallback drift");
        };
        assert!((small - 5.0).abs() < 1e-9);
        assert!((wide - 0.2).abs() < 1e-9);
        assert!((other - 5.0).abs() < 1e-9);
        assert_eq!(drift[2].for_row("wide/open"), None);
    }

    #[test]
    fn benches_in_set_matches_rows_for_reference_provenance() {
        let rows = vec![
            Row {
                name: "medium/arrow_open".to_owned(),
                sets: vec![Some(single(100.0, 99.0, 101.0))],
                reference: None,
            },
            Row {
                name: "thrift_medium/arrow_skip".to_owned(),
                sets: vec![Some(single(50.0, 49.0, 51.0))],
                reference: None,
            },
        ];
        assert_eq!(
            benches_in_set(&rows, 0),
            vec![BenchTarget::Metadata, BenchTarget::Thrift]
        );
    }

    #[test]
    fn helpers_parse_criterion_estimates_and_round_sets() {
        let est_json = r#"{"median":{"confidence_interval":{"lower_bound":95.0,"upper_bound":105.0},"point_estimate":100.0}}"#;
        let benchmark_json = r#"{"group_id":"medium","throughput":{"Bytes":1024}}"#;
        let Some(est) = parse_estimate(est_json, Some(benchmark_json)) else {
            panic!("estimate should parse");
        };
        assert!((est.median - 100.0).abs() < 1e-9);
        assert!((est.lower - 95.0).abs() < 1e-9);
        assert!((est.upper - 105.0).abs() < 1e-9);
        assert_eq!(est.bytes, Some(1024.0));
        let without_throughput = parse_estimate(est_json, Some(r#"{"throughput":null}"#));
        assert_eq!(without_throughput.and_then(|parsed| parsed.bytes), None);
        assert!(parse_estimate("{}", None).is_none());

        assert!(is_paired_sets("exp.a", "exp.b"));
        assert!(is_paired_sets("exp.b", "exp.a"));
        assert!(!is_paired_sets("exp.a", "exp.a"));
        assert!(!is_paired_sets("exp.a", "other.b"));
        assert!(!is_paired_sets("exp.a1", "exp.b1"));

        let rec = parse_record("a.commit: aaa\nb.commit: bbb\nrounds: 6\n", Some('b'));
        assert_eq!(rec.get("commit").map(String::as_str), Some("bbb"));
        assert_eq!(rec.get("rounds").map(String::as_str), Some("6"));
    }
}
