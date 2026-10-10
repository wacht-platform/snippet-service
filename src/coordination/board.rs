//! Per-agent coordination memory.
//!
//! One agent's record of what it sent out, what came back, and what it learned.
//! Board rows are NARRATIVE; durable work lives in `tasks`, which owns dispatch
//! and the task board. The board is how an agent recalls its own history.

use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::types::CoordinationEvent;
use super::{Store, StoreError};

/// What a board row is about.
///
/// The kind is what allows `read_coordination_board` to answer "what did I
/// dispatch" separately from "what did I learn".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoardEntryKind {
    /// This agent dispatched work.
    Dispatched,
    /// A dispatched session reported an outcome.
    Reported,
    /// The agent's own observation or conclusion.
    Noted,
}

impl BoardEntryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Dispatched => "dispatched",
            Self::Reported => "reported",
            Self::Noted => "noted",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "dispatched" => Some(Self::Dispatched),
            "reported" => Some(Self::Reported),
            "noted" => Some(Self::Noted),
            _ => None,
        }
    }
}

/// One row on an agent's board.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BoardEntry {
    pub id: i64,
    pub agent_id: String,
    pub kind: String,
    /// The session the work concerns, when it concerns one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The workspace the work concerns, for folder-scoped recall.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub summary: String,
    /// Links a dispatch to its later report (assignment id or goal id), so the
    /// agent can correlate what it sent with what came back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    pub created_at: String,
}

/// What a board row records. Bundled so the recorder's signature does not grow a
/// parameter per optional field.
#[derive(Debug, Clone, Default)]
pub struct NewBoardEntry<'a> {
    pub session_id: Option<&'a str>,
    pub workspace: Option<&'a str>,
    pub summary: &'a str,
    pub correlation_id: Option<&'a str>,
    pub created_at: &'a str,
}

/// Optional narrowing for board recall. Applied in SQL so a filtered page is
/// still filled to `limit`.
#[derive(Debug, Clone, Default)]
pub struct BoardQuery<'a> {
    /// Only rows about this workspace (folder-scoped recall).
    pub workspace: Option<&'a str>,
    /// Substring match over the summary (string search).
    pub contains: Option<&'a str>,
    /// Only rows of this kind.
    pub kind: Option<BoardEntryKind>,
}

const ENTRY_COLUMNS: &str = "id, agent_id, kind, session_id, workspace, summary, \
                             correlation_id, created_at";

fn entry_from_row(row: &rusqlite::Row<'_>) -> Result<BoardEntry, rusqlite::Error> {
    Ok(BoardEntry {
        id: row.get(0)?,
        agent_id: row.get(1)?,
        kind: row.get(2)?,
        session_id: row.get(3)?,
        workspace: row.get(4)?,
        summary: row.get(5)?,
        correlation_id: row.get(6)?,
        created_at: row.get(7)?,
    })
}

