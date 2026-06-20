//! Workspace write-coordination for the conversation agent and its delegated lanes.
//!
//! Lanes share one workspace, so two writing the same file would clobber each
//! other. Two mechanisms keep that safe:
//!
//! 1. **Short locks (≤30s).** A lock claims a file/folder for one owner around the
//!    moment it writes. While held, every *other* owner is read-only on that path.
//!    Locks are advisory advance-notice — an agent announces what it's about to
//!    touch so others can plan — and expire quickly so a crashed lane can't block
//!    the workspace. They live on disk as one JSON file per lock (`.snippet/locks/`),
//!    so any agent can list/read the board directly.
//! 2. **Read-before-write.** Every read records the file version the owner saw; every
//!    write bumps that version. If another owner has written a file since you last
//!    read it, your write is rejected until you re-read it. This is the real guard —
//!    locks just reduce contention.
//!
//! Enforced on `write_file` / `edit_file`; `bash` bypasses both, so file edits should
//! go through the file tools.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::Utc;
use serde::{Deserialize, Serialize};

/// How long a lock stays valid after it's claimed. Locks are meant to wrap a single
/// write, so this is short — past it the lock is treated as absent and its file is
/// swept, which keeps a crashed lane from blocking writers.
const LOCK_TTL_SECONDS: i64 = 30;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockInfo {
    pub path: PathBuf,
    pub owner: String,
    pub reason: String,
    pub claimed_at: String,
    pub expires_at: String,
}

