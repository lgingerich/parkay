//! `cargo xtask status`: concise overview of saved baselines, snapshots, references,
//! asm/profile saves, fuzz artifacts/corpora, and any active measurement or build lock.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::Path;
use std::process::Command;

use crate::asm::saved_asm_name;
use crate::bench::git::{Tree, working_tree};
use crate::bench::{
    Baselines, BenchTarget, Freshness, RoundName, baseline_dirs, criterion_bench_dirs,
    reference_status,
};
use crate::fuzz::{target_artifacts, visible_entries};
use crate::profile::saved_profile_name;
use crate::{output, read_lock_owner, rel_path, root};

/// Maximum items listed inline before summarizing the rest.
const LIST_LIMIT: usize = 8;

/// Suggest `--minimize` once total corpus files exceed this count.
const CORPUS_MINIMIZE_HINT_THRESHOLD: usize = 5_000;

/// Prints a concise overview of harness state under `target/` and `fuzz/`.
pub(crate) fn status() {
    let repo = root();
    if let Some(lock_line) = lock_status(&repo) {
        println!("lock:       {lock_line}");
    }
    let tree = working_tree().ok();
    for (bench, label) in [
        (BenchTarget::Metadata, "ref meta:"),
        (BenchTarget::Thrift, "ref thrift:"),
    ] {
        println!(
            "{label:<11} {}",
            reference_line(&repo, bench, tree.as_ref())
        );
    }
    let baselines = saved_baselines(&repo);
    println!("baselines:  {}", format_list(&baselines, "none"));
    let snapshots = git_snapshots(&repo);
    println!("snapshots:  {}", format_list(&snapshots, "none"));
    let asm = saved_names(&repo.join("target/xtask/asm"), saved_asm_name);
    println!("asm:        {}", format_list(&asm, "none"));
    let profiles = saved_names(&repo.join("target/xtask/profile"), saved_profile_name);
    println!("profile:    {}", format_list(&profiles, "none"));
    let artifacts = fuzz_artifacts(&repo);
    println!("fuzz:       {}", format_fuzz_artifacts(&artifacts));
    let corpus = fuzz_corpus_counts(&repo);
    println!("corpus:     {}", format_fuzz_corpus(&corpus));
}

/// Reports an active measurement/build lock or a stale `target/xtask/measure.owner`.
fn lock_status(repo: &Path) -> Option<String> {
    let xtask = repo.join("target/xtask");
    let owner_path = xtask.join("measure.owner");
    let note = read_lock_owner(&owner_path);
    let lock_path = xtask.join("measure.lock");
    let state = probe_lock(&lock_path);
    match (state, note) {
        (LockState::Exclusive, Some(note)) => Some(format!("ACTIVE ({note})")),
        (LockState::Exclusive, None) => Some("ACTIVE (exclusive measurement lock held)".to_owned()),
        (LockState::Shared, _) => {
            Some("ACTIVE (shared build lock: check, fuzz, or asm)".to_owned())
        }
        (LockState::Free, Some(note)) => Some(format!("stale {} ({note})", rel_path(&owner_path))),
        (LockState::Free, None) => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LockState {
    Free,
    Shared,
    Exclusive,
}

fn probe_lock(lock_path: &Path) -> LockState {
    let Ok(file) = File::options().write(true).open(lock_path) else {
        return LockState::Free;
    };
    match file.try_lock() {
        Ok(()) | Err(std::fs::TryLockError::Error(_)) => LockState::Free,
        Err(std::fs::TryLockError::WouldBlock) => match file.try_lock_shared() {
            Ok(()) => LockState::Shared,
            Err(std::fs::TryLockError::WouldBlock) => LockState::Exclusive,
            Err(std::fs::TryLockError::Error(_)) => LockState::Free,
        },
    }
}

/// Describes `bench`'s saved reference and its freshness, as `cargo xtask report` judges it.
fn reference_line(repo: &Path, bench: BenchTarget, tree: Option<&Tree>) -> String {
    let command = bench.reference_command();
    let Some((fields, freshness)) = reference_status(bench, tree) else {
        let criterion = repo.join("target/criterion");
        return if baseline_dirs(&criterion, Baselines::Reference(bench)).is_empty() {
            format!("missing (run `{command}`)")
        } else {
            format!("present (no record; refresh: {command})")
        };
    };
    let started = fields.get("started").map_or("unknown time", String::as_str);
    let mut notes = Vec::new();
    if fields.get("problems").is_some_and(|p| p != "none") {
        notes.push("noisy");
    }
    if fields.get("slept").is_some_and(|s| s != "no") {
        notes.push("slept");
    }
    let suffix = if notes.is_empty() {
        String::new()
    } else {
        format!(", {}", notes.join(", "))
    };
    match freshness {
        Freshness::Current => format!("current (saved {started}{suffix})"),
        Freshness::Stale => format!(
            "stale (machine, toolchain, Cargo.toml, Cargo.lock, or harness changed), saved \
             {started}{suffix} (refresh: {command})"
        ),
        Freshness::Unverifiable => format!("present (unverifiable) (saved {started}{suffix})"),
    }
}

/// Scans `target/criterion/<group>/<bench>/<baseline>/estimates.json` and groups paired
/// `--against` round directories.
fn saved_baselines(repo: &Path) -> Vec<String> {
    let criterion = repo.join("target/criterion");
    let mut raw = BTreeSet::new();
    for (_, _, bench_dir) in criterion_bench_dirs(&criterion) {
        for baseline in std::fs::read_dir(&bench_dir)
            .into_iter()
            .flatten()
            .flatten()
        {
            let Ok(name) = baseline.file_name().into_string() else {
                continue;
            };
            if matches!(name.as_str(), "new" | "change" | "reference") {
                continue;
            }
            if baseline.path().join("estimates.json").is_file() {
                raw.insert(name);
            }
        }
    }
    group_baselines(&raw)
}

/// Groups paired `NAME.a<N>` / `NAME.b<N>` baseline directories as `NAME (paired, N rounds)`
/// alongside plain baseline names.
fn group_baselines(raw: &BTreeSet<String>) -> Vec<String> {
    let mut paired: BTreeMap<&str, (BTreeSet<u32>, BTreeSet<u32>)> = BTreeMap::new();
    let mut plain: BTreeSet<&str> = BTreeSet::new();
    for name in raw {
        match RoundName::parse(name) {
            Some(RoundName {
                base,
                side,
                round: Some(round),
            }) => {
                let entry = paired.entry(base).or_default();
                if side == 'a' {
                    entry.0.insert(round);
                } else {
                    entry.1.insert(round);
                }
            }
            Some(RoundName { round: None, .. }) | None => {
                plain.insert(name.as_str());
            }
        }
    }
    let mut items: Vec<String> = plain.into_iter().map(str::to_owned).collect();
    for (base, (rounds_a, rounds_b)) in paired {
        let count_a = rounds_a.len();
        let count_b = rounds_b.len();
        let summary = if count_a == count_b {
            let unit = if count_a == 1 { "round" } else { "rounds" };
            format!("{base} (paired, {count_a} {unit})")
        } else {
            format!("{base} (paired, {count_a}/{count_b} rounds)")
        };
        items.push(summary);
    }
    items.sort();
    items
}

fn git_snapshots(repo: &Path) -> Vec<String> {
    let Some(out) = output(Command::new("git").current_dir(repo).args([
        "for-each-ref",
        "--sort=-committerdate",
        "--format=%(refname:lstrip=2)\t%(objectname:short)\t%(committerdate:short)",
        "refs/xtask",
    ])) else {
        return Vec::new();
    };
    parse_snapshots(&out)
}

fn parse_snapshots(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let name = parts.next()?.trim();
            let commit = parts.next()?.trim();
            let date = parts.next()?.trim();
            if name.is_empty() {
                return None;
            }
            Some(format!("{name} ({commit}, {date})"))
        })
        .collect()
}

