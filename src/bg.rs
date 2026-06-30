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
        if pid_alive(entry.pid) {
            lines.push(format!("- [{}] `{}` — pid {}, running. log: {}", entry.id, cmd, entry.pid, entry.log));
        } else {
            lines.push(format!("- [{}] `{}` — exited. log: {}", entry.id, cmd, entry.log));
            let _ = std::fs::remove_file(&path); // surfaced once; drop the record
        }
    }
    if lines.is_empty() {
        return None;
    }
    lines.sort();
    Some(format!("{}\n", lines.join("\n")))
}
