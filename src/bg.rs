//! Background processes the agent starts via `bash {background:true}`. Each is
//! recorded as a JSON file under `<workspace>/.snippet/scratch/bg/<id>.json` and
//! its output redirected to a sibling `<id>.log`. The live list is surfaced to the
//! agent every turn (see `harness::build_live_context`) so it knows what's running.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

pub fn bg_dir(workspace: &Path) -> PathBuf {
    workspace.join(".snippet").join("scratch").join("bg")
}

pub fn log_path(workspace: &Path, id: &str) -> PathBuf {
    bg_dir(workspace).join(format!("{id}.log"))
}

/// Exit-status file: written when the process exits (the code, or "signal"/"?").
pub fn status_path(workspace: &Path, id: &str) -> PathBuf {
    bg_dir(workspace).join(format!("{id}.status"))
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BgEntry {
    pub id: String,
    pub command: String,
    #[serde(default)]
    pub label: Option<String>,
    pub pid: u32,
    pub started_at: String,
    pub log: String,
}

/// A short, file-safe id for a new background process.
pub fn new_id() -> String {
    uuid::Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(8)
        .collect()
}

/// Persist a registry entry for a freshly-spawned background process.
pub fn record(
    workspace: &Path,
    id: &str,
    command: &str,
    label: Option<&str>,
    pid: u32,
) -> std::io::Result<()> {
    let dir = bg_dir(workspace);
    std::fs::create_dir_all(&dir)?;
    let entry = BgEntry {
        id: id.to_string(),
        command: command.to_string(),
        label: label.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string),
        pid,
        started_at: chrono::Utc::now().to_rfc3339(),
        log: log_path(workspace, id).display().to_string(),
    };
    std::fs::write(
        dir.join(format!("{id}.json")),
        serde_json::to_string_pretty(&entry).unwrap_or_default(),
    )?;
    watch(workspace);
    Ok(())
}

fn watched() -> &'static Mutex<HashMap<PathBuf, String>> {
    static WATCHED: OnceLock<Mutex<HashMap<PathBuf, String>>> = OnceLock::new();
    WATCHED.get_or_init(|| Mutex::new(HashMap::new()))
}

fn fingerprint(procs: &[BgStatus]) -> String {
    procs
        .iter()
        .map(|p| format!("{}:{}", p.id, p.running))
        .collect::<Vec<_>>()
        .join(",")
}

fn notify_changed(workspace: &Path) {
    crate::session::emit_device_event(serde_json::json!({
        "kind": "process",
        "workspace": workspace.display().to_string(),
    }));
}

fn watch(workspace: &Path) {
    let print = fingerprint(&list(workspace));
    watched()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(workspace.to_path_buf(), print);
    notify_changed(workspace);
}

pub async fn watch_loop() {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let snapshot: Vec<(PathBuf, String)> = watched()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (workspace, before) in snapshot {
            let procs = list(&workspace);
            let after = fingerprint(&procs);
            let any_running = procs.iter().any(|p| p.running);
            let mut map = watched().lock().unwrap_or_else(|e| e.into_inner());
            if after != before {
                notify_changed(&workspace);
            }
            if any_running {
                map.insert(workspace, after);
            } else {
                map.remove(&workspace);
            }
        }
    }
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

/// Whether `pid` is alive AND is the same process the record refers to. Records
/// survive reboots while pids get recycled; a pid whose process started AFTER the
/// record was written is some other process, not our background job. Compared via
/// `ps` elapsed time (portable across Linux/macOS); parse failures fall back to
/// plain liveness so we never wrongly kill a live record.
fn pid_is_recorded_process(pid: u32, started_at: &str) -> bool {
    if !pid_alive(pid) {
        return false;
    }
    let Ok(started) = chrono::DateTime::parse_from_rfc3339(started_at) else {
        return true;
    };
    let record_age = (chrono::Utc::now() - started.with_timezone(&chrono::Utc)).num_seconds();
    let Some(elapsed) = process_elapsed_seconds(pid) else {
        return true;
    };
    // 5s slack: `ps` elapsed and our timestamps aren't sampled atomically.
    elapsed + 5 >= record_age
}