/// The sorted `--save` names of the files in `dir`, which `saved_name` picks out of the
/// transient files beside them.
fn saved_names(dir: &Path, saved_name: fn(&str) -> Option<&str>) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().is_file())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter_map(|name| saved_name(&name).map(str::to_owned))
        .collect();
    names.sort();
    names
}

/// Returns relative paths of crash artifacts under `fuzz/artifacts/<target>/`.
fn fuzz_artifacts(repo: &Path) -> Vec<String> {
    let dir = repo.join("fuzz/artifacts");
    let mut artifacts = Vec::new();
    for target in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        if !target.path().is_dir() {
            continue;
        }
        let target_name = target.file_name().to_string_lossy().into_owned();
        for path in target_artifacts(repo, &target_name) {
            if let Some(file_name) = path.file_name() {
                artifacts.push(format!("{target_name}/{}", file_name.to_string_lossy()));
            }
        }
    }
    artifacts.sort();
    artifacts
}

/// Counts non-dotfile entries in each `fuzz/corpus/<target>/` directory without
/// per-file `stat` calls so large corpora stay instantaneous to inspect.
fn fuzz_corpus_counts(repo: &Path) -> BTreeMap<String, usize> {
    let dir = repo.join("fuzz/corpus");
    let mut counts = BTreeMap::new();
    for target in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        if !target.path().is_dir() {
            continue;
        }
        let target_name = target.file_name().to_string_lossy().into_owned();
        let count = visible_entries(&target.path()).count();
        if count > 0 {
            counts.insert(target_name, count);
        }
    }
    counts
}

fn format_fuzz_corpus(counts: &BTreeMap<String, usize>) -> String {
    if counts.is_empty() {
        return "none".to_owned();
    }
    let total: usize = counts.values().sum();
    let targets = counts.len();
    let unit = if targets == 1 { "target" } else { "targets" };
    let hint = if total > CORPUS_MINIMIZE_HINT_THRESHOLD {
        " (minimize: cargo xtask fuzz --all --minimize)"
    } else {
        ""
    };
    format!("{total} files across {targets} {unit}{hint}")
}