impl Store {
    /// Append one row to an agent's board.
    ///
    /// Append-only on purpose: the board is a record of what happened, so an
    /// edit would erase the agent's own history. A correction is a new row.
    pub fn record_board_entry(
        &self,
        agent_id: &str,
        kind: BoardEntryKind,
        entry: &NewBoardEntry<'_>,
    ) -> Result<i64, StoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO agent_board
                 (agent_id, kind, session_id, workspace, summary, correlation_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    agent_id,
                    kind.as_str(),
                    entry.session_id,
                    entry.workspace,
                    entry.summary,
                    entry.correlation_id,
                    entry.created_at,
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    /// An agent's board, newest first, with optional workspace / text / kind
    /// narrowing.
    ///
    /// Newest first because both uses — "what did I just do" and "have I dealt
    /// with this before" — want recent memory before old.
    pub fn read_board(
        &self,
        agent_id: &str,
        query: &BoardQuery<'_>,
        limit: u32,
    ) -> Result<Vec<BoardEntry>, StoreError> {
        // A LIKE pattern is built here rather than in SQL so the wildcards are
        // escaped: a summary containing `%` must not turn into a match-anything.
        let contains = query.contains.map(|text| {
            let escaped = text
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            format!("%{escaped}%")
        });
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {ENTRY_COLUMNS} FROM agent_board
                 WHERE agent_id = ?1
                   AND (?2 IS NULL OR workspace = ?2)
                   AND (?3 IS NULL OR kind = ?3)
                   AND (?4 IS NULL OR summary LIKE ?4 ESCAPE '\\')
                 ORDER BY created_at DESC, id DESC
                 LIMIT ?5"
            ))?;
            let rows = stmt.query_map(
                params![
                    agent_id,
                    query.workspace,
                    query.kind.map(|k| k.as_str()),
                    contains,
                    limit
                ],
                entry_from_row,
            )?;
            rows.collect()
        })
    }

    /// Workspaces this agent has board rows for, most recently used first.
    ///
    /// Lets recall start from "which folders have I worked in" without the agent
    /// having to enumerate sessions.
    pub fn board_workspaces(&self, agent_id: &str) -> Result<Vec<String>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT workspace FROM agent_board
                 WHERE agent_id = ?1 AND workspace IS NOT NULL AND workspace <> ''
                 GROUP BY workspace
                 ORDER BY MAX(created_at) DESC",
            )?;
            let rows = stmt.query_map(params![agent_id], |row| row.get::<_, String>(0))?;
            rows.collect()
        })
    }

    /// The correlation ids this agent has dispatched but not yet seen a report
    /// for — "what is still outstanding from my side".
    ///
    /// Derived rather than stored, so it can never disagree with the board.
    pub fn board_awaiting_report(&self, agent_id: &str) -> Result<Vec<String>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT d.correlation_id FROM agent_board d
                 WHERE d.agent_id = ?1 AND d.kind = 'dispatched'
                   AND d.correlation_id IS NOT NULL AND d.correlation_id <> ''
                   AND NOT EXISTS (
                       SELECT 1 FROM agent_board r
                       WHERE r.agent_id = d.agent_id
                         AND r.kind = 'reported'
                         AND r.correlation_id = d.correlation_id
                   )
                 ORDER BY d.created_at DESC",
            )?;
            let rows = stmt.query_map(params![agent_id], |row| row.get::<_, String>(0))?;
            rows.collect()
        })
    }
}

