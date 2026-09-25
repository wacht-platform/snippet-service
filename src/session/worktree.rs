use std::path::{Path, PathBuf};
use std::process::Command;

pub fn prepare_new_session_workspace(folder: &Path) -> PathBuf {
    try_session_worktree(folder).unwrap_or_else(|| folder.to_path_buf())
}

pub(crate) fn git_stdout(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn sanitize_repo_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if s.is_empty() { "repo".into() } else { s }
}

fn unique_worktree_path(parent: &Path) -> Option<PathBuf> {
    for _ in 0..8 {
        let id = uuid::Uuid::new_v4().to_string();
        let dest = parent.join(&id[..8]);
        if !dest.exists() {
            return Some(dest);
        }
    }
    Some(parent.join(uuid::Uuid::new_v4().to_string()))
}

fn try_session_worktree(folder: &Path) -> Option<PathBuf> {
    let inside = git_stdout(folder, &["rev-parse", "--is-inside-work-tree"])?;
    if inside != "true" {
        return None;
    }
    let toplevel = PathBuf::from(git_stdout(folder, &["rev-parse", "--show-toplevel"])?);
    let root = crate::config::worktrees_root();
    if folder.starts_with(&root) || toplevel.starts_with(&root) {
        return None;
    }
    let repo = sanitize_repo_name(toplevel.file_name()?.to_str()?);
    let parent = root.join(&repo);
    std::fs::create_dir_all(&parent).ok()?;
    let dest = unique_worktree_path(&parent)?;
    let branch = dest
        .file_name()
        .and_then(|s| s.to_str())
        .map(|id| format!("snippet/{id}"))
        .unwrap_or_else(|| "snippet/session".into());
    let status = Command::new("git")
        .arg("-C")
        .arg(&toplevel)
        .args(["worktree", "add", "-b", &branch])
        .arg(&dest)
        .status()
        .ok()?;
    if !status.success() {
        let _ = std::fs::remove_dir_all(&dest);
        return None;
    }
    let workspace = match folder.strip_prefix(&toplevel) {
        Ok(rel) if !rel.as_os_str().is_empty() => dest.join(rel),
        _ => dest,
    };
    Some(workspace)
}

pub(crate) fn workspace_is_worktree(folder: &Path) -> bool {
    let folder = folder
        .canonicalize()
        .unwrap_or_else(|_| folder.to_path_buf());
    linked_worktree_root(&folder).is_some()
}

pub(crate) fn drop_session_worktree(folder: &Path) {
    let folder = folder
        .canonicalize()
        .unwrap_or_else(|_| folder.to_path_buf());
    let Some(worktree) = linked_worktree_root(&folder) else {
        return;
    };
    if let Some(common) = git_stdout(&worktree, &["rev-parse", "--git-common-dir"]) {
        let common_path = PathBuf::from(&common);
        let common_path = if common_path.is_absolute() {
            common_path
        } else {
            worktree.join(common_path)
        };
        if let Some(main) = common_path.parent() {
            let _ = Command::new("git")
                .arg("-C")
                .arg(main)
                .args(["worktree", "remove", "--force"])
                .arg(&worktree)
                .status();
        }
    }
    if worktree.exists() {
        let _ = std::fs::remove_dir_all(&worktree);
    }
}

pub(crate) fn linked_worktree_root(folder: &Path) -> Option<PathBuf> {
    let mut cur = folder.to_path_buf();
    loop {
        let git = cur.join(".git");
        if git.is_file() {
            return Some(cur);
        }
        if git.is_dir() {
            return None;
        }
        if !cur.pop() {
            return None;
        }
    }
}
