//! Best-effort JSONL transcripts for delegated lane runs.
//!
//! These files intentionally live in the OS temp directory. They can contain
//! model responses, tool arguments, and tool results, so they are diagnostic
//! artifacts and must not be treated as durable or sanitized session storage.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::Serialize;
use serde_json::json;

use crate::harness::HarnessEvent;

pub struct LaneLog {
    file: File,
    path: PathBuf,
}

impl LaneLog {
    pub fn open(lane_id: &str) -> io::Result<Self> {
        let path = std::env::temp_dir()
            .join("snippet-lane-logs")
            .join(format!("{lane_id}-{}.jsonl", std::process::id()));
        Self::open_at(path)
    }

    fn open_at(path: impl AsRef<Path>) -> io::Result<Self> {
        if let Some(parent) = path.as_ref().parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            file,
            path: path.as_ref().to_path_buf(),
        })
    }

    pub fn write_event(&mut self, event: &HarnessEvent) -> io::Result<()> {
        self.write_json(&json!({
            "ts": Utc::now().to_rfc3339(),
            "event": event,
        }))
    }

    pub fn write_json<T: Serialize>(&mut self, value: &T) -> io::Result<()> {
        serde_json::to_writer(&mut self.file, value).map_err(io::Error::other)?;
        self.file.write_all(b"\n")?;
        self.file.flush()
    }

    pub fn write_start(&mut self, lane_id: &str, brief: &str, read_only: bool) -> io::Result<()> {
        self.write_json(&json!({
            "ts": Utc::now().to_rfc3339(),
            "event": "lane_start",
            "lane_id": lane_id,
            "brief": brief,
            "read_only": read_only,
            "path": self.path,
        }))
    }

    pub fn write_end(
        &mut self,
        lane_id: &str,
        status: &str,
        iterations: Option<usize>,
        error: Option<&str>,
        final_text: Option<&str>,
    ) -> io::Result<()> {
        self.write_json(&json!({
            "ts": Utc::now().to_rfc3339(),
            "event": "lane_end",
            "lane_id": lane_id,
            "status": status,
            "iterations": iterations,
            "error": error,
            "final_text": final_text,
        }))
    }
}
