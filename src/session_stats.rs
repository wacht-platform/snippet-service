//! Extras for the session list: uncommitted change size per workspace and when a run began.
//!
//! The list only ever READS the cache. A background thread refreshes stale entries, so
//! `git` never runs on the request path.

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;

pub const TTL_SECS: i64 = 30;
pub const MAX_SESSIONS: usize = 50;
const MAX_CONCURRENT_GIT: usize = 3;
const GIT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Changes {
    pub files: u32,
    pub added: u32,
    pub removed: u32,
}

/// `git diff --shortstat` output to a size; `None` when it reports no changes.
pub fn parse_shortstat(text: &str) -> Option<Changes> {
    let mut c = Changes { files: 0, added: 0, removed: 0 };
    for part in text.split(',') {
        let part = part.trim();
        let Some(n) = part.split_whitespace().next().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        if part.contains("changed") {
            c.files = n;
        } else if part.contains("insertion") {
            c.added = n;
        } else if part.contains("deletion") {
            c.removed = n;
        }
    }
    (c.files > 0).then_some(c)
}

#[derive(Default)]
struct Entry {
    value: Option<Changes>,
    fetched_at: Option<i64>,
    refreshing: bool,
}

/// Per-key cache that serves stale values while a refresh runs.
#[derive(Default)]
pub struct ChangeCache {
    entries: HashMap<String, Entry>,
}

impl ChangeCache {
    /// The cached value and whether the caller must start a refresh (claimed here).
    pub fn read(&mut self, key: &str, now: i64) -> (Option<Changes>, bool) {
        let e = self.entries.entry(key.to_string()).or_default();
        let stale = e.fetched_at.is_none_or(|at| now - at >= TTL_SECS);
        let start = stale && !e.refreshing;
        if start {
            e.refreshing = true;
        }
        (e.value, start)
    }

    pub fn store(&mut self, key: &str, value: Option<Changes>, now: i64) {
        let e = self.entries.entry(key.to_string()).or_default();
        e.value = value;
        e.fetched_at = Some(now);
        e.refreshing = false;
    }

    /// A refresh that could not run: release the claim, keep the old value.
    pub fn abandon(&mut self, key: &str) {
        if let Some(e) = self.entries.get_mut(key) {
            e.refreshing = false;
        }
    }
}

static CACHE: Mutex<Option<ChangeCache>> = Mutex::new(None);
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
static RUN_STARTS: Mutex<Option<HashMap<String, i64>>> = Mutex::new(None);

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn git_changes(folder: &str) -> Option<Changes> {
    if !Path::new(folder).join(".git").exists() {
        return None;
    }
    let mut child = Command::new("git")
        .args(["-C", folder, "diff", "--shortstat", "HEAD"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + GIT_TIMEOUT;
    loop {
        match child.try_wait().ok()? {
            Some(status) if status.success() => break,
            Some(_) => return None,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
    parse_shortstat(&out)
}

fn refresh_in_background(folder: String) {
    if IN_FLIGHT.fetch_add(1, Ordering::AcqRel) >= MAX_CONCURRENT_GIT {
        IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
        if let Some(c) = CACHE.lock().unwrap().as_mut() {
            c.abandon(&folder);
        }
        return;
    }
    std::thread::spawn(move || {
        let value = git_changes(&folder);
        CACHE.lock().unwrap().get_or_insert_with(Default::default).store(&folder, value, now());
        IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
    });
}

/// Fill `changes` from the cache for the most recently active sessions, starting
/// refreshes for stale entries. `sessions` is in list order (most recent first).
pub fn attach_changes(sessions: &mut [crate::session::SessionInfo]) {
    let t = now();
    let mut seen = std::collections::HashSet::new();
    let mut counted = 0;
    for s in sessions.iter_mut() {
        if crate::mission_control::is_session_id(&s.id) || crate::session::is_inbox_session_id(&s.id) {
            continue;
        }
        if counted >= MAX_SESSIONS {
            break;
        }
        counted += 1;
        let (value, start) = CACHE
            .lock()
            .unwrap()
            .get_or_insert_with(Default::default)
            .read(&s.folder, t);
        if start && seen.insert(s.folder.clone()) {
            refresh_in_background(s.folder.clone());
        }
        s.changes = value;
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortstat_parsing() {
        let c = parse_shortstat(" 3 files changed, 10 insertions(+), 2 deletions(-)\n").unwrap();
        assert_eq!(c, Changes { files: 3, added: 10, removed: 2 });
        let c = parse_shortstat(" 1 file changed, 1 insertion(+)\n").unwrap();
        assert_eq!(c, Changes { files: 1, added: 1, removed: 0 });
        let c = parse_shortstat(" 2 files changed, 5 deletions(-)").unwrap();
        assert_eq!(c, Changes { files: 2, added: 0, removed: 5 });
        assert_eq!(parse_shortstat(""), None);
        assert_eq!(parse_shortstat("garbage"), None);
    }

    #[test]
    fn cache_serves_stale_while_one_refresh_runs() {
        let mut c = ChangeCache::default();
        let (v, start) = c.read("w", 100);
        assert!((v, start) == (None, true), "first read starts a refresh");
        assert_eq!(c.read("w", 101), (None, false), "refresh already claimed");
        let one = Some(Changes { files: 1, added: 2, removed: 3 });
        c.store("w", one, 105);
        assert_eq!(c.read("w", 105 + TTL_SECS - 1), (one, false), "fresh");
        assert_eq!(c.read("w", 105 + TTL_SECS), (one, true), "stale value served, refresh starts");
        assert_eq!(c.read("w", 105 + TTL_SECS + 1), (one, false), "only one refresh at a time");
        c.store("w", None, 200);
        assert_eq!(c.read("w", 201), (None, false), "cleared value is cached");
    }

    #[test]
    fn abandoned_refresh_can_be_retried() {
        let mut c = ChangeCache::default();
        assert!(c.read("w", 1).1);
        c.abandon("w");
        assert!(c.read("w", 2).1);
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
}
