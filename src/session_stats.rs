//! Extras for the session list: last-turn change size and when a run began.

use serde::{Deserialize, Serialize};

/// Totals of the most recent turn that changed files (from its `turn_changes` event).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changes {
    pub files: u32,
    pub added: u32,
    pub removed: u32,
}

static RUN_STARTS: std::sync::Mutex<Option<std::collections::HashMap<String, i64>>> = std::sync::Mutex::new(None);

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// When the session's current run began, while it is running.
pub fn running_since(id: &str, status: &str, turn_started_at: Option<&str>, observed_now: i64) -> Option<i64> {
    let mut map = RUN_STARTS.lock().unwrap();
    let map = map.get_or_insert_with(Default::default);
    if status != "running" {
        map.remove(id);
        return None;
    }
    if let Some(at) = turn_started_at.and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()) {
        return Some(at.timestamp());
    }
    Some(*map.entry(id.to_string()).or_insert(observed_now))
}

/// `running_since` for a listed session, reading the harness's turn start when it has one.
pub fn attach_running_since(s: &mut crate::session::SessionInfo) {
    let turn = (s.status == "running")
        .then(|| crate::session::state_path_for_id(&s.id).and_then(|p| crate::session::read_session_meta(&p)))
        .flatten()
        .and_then(|st| st.turn_started_at);
    s.running_since = running_since(&s.id, &s.status, turn.as_deref(), now());
}

/// How long a folder's git lookup is reused by the session list.
pub const GIT_TTL_SECS: i64 = 10;

/// The branch (or short sha when detached) of the repo containing `folder`, read from `.git`
/// without running git. `None` when the folder is not inside a repo.
pub fn git_branch(folder: &std::path::Path) -> Option<String> {
    for dir in folder.ancestors() {
        let dot_git = dir.join(".git");
        let gitdir = if dot_git.is_dir() {
            dot_git
        } else if dot_git.is_file() {
            let text = std::fs::read_to_string(&dot_git).ok()?;
            let target = std::path::Path::new(text.trim().strip_prefix("gitdir:")?.trim());
            if target.is_absolute() { target.to_path_buf() } else { dir.join(target) }
        } else {
            continue;
        };
        return parse_head(&std::fs::read_to_string(gitdir.join("HEAD")).ok()?);
    }
    None
}

fn parse_head(head: &str) -> Option<String> {
    let head = head.trim();
    if let Some(reference) = head.strip_prefix("ref:") {
        let reference = reference.trim();
        let name = reference.strip_prefix("refs/heads/").or_else(|| reference.strip_prefix("refs/")).unwrap_or(reference);
        return (!name.is_empty()).then(|| name.to_string());
    }
    (head.len() >= 7 && head.chars().all(|c| c.is_ascii_hexdigit())).then(|| head[..7].to_string())
}

/// Per-folder cache with a TTL, so a list of many sessions reads each folder once.
#[derive(Default)]
pub struct GitCache {
    entries: std::collections::HashMap<String, (i64, Option<String>)>,
}

impl GitCache {
    pub fn get(&mut self, folder: &str, now: i64, resolve: impl FnOnce() -> Option<String>) -> Option<String> {
        if let Some((at, value)) = self.entries.get(folder)
            && now - at < GIT_TTL_SECS
        {
            return value.clone();
        }
        let value = resolve();
        self.entries.insert(folder.to_string(), (now, value.clone()));
        value
    }
}

static GIT_CACHE: std::sync::Mutex<Option<GitCache>> = std::sync::Mutex::new(None);

/// Mark every listed session that lives in a git repo with `git: true` and its `branch`.
/// A branch already set (worktree sessions) is kept.
pub fn attach_git(sessions: &mut [crate::session::SessionInfo]) {
    let t = now();
    for s in sessions.iter_mut() {
        if crate::mission_control::is_session_id(&s.id) || crate::session::is_inbox_session_id(&s.id) {
            continue;
        }
        let found = GIT_CACHE
            .lock()
            .unwrap()
            .get_or_insert_with(Default::default)
            .get(&s.folder, t, || git_branch(std::path::Path::new(&s.folder)));
        if let Some(branch) = found {
            s.git = true;
            if s.branch.is_none() {
                s.branch = Some(branch);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_totals_round_trip_from_the_stored_json() {
        let c: Changes = serde_json::from_str(r#"{"files":3,"added":10,"removed":2}"#).unwrap();
        assert_eq!(c, Changes { files: 3, added: 10, removed: 2 });
    }

    #[test]
    fn running_since_prefers_turn_start_then_first_observation() {
        let id = "rs-test";
        assert_eq!(running_since(id, "idle", None, 50), None);
        assert_eq!(running_since(id, "running", None, 100), Some(100));
        assert_eq!(running_since(id, "running", None, 200), Some(100), "kept while running");
        assert_eq!(running_since(id, "running", Some("1970-01-01T00:01:00Z"), 300), Some(60));
        assert_eq!(running_since(id, "idle", None, 400), None);
        assert_eq!(running_since(id, "running", None, 500), Some(500), "cleared on stop");
        running_since(id, "idle", None, 0);
    }

    fn write(path: &std::path::Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn git_branch_cases() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();

        write(&root.join("repo/.git/HEAD"), "ref: refs/heads/feature/x\n");
        std::fs::create_dir_all(root.join("repo/a/b")).unwrap();
        assert_eq!(git_branch(&root.join("repo")).as_deref(), Some("feature/x"));
        assert_eq!(git_branch(&root.join("repo/a/b")).as_deref(), Some("feature/x"), "nested subfolder");

        write(&root.join("main/.git/worktrees/wt/HEAD"), "ref: refs/heads/wt-branch\n");
        write(&root.join("wt/.git"), &format!("gitdir: {}\n", root.join("main/.git/worktrees/wt").display()));
        assert_eq!(git_branch(&root.join("wt")).as_deref(), Some("wt-branch"));
        write(&root.join("rel/.git"), "gitdir: ../main/.git/worktrees/wt\n");
        assert_eq!(git_branch(&root.join("rel")).as_deref(), Some("wt-branch"), "relative gitdir");

        write(&root.join("det/.git/HEAD"), "0123456789abcdef0123456789abcdef01234567\n");
        assert_eq!(git_branch(&root.join("det")).as_deref(), Some("0123456"));

        std::fs::create_dir_all(root.join("plain")).unwrap();
        // The temp dir itself may sit under a repo on some machines; only assert on a bare root.
        if git_branch(root).is_none() {
            assert_eq!(git_branch(&root.join("plain")), None);
        }
    }

    #[test]
    fn git_cache_reuses_within_ttl() {
        let mut c = GitCache::default();
        let mut calls = 0;
        let mut get = |c: &mut GitCache, now| {
            c.get("f", now, || {
                calls += 1;
                Some(format!("b{calls}"))
            })
        };
        assert_eq!(get(&mut c, 100).as_deref(), Some("b1"));
        assert_eq!(get(&mut c, 100 + GIT_TTL_SECS - 1).as_deref(), Some("b1"));
        assert_eq!(get(&mut c, 100 + GIT_TTL_SECS).as_deref(), Some("b2"));
    }
}
