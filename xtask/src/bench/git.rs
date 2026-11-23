//! Git working-tree hashing, commit export for `--against`, and `snapshot`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::{ChildGuard, output_checked, parse_name, rel_path, run, xtask_dir};

#[derive(clap::Args)]
pub struct SnapshotArgs {
    /// Snapshot name, stored as `refs/xtask/NAME`.
    #[arg(value_name = "NAME", value_parser = parse_name)]
    pub name: String,
}

/// A version of the code: the commit it is based on and the exact tree hash measured.
pub struct Tree {
    pub commit: String,
    pub hash: String,
    pub dirty: bool,
}

pub fn git(args: &[&str]) -> Result<String, String> {
    git_with_env(&[], args)
}

/// Runs `git ARGS` from the repository root with extra environment variables, and
/// returns its trimmed stdout.
fn git_with_env(env: &[(&str, &OsStr)], args: &[&str]) -> Result<String, String> {
    let mut command = Command::new("git");
    command
        .current_dir(crate::root())
        .envs(env.iter().copied())
        .args(args);
    output_checked(&mut command, &format!("`git {}`", args.join(" "))).map(|s| s.trim().to_owned())
}

/// Removes a temporary git index file on drop, preventing races and leftover files.
struct TempIndex(PathBuf);

impl Drop for TempIndex {
    fn drop(&mut self) {
        drop(std::fs::remove_file(&self.0));
        let mut lock = self.0.as_os_str().to_owned();
        lock.push(".lock");
        drop(std::fs::remove_file(PathBuf::from(lock)));
    }
}

/// Hashes the working tree, untracked files included, without touching the index:
/// `git add -A` runs against a PID-scoped temporary copy of it.
pub fn working_tree() -> Result<Tree, String> {
    let temp = TempIndex(xtask_dir("")?.join(format!("tree.{}.index", std::process::id())));
    let real_index = crate::root().join(git(&["rev-parse", "--git-path", "index"])?);
    // Starting from the real index keeps git's stat cache, so unchanged files are not
    // hashed again. Without one, `add -A` hashes everything, which is slower, not wrong.
    drop(std::fs::copy(&real_index, &temp.0));
    let index = [("GIT_INDEX_FILE", temp.0.as_os_str())];
    git_with_env(&index, &["add", "-A"])?;
    let hash = git_with_env(&index, &["write-tree"])?;
    let commit = git(&["rev-parse", "HEAD"])?;
    let dirty = hash != git(&["rev-parse", "HEAD^{tree}"])?;
    Ok(Tree {
        commit,
        hash,
        dirty,
    })
}

/// A snapshot name (`refs/xtask/NAME`) or any git revision, as a full commit hash.
pub fn resolve(reference: &str) -> Result<String, String> {
    let snapshot = format!("refs/xtask/{reference}^{{commit}}");
    git(&["rev-parse", "--verify", "--quiet", &snapshot])
        .ok()
        .or_else(|| {
            git(&[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{reference}^{{commit}}"),
            ])
            .ok()
        })
        .ok_or_else(|| format!("`{reference}` is neither a snapshot nor a git revision"))
}

/// Exports `commit`'s files to the reusable directory `target/xtask/against/tree/`,
/// with submodules linked to this checkout's, and returns the directory.
///
/// Records `commit` in `target/xtask/against/tree.rev` once export completes; if
/// `tree/` already holds `commit`, skips re-exporting so Cargo's incremental cache in
/// `against-target/` stays warm. When switching commits, wipes `tree/` and extracts
/// with `tar -xm` so file mtimes are current and Cargo rebuilds changed crates.
pub fn export(commit: &str) -> Result<PathBuf, String> {
    let against_dir = xtask_dir("against")?;
    let dir = against_dir.join("tree");
    let rev_file = against_dir.join("tree.rev");
    if dir.is_dir()
        && std::fs::read_to_string(&rev_file)
            .ok()
            .as_deref()
            .map(str::trim)
            == Some(commit)
    {
        return Ok(dir);
    }

    // Clear the rev marker first so an interrupted export is never mistaken for complete.
    drop(std::fs::remove_file(&rev_file));
    drop(std::fs::remove_dir_all(&dir));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", rel_path(&dir)))?;
    let mut archive = Command::new("git");
    let mut archive = archive
        .current_dir(crate::root())
        .args(["archive", "--format=tar", commit])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to start git archive: {e}"))?;
    let tar = archive.stdout.take().ok_or("git archive has no stdout")?;
    let archive_guard = ChildGuard::new(archive);
    run(Command::new("tar").args(["-xm", "-C"]).arg(&dir).stdin(tar))?;
    let archived = archive_guard.wait()?;
    if !archived.success() {
        return Err(format!("git archive {commit} failed"));
    }
    // `git archive` leaves submodules out; the fixtures live in one.
    let modules = git(&["config", "-f", ".gitmodules", "--get-regexp", "path"]).unwrap_or_default();
    for path in modules
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
    {
        let link = dir.join(path);
        drop(std::fs::remove_dir(&link));
        if let Some(parent) = link.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", rel_path(parent)))?;
        }
        symlink(&crate::root().join(path), &link)?;
    }
    std::fs::write(&rev_file, format!("{commit}\n"))
        .map_err(|e| format!("{}: {e}", rel_path(&rev_file)))?;
    Ok(dir)
}

#[cfg(unix)]
fn symlink(original: &Path, link: &Path) -> Result<(), String> {
    std::os::unix::fs::symlink(original, link).map_err(|e| format!("{}: {e}", rel_path(link)))
}

#[cfg(not(unix))]
fn symlink(_original: &Path, link: &Path) -> Result<(), String> {
    Err(format!(
        "{}: --against needs symlinks (unix only)",
        rel_path(link)
    ))
}

pub fn snapshot(args: &SnapshotArgs) -> Result<(), String> {
    let name = &args.name;
    let tree = working_tree()?;
    let message = format!("xtask snapshot {name}");
    // A fixed identity: the snapshot is a local measuring aid, not authored work.
    let identity = [
        ("GIT_AUTHOR_NAME", OsStr::new("xtask")),
        ("GIT_AUTHOR_EMAIL", OsStr::new("xtask@localhost")),
        ("GIT_COMMITTER_NAME", OsStr::new("xtask")),
        ("GIT_COMMITTER_EMAIL", OsStr::new("xtask@localhost")),
    ];
    let commit = git_with_env(
        &identity,
        &[
            "commit-tree",
            &tree.hash,
            "-p",
            &tree.commit,
            "-m",
            &message,
        ],
    )?;
    git(&["update-ref", &format!("refs/xtask/{name}"), &commit])?;
    println!(
        "refs/xtask/{name} = {} (tree {}{})",
        short(&commit),
        short(&tree.hash),
        if tree.dirty {
            ", with uncommitted changes"
        } else {
            ""
        }
    );
    println!("measure it with: cargo xtask bench --against {name} --save NAME");
    Ok(())
}

pub fn short(hash: &str) -> &str {
    &hash[..hash.len().min(10)]
}
