//! The task ledger: one versioned record per task holding what everyone on it
//! needs to agree on (brief, acceptance criteria, decisions, changed files,
//! open questions). Every envelope a worker receives renders from this record
//! instead of re-deriving a one-line summary from the task row.

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::tasks::{TASK_COLUMNS, task_from_row};
use super::{Store, StoreError};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerifyMethod {
    Command,
    Artifact,
    Visual,
    Human,
}

impl VerifyMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Artifact => "artifact",
            Self::Visual => "visual",
            Self::Human => "human",
        }
    }
}

/// Who confirms a task is done before it completes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerifyMode {
    #[serde(rename = "self")]
    SelfCheck,
    Reviewer,
    MissionControl,
    User,
}

impl VerifyMode {
    pub fn parse(raw: &str) -> Option<Self> {
        serde_json::from_value(serde_json::Value::String(raw.trim().to_string())).ok()
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SelfCheck => "self",
            Self::Reviewer => "reviewer",
            Self::MissionControl => "mission_control",
            Self::User => "user",
        }
    }
}

/// What the worker offers for one criterion when it reports done.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Evidence {
    /// The criterion's number (`1`) or its text.
    pub criterion: String,
    /// The command and its result, or what was seen.
    pub evidence: String,
    /// An artifact or screenshot; required for `artifact` and `visual` criteria.
    #[serde(default)]
    pub path: Option<String>,
}

/// The worker's report, held while the task is in review.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingReport {
    pub summary: String,
    pub artifacts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Criterion {
    pub criterion: String,
    pub method: VerifyMethod,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Decision {
    pub author: String,
    pub at: String,
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskState {
    #[serde(default)]
    pub task_id: String,
    #[serde(default)]
    pub version: u64,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub brief: String,
    #[serde(default, alias = "acceptance")]
    pub criteria: Vec<Criterion>,
    #[serde(default)]
    pub deliverables: Vec<String>,
    #[serde(default)]
    pub decisions: Vec<Decision>,
    #[serde(default)]
    pub changed_files: Vec<String>,
    #[serde(default)]
    pub open_questions: Vec<String>,
    #[serde(default)]
    pub current_step: String,
    #[serde(default)]
    pub audience: String,
    #[serde(default)]
    pub definition_of_good: String,
    /// HEAD of the task's workspace when work was first dispatched; changed
    /// files and the review diff are measured from it. Daemon-set only.
    #[serde(default)]
    pub dispatch_rev: String,
    /// Unset means: reviewer when the task has criteria, otherwise self.
    #[serde(default)]
    pub verify_mode: Option<VerifyMode>,
    #[serde(default)]
    pub revises_task_id: String,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
    #[serde(default)]
    pub pending_report: Option<PendingReport>,
    /// Reviews that sent the work back.
    #[serde(default)]
    pub review_rounds: u32,
    #[serde(default)]
    pub review_findings: Vec<String>,
    #[serde(default)]
    pub review_started_at: String,
    #[serde(default)]
    pub review_escalated: bool,
    /// Why verification was waived (set by Mission Control on the user's word).
    #[serde(default)]
    pub waiver: String,
    /// How many times finished work was reopened with feedback.
    #[serde(default)]
    pub revision: u32,
    /// The report the last revision started from.
    #[serde(default)]
    pub previous_result: Option<PendingReport>,
}

/// A partial update. Scalars and lists replace; `add_*` append without duplicates.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatePatch {
    pub brief: Option<String>,
    #[serde(alias = "acceptance")]
    pub criteria: Option<Vec<Criterion>>,
    pub deliverables: Option<Vec<String>>,
    #[serde(default, alias = "decisions")]
    pub add_decisions: Vec<String>,
    #[serde(default)]
    pub add_changed_files: Vec<String>,
    pub open_questions: Option<Vec<String>>,
    pub current_step: Option<String>,
    pub audience: Option<String>,
    pub definition_of_good: Option<String>,
    pub verify_mode: Option<VerifyMode>,
}

fn clip(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        format!("{}…", flat.chars().take(max).collect::<String>())
    }
}

fn set_text(slot: &mut String, new: &Option<String>, name: &str, changes: &mut Vec<String>) {
    if let Some(new) = new.as_deref().map(str::trim)
        && new != slot
    {
        *slot = new.to_string();
        changes.push(format!("{name} is now: {}", clip(new, 400)));
    }
}

impl TaskState {
    pub fn seeded(task: &super::Task) -> Self {
        let brief = task
            .handoff
            .as_ref()
            .map(|h| h.description.as_str())
            .filter(|d| !d.trim().is_empty())
            .unwrap_or(task.description.as_str());
        Self {
            task_id: task.id.clone(),
            version: 1,
            updated_at: task.created_at.clone(),
            brief: brief.trim().to_string(),
            ..Self::default()
        }
    }