fn format_fuzz_artifacts(artifacts: &[String]) -> String {
    if artifacts.is_empty() {
        return "none".to_owned();
    }
    let unit = if artifacts.len() == 1 {
        "artifact"
    } else {
        "artifacts"
    };
    format!(
        "{} {unit}: {}",
        artifacts.len(),
        format_list(artifacts, "none")
    )
}

fn format_list(items: &[String], empty: &str) -> String {
    if items.is_empty() {
        return empty.to_owned();
    }
    if items.len() <= LIST_LIMIT {
        return items.join(", ");
    }
    let shown = items[..LIST_LIMIT].join(", ");
    format!("{shown}, ... (+{} more)", items.len() - LIST_LIMIT)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs::File;

    use super::{
        format_fuzz_artifacts, format_fuzz_corpus, format_list, group_baselines, lock_status,
        parse_snapshots, saved_asm_name, saved_profile_name,
    };
    use crate::TempDir;

    #[test]
    fn group_baselines_combines_paired_rounds_and_keeps_plain_names() {
        let raw: BTreeSet<String> = [
            "latest",
            "base",
            "exp.a1",
            "exp.b1",
            "exp.a2",
            "exp.b2",
            "exp.a3",
            "exp.b3",
            "v1.2.a1",
            "v1.2.b1",
            "partial.a1",
            "partial.a2",
            "partial.b1",
            "foo.alpha",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        assert_eq!(
            group_baselines(&raw),
            vec![
                "base",
                "exp (paired, 3 rounds)",
                "foo.alpha",
                "latest",
                "partial (paired, 2/1 rounds)",
                "v1.2 (paired, 1 round)",
            ]
        );
    }

    #[test]
    fn lock_status_detects_free_stale_shared_and_exclusive_locks() {
        let repo = TempDir::new("status-lock");
        let xtask_dir = repo.join("target/xtask");
        std::fs::create_dir_all(&xtask_dir).expect("create temp xtask dir");
        let lock_path = xtask_dir.join("measure.lock");
        let owner_path = xtask_dir.join("measure.owner");

        assert_eq!(lock_status(&repo), None);

        let shared_holder = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .expect("open lock");

        shared_holder
            .try_lock_shared()
            .expect("acquire shared lock");
        let shared = lock_status(&repo).expect("shared lock should be reported");
        assert!(shared.contains("shared build lock"), "got: {shared}");
        drop(shared_holder);

        std::fs::write(&owner_path, "pid 999: cargo xtask bench\n").expect("write owner");
        let stale = lock_status(&repo).expect("stale owner should be reported");
        assert!(stale.starts_with("stale "), "got: {stale}");

        let exclusive_holder = File::options()
            .write(true)
            .open(&lock_path)
            .expect("reopen lock");
        exclusive_holder.try_lock().expect("acquire exclusive lock");
        assert_eq!(
            lock_status(&repo).as_deref(),
            Some("ACTIVE (pid 999: cargo xtask bench)")
        );
        drop(exclusive_holder);
    }

    #[test]
    fn saved_asm_and_profile_filters_skip_transient_files() {
        assert_eq!(saved_asm_name("before.s"), Some("before"));
        assert_eq!(saved_asm_name("before.current.s"), None);
        assert_eq!(saved_asm_name("diff.before.s"), None);
        assert_eq!(saved_asm_name("diff.after.s"), None);
        assert_eq!(saved_asm_name("notes.txt"), None);

        assert_eq!(saved_profile_name("before.txt"), Some("before"));
        assert_eq!(saved_profile_name("sample.txt"), None);
        assert_eq!(saved_profile_name("build.log"), None);
        assert_eq!(saved_profile_name("profile.json.gz"), None);
    }

    #[test]
    fn parse_snapshots_extracts_expected_values() {
        let refs = "base\tabc1234\t2026-11-23\nopt1\tdef5678\t2026-11-22\n";
        assert_eq!(
            parse_snapshots(refs),
            vec!["base (abc1234, 2026-11-23)", "opt1 (def5678, 2026-11-22)"]
        );
    }

    #[test]
    fn format_fuzz_artifacts_corpus_and_list_truncate_long_output() {
        assert_eq!(format_fuzz_artifacts(&[]), "none");
        assert_eq!(
            format_fuzz_artifacts(&["thrift_skip/crash-1".to_owned()]),
            "1 artifact: thrift_skip/crash-1"
        );
        assert_eq!(format_fuzz_corpus(&BTreeMap::new()), "none");
        let small: BTreeMap<String, usize> = [("thrift_skip".to_owned(), 42)].into_iter().collect();
        assert_eq!(format_fuzz_corpus(&small), "42 files across 1 target");
        let large: BTreeMap<String, usize> = [
            ("metadata_open".to_owned(), 4_000),
            ("thrift_skip".to_owned(), 2_000),
        ]
        .into_iter()
        .collect();
        assert!(format_fuzz_corpus(&large).contains("--minimize"));

        let many: Vec<String> = (1..=10).map(|i| format!("item-{i}")).collect();
        let formatted = format_list(&many, "none");
        assert!(formatted.contains("item-1"));
        assert!(formatted.contains("item-8"));
        assert!(formatted.ends_with("... (+2 more)"));
    }
}
