//! Working-tree checkpoints via a private "shadow" git repository.
//!
//! The agent mutates the live workspace directly (file tools AND bash), so a
//! plain undo is hard. We keep a dedicated git dir under `<workspace>/.snippet/`
//! whose work-tree IS the workspace, and snapshot the whole tree to a hidden
//! commit before each turn. Restoring resets the work-tree to a snapshot. The
//! user's own `.git` (history, branch, index) is never touched — git ignores the
//! nested `.git`, and `.snippet/` is excluded — so this coexists with their VCS
//! and captures bash changes too (the gap edit-only checkpointers miss).

use std::path::{Path, PathBuf};
use std::process::Command;

fn shadow_dir(workspace: &Path) -> PathBuf {
    workspace.join(".snippet").join("shadow.git")
}

/// Run a git command against the shadow repo (its own git-dir, work-tree = the
/// workspace). Identity is supplied via env so no repo config is required.
fn git(workspace: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(shadow_dir(workspace))
        .arg("--work-tree")
        .arg(workspace)
        .args(args)
        .current_dir(workspace)
        .env("GIT_AUTHOR_NAME", "snippet")
        .env("GIT_AUTHOR_EMAIL", "snippet@local")
        .env("GIT_COMMITTER_NAME", "snippet")
        .env("GIT_COMMITTER_EMAIL", "snippet@local")
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Create the shadow repo on first use and exclude snippet's own dirs.
fn ensure_init(workspace: &Path) -> Result<(), String> {
    let shadow = shadow_dir(workspace);
    if shadow.exists() {
        return Ok(());
    }
    if let Some(parent) = shadow.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    git(workspace, &["init", "-q"])?;
    // git already skips the nested `.git`; also keep snippet's scratch/shadow out.
    let exclude = shadow.join("info").join("exclude");
    let _ = std::fs::write(&exclude, ".snippet/\n");
    Ok(())
}

/// Snapshot the whole work-tree to a hidden commit and return its id. Best-effort:
/// returns `None` (never errors the turn) if git is missing or the snapshot fails.
pub fn snapshot(workspace: &Path, label: &str) -> Option<String> {
    if !git_available() {
        return None;
    }
    ensure_init(workspace).ok()?;
    git(workspace, &["add", "-A"]).ok()?;
    let tree = git(workspace, &["write-tree"]).ok()?;
    let parent = git(workspace, &["rev-parse", "-q", "--verify", "refs/heads/snapshots"])
        .ok()
        .filter(|s| !s.is_empty());

    let mut args: Vec<String> = vec!["commit-tree".into(), tree, "-m".into(), label.into()];
    if let Some(parent) = &parent {
        args.push("-p".into());
        args.push(parent.clone());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let commit = git(workspace, &arg_refs).ok()?;
    git(workspace, &["update-ref", "refs/heads/snapshots", &commit]).ok()?;
    Some(commit)
}

/// Restore the work-tree to a checkpoint commit. Snapshots the current state first
/// so files created since the checkpoint are tracked and get removed cleanly, then
/// resets index + work-tree to the target tree.
pub fn restore(workspace: &Path, commit: &str) -> Result<(), String> {
    if !git_available() {
        return Err("git is not available — checkpoints need git installed.".to_string());
    }
    ensure_init(workspace)?;
    let _ = snapshot(workspace, "pre-restore safety snapshot");
    git(workspace, &["read-tree", "-u", "--reset", commit])?;
    Ok(())
}