    /// Apply a patch; the returned lines say what changed, and are empty when
    /// the patch changed nothing (the version then stays put).
    pub fn apply(&mut self, patch: &StatePatch, author: &str, now: &str) -> Vec<String> {
        let mut changes = Vec::new();
        set_text(&mut self.brief, &patch.brief, "brief", &mut changes);
        set_text(&mut self.current_step, &patch.current_step, "current step", &mut changes);
        set_text(&mut self.audience, &patch.audience, "audience", &mut changes);
        set_text(&mut self.definition_of_good, &patch.definition_of_good, "definition of good", &mut changes);
        if let Some(criteria) = patch.criteria.as_ref().filter(|c| **c != self.criteria) {
            self.criteria = criteria.clone();
            let lines: Vec<String> = criteria
                .iter()
                .enumerate()
                .map(|(i, c)| format!("  {}. [{}] {}", i + 1, c.method.as_str(), clip(&c.criterion, 200)))
                .collect();
            changes.push(format!("acceptance criteria are now:\n{}", lines.join("\n")));
        }
        if let Some(items) = patch.deliverables.as_ref().filter(|d| **d != self.deliverables) {
            self.deliverables = items.clone();
            changes.push(format!("deliverables are now: {}", items.join("; ")));
        }
        if let Some(items) = patch.open_questions.as_ref().filter(|q| **q != self.open_questions) {
            self.open_questions = items.clone();
            changes.push(if items.is_empty() {
                "open questions: none left".to_string()
            } else {
                format!("open questions are now: {}", items.join("; "))
            });
        }
        for text in patch.add_decisions.iter().map(|t| t.trim()).filter(|t| !t.is_empty()) {
            if self.decisions.iter().any(|d| d.text == text) {
                continue;
            }
            self.decisions.push(Decision { author: author.to_string(), at: now.to_string(), text: text.to_string() });
            changes.push(format!("decision by {author}: {text}"));
        }
        let added: Vec<String> = patch
            .add_changed_files
            .iter()
            .map(|f| f.trim().to_string())
            .filter(|f| !f.is_empty() && !self.changed_files.contains(f))
            .collect();
        if !added.is_empty() {
            for file in &added {
                if !self.changed_files.contains(file) {
                    self.changed_files.push(file.clone());
                }
            }
            changes.push(format!("changed files +{}: {}", added.len(), added.join(", ")));
        }
        if let Some(mode) = patch.verify_mode.filter(|m| Some(*m) != self.verify_mode) {
            self.verify_mode = Some(mode);
            changes.push(format!("verification is now: {}", mode.as_str()));
        }
        changes
    }

    /// The mode in force: the one set, else a reviewer when there is something
    /// to check against, else the worker's own word. A waiver always wins.
    pub fn effective_mode(&self) -> VerifyMode {
        if !self.waiver.is_empty() {
            return VerifyMode::SelfCheck;
        }
        self.verify_mode.unwrap_or(if self.criteria.is_empty() { VerifyMode::SelfCheck } else { VerifyMode::Reviewer })
    }

