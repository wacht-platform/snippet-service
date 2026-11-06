//! Per-agent coordination memory.
//!
//! One agent's record of what it sent out, what came back, and what it learned.
//! Board rows are NARRATIVE; durable work lives in `tasks`, which owns dispatch
//! and the task board. The board is how an agent recalls its own history.

use rusqlite::params;
use serde::{Deserialize, Serialize};

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
    /// A working session's account of its work so far.
    Progress,
}

impl BoardEntryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Dispatched => "dispatched",
            Self::Reported => "reported",
            Self::Noted => "noted",
            Self::Progress => "progress",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "dispatched" => Some(Self::Dispatched),
            "reported" => Some(Self::Reported),
            "noted" => Some(Self::Noted),
            "progress" => Some(Self::Progress),
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
    /// The project the workspace belongs to, the same for all its worktrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
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
    /// Only rows older than this id, to page past `limit`.
    pub before_id: Option<i64>,
    /// Only dispatches with no report yet.
    pub outstanding: bool,
    /// Only rows linked to this task.
    pub correlation: Option<&'a str>,
}

/// How a board reads as groups rather than a flat list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardGrouping {
    Task,
    Folder,
}

impl BoardGrouping {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "task" => Some(Self::Task),
            "folder" => Some(Self::Folder),
            _ => None,
        }
    }
}

/// One group's newest row, how many rows it holds and the task it belongs to.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct BoardGroup {
    #[serde(flatten)]
    pub latest: BoardEntry,
    pub group_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_status: Option<String>,
}

/// Pass as the agent to read every agent's board at once.
pub const ALL_AGENTS: &str = "*";

const ENTRY_COLUMNS: &str = "id, agent_id, kind, session_id, workspace, summary, \
                             correlation_id, created_at, folder";