/// The process's elapsed running time in seconds, via `ps -o etime=` (format
/// `[[dd-]hh:]mm:ss`). None when ps fails or the output doesn't parse.
fn process_elapsed_seconds(pid: u32) -> Option<i64> {
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "etime="])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        return None;
    }
    let (days, clock) = match text.split_once('-') {
        Some((d, rest)) => (d.parse::<i64>().ok()?, rest),
        None => (0, text.as_str()),
    };
    let parts: Vec<i64> = clock
        .split(':')
        .map(|p| p.trim().parse::<i64>())
        .collect::<Result<_, _>>()
        .ok()?;
    let (h, m, s) = match parts.as_slice() {
        [h, m, s] => (*h, *m, *s),
        [m, s] => (0, *m, *s),
        _ => return None,
    };
    Some(days * 86_400 + h * 3_600 + m * 60 + s)
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct BgStatus {
    pub id: String,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub pid: u32,
    pub started_at: String,
    pub log: String,
    pub running: bool,
    /// Exit code / "signal" once it has exited; None while running.
    pub status: Option<String>,
}

/// Snapshot the background-process registry for a client (non-mutating, unlike
/// `render_live` which prunes exited records for the agent).
pub fn list(workspace: &Path) -> Vec<BgStatus> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(bg_dir(workspace)) else {
        return out;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Ok(txt) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(entry) = serde_json::from_str::<BgEntry>(&txt) else {
            continue;
        };
        let running = pid_is_recorded_process(entry.pid, &entry.started_at);
        let status = if running {
            None
        } else {
            std::fs::read_to_string(status_path(workspace, &entry.id))
                .ok()
                .map(|s| s.trim().to_string())
        };
        out.push(BgStatus {
            id: entry.id,
            command: entry.command,
            label: entry.label,
            pid: entry.pid,
            started_at: entry.started_at,
            log: entry.log,
            running,
            status,
        });
    }
    out.sort_by(|a, b| a.started_at.cmp(&b.started_at));
    out
}

/// Terminate a recorded background process (its group if it leads one, else the
/// process). Returns Ok(true) if the process was running and signaled, Ok(false)
/// if it had already exited. Returns Err if the record doesn't exist or is invalid.
pub fn kill_by_id(workspace: &Path, id: &str) -> std::io::Result<bool> {
    let path = bg_dir(workspace).join(format!("{id}.json"));
    let txt = std::fs::read_to_string(&path)?;
    let entry: BgEntry = serde_json::from_str(&txt)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if pid_alive(entry.pid) {
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(format!("-{}", entry.pid))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(entry.pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        watch(workspace);
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Read trailing lines from a background process's log file.
/// Returns (content, truncated_flag).
pub fn tail_log(workspace: &Path, id: &str, max_lines: usize) -> std::io::Result<(String, bool)> {
    let path = log_path(workspace, id);
    if !path.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Log file not found: {}", path.display()),
        ));
    }
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(&path)?;
    let meta = file.metadata()?;
    let len = meta.len();
    let max_read = 256 * 1024;
    let (buf, byte_truncated) = if len > max_read {
        file.seek(SeekFrom::End(-(max_read as i64)))?;
        let mut buf = Vec::with_capacity(max_read as usize);
        file.read_to_end(&mut buf)?;
        (buf, true)
    } else {
        let mut buf = Vec::with_capacity(len as usize);
        file.read_to_end(&mut buf)?;
        (buf, false)
    };
    let text = String::from_utf8_lossy(&buf);
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max_lines && !byte_truncated {
        Ok((lines.join("\n"), false))
    } else {
        let start = lines.len().saturating_sub(max_lines);
        Ok((lines[start..].join("\n"), true))
    }
}