    /// Criteria the evidence does not cover, as `n. text (why)`. Artifact and
    /// visual criteria need a `path` that exists (relative to `workspace`).
    pub fn uncovered(&self, evidence: &[Evidence], workspace: &std::path::Path) -> Vec<String> {
        let norm = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
        self.criteria
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                let number = (i + 1).to_string();
                let hits: Vec<&Evidence> = evidence
                    .iter()
                    .filter(|e| {
                        let key = e.criterion.trim().trim_start_matches('#');
                        key == number || norm(key) == norm(&c.criterion)
                    })
                    .collect();
                let why = if hits.is_empty() {
                    "no evidence"
                } else if hits.iter().all(|e| e.evidence.trim().is_empty()) {
                    "evidence is empty"
                } else if matches!(c.method, VerifyMethod::Artifact | VerifyMethod::Visual)
                    && !hits.iter().any(|e| e.path.as_deref().is_some_and(|p| !p.trim().is_empty() && workspace.join(p.trim()).exists()))
                {
                    "needs a path to an existing artifact or screenshot"
                } else {
                    return None;
                };
                Some(format!("{number}. [{}] {} ({why})", c.method.as_str(), clip(&c.criterion, 160)))
            })
            .collect()
    }

    /// The ledger as an envelope block. `full` lists everything; otherwise it is
    /// the compact form used where a worker already holds the brief.
    pub fn render(&self, full: bool) -> String {
        let mut out = format!("[task_ledger v{}]\n", self.version);
        let line = |label: &str, value: &str, out: &mut String| {
            if !value.trim().is_empty() {
                out.push_str(&format!("{label}: {}\n", value.trim()));
            }
        };
        if full {
            line("brief", &self.brief, &mut out);
        } else {
            line("brief", &clip(&self.brief, 300), &mut out);
        }
        if !self.criteria.is_empty() {
            out.push_str("acceptance (report evidence for each):\n");
            for (i, c) in self.criteria.iter().enumerate() {
                let method = c.method.as_str();
                let text = if full { c.criterion.trim().to_string() } else { clip(&c.criterion, 140) };
                out.push_str(&format!("  {}. [{method}] {text}\n", i + 1));
            }
        }
        line("audience", &self.audience, &mut out);
        line("definition of good", &self.definition_of_good, &mut out);
        if !self.deliverables.is_empty() {
            line("deliverables", &self.deliverables.join("; "), &mut out);
        }
        let (keep, files) = if full { (10, 20) } else { (3, 8) };
        if !self.decisions.is_empty() {
            out.push_str("decisions:\n");
            let skip = self.decisions.len().saturating_sub(keep);
            if skip > 0 {
                out.push_str(&format!("  (+{skip} earlier)\n"));
            }
            for d in self.decisions.iter().skip(skip) {
                out.push_str(&format!("  - {} ({}): {}\n", d.author, d.at.get(..10).unwrap_or(&d.at), clip(&d.text, 240)));
            }
        }
        if !self.open_questions.is_empty() {
            line("open questions", &self.open_questions.join("; "), &mut out);
        }
        if self.revision > 0 {
            line("revision", &self.revision.to_string(), &mut out);
        }
        if let Some(mode) = self.verify_mode {
            line("verification", mode.as_str(), &mut out);
        }
        if !self.waiver.is_empty() {
            line("verification waived", &self.waiver, &mut out);
        }
        if !self.review_findings.is_empty() {
            out.push_str(&format!("review findings to fix (round {}):\n", self.review_rounds));
            for f in &self.review_findings {
                out.push_str(&format!("  - {}\n", clip(f, 400)));
            }
        }
        line("current step", &self.current_step, &mut out);
        if !self.changed_files.is_empty() {
            let shown: Vec<&str> = self.changed_files.iter().take(files).map(String::as_str).collect();
            let more = self.changed_files.len().saturating_sub(files);
            let tail = if more > 0 { format!(" (+{more} more)") } else { String::new() };
            line(&format!("changed files ({})", self.changed_files.len()), &format!("{}{tail}", shown.join(", ")), &mut out);
        }
        out.push_str("[/task_ledger]");
        out
    }
}

fn decode(row: (String, i64, String, String)) -> Result<TaskState, rusqlite::Error> {
    let (task_id, version, json, updated_at) = row;
    let mut state: TaskState = serde_json::from_str(&json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
    })?;
    state.task_id = task_id;
    state.version = version as u64;
    state.updated_at = updated_at;
    Ok(state)
}

fn read_state(conn: &rusqlite::Connection, task_id: &str) -> Result<Option<TaskState>, rusqlite::Error> {
    conn.query_row(
        "SELECT task_id, version, state_json, updated_at FROM task_state WHERE task_id = ?1",
        params![task_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )
    .optional()?
    .map(decode)
    .transpose()
}

fn write_state(conn: &rusqlite::Connection, state: &TaskState) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO task_state (task_id, version, state_json, updated_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(task_id) DO UPDATE SET version = ?2, state_json = ?3, updated_at = ?4",
        params![
            state.task_id,
            state.version as i64,
            serde_json::to_string(state).unwrap_or_else(|_| "{}".into()),
            state.updated_at
        ],
    )?;
    Ok(())
}

impl Store {
    /// The ledger, created from the task's briefing the first time it is read.
    pub fn task_state(&self, task_id: &str) -> Result<Option<TaskState>, StoreError> {
        self.with_connection(|conn| {
            if let Some(state) = read_state(conn, task_id)? {
                return Ok(Some(state));
            }
            let task = conn
                .query_row(&format!("SELECT {TASK_COLUMNS} FROM tasks WHERE id = ?1"), params![task_id], task_from_row)
                .optional()?;
            let Some(task) = task else { return Ok(None) };
            let state = TaskState::seeded(&task);
            write_state(conn, &state)?;
            Ok(Some(state))
        })
    }

    /// The ledger if it exists; unlike [`Self::task_state`] it writes nothing,
    /// so a list endpoint can call it for every task.
    pub fn peek_task_state(&self, task_id: &str) -> Result<Option<TaskState>, StoreError> {
        self.with_connection(|conn| read_state(conn, task_id))
    }

    /// Change daemon-owned fields (no version bump, no wake): the dispatch
    /// revision, and later the verification record.
    pub fn modify_task_state(
        &self,
        task_id: &str,
        now: &str,
        f: impl FnOnce(&mut TaskState),
    ) -> Result<TaskState, StoreError> {
        self.task_state(task_id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            let mut state = read_state(&tx, task_id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            f(&mut state);
            state.updated_at = now.to_string();
            write_state(&tx, &state)?;
            tx.commit()?;
            Ok(state)
        })
    }