// Stored rather than resolved on read: session worktrees are deleted when a session settles.
fn project_of(workspace: &str) -> String {
    crate::memory::repository_root(std::path::Path::new(workspace)).display().to_string()
}

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
        folder: row.get(8)?,
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
        let folder = entry.workspace.filter(|w| !w.trim().is_empty()).map(project_of);
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO agent_board
                 (agent_id, kind, session_id, workspace, summary, correlation_id, created_at, folder)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    agent_id,
                    kind.as_str(),
                    entry.session_id,
                    entry.workspace,
                    entry.summary,
                    entry.correlation_id,
                    entry.created_at,
                    folder,
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
        let project = query.workspace.map(project_of);
        if project.is_some() {
            self.fill_board_folders()?;
        }
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {ENTRY_COLUMNS} FROM agent_board
                 WHERE (?1 = '*' OR agent_id = ?1)
                   AND (?2 IS NULL OR workspace = ?2 OR folder = ?9)
                   AND (?3 IS NULL OR kind = ?3)
                   AND (?4 IS NULL OR summary LIKE ?4 ESCAPE '\\')
                   AND (?6 IS NULL OR id < ?6)
                   AND (?8 IS NULL OR correlation_id = ?8)
                   AND (NOT ?7 OR (kind = 'dispatched'
                        AND correlation_id IS NOT NULL AND correlation_id <> ''
                        AND NOT EXISTS (
                            SELECT 1 FROM agent_board r
                            WHERE r.agent_id = agent_board.agent_id
                              AND r.kind = 'reported'
                              AND r.correlation_id = agent_board.correlation_id)
                        AND NOT EXISTS (
                            SELECT 1 FROM tasks t
                            WHERE t.id = agent_board.correlation_id
                              AND t.status IN ('done', 'cancelled', 'failed'))))
                 ORDER BY id DESC
                 LIMIT ?5"
            ))?;
            let rows = stmt.query_map(
                params![
                    agent_id,
                    query.workspace,
                    query.kind.map(|k| k.as_str()),
                    contains,
                    limit,
                    query.before_id,
                    query.outstanding,
                    query.correlation,
                    project
                ],
                entry_from_row,
            )?;
            rows.collect()
        })
    }

    /// The board as groups, newest activity first. Rows with no task or folder
    /// form one group of their own, so nothing drops out of the view.
    pub fn read_board_groups(
        &self,
        agent_id: &str,
        grouping: BoardGrouping,
        query: &BoardQuery<'_>,
        limit: u32,
    ) -> Result<Vec<BoardGroup>, StoreError> {
        let key = match grouping {
            BoardGrouping::Task => "COALESCE(correlation_id, '')",
            BoardGrouping::Folder => "COALESCE(NULLIF(folder, ''), NULLIF(workspace, ''), '')",
        };
        if grouping == BoardGrouping::Folder {
            self.fill_board_folders()?;
        }
        let contains = query.contains.map(|text| {
            let escaped = text.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
            format!("%{escaped}%")
        });
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT b.id, b.agent_id, b.kind, b.session_id, b.workspace, b.summary,
                        b.correlation_id, b.created_at, b.folder, g.n, t.title, t.status
                 FROM (SELECT MAX(id) AS last, COUNT(*) AS n FROM agent_board
                       WHERE (?1 = '*' OR agent_id = ?1)
                         AND (?2 IS NULL OR kind = ?2)
                         AND (?3 IS NULL OR summary LIKE ?3 ESCAPE '\\')
                       GROUP BY {key}) g
                 JOIN agent_board b ON b.id = g.last
                 LEFT JOIN tasks t ON t.id = b.correlation_id
                 WHERE (?4 IS NULL OR b.id < ?4)
                 ORDER BY b.id DESC
                 LIMIT ?5"
            ))?;
            let rows = stmt.query_map(
                params![agent_id, query.kind.map(|k| k.as_str()), contains, query.before_id, limit],
                |row| {
                    Ok(BoardGroup {
                        latest: entry_from_row(row)?,
                        group_count: row.get(9)?,
                        task_title: row.get(10)?,
                        task_status: row.get(11)?,
                    })
                },
            )?;
            rows.collect()
        })
    }

    /// Rows from before `folder` existed get it once, on the first read that needs it.
    fn fill_board_folders(&self) -> Result<(), StoreError> {
        let pending: Vec<String> = self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT workspace FROM agent_board
                 WHERE folder IS NULL AND workspace IS NOT NULL AND workspace <> ''",
            )?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect()
        })?;
        for workspace in pending {
            let folder = project_of(&workspace);
            self.with_connection(|conn| {
                conn.execute(
                    "UPDATE agent_board SET folder = ?1 WHERE workspace = ?2 AND folder IS NULL",
                    params![folder, workspace],
                )
            })?;
        }
        // Dispatch and report rows name no workspace; their task or session does.
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE agent_board SET folder = COALESCE(
                     (SELECT o.folder FROM agent_board o
                      WHERE o.correlation_id = agent_board.correlation_id AND o.folder IS NOT NULL LIMIT 1),
                     (SELECT o.folder FROM agent_board o
                      WHERE o.session_id = agent_board.session_id AND o.folder IS NOT NULL LIMIT 1))
                 WHERE folder IS NULL",
                [],
            )
        })?;
        Ok(())
    }

    /// One task's running log across everyone who worked on it, newest first.
    pub fn task_log(&self, task_id: &str, before_id: Option<i64>, limit: u32) -> Result<Vec<BoardEntry>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {ENTRY_COLUMNS} FROM agent_board
                 WHERE correlation_id = ?1
                   AND kind IN ('progress', 'noted', 'reported')
                   AND (?2 IS NULL OR id < ?2)
                 ORDER BY id DESC
                 LIMIT ?3"
            ))?;
            let rows = stmt.query_map(params![task_id, before_id, limit], entry_from_row)?;
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
    /// for and whose task is still open: "what is still outstanding from my side".
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
                   AND NOT EXISTS (
                       SELECT 1 FROM tasks t
                       WHERE t.id = d.correlation_id
                         AND t.status IN ('done', 'cancelled', 'failed'))
                 ORDER BY d.created_at DESC",
            )?;
            let rows = stmt.query_map(params![agent_id], |row| row.get::<_, String>(0))?;
            rows.collect()
        })
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
}
