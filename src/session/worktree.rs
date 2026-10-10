use std::path::{Path, PathBuf};
use std::process::Command;

/// Where a new session works: an isolated git worktree of the folder, or the
/// folder itself. Always chosen by the caller; there is no implicit default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMode {
    Worktree,
    Folder,
}

/// The workspace for a new session in `folder`. A worktree needs a git repo;
/// anywhere else the folder itself is used.
pub fn session_workspace(folder: &Path, mode: WorkspaceMode) -> PathBuf {
    match mode {
        WorkspaceMode::Worktree => prepare_new_session_workspace(folder),
        WorkspaceMode::Folder => folder.to_path_buf(),
    }
}

pub fn prepare_new_session_workspace(folder: &Path) -> PathBuf {
    try_session_worktree(folder).unwrap_or_else(|| folder.to_path_buf())
}

/// The folder a linked worktree was made from, and the branch it has out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeOrigin {
    /// The same place in the main checkout: the repository root plus the
    /// workspace's path inside the worktree.
    pub folder: PathBuf,
    pub branch: Option<String>,
}

/// Where a workspace inside a linked worktree comes from, read from git's own
/// files so a session list can ask for every row without running git.
pub fn worktree_origin(folder: &Path) -> Option<WorktreeOrigin> {
    let root = linked_worktree_root(folder)?;
    let pointer = std::fs::read_to_string(root.join(".git")).ok()?;
    let gitdir = PathBuf::from(pointer.trim().strip_prefix("gitdir:")?.trim());
    let gitdir = if gitdir.is_absolute() { gitdir } else { root.join(gitdir) };
    // `<repo>/.git/worktrees/<name>`; a submodule's pointer names
    // `.git/modules/…` instead and is not a worktree.
    let worktrees = gitdir.parent()?;
    if worktrees.file_name()? != "worktrees" {
        return None;
    }
    let repo = worktrees.parent()?.parent()?;
    let inside = folder.strip_prefix(&root).unwrap_or(Path::new(""));
    let branch = std::fs::read_to_string(gitdir.join("HEAD"))
        .ok()
        .and_then(|head| {
            head.trim()
                .strip_prefix("ref: refs/heads/")
                .map(str::to_string)
        });
    Some(WorktreeOrigin {
        folder: repo.join(inside),
        branch,
    })
}

/// One checkout of a repository.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: Option<String>,
}

/// A repository's main checkout and its linked worktrees, for the folder a new
/// session is being started in. `None` outside a git repository.
pub fn repo_worktrees(folder: &Path) -> Option<(PathBuf, Vec<Worktree>)> {
    let listing = git_stdout(folder, &["worktree", "list", "--porcelain"])?;
    let mut all = Vec::new();
    for block in listing.split("\n\n") {
        let mut path = None;
        let mut branch = None;
        for line in block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
                branch = Some(b.to_string());
            }
        }
        if let Some(path) = path {
            all.push(Worktree { path, branch });
        }
    }
    if all.is_empty() {
        return None;
    }
    let main = all.remove(0);
    // Worktrees whose directory is gone are listed by git until pruned.
    all.retain(|w| w.path.is_dir());
    Some((main.path, all))
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
    // Only worktrees snippet made. A session started in a person's own
    // worktree must never delete it.
    let root = crate::config::worktrees_root();
    let root = root.canonicalize().unwrap_or(root);
    if !worktree.starts_with(&root) {
        return;
    }
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