impl LockInfo {
    fn is_expired(&self) -> bool {
        match chrono::DateTime::parse_from_rfc3339(&self.expires_at) {
            Ok(expiry) => Utc::now() >= expiry,
            // Unparseable expiry: keep the lock rather than silently dropping it.
            Err(_) => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeInfo {
    pub path: PathBuf,
    pub owner: String,
}

/// Why a write was rejected.
pub enum WriteBlock {
    /// Another owner holds a lock over the path (or an ancestor folder).
    Locked(LockInfo),
    /// Another owner has written the file since this owner last read it.
    Stale { last_writer: String },
}

/// In-memory version bookkeeping for read-before-write. Shared across all owners in
/// the run; the same mutex also serializes lock claims so scan-then-write is atomic.
#[derive(Default)]
struct Inner {
    /// path -> (write count, last writer).
    versions: HashMap<PathBuf, (u64, String)>,
    /// (owner, path) -> the version that owner last observed via a read or its own write.
    seen: HashMap<(String, PathBuf), u64>,
}

/// Shared across the parent agent and every lane (via `Arc`). Locks live as files
/// under `dir`; version state lives in memory (lanes are tasks in one process).
pub struct LockRegistry {
    dir: PathBuf,
    changes_file: PathBuf,
    inner: Mutex<Inner>,
}

impl LockRegistry {
    pub fn new(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        let changes_file = dir.join("_changes.json");
        Self {
            changes_file,
            dir,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Claim a lock for `owner` over `path`. Returns the conflicting lock when
    /// another owner already holds the path or an ancestor/descendant of it.
    pub fn claim(&self, path: &Path, owner: &str, reason: &str) -> Result<(), LockInfo> {
        let _guard = self.inner.lock().unwrap();
        for lock in self.read_locks() {
            if lock.owner != owner && (covers(&lock.path, path) || covers(path, &lock.path)) {
                return Err(lock);
            }
        }
        let now = Utc::now();
        let info = LockInfo {
            path: path.to_path_buf(),
            owner: owner.to_string(),
            reason: reason.to_string(),
            claimed_at: now.to_rfc3339(),
            expires_at: (now + chrono::Duration::seconds(LOCK_TTL_SECONDS)).to_rfc3339(),
        };
        if let Ok(bytes) = serde_json::to_vec_pretty(&info) {
            let _ = std::fs::write(self.dir.join(lock_filename(path)), bytes);
        }
        Ok(())
    }

    pub fn release(&self, path: &Path, owner: &str) {
        let _guard = self.inner.lock().unwrap();
        for (file, lock) in self.read_lock_files() {
            if lock.path == path && lock.owner == owner {
                let _ = std::fs::remove_file(file);
            }
        }
    }

    /// Release every lock held by `owner` — called when a lane finishes.
    pub fn release_all(&self, owner: &str) {
        let _guard = self.inner.lock().unwrap();
        for (file, lock) in self.read_lock_files() {
            if lock.owner == owner {
                let _ = std::fs::remove_file(file);
            }
        }
    }

    /// Record that `owner` has seen the current version of `path` (called on read).
    pub fn mark_read(&self, path: &Path, owner: &str) {
        let mut inner = self.inner.lock().unwrap();
        let version = inner.versions.get(path).map(|(v, _)| *v).unwrap_or(0);
        inner.seen.insert((owner.to_string(), path.to_path_buf()), version);
    }

    /// Whether `owner` may write `path`. Blocked when another owner holds a covering
    /// lock, or has written the file since this owner last read it.
    pub fn check_write(&self, path: &Path, owner: &str) -> Result<(), WriteBlock> {
        for lock in self.read_locks() {
            if lock.owner != owner && covers(&lock.path, path) {
                return Err(WriteBlock::Locked(lock));
            }
        }
        let inner = self.inner.lock().unwrap();
        if let Some((version, last_writer)) = inner.versions.get(path) {
            if last_writer != owner {
                let seen = inner
                    .seen
                    .get(&(owner.to_string(), path.to_path_buf()))
                    .copied()
                    .unwrap_or(0);
                if seen < *version {
                    return Err(WriteBlock::Stale {
                        last_writer: last_writer.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    pub fn record_change(&self, path: &Path, owner: &str) {
        let mut inner = self.inner.lock().unwrap();
        let version = {
            let entry = inner
                .versions
                .entry(path.to_path_buf())
                .or_insert((0, owner.to_string()));
            entry.0 += 1;
            entry.1 = owner.to_string();
            entry.0
        };
        // Writing implies you've seen your own write — don't make the writer re-read.
        inner
            .seen
            .insert((owner.to_string(), path.to_path_buf()), version);

        let mut changes = self.read_changes();
        if !changes
            .iter()
            .any(|change| change.path == path && change.owner == owner)
        {
            changes.push(ChangeInfo {
                path: path.to_path_buf(),
                owner: owner.to_string(),
            });
            if let Ok(bytes) = serde_json::to_vec_pretty(&changes) {
                let _ = std::fs::write(&self.changes_file, bytes);
            }
        }
    }

    pub fn snapshot(&self) -> (Vec<LockInfo>, Vec<ChangeInfo>) {
        (self.read_locks(), self.read_changes())
    }

    /// Read live lock files, sweeping any that have expired.
    fn read_lock_files(&self) -> Vec<(PathBuf, LockInfo)> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_lock = path.extension().and_then(|e| e.to_str()) == Some("json")
                && !path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with('_'))
                    .unwrap_or(true);
            if !is_lock {
                continue;
            }
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(lock) = serde_json::from_slice::<LockInfo>(&bytes) {
                    if lock.is_expired() {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    out.push((path, lock));
                }
            }
        }
        out
    }

    fn read_locks(&self) -> Vec<LockInfo> {
        self.read_lock_files().into_iter().map(|(_, l)| l).collect()
    }

    fn read_changes(&self) -> Vec<ChangeInfo> {
        std::fs::read(&self.changes_file)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }
}

/// A readable, collision-free filename for a lock over `path` (basename + a hash of
/// the full path). The file's contents are authoritative — lookups read content,
/// not the name.
fn lock_filename(path: &Path) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.to_string_lossy().hash(&mut hasher);
    let hash = hasher.finish();
    let tail: String = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("lock")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect();
    format!("{tail}.{hash:016x}.json")
}

/// Does the lock at `lock_path` cover `target`? True for the same path, or when
/// `target` sits under `lock_path` (a folder lock covers its descendants).
fn covers(lock_path: &Path, target: &Path) -> bool {
    target == lock_path || target.starts_with(lock_path)
}