    /// Apply a patch in one transaction. `expected_version` of `Some` must equal
    /// the stored version or the patch is refused; `None` is for the daemon's own
    /// appends (changed files), which commute with anything.
    pub fn patch_task_state(
        &self,
        task_id: &str,
        patch: &StatePatch,
        expected_version: Option<u64>,
        author: &str,
        now: &str,
    ) -> Result<(TaskState, Vec<String>), StoreError> {
        self.task_state(task_id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        let outcome = self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            let mut state = read_state(&tx, task_id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            if let Some(expected) = expected_version.filter(|v| *v != state.version) {
                return Ok(Err((expected, state.version)));
            }
            let changes = state.apply(patch, author, now);
            if !changes.is_empty() {
                state.version += 1;
                state.updated_at = now.to_string();
                write_state(&tx, &state)?;
            }
            tx.commit()?;
            Ok(Ok((state, changes)))
        })?;
        outcome.map_err(|(expected, actual)| StoreError::StaleTaskState { task_id: task_id.to_string(), expected, actual })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::Task;

    fn store_with_task() -> (Store, Task) {
        let db = Store::open_in_memory().unwrap();
        let task = Task::filed_by_human("t1".into(), "Title".into(), "Make the thing".into(), "s1".into(), 0, "2026-01-01T00:00:00Z".into());
        db.create_task(&task).unwrap();
        (db, task)
    }

    fn patch(json: serde_json::Value) -> StatePatch {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn the_ledger_is_seeded_from_the_briefing_at_version_one() {
        let (db, _) = store_with_task();
        let state = db.task_state("t1").unwrap().unwrap();
        assert_eq!((state.version, state.brief.as_str()), (1, "Make the thing"));
        assert!(db.task_state("nope").unwrap().is_none());
    }

    #[test]
    fn a_patch_bumps_the_version_and_reports_only_what_changed() {
        let (db, _) = store_with_task();
        let (state, changes) = db
            .patch_task_state("t1", &patch(serde_json::json!({
                "criteria": [{"criterion": "cargo test passes", "method": "command"}],
                "add_decisions": ["use sqlite"],
                "current_step": "writing tests",
            })), Some(1), "mission-control", "2026-01-02T00:00:00Z")
            .unwrap();
        assert_eq!(state.version, 2);
        assert_eq!(changes.len(), 3);
        assert!(changes.iter().any(|c| c.contains("decision by mission-control: use sqlite")));
        let (again, none) = db
            .patch_task_state("t1", &patch(serde_json::json!({"current_step": "writing tests"})), Some(2), "w", "2026-01-03T00:00:00Z")
            .unwrap();
        assert!(none.is_empty());
        assert_eq!(again.version, 2);
    }

    #[test]
    fn a_stale_version_is_refused_and_changes_nothing() {
        let (db, _) = store_with_task();
        db.patch_task_state("t1", &patch(serde_json::json!({"audience": "ops"})), Some(1), "a", "2026-01-02T00:00:00Z").unwrap();
        let err = db
            .patch_task_state("t1", &patch(serde_json::json!({"audience": "devs"})), Some(1), "b", "2026-01-03T00:00:00Z")
            .unwrap_err();
        assert!(matches!(err, StoreError::StaleTaskState { expected: 1, actual: 2, .. }), "{err}");
        assert_eq!(db.task_state("t1").unwrap().unwrap().audience, "ops");
    }

    #[test]
    fn changed_files_append_without_duplicates_and_render_in_the_ledger() {
        let (db, _) = store_with_task();
        let p = patch(serde_json::json!({"add_changed_files": ["src/a.rs", "src/b.rs"]}));
        db.patch_task_state("t1", &p, None, "daemon", "2026-01-02T00:00:00Z").unwrap();
        let (state, changes) = db.patch_task_state("t1", &p, None, "daemon", "2026-01-03T00:00:00Z").unwrap();
        assert!(changes.is_empty());
        assert_eq!(state.changed_files, ["src/a.rs", "src/b.rs"]);
        let text = state.render(false);
        assert!(text.contains("changed files (2): src/a.rs, src/b.rs") && text.starts_with("[task_ledger v2]"), "{text}");
    }

    #[test]
    fn unknown_patch_fields_and_methods_are_rejected() {
        assert!(serde_json::from_value::<StatePatch>(serde_json::json!({"bogus": 1})).is_err());
        assert!(serde_json::from_value::<StatePatch>(serde_json::json!({"criteria": [{"criterion": "x", "method": "vibes"}]})).is_err());
    }
}
