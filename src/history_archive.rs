use std::io::{Read, Write};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::llm::HarnessMessage;
use crate::store::{Store, StoreError};

/// Ensure SQLite archive and FTS5 tables are created.
pub fn ensure_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS session_history_archive (
            archive_id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            ordinal INTEGER NOT NULL,
            role TEXT NOT NULL,
            tool_name TEXT,
            summary TEXT NOT NULL,
            affected_paths TEXT NOT NULL,
            status TEXT NOT NULL,
            payload_compressed BLOB NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_sha_session ON session_history_archive(session_id, ordinal);

        CREATE VIRTUAL TABLE IF NOT EXISTS session_history_fts USING fts5(
            archive_id UNINDEXED,
            session_id UNINDEXED,
            tool_name,
            summary,
            affected_paths,
            searchable_text,
            tokenize = 'unicode61'
        );",
    )
}

pub fn compress_payload(data: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data)?;
    encoder.finish()
}

pub fn decompress_payload(compressed: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    let mut decoder = GzDecoder::new(compressed);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out)?;
    Ok(out)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchivedTurnSummary {
    pub archive_id: i64,
    pub ordinal: usize,
    pub role: String,
    pub tool_name: Option<String>,
    pub summary: String,
    pub affected_paths: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchivedTurnDetail {
    pub archive_id: i64,
    pub session_id: String,
    pub ordinal: i64,
    pub role: String,
    pub tool_name: Option<String>,
    pub summary: String,
    pub affected_paths: String,
    pub status: String,
    pub created_at: String,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistorySearchResult {
    pub archive_id: i64,
    pub role: String,
    pub tool_name: Option<String>,
    pub summary: String,
    pub affected_paths: String,
    pub status: String,
    pub snippet: String,
}

/// Archive a sequence of messages into SQLite and FTS5 index, returning the
/// summary records used to generate micro-pointers.
pub fn archive_messages(
    store: &Store,
    session_id: &str,
    messages: &[HarnessMessage],
    start_ordinal: usize,
    now: &str,
) -> Result<Vec<ArchivedTurnSummary>, StoreError> {
    store.with_connection(|conn| {
        let tx = conn.unchecked_transaction()?;
        let mut summaries = Vec::with_capacity(messages.len());

        for (idx, msg) in messages.iter().enumerate() {
            let ordinal = (start_ordinal + idx) as i64;
            let (role, tool_name, summary, affected_paths, status, searchable_text) =
                extract_message_metadata(msg);

            let raw_json = serde_json::to_vec(msg).map_err(|e| {
                rusqlite::Error::ToSqlConversionFailure(Box::new(e))
            })?;
            let compressed = compress_payload(&raw_json).map_err(|e| {
                rusqlite::Error::ToSqlConversionFailure(Box::new(e))
            })?;

            tx.execute(
                "INSERT INTO session_history_archive (
                    session_id, ordinal, role, tool_name, summary, affected_paths, status, payload_compressed, created_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    session_id,
                    ordinal,
                    role,
                    tool_name,
                    summary,
                    affected_paths,
                    status,
                    compressed,
                    now
                ],
            )?;

            let archive_id = tx.last_insert_rowid();

            // Insert into full text search index
            tx.execute(
                "INSERT INTO session_history_fts (
                    archive_id, session_id, tool_name, summary, affected_paths, searchable_text
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    archive_id,
                    session_id,
                    tool_name.as_deref().unwrap_or(""),
                    &summary,
                    &affected_paths,
                    &searchable_text
                ],
            )?;

            summaries.push(ArchivedTurnSummary {
                archive_id,
                ordinal: ordinal as usize,
                role,
                tool_name,
                summary,
                affected_paths,
                status,
            });
        }

        tx.commit()?;
        Ok(summaries)
    })
}

/// Retrieve and hydrate the unabridged turn payload by archive_id.
pub fn recall_turn(store: &Store, archive_id: i64) -> Result<Option<ArchivedTurnDetail>, StoreError> {
    store.with_connection(|conn| {
        let mut stmt = conn.prepare(
            "SELECT archive_id, session_id, ordinal, role, tool_name, summary, affected_paths, status, created_at, payload_compressed
             FROM session_history_archive
             WHERE archive_id = ?1",
        )?;

        let row = stmt.query_row(params![archive_id], |row| {
            let archive_id: i64 = row.get(0)?;
            let session_id: String = row.get(1)?;
            let ordinal: i64 = row.get(2)?;
            let role: String = row.get(3)?;
            let tool_name: Option<String> = row.get(4)?;
            let summary: String = row.get(5)?;
            let affected_paths: String = row.get(6)?;
            let status: String = row.get(7)?;
            let created_at: String = row.get(8)?;
            let compressed: Vec<u8> = row.get(9)?;
            Ok((
                archive_id,
                session_id,
                ordinal,
                role,
                tool_name,
                summary,
                affected_paths,
                status,
                created_at,
                compressed,
            ))
        }).optional()?;

        let Some((
            archive_id,
            session_id,
            ordinal,
            role,
            tool_name,
            summary,
            affected_paths,
            status,
            created_at,
            compressed,
        )) = row else {
            return Ok(None);
        };

        let raw = decompress_payload(&compressed).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Blob, Box::new(e))
        })?;
        let payload: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);

        Ok(Some(ArchivedTurnDetail {
            archive_id,
            session_id,
            ordinal,
            role,
            tool_name,
            summary,
            affected_paths,
            status,
            created_at,
            payload,
        }))
    })
}

/// Retrieve and hydrate multiple turns by IDs.
pub fn recall_turns(
    store: &Store,
    archive_ids: &[i64],
) -> Result<Vec<ArchivedTurnDetail>, StoreError> {
    let mut turns = Vec::with_capacity(archive_ids.len());
    for &id in archive_ids {
        if let Some(turn) = recall_turn(store, id)? {
            turns.push(turn);
        }
    }
    Ok(turns)
}

/// Retrieve and hydrate a contiguous range of turns [from_id..=to_id].
pub fn recall_turn_range(
    store: &Store,
    session_id: &str,
    from_id: i64,
    to_id: i64,
    limit: usize,
) -> Result<Vec<ArchivedTurnDetail>, StoreError> {
    store.with_connection(|conn| {
        let mut stmt = conn.prepare(
            "SELECT archive_id, session_id, ordinal, role, tool_name, summary, affected_paths, status, created_at, payload_compressed
             FROM session_history_archive
             WHERE session_id = ?1 AND archive_id >= ?2 AND archive_id <= ?3
             ORDER BY archive_id ASC
             LIMIT ?4",
        )?;

        let rows = stmt.query_map(params![session_id, from_id, to_id, limit as i64], |row| {
            let archive_id: i64 = row.get(0)?;
            let session_id: String = row.get(1)?;
            let ordinal: i64 = row.get(2)?;
            let role: String = row.get(3)?;
            let tool_name: Option<String> = row.get(4)?;
            let summary: String = row.get(5)?;
            let affected_paths: String = row.get(6)?;
            let status: String = row.get(7)?;
            let created_at: String = row.get(8)?;
            let compressed: Vec<u8> = row.get(9)?;
            Ok((
                archive_id,
                session_id,
                ordinal,
                role,
                tool_name,
                summary,
                affected_paths,
                status,
                created_at,
                compressed,
            ))
        })?;

        let mut results = Vec::new();
        for r in rows {
            let (
                archive_id,
                session_id,
                ordinal,
                role,
                tool_name,
                summary,
                affected_paths,
                status,
                created_at,
                compressed,
            ) = r?;
            let raw = decompress_payload(&compressed).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Blob, Box::new(e))
            })?;
            let payload: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
            results.push(ArchivedTurnDetail {
                archive_id,
                session_id,
                ordinal,
                role,
                tool_name,
                summary,
                affected_paths,
                status,
                created_at,
                payload,
            });
        }
        Ok(results)
    })
}

/// Search conversation history using SQLite FTS5 (BM25 ranking).
pub fn search_history(
    store: &Store,
    session_id: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<HistorySearchResult>, StoreError> {
    let sanitized_query = sanitize_fts5_query(query);
    if sanitized_query.is_empty() {
        return Ok(Vec::new());
    }

    store.with_connection(|conn| {
        let mut stmt = conn.prepare(
            "SELECT a.archive_id, a.role, a.tool_name, a.summary, a.affected_paths, a.status,
                    snippet(session_history_fts, 5, '<b>', '</b>', '...', 16) AS snip
             FROM session_history_fts f
             JOIN session_history_archive a ON a.archive_id = f.archive_id
             WHERE session_history_fts MATCH ?1 AND f.session_id = ?2
             ORDER BY rank
             LIMIT ?3",
        )?;

        let rows = stmt.query_map(params![sanitized_query, session_id, limit as i64], |row| {
            Ok(HistorySearchResult {
                archive_id: row.get(0)?,
                role: row.get(1)?,
                tool_name: row.get(2)?,
                summary: row.get(3)?,
                affected_paths: row.get(4)?,
                status: row.get(5)?,
                snippet: row.get(6)?,
            })
        })?;

        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    })
}

/// Format the ultra-dense IBM-style micro-pointer manifest.
/// Produces ~12-15 tokens per turn instead of thousands of tokens of lossy text.
pub fn render_micro_pointers(goal: &str, turns: &[ArchivedTurnSummary]) -> String {
    let mut out = String::from("[archived_history]\n");
    if !goal.trim().is_empty() {
        let g = goal.trim().lines().next().unwrap_or("").chars().take(120).collect::<String>();
        out.push_str(&format!("Initial Goal: {g}\n"));
    }
    out.push_str("#id|tool|summary|paths|status\n");
    for turn in turns {
        let tool_or_role = turn.tool_name.as_deref().unwrap_or(&turn.role);
        let safe_sum = turn.summary.replace('|', "/").replace('\n', " ");
        let safe_paths = turn.affected_paths.replace('|', "/");
        out.push_str(&format!(
            "#{id}|{tool}|{summary}|{paths}|{status}\n",
            id = turn.archive_id,
            tool = tool_or_role,
            summary = safe_sum,
            paths = safe_paths,
            status = turn.status
        ));
    }
    out.push_str("Hydrate any raw payload with `recall_context(archive_id)` or find turns via `search_history(query)`.\n");
    out
}

fn sanitize_fts5_query(raw: &str) -> String {
    // Keep alphanumeric and common word chars, strip FTS5 operators like *, ^, :, NEAR
    let tokens: Vec<String> = raw
        .split_whitespace()
        .filter_map(|w| {
            let cleaned: String = w.chars().filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-').collect();
            if cleaned.is_empty() {
                None
            } else {
                Some(format!("\"{cleaned}\""))
            }
        })
        .collect();
    tokens.join(" ")
}

fn extract_message_metadata(
    msg: &HarnessMessage,
) -> (String, Option<String>, String, String, String, String) {
    match msg {
        HarnessMessage::User { content } => {
            let first_line = content.lines().next().unwrap_or("").trim();
            let summary = if first_line.chars().count() > 80 {
                first_line.chars().take(80).collect::<String>() + "…"
            } else {
                first_line.to_string()
            };
            ("user".to_string(), None, summary, String::new(), "ok".to_string(), content.clone())
        }
        HarnessMessage::Assistant { content, tool_calls } => {
            if tool_calls.is_empty() {
                let first_line = content.lines().next().unwrap_or("").trim();
                let summary = if first_line.chars().count() > 80 {
                    first_line.chars().take(80).collect::<String>() + "…"
                } else {
                    first_line.to_string()
                };
                ("assistant".to_string(), None, summary, String::new(), "ok".to_string(), content.clone())
            } else {
                let tool_names = tool_calls
                    .iter()
                    .map(|tc| tc.name.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                let paths = tool_calls
                    .iter()
                    .filter_map(|tc| {
                        tc.arguments.get("path").and_then(Value::as_str)
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                let summary = if let Some(first_tc) = tool_calls.first() {
                    if let Some(cmd) = first_tc.arguments.get("command").and_then(Value::as_str) {
                        let c = cmd.lines().next().unwrap_or("").chars().take(60).collect::<String>();
                        format!("run: {c}")
                    } else if let Some(p) = first_tc.arguments.get("path").and_then(Value::as_str) {
                        format!("{}: {p}", first_tc.name)
                    } else {
                        format!("invoked {}", first_tc.name)
                    }
                } else {
                    "assistant tool calls".to_string()
                };
                let mut searchable = content.clone();
                for tc in tool_calls {
                    searchable.push(' ');
                    searchable.push_str(&tc.name);
                    searchable.push(' ');
                    searchable.push_str(&tc.arguments.to_string());
                }
                ("assistant".to_string(), Some(tool_names), summary, paths, "ok".to_string(), searchable)
            }
        }
        HarnessMessage::ToolResult {
            tool_name,
            content,
            ..
        } => {
            let mut paths = String::new();
            if let Some(p) = content.get("path").and_then(Value::as_str) {
                paths = p.to_string();
            } else if let Some(p) = content.get("saved_output_path").and_then(Value::as_str) {
                paths = p.to_string();
            }

            let status = if let Some(exit) = content.get("exit_code").and_then(Value::as_i64) {
                if exit == 0 { "ok".to_string() } else { format!("exit {exit}") }
            } else if content.get("error").is_some() || content.get("is_error").and_then(Value::as_bool) == Some(true) {
                "error".to_string()
            } else {
                "ok".to_string()
            };

            let summary = match tool_name.as_str() {
                "bash" => {
                    let cmd = content.get("command").and_then(Value::as_str).unwrap_or("");
                    let first = cmd.lines().next().unwrap_or("").trim();
                    let short_cmd = if first.chars().count() > 45 {
                        first.chars().take(45).collect::<String>() + "…"
                    } else if cmd.lines().count() > 1 {
                        format!("{first}…")
                    } else {
                        first.to_string()
                    };
                    format!("bash: {short_cmd}")
                }
                "edit_file" => format!("edited {paths}"),
                "read_file" => format!("read {paths}"),
                "write_file" => format!("wrote {paths}"),
                _ => format!("{tool_name} completed"),
            };

            let searchable = content.to_string();
            ("tool".to_string(), Some(tool_name.clone()), summary, paths, status, searchable)
        }
        HarnessMessage::Summary { kind, content } => {
            ("summary".to_string(), Some(kind.clone()), format!("summary ({kind})"), String::new(), "ok".to_string(), content.clone())
        }
        HarnessMessage::System { content } => {
            ("system".to_string(), None, "system prompt".to_string(), String::new(), "ok".to_string(), content.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compress_and_decompress_roundtrip() {
        let original = b"Lorem ipsum dolor sit amet, consectetur adipiscing elit. Exact error code E0308!";
        let compressed = compress_payload(original).expect("compress");
        assert!(compressed.len() > 0);
        let decompressed = decompress_payload(&compressed).expect("decompress");
        assert_eq!(decompressed, original);
    }

    #[test]
    fn archive_recall_and_fts_search() {
        let store = Store::open_in_memory().expect("open store");
        let messages = vec![
            HarnessMessage::User {
                content: "Please fix compiler error E0308 in auth.rs".to_string(),
            },
            HarnessMessage::ToolResult {
                tool_call_id: "call_1".to_string(),
                tool_name: "bash".to_string(),
                content: json!({
                    "command": "cargo check",
                    "exit_code": 1,
                    "stderr": "error[E0308]: mismatched types in src/auth.rs:42"
                }),
            },
            HarnessMessage::ToolResult {
                tool_call_id: "call_2".to_string(),
                tool_name: "edit_file".to_string(),
                content: json!({
                    "path": "src/auth.rs",
                    "edited": true
                }),
            },
        ];

        let summaries = archive_messages(&store, "session-1", &messages, 0, "2026-09-25T16:00:00Z")
            .expect("archive messages");
        assert_eq!(summaries.len(), 3);

        // Verify micro-pointer output
        let manifest = render_micro_pointers("Fix error E0308", &summaries);
        assert!(manifest.contains("[archived_history]"));
        assert!(manifest.contains("#1|user|Please fix compiler error E0308 in auth.rs||ok"));
        assert!(manifest.contains("#2|bash|bash: cargo check||exit 1"));
        assert!(manifest.contains("#3|edit_file|edited src/auth.rs|src/auth.rs|ok"));

        // Recall turn 2
        let turn2 = recall_turn(&store, summaries[1].archive_id)
            .expect("recall")
            .expect("found turn 2");
        assert_eq!(turn2.tool_name.as_deref(), Some("bash"));
        assert_eq!(turn2.payload["content"]["command"], "cargo check");
        assert_eq!(turn2.status, "exit 1");

        // FTS search for exact error code E0308
        let results = search_history(&store, "session-1", "E0308", 5).expect("search");
        assert!(!results.is_empty());
        assert!(results.iter().any(|r| r.archive_id == summaries[0].archive_id));
        assert!(results.iter().any(|r| r.archive_id == summaries[1].archive_id));

        // Batch recall
        let batch = recall_turns(&store, &[summaries[0].archive_id, summaries[2].archive_id])
            .expect("batch recall");
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].archive_id, summaries[0].archive_id);
        assert_eq!(batch[1].archive_id, summaries[2].archive_id);

        // Range recall
        let range = recall_turn_range(
            &store,
            "session-1",
            summaries[0].archive_id,
            summaries[2].archive_id,
            10,
        )
        .expect("range recall");
        assert_eq!(range.len(), 3);
        assert_eq!(range[0].archive_id, summaries[0].archive_id);
        assert_eq!(range[1].archive_id, summaries[1].archive_id);
        assert_eq!(range[2].archive_id, summaries[2].archive_id);
    }
}