/// Render the live background-process list for the agent's steering block.
/// Running ones are listed; exited ones are surfaced once, then their record is
/// pruned (the log file is kept for inspection). Returns None when there are none.
pub fn render_live(workspace: &Path) -> Option<String> {
    let entries = std::fs::read_dir(bg_dir(workspace)).ok()?;
    let mut lines: Vec<String> = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Ok(txt) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(entry) = serde_json::from_str::<BgEntry>(&txt) else {
            continue;
        };
        let cmd = entry.command.replace('\n', " ");
        let display_name = match &entry.label {
            Some(lbl) if !lbl.trim().is_empty() => lbl.trim().to_string(),
            _ => format!("`{cmd}`"),
        };
        let log = entry
            .log
            .strip_prefix(workspace.to_string_lossy().as_ref())
            .map(|p| p.trim_start_matches('/').to_string())
            .unwrap_or_else(|| entry.log.clone());
        if pid_is_recorded_process(entry.pid, &entry.started_at) {
            lines.push(format!(
                "- [{}] {} — pid {}, running. log: {}",
                entry.id, display_name, entry.pid, log
            ));
        } else {
            // Exited: report the captured exit status, then drop the record (keep the log).
            let code = std::fs::read_to_string(status_path(workspace, &entry.id))
                .ok()
                .map(|s| s.trim().to_string());
            let status = match code.as_deref() {
                Some("0") => "exited (ok)".to_string(),
                Some("signal") => "killed".to_string(),
                Some(c) if !c.is_empty() => format!("exited (code {c})"),
                _ => "exited".to_string(),
            };
            lines.push(format!(
                "- [{}] {} — {}. log: {}",
                entry.id, display_name, status, log
            ));
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(status_path(workspace, &entry.id));
        }
    }
    if lines.is_empty() {
        return None;
    }
    lines.sort();
    Some(format!("{}\n", lines.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_and_list_with_label() {
        let temp = tempfile::tempdir().unwrap();
        let ws = temp.path();
        let id = "test1234";
        record(ws, id, "cargo run", Some("Start API Server"), 99999).unwrap();

        let entries = list(ws);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, id);
        assert_eq!(entries[0].command, "cargo run");
        assert_eq!(entries[0].label.as_deref(), Some("Start API Server"));
        assert_eq!(entries[0].pid, 99999);
        assert!(!entries[0].running);
    }

    #[test]
    fn test_record_without_label_legacy_compatibility() {
        let temp = tempfile::tempdir().unwrap();
        let ws = temp.path();
        let id = "legacy12";
        // Simulate legacy record without label in json
        let dir = bg_dir(ws);
        std::fs::create_dir_all(&dir).unwrap();
        let legacy_json = format!(r#"{{
            "id": "{id}",
            "command": "python3 -m http.server",
            "pid": 88888,
            "started_at": "2026-09-25T18:00:00Z",
            "log": "/tmp/{id}.log"
        }}"#);
        std::fs::write(dir.join(format!("{id}.json")), legacy_json).unwrap();

        let entries = list(ws);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, id);
        assert_eq!(entries[0].label, None);

        // render_live should render exited process using command
        let rendered = render_live(ws).unwrap();
        assert!(rendered.contains("- [legacy12] `python3 -m http.server` — exited."));
    }

    #[test]
    fn test_render_live_with_label() {
        let temp = tempfile::tempdir().unwrap();
        let ws = temp.path();
        let id = "srv9999";
        record(ws, id, "cargo run --bin auth", Some("Start Auth Server"), 77777).unwrap();
        // Record is dead (pid 77777 doesn't exist), so render_live reports it exited
        std::fs::write(status_path(ws, id), "0").unwrap();

        let rendered = render_live(ws).unwrap();
        assert!(rendered.contains("- [srv9999] Start Auth Server — exited (ok)."));
    }

    #[test]
    fn test_tail_log() {
        let temp = tempfile::tempdir().unwrap();
        let ws = temp.path();
        let id = "logproc";
        record(ws, id, "echo test", Some("Echo Test"), 12345).unwrap();

        let lpath = log_path(ws, id);
        let log_content = "line 1\nline 2\nline 3\nline 4\nline 5\n";
        std::fs::write(&lpath, log_content).unwrap();

        let (tail, truncated) = tail_log(ws, id, 3).unwrap();
        assert_eq!(tail, "line 3\nline 4\nline 5");
        assert!(truncated);

        let (all, truncated_all) = tail_log(ws, id, 10).unwrap();
        assert_eq!(all, "line 1\nline 2\nline 3\nline 4\nline 5");
        assert!(!truncated_all);
    }

    #[test]
    fn test_kill_by_id_nonexistent() {
        let temp = tempfile::tempdir().unwrap();
        let ws = temp.path();
        let res = kill_by_id(ws, "nonexistent");
        assert!(res.is_err());
    }
}
