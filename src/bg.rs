//! Background processes the agent starts via `bash {background:true}`. Each is
//! recorded as a JSON file under `<workspace>/.snippet/scratch/bg/<id>.json` and
//! its output redirected to a sibling `<id>.log`. The live list is surfaced to the
//! agent every turn (see `harness::build_live_context`) so it knows what's running.

use std::path::{Path, PathBuf};

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

#[derive(Serialize, Deserialize)]
pub struct BgEntry {
    pub id: String,
    pub command: String,
    pub pid: u32,
    pub started_at: String,
    pub log: String,
}

/// A short, file-safe id for a new background process.
pub fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string().chars().take(8).collect()
}

/// Persist a registry entry for a freshly-spawned background process.
pub fn record(workspace: &Path, id: &str, command: &str, pid: u32) -> std::io::Result<()> {
    let dir = bg_dir(workspace);
    std::fs::create_dir_all(&dir)?;
    let entry = BgEntry {
        id: id.to_string(),
        command: command.to_string(),
        pid,
        started_at: chrono::Utc::now().to_rfc3339(),
        log: log_path(workspace, id).display().to_string(),
    };
    std::fs::write(
        dir.join(format!("{id}.json")),
        serde_json::to_string_pretty(&entry).unwrap_or_default(),
    )
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

/// Render the live background-process list for the agent's runtime context.
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
        let Ok(txt) = std::fs::read_to_string(&path) else { continue };
        let Ok(entry) = serde_json::from_str::<BgEntry>(&txt) else { continue };
        let cmd = entry.command.replace('\n', " ");
        if pid_is_recorded_process(entry.pid, &entry.started_at) {
            lines.push(format!("- [{}] `{}` — pid {}, running. log: {}", entry.id, cmd, entry.pid, entry.log));
        } else {
            // Exited: report the captured exit status, then drop the record (keep the log).
            let code = std::fs::read_to_string(status_path(workspace, &entry.id)).ok().map(|s| s.trim().to_string());
            let status = match code.as_deref() {
                Some("0") => "exited (ok)".to_string(),
                Some("signal") => "killed".to_string(),
                Some(c) if !c.is_empty() => format!("exited (code {c})"),
                _ => "exited".to_string(),
            };
            lines.push(format!("- [{}] `{}` — {}. log: {}", entry.id, cmd, status, entry.log));
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
