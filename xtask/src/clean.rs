//! `cargo xtask clean`: remove transient xtask files or named saved baselines,
//! snapshots, assembly saves, and profile breakdowns.

use std::path::{Path, PathBuf};

use crate::asm::{self, is_asm_scratch};
use crate::bench::git::git as run_git;
use crate::bench::{Baselines, BenchTarget, REFERENCE, baseline_dirs};
use crate::profile;
use crate::{Lock, lock, parse_name, rel_path, root};

#[derive(clap::Args)]
pub struct Args {
    /// Preview what would be removed without deleting anything.
    #[arg(long)]
    pub dry_run: bool,
    /// Remove a saved Criterion baseline (plain or paired) and its run record/log.
    #[arg(long = "baseline", value_name = "NAME", value_parser = parse_name)]
    pub baselines: Vec<String>,
    /// Remove a saved git snapshot (`refs/xtask/NAME`).
    #[arg(long = "snapshot", value_name = "NAME", value_parser = parse_name)]
    pub snapshots: Vec<String>,
    /// Remove a saved assembly snapshot (`target/xtask/asm/NAME.s`).
    #[arg(long = "asm", value_name = "NAME", value_parser = parse_name)]
    pub asm: Vec<String>,
    /// Remove a saved profile breakdown (`target/xtask/profile/NAME.txt`).
    #[arg(long = "profile", value_name = "NAME", value_parser = parse_name)]
    pub profiles: Vec<String>,
    /// Also remove transient logs, `--against` exports/targets, and temp files when
    /// combined with named targets (always done when no named targets are given).
    #[arg(long)]
    pub transient: bool,
}

pub fn clean(args: &Args) -> Result<(), String> {
    let _lock = lock(Lock::Measure, "clean")?;
    let repo = root();
    let has_named = !(args.baselines.is_empty()
        && args.snapshots.is_empty()
        && args.asm.is_empty()
        && args.profiles.is_empty());
    let clean_transient = args.transient || !has_named;
    let mut removed = Vec::new();

    if clean_transient {
        collect_and_clean_transient(&repo, args.dry_run, &mut removed);
    }
    for name in &args.baselines {
        clean_baseline(&repo, name, args.dry_run, &mut removed);
    }
    for name in &args.snapshots {
        clean_snapshot(name, args.dry_run, &mut removed)?;
    }
    for name in &args.asm {
        clean_asm(&repo, name, args.dry_run, &mut removed);
    }
    for name in &args.profiles {
        clean_profile(&repo, name, args.dry_run, &mut removed);
    }

    if removed.is_empty() {
        println!("nothing to clean");
        return Ok(());
    }
    let verb = if args.dry_run {
        "would remove"
    } else {
        "removed"
    };
    for item in &removed {
        println!("{verb}: {item}");
    }
    Ok(())
}

fn remove_path(path: &Path, dry_run: bool, removed: &mut Vec<String>) {
    if !path.exists() {
        return;
    }
    if dry_run {
        removed.push(rel_path(path));
        return;
    }
    let result = if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    if result.is_ok() {
        removed.push(rel_path(path));
    }
}

fn collect_and_clean_transient(repo: &Path, dry_run: bool, removed: &mut Vec<String>) {
    let xtask = repo.join("target/xtask");
    if !xtask.is_dir() {
        return;
    }
    // Whole transient directories: logs and `--against` build/export caches.
    for sub in ["check", "bench", "fuzz", "against", "against-target"] {
        remove_path(&xtask.join(sub), dry_run, removed);
    }
    // Stale temporary git index files at `target/xtask/tree*.index*`.
    for entry in std::fs::read_dir(&xtask).into_iter().flatten().flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if is_transient_root_file(&name) {
            remove_path(&entry.path(), dry_run, removed);
        }
    }
    // Transient asm diff/current files (never user `--save` files).
    let asm_dir = xtask.join("asm");
    for entry in std::fs::read_dir(&asm_dir).into_iter().flatten().flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if is_asm_scratch(&name) {
            remove_path(&entry.path(), dry_run, removed);
        }
    }
    // Transient profile files (never user `--save` breakdowns).
    let profile_dir = xtask.join("profile");
    for file in profile::SCRATCH_FILES {
        remove_path(&profile_dir.join(file), dry_run, removed);
    }
}

fn is_transient_root_file(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("tree.") else {
        return false;
    };
    let stem = rest.strip_suffix(".lock").unwrap_or(rest);
    stem == "index"
        || Path::new(stem)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("index"))
}

fn clean_baseline(repo: &Path, name: &str, dry_run: bool, removed: &mut Vec<String>) {
    let criterion = repo.join("target/criterion");
    if name == REFERENCE {
        let benches = [BenchTarget::Metadata, BenchTarget::Thrift];
        let dirs: Vec<PathBuf> = benches
            .into_iter()
            .flat_map(|bench| baseline_dirs(&criterion, Baselines::Reference(bench)))
            .collect();
        remove_baseline_dirs(
            &dirs,
            &format!("target/criterion/*/*/{REFERENCE}"),
            dry_run,
            removed,
        );
        for bench in benches {
            remove_path(
                &repo.join(format!("target/xtask/runs/{REFERENCE}-{bench}.txt")),
                dry_run,
                removed,
            );
            remove_path(
                &repo.join(format!("target/xtask/bench/{REFERENCE}-{bench}.log")),
                dry_run,
                removed,
            );
        }
        return;
    }
    remove_baseline_dirs(
        &baseline_dirs(&criterion, Baselines::Named(name)),
        &format!("target/criterion/*/*/{name}[.a*|.b*]"),
        dry_run,
        removed,
    );
    remove_path(
        &repo.join(format!("target/xtask/runs/{name}.txt")),
        dry_run,
        removed,
    );
    remove_path(
        &repo.join(format!("target/xtask/bench/{name}.log")),
        dry_run,
        removed,
    );
}

