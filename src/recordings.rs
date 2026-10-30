//! Durable markdown copies of tracked conversations, kept in the Mission Control workspace.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

pub fn dir() -> PathBuf {
    crate::mission_control::workspace_path().join("recordings")
}

fn iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|t| t.to_rfc3339()).unwrap_or_default()
}

pub fn write(dir: &Path, id: &str, device: &str, started_ms: i64, ended_ms: i64, lines: usize, transcript: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let day = chrono::DateTime::from_timestamp_millis(started_ms).map(|t| t.format("%Y-%m-%d").to_string()).unwrap_or_else(|| "undated".into());
    let short: String = id.chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
    let path = dir.join(format!("{day}-{short}.md"));
    let body = format!(
        "# Recorded conversation\n\n- id: {id}\n- device: {device}\n- started: {}\n- ended: {}\n- lines: {lines}\n\nLines are not labelled by speaker.\n\n## Transcript\n\n{transcript}",
        iso(started_ms),
        iso(ended_ms),
    );
    std::fs::write(&path, body)?;
    path.canonicalize().or(Ok(path))
}

/// Newest first by start time.
pub fn list(dir: &Path, limit: usize) -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut rows: Vec<Value> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "md"))
        .filter_map(|e| {
            let text = std::fs::read_to_string(e.path()).ok()?;
            let field = |key: &str| {
                let prefix = format!("- {key}: ");
                text.lines().take(10).find_map(|l| l.strip_prefix(prefix.as_str())).map(str::to_string)
            };
            Some(json!({
                "id": field("id")?,
                "started": field("started").unwrap_or_default(),
                "ended": field("ended").unwrap_or_default(),
                "lines": field("lines").and_then(|n| n.parse::<u64>().ok()).unwrap_or(0),
                "path": e.path().to_string_lossy(),
            }))
        })
        .collect();
    rows.sort_by(|a, b| b["started"].as_str().cmp(&a["started"].as_str()));
    rows.truncate(limit.clamp(1, 100));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn written_file_is_listed_newest_first_with_the_whole_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("recordings");
        let t0 = 1_800_000_000_000;
        let long = "[10:00:00] word\n".repeat(10_000);
        let a = write(&dir, "aaaaaaaa-1111", "mac", t0, t0 + 60_000, 10_000, &long).unwrap();
        let b = write(&dir, "bbbbbbbb-2222", "phone", t0 + 86_400_000, t0 + 86_460_000, 3, "[10:00:00] hi\n").unwrap();
        assert!(std::fs::read_to_string(&a).unwrap().ends_with(&long));
        assert!(a.is_absolute() && a.file_name().unwrap().to_string_lossy().ends_with("-aaaaaaaa.md"));
        let rows = list(&dir, 10);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["id"], "bbbbbbbb-2222");
        assert_eq!(rows[0]["lines"], 3);
        assert_eq!(rows[0]["path"], b.to_string_lossy().as_ref());
        assert!(rows[0]["started"].as_str().unwrap() > rows[1]["started"].as_str().unwrap());
        assert_eq!(rows[1]["ended"].as_str().unwrap(), "2027-01-15T08:01:00+00:00");
        assert_eq!(list(&dir, 1).len(), 1);
        assert!(list(&tmp.path().join("none"), 5).is_empty());
    }
}
