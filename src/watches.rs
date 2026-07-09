//! File watches: the agent registers a path with the `monitor` meta-tool, ends
//! its turn, and is woken with the text APPENDED to that file (optionally only
//! when it matches a filter regex) — the file-output twin of a lane report.
//!
//! Detection is a cheap async poll of (size, mtime) rather than inotify: the
//! debounce window below dominates end-to-end latency anyway, polling behaves
//! identically on every filesystem (NFS, bind mounts, containers), and it needs
//! no extra dependency or thread-bridging. Each watch is one tokio task tailing
//! a byte offset; appends are debounced until the file goes quiet, capped, then
//! delivered to the resident loop through the same kind of channel lanes use.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tokio::time::{Duration, sleep};

/// Max concurrent watches per conversation — a cost/runaway guard like MAX_ACTIVE_LANES.
const MAX_WATCHES: usize = 8;
/// Poll cadence. Latency floor is the debounce window, so faster polling buys nothing.
const POLL_MS: u64 = 300;
/// A burst of appends must go quiet for this long before it's delivered — one
/// wake per burst instead of one model turn per log line.
const DEBOUNCE_MS: u64 = 700;
/// ...but never hold a delivery hostage to a file that's continuously written.
const DEBOUNCE_MAX_MS: u64 = 5_000;
/// Delivered-text cap per wake; the OLDEST part of an oversized delta is dropped
/// (the tail is where a build/test outcome lands).
const MAX_DELIVER_BYTES: usize = 8 * 1024;

/// Persisted, render-friendly snapshot of a watch (kept in `HarnessState`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WatchRecord {
    pub id: String,
    /// Agent-chosen subject ("watch the build") — how it's shown and spoken of.
    pub label: String,
    pub path: String,
    /// Optional regex; the wake fires only when the appended chunk matches.
    /// Non-matching appends advance the offset silently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// Tail position: everything before this byte offset has been seen/delivered.
    #[serde(default)]
    pub offset: u64,
    pub created_at: String,
}

/// One wake-up: text appended to a watched file (post-filter, post-debounce).
#[derive(Debug, Clone)]
pub struct WatchEvent {
    pub id: String,
    pub label: String,
    pub path: String,
    pub appended: String,
    /// Bytes dropped from the FRONT of the delta when it exceeded the cap.
    pub skipped: u64,
    /// The tail position after this delivery — persisted so a daemon restart
    /// resumes the tail (and catches appends that landed while it was down).
    pub new_offset: u64,
}

pub struct WatchManager {
    records: Vec<WatchRecord>,
    tasks: Vec<(String, tokio::task::JoinHandle<()>)>,
    tx: mpsc::UnboundedSender<WatchEvent>,
    counter: usize,
    /// Base for resolving relative paths (the conversation's workspace root).
    workspace: PathBuf,
}

impl WatchManager {
    pub fn new(workspace: PathBuf, tx: mpsc::UnboundedSender<WatchEvent>) -> Self {
        Self { records: Vec::new(), tasks: Vec::new(), tx, counter: 0, workspace }
    }

    pub fn records(&self) -> &[WatchRecord] {
        &self.records
    }

    /// Re-arm persisted watches on resume. Offsets are clamped by the tail task
    /// itself (a shrunken/rotated file resets to 0), so stale offsets are safe.
    pub fn restore(&mut self, records: &[WatchRecord]) {
        for r in records {
            if self.records.iter().any(|x| x.id == r.id) {
                continue;
            }
            // Keep the counter ahead of restored ids ("watch-3" → counter ≥ 3).
            if let Some(n) = r.id.strip_prefix("watch-").and_then(|s| s.parse::<usize>().ok()) {
                self.counter = self.counter.max(n);
            }
            self.records.push(r.clone());
            self.spawn_tail(r.clone());
        }
    }

    /// Register a new watch and start tailing. Returns the record on success.
    pub fn add(
        &mut self,
        path: &str,
        label: &str,
        filter: Option<&str>,
    ) -> Result<WatchRecord, String> {
        if self.records.len() >= MAX_WATCHES {
            return Err(format!(
                "{MAX_WATCHES} watches are already active — remove one (action:\"remove\") before adding more."
            ));
        }
        let resolved = self.resolve(path);
        if let Some(f) = filter {
            regex::Regex::new(f).map_err(|e| format!("filter is not a valid regex: {e}"))?;
        }
        if self.records.iter().any(|r| r.path == resolved.display().to_string()) {
            return Err(format!(
                "`{path}` is already being watched — remove it first to change the filter."
            ));
        }
        // Start the tail at the CURRENT end of file: "monitor this" means wake on
        // what happens next, not re-deliver history the agent can read_file itself.
        let offset = std::fs::metadata(&resolved).map(|m| m.len()).unwrap_or(0);
        self.counter += 1;
        let record = WatchRecord {
            id: format!("watch-{}", self.counter),
            label: label.to_string(),
            path: resolved.display().to_string(),
            filter: filter.map(str::to_string),
            offset,
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        self.records.push(record.clone());
        self.spawn_tail(record.clone());
        Ok(record)
    }

    /// Remove a watch by id, label, or path. Returns its label.
    pub fn remove(&mut self, key: &str) -> Result<String, String> {
        let resolved = self.resolve(key).display().to_string();
        let Some(pos) = self
            .records
            .iter()
            .position(|r| r.id == key || r.label == key || r.path == key || r.path == resolved)
        else {
            let known: Vec<String> = self
                .records
                .iter()
                .map(|r| format!("\"{}\" ({})", r.label, r.id))
                .collect();
            return Err(format!(
                "no watch matches `{key}`. Active: [{}].",
                known.join(", ")
            ));
        };
        let record = self.records.remove(pos);
        if let Some(tpos) = self.tasks.iter().position(|(id, _)| *id == record.id) {
            let (_, handle) = self.tasks.remove(tpos);
            handle.abort();
        }
        Ok(record.label)
    }

    /// Record the delivered tail position so persistence survives restarts.
    pub fn advance_offset(&mut self, id: &str, offset: u64) {
        if let Some(r) = self.records.iter_mut().find(|r| r.id == id) {
            r.offset = offset;
        }
    }

    pub fn abort_all(&mut self) {
        for (_, handle) in self.tasks.drain(..) {
            handle.abort();
        }
    }

    fn resolve(&self, path: &str) -> PathBuf {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.workspace.join(p)
        }
    }

    /// The tail task: poll (size), detect growth, debounce until quiet, read the
    /// delta, filter, cap, deliver. A missing file is fine — it may be created
    /// later (watching a log before the build starts); truncation/rotation
    /// (size < offset) restarts the tail from 0 so the new content is seen.
    fn spawn_tail(&mut self, record: WatchRecord) {
        let tx = self.tx.clone();
        let id = record.id.clone();
        let handle = tokio::spawn(async move {
            let path = PathBuf::from(&record.path);
            let filter = record
                .filter
                .as_deref()
                .and_then(|f| regex::Regex::new(f).ok());
            let mut offset = record.offset;
            loop {
                sleep(Duration::from_millis(POLL_MS)).await;
                let Ok(meta) = tokio::fs::metadata(&path).await else {
                    continue; // not created yet, or deleted — keep waiting
                };
                let mut size = meta.len();
                if size < offset {
                    offset = 0; // truncated / rotated: treat what follows as new
                }
                if size == offset {
                    continue;
                }
                // Debounce: wait for the burst to go quiet (bounded).
                let started = tokio::time::Instant::now();
                loop {
                    sleep(Duration::from_millis(DEBOUNCE_MS)).await;
                    let now = tokio::fs::metadata(&path).await.map(|m| m.len()).unwrap_or(size);
                    if now == size || started.elapsed() >= Duration::from_millis(DEBOUNCE_MAX_MS) {
                        size = now.max(size);
                        break;
                    }
                    size = now;
                }
                if size < offset {
                    offset = 0;
                    continue;
                }
                let Ok(bytes) = read_range(&path, offset, size).await else {
                    continue; // transient read failure — retry next poll
                };
                let delta = String::from_utf8_lossy(&bytes).into_owned();
                offset = size;
                // Filter gates the WAKE, not the offset — non-matching appends
                // are consumed silently so the next match delivers only fresh text.
                if let Some(re) = &filter {
                    if !re.is_match(&delta) {
                        continue;
                    }
                }
                let (appended, skipped) = cap_tail(&delta, MAX_DELIVER_BYTES);
                let _ = tx.send(WatchEvent {
                    id: id.clone(),
                    label: record.label.clone(),
                    path: record.path.clone(),
                    appended,
                    skipped,
                    new_offset: offset,
                });
            }
        });
        self.tasks.push((record.id.clone(), handle));
    }
}

impl Drop for WatchManager {
    fn drop(&mut self) {
        self.abort_all();
    }
}

async fn read_range(path: &Path, from: u64, to: u64) -> std::io::Result<Vec<u8>> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut f = tokio::fs::File::open(path).await?;
    f.seek(std::io::SeekFrom::Start(from)).await?;
    let mut buf = vec![0u8; (to - from) as usize];
    f.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Keep the TAIL of an oversized delta (outcomes land at the end), returning
/// (text, bytes_dropped_from_front). Cuts on a char boundary.
fn cap_tail(delta: &str, max: usize) -> (String, u64) {
    if delta.len() <= max {
        return (delta.to_string(), 0);
    }
    let mut start = delta.len() - max;
    while !delta.is_char_boundary(start) {
        start += 1;
    }
    ((&delta[start..]).to_string(), start as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_keeps_tail() {
        let (text, skipped) = cap_tail("aaaabbbb", 4);
        assert_eq!(text, "bbbb");
        assert_eq!(skipped, 4);
    }

    #[test]
    fn cap_respects_char_boundary() {
        let s = "aé日本語end";
        let (text, _) = cap_tail(s, 4);
        assert!(s.ends_with(&text));
        assert!(!text.is_empty());
    }

    #[tokio::test]
    async fn add_remove_and_tail_appends() {
        let dir = std::env::temp_dir().join(format!("watchtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("out.log");
        std::fs::write(&file, "old content\n").unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut mgr = WatchManager::new(dir.clone(), tx);
        let rec = mgr.add(file.to_str().unwrap(), "test watch", None).unwrap();
        assert_eq!(rec.offset, "old content\n".len() as u64); // starts at EOF

        // Append after registration → exactly the new text is delivered.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap();
        std::fs::write(&file, "old content\nBUILD FAILED here\n").unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("watch should fire")
            .expect("channel open");
        assert_eq!(ev.appended, "BUILD FAILED here\n");
        assert_eq!(ev.skipped, 0);

        mgr.remove("test watch").unwrap();
        assert!(mgr.records().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn filter_must_be_valid_regex() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut mgr = WatchManager::new(std::env::temp_dir(), tx);
        assert!(mgr.add("x.log", "bad", Some("(unclosed")).is_err());
    }
}
