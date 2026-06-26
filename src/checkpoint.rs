//! Working-tree checkpoints via a private "shadow" git repository.
//!
//! The agent mutates the live workspace directly (file tools AND bash), so a
//! plain undo is hard. We keep a dedicated git dir in the OS temp dir (keyed by
//! the workspace path) whose work-tree IS the workspace, and snapshot the whole
//! tree to a hidden commit before each turn. Restoring resets the work-tree to a
//! snapshot. The user's own `.git` (history, branch, index) is never touched —
//! git ignores the nested `.git`, and `.snippet/` is excluded — so this coexists
//! with their VCS and captures bash changes too (the gap edit-only checkpointers
//! miss). The shadow lives outside the project, so it never clutters the repo.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The shadow git-dir lives in the OS temp dir (not inside the project), keyed by a
/// stable hash of the workspace path so the same workspace always maps to the same
/// shadow across turns/restarts. The work-tree is still the workspace. Tradeoff:
/// temp is cleared on reboot, so checkpoints are session/boot-scoped (fine — they
/// exist for in-run `/rewind`, not durable history).
pub fn shadow_dir(workspace: &Path) -> PathBuf {
    let canon = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let mut h = std::collections::hash_map::DefaultHasher::new();
    canon.hash(&mut h);
    let id = format!("{:016x}", h.finish());
    std::env::temp_dir().join("snippet-shadows").join(format!("{id}.git"))
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
    if !shadow.exists() {
        if let Some(parent) = shadow.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        git(workspace, &["init", "-q"])?;
        // git already skips the nested `.git`; also keep snippet's scratch/shadow out.
        let exclude = shadow.join("info").join("exclude");
        let _ = std::fs::write(&exclude, ".snippet/\n");
    }
    // Keep the shadow repo small (idempotent; also upgrades pre-existing repos):
    // no reflogs to pin dropped snapshots, and `gc --auto` packs + prunes
    // unreachable objects promptly so disk stays bounded.
    let _ = git(workspace, &["config", "core.logAllRefUpdates", "false"]);
    let _ = git(workspace, &["config", "gc.auto", "128"]);
    let _ = git(workspace, &["config", "gc.pruneExpire", "now"]);
    let _ = git(workspace, &["config", "gc.reflogExpire", "now"]);
    let _ = git(workspace, &["config", "gc.reflogExpireUnreachable", "now"]);
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
    // Standalone commit (no parent chain) so a dropped snapshot becomes
    // unreachable and is freed by gc. Each snapshot is kept alive only by its own
    // `refs/snapshots/<id>` ref until `prune` removes it.
    let commit = git(workspace, &["commit-tree", &tree, "-m", label]).ok()?;
    let refname = format!("refs/snapshots/{commit}");
    git(workspace, &["update-ref", &refname, &commit]).ok()?;
    // Stable handle to the most recent checkpoint so the agent can review its own
    // changes since the turn began: `git --git-dir=$SNIPPET_SHADOW_GIT
    // --work-tree=. diff checkpoint`. Kept across prunes (it points into `keep`).
    let _ = git(workspace, &["update-ref", "refs/heads/checkpoint", &commit]);
    Some(commit)
}

/// Keep only the snapshots whose commit id is in `keep` reachable; drop the rest
/// and garbage-collect the shadow repo so disk stays bounded. Best-effort (never
/// errors). Also migrates the legacy `refs/heads/snapshots` chain.
pub fn prune(workspace: &Path, keep: &[String]) {
    if !git_available() || !shadow_dir(workspace).exists() {
        return;
    }
    // Ensure each retained checkpoint has its own ref — covers ids that were only
    // reachable via the old chain, so deleting the chain below won't lose them.
    for id in keep {
        let refname = format!("refs/snapshots/{id}");
        let _ = git(workspace, &["update-ref", &refname, id]);
    }
    // Drop the legacy chain branch so its intermediate commits become collectable.
    let _ = git(workspace, &["update-ref", "-d", "refs/heads/snapshots"]);
    // Delete refs for snapshots no longer retained.
    if let Ok(out) = git(
        workspace,
        &["for-each-ref", "--format=%(refname) %(objectname)", "refs/snapshots"],
    ) {
        for line in out.lines() {
            let mut it = line.split_whitespace();
            if let (Some(refname), Some(oid)) = (it.next(), it.next()) {
                if !keep.iter().any(|k| k == oid) {
                    let _ = git(workspace, &["update-ref", "-d", refname]);
                }
            }
        }
    }
    // Pack + prune now-unreachable objects (gc.auto/pruneExpire set in ensure_init).
    let _ = git(workspace, &["gc", "--auto", "--quiet"]);
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