/// Build a report summary from a completed work session's own event log.
///
/// Kept here so both the coordinator's board and any future consumer describe a
/// finished dispatch the same way.
pub fn completion_summary(
    session_id: &str,
    events: &[CoordinationEvent],
) -> String {
    let last_body = events
        .iter()
        .rev()
        .find_map(|event| event.payload.get("body").and_then(|v| v.as_str()))
        .map(|body| body.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    if last_body.is_empty() {
        format!("session {session_id} finished")
    } else {
        let clipped: String = last_body.chars().take(280).collect();
        format!("session {session_id} finished: {clipped}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry<'a>(summary: &'a str, at: &'a str) -> NewBoardEntry<'a> {
        NewBoardEntry {
            summary,
            created_at: at,
            ..Default::default()
        }
    }

    #[test]
    fn board_is_per_agent() {
        let db = Store::open_in_memory().unwrap();
        db.record_board_entry("a", BoardEntryKind::Noted, &entry("mine", "t1"))
            .unwrap();
        db.record_board_entry("b", BoardEntryKind::Noted, &entry("theirs", "t2"))
            .unwrap();

        let mine = db.read_board("a", &BoardQuery::default(), 50).unwrap();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].summary, "mine");
        assert_eq!(mine[0].agent_id, "a");
    }

    #[test]
    fn recall_is_newest_first() {
        let db = Store::open_in_memory().unwrap();
        for (i, text) in ["oldest", "middle", "newest"].iter().enumerate() {
            db.record_board_entry(
                "a",
                BoardEntryKind::Noted,
                &entry(text, &format!("t{i}")),
            )
            .unwrap();
        }
        let rows = db.read_board("a", &BoardQuery::default(), 50).unwrap();
        assert_eq!(
            rows.iter().map(|r| r.summary.as_str()).collect::<Vec<_>>(),
            ["newest", "middle", "oldest"]
        );
    }

    #[test]
    fn recall_filters_by_workspace() {
        let db = Store::open_in_memory().unwrap();
        db.record_board_entry(
            "a",
            BoardEntryKind::Dispatched,
            &NewBoardEntry {
                workspace: Some("/code/alpha"),
                summary: "alpha work",
                created_at: "t1",
                ..Default::default()
            },
        )
        .unwrap();
        db.record_board_entry(
            "a",
            BoardEntryKind::Dispatched,
            &NewBoardEntry {
                workspace: Some("/code/beta"),
                summary: "beta work",
                created_at: "t2",
                ..Default::default()
            },
        )
        .unwrap();

        let filtered = db
            .read_board(
                "a",
                &BoardQuery {
                    workspace: Some("/code/alpha"),
                    ..Default::default()
                },
                50,
            )
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].summary, "alpha work");
    }

    #[test]
    fn recall_searches_the_summary_text() {
        let db = Store::open_in_memory().unwrap();
        db.record_board_entry(
            "a",
            BoardEntryKind::Noted,
            &entry("refactored the file picker", "t1"),
        )
        .unwrap();
        db.record_board_entry("a", BoardEntryKind::Noted, &entry("tuned the retry logic", "t2"))
            .unwrap();

        let hits = db
            .read_board(
                "a",
                &BoardQuery {
                    contains: Some("picker"),
                    ..Default::default()
                },
                50,
            )
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].summary, "refactored the file picker");
    }

    #[test]
    fn a_wildcard_in_the_search_is_literal() {
        // A summary containing `%` must not become a match-anything pattern.
        let db = Store::open_in_memory().unwrap();
        db.record_board_entry("a", BoardEntryKind::Noted, &entry("100% done", "t1"))
            .unwrap();
        db.record_board_entry("a", BoardEntryKind::Noted, &entry("unrelated", "t2"))
            .unwrap();

        let hits = db
            .read_board(
                "a",
                &BoardQuery {
                    contains: Some("%"),
                    ..Default::default()
                },
                50,
            )
            .unwrap();
        assert_eq!(hits.len(), 1, "only the row that literally contains %");
        assert_eq!(hits[0].summary, "100% done");
    }

    #[test]
    fn recall_filters_by_kind() {
        let db = Store::open_in_memory().unwrap();
        db.record_board_entry("a", BoardEntryKind::Dispatched, &entry("sent it", "t1"))
            .unwrap();
        db.record_board_entry("a", BoardEntryKind::Noted, &entry("thought it", "t2"))
            .unwrap();

        let dispatched = db
            .read_board(
                "a",
                &BoardQuery {
                    kind: Some(BoardEntryKind::Dispatched),
                    ..Default::default()
                },
                50,
            )
            .unwrap();
        assert_eq!(dispatched.len(), 1);
        assert_eq!(dispatched[0].summary, "sent it");
    }

    #[test]
    fn awaiting_report_lists_only_unanswered_dispatches() {
        let db = Store::open_in_memory().unwrap();
        for correlation in ["asg-1", "asg-2"] {
            db.record_board_entry(
                "a",
                BoardEntryKind::Dispatched,
                &NewBoardEntry {
                    summary: "sent",
                    correlation_id: Some(correlation),
                    created_at: "t1",
                    ..Default::default()
                },
            )
            .unwrap();
        }
        // Only asg-1 has come back.
        db.record_board_entry(
            "a",
            BoardEntryKind::Reported,
            &NewBoardEntry {
                summary: "done",
                correlation_id: Some("asg-1"),
                created_at: "t2",
                ..Default::default()
            },
        )
        .unwrap();

        let awaiting = db.board_awaiting_report("a").unwrap();
        assert_eq!(awaiting, ["asg-2"]);
    }

    #[test]
    fn workspaces_are_most_recently_used_first() {
        let db = Store::open_in_memory().unwrap();
        for (ws, at) in [("/code/old", "t1"), ("/code/new", "t2")] {
            db.record_board_entry(
                "a",
                BoardEntryKind::Noted,
                &NewBoardEntry {
                    workspace: Some(ws),
                    summary: "x",
                    created_at: at,
                    ..Default::default()
                },
            )
            .unwrap();
        }
        assert_eq!(
            db.board_workspaces("a").unwrap(),
            ["/code/new", "/code/old"]
        );
    }

    #[test]
    fn completion_summary_collapses_and_clips() {
        let events = vec![CoordinationEvent {
            event_id: "e1".into(),
            thread_id: "t".into(),
            partition_key: "p".into(),
            sequence: 1,
            event_type: "message.posted".into(),
            actor_kind: "agent".into(),
            actor_id: "w".into(),
            payload_version: 1,
            payload: serde_json::json!({"body": "line one\nline two"}),
            causation_id: None,
            correlation_id: None,
            idempotency_key: "k".into(),
            created_at: "t".into(),
        }];
        let summary = completion_summary("s1", &events);
        assert!(summary.contains("line one line two"), "newlines collapse");
        assert!(summary.starts_with("session s1 finished"));
    }

    #[test]
    fn completion_summary_handles_an_empty_log() {
        let summary = completion_summary("s1", &[]);
        assert_eq!(summary, "session s1 finished");
    }
}