//! Git operations, done by shelling out to the `git` binary (S8).

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

pub fn run(home: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(home)
        .output()
        .context("failed to run git")?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("git {} failed: {}", args.join(" "), stderr.trim())
}

pub fn capture(home: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(home)
        .output()
        .context("failed to run git")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Whether a path has uncommitted changes (staged, unstaged, or untracked).
pub fn is_dirty(home: &Path, pathspec: &str) -> Result<bool> {
    let output = capture(home, &["status", "--porcelain", "--", pathspec])?;
    Ok(!output.trim().is_empty())
}

/// Stage the given paths and commit. A no-op if nothing changed.
pub fn commit(home: &Path, pathspecs: &[&str], message: &str) -> Result<bool> {
    let mut add = vec!["add", "--"];
    add.extend(pathspecs);
    run(home, &add)?;

    let staged = capture(home, &["diff", "--cached", "--name-only"])?;
    if staged.trim().is_empty() {
        return Ok(false);
    }
    run(home, &["commit", "-m", message])?;
    Ok(true)
}