/// Removes saved baseline `dirs` (unless `dry_run`) and records them as one
/// `pattern (N dirs)` line, since a baseline spans every benchmark directory.
fn remove_baseline_dirs(dirs: &[PathBuf], pattern: &str, dry_run: bool, removed: &mut Vec<String>) {
    if dirs.is_empty() {
        return;
    }
    if !dry_run {
        for dir in dirs {
            drop(std::fs::remove_dir_all(dir));
        }
    }
    removed.push(format!("{pattern} ({} dirs)", dirs.len()));
}

fn clean_snapshot(name: &str, dry_run: bool, removed: &mut Vec<String>) -> Result<(), String> {
    let ref_name = format!("refs/xtask/{name}");
    if run_git(&["rev-parse", "--verify", "--quiet", &ref_name]).is_err() {
        return Ok(());
    }
    if !dry_run {
        run_git(&["update-ref", "-d", &ref_name])?;
    }
    removed.push(ref_name);
    Ok(())
}

fn clean_asm(repo: &Path, name: &str, dry_run: bool, removed: &mut Vec<String>) {
    let dir = repo.join("target/xtask/asm");
    remove_path(&dir.join(format!("{name}.s")), dry_run, removed);
    remove_path(&asm::current_path(&dir, name), dry_run, removed);
}

fn clean_profile(repo: &Path, name: &str, dry_run: bool, removed: &mut Vec<String>) {
    let dir = repo.join("target/xtask/profile");
    remove_path(&dir.join(format!("{name}.txt")), dry_run, removed);
}

#[cfg(test)]
mod tests {
    use super::{
        clean_asm, clean_baseline, clean_profile, collect_and_clean_transient,
        is_transient_root_file,
    };
    use crate::TempDir;

    #[test]
    fn transient_cleanup_preserves_saved_asm_profiles_and_runs() {
        let repo = TempDir::new("clean");
        let xtask = repo.join("target/xtask");
        for dir in [
            xtask.join("check"),
            xtask.join("against/tree"),
            xtask.join("asm"),
            xtask.join("profile"),
            xtask.join("runs"),
            repo.join("target/criterion/medium/open/exp.a1"),
            repo.join("target/criterion/medium/open/exp.b1"),
            repo.join("target/criterion/medium/open/keep"),
        ] {
            std::fs::create_dir_all(&dir).expect("create temp dirs");
        }
        std::fs::write(xtask.join("tree.index"), b"stale").expect("write stale index");
        std::fs::write(xtask.join("asm/saved.s"), b"ret").expect("write saved asm");
        std::fs::write(xtask.join("asm/saved.current.s"), b"ret").expect("write current asm");
        std::fs::write(xtask.join("asm/diff.before.s"), b"ret").expect("write diff tmp");
        std::fs::write(xtask.join("profile/saved.txt"), b"1.0\tcore").expect("write saved prof");
        std::fs::write(xtask.join("profile/sample.txt"), b"raw").expect("write sample");
        std::fs::write(xtask.join("runs/exp.txt"), b"name: exp\n").expect("write exp run");
        std::fs::write(xtask.join("runs/keep.txt"), b"name: keep\n").expect("write keep run");

        // Dry run touches nothing.
        let mut dry = Vec::new();
        collect_and_clean_transient(&repo, true, &mut dry);
        assert!(dry.iter().any(|p| p.ends_with("tree.index")));
        assert!(xtask.join("tree.index").exists());
        assert!(xtask.join("asm/saved.current.s").exists());

        // Real transient clean removes transient files only, keeping saved artifacts.
        let mut removed = Vec::new();
        collect_and_clean_transient(&repo, false, &mut removed);
        assert!(!xtask.join("tree.index").exists());
        assert!(!xtask.join("check").exists());
        assert!(!xtask.join("against").exists());
        assert!(!xtask.join("asm/saved.current.s").exists());
        assert!(!xtask.join("asm/diff.before.s").exists());
        assert!(!xtask.join("profile/sample.txt").exists());
        assert!(xtask.join("asm/saved.s").exists());
        assert!(xtask.join("profile/saved.txt").exists());
        assert!(xtask.join("runs/exp.txt").exists());

        // Named clean removes only the named baseline / asm / profile.
        let mut named = Vec::new();
        clean_baseline(&repo, "exp", false, &mut named);
        clean_asm(&repo, "saved", false, &mut named);
        clean_profile(&repo, "saved", false, &mut named);
        assert!(!repo.join("target/criterion/medium/open/exp.a1").exists());
        assert!(!repo.join("target/criterion/medium/open/exp.b1").exists());
        assert!(!xtask.join("runs/exp.txt").exists());
        assert!(!xtask.join("asm/saved.s").exists());
        assert!(!xtask.join("profile/saved.txt").exists());
        assert!(repo.join("target/criterion/medium/open/keep").exists());
        assert!(xtask.join("runs/keep.txt").exists());

        assert!(is_transient_root_file("tree.index"));
        assert!(is_transient_root_file("tree.1234.index.lock"));
        assert!(!is_transient_root_file("measure.lock"));
    }
}
