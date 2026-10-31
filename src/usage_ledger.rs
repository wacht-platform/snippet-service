use async_trait::async_trait;
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::llm::{
    AgentModel, HarnessMessage, ModelOutput, NativeToolDefinition, StreamHandle, TokenUsage,
};
use crate::store::{Store, StoreError};
use crate::tools::ToolError;

const BUCKET_SECS: i64 = 900;
const LEDGER_RETENTION_SECS: i64 = 90 * 24 * 3600;

pub fn ensure_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS usage_ledger (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            at INTEGER NOT NULL,
            session_id TEXT NOT NULL,
            provider TEXT NOT NULL,
            model TEXT NOT NULL,
            prompt_tokens INTEGER NOT NULL,
            completion_tokens INTEGER NOT NULL,
            cache_read_tokens INTEGER NOT NULL,
            cache_creation_tokens INTEGER NOT NULL,
            total_tokens INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_usage_ledger_at ON usage_ledger(at);
        CREATE TABLE IF NOT EXISTS usage_rollup (
            bucket INTEGER NOT NULL,
            provider TEXT NOT NULL,
            model TEXT NOT NULL,
            calls INTEGER NOT NULL,
            prompt_tokens INTEGER NOT NULL,
            completion_tokens INTEGER NOT NULL,
            cache_read_tokens INTEGER NOT NULL,
            cache_creation_tokens INTEGER NOT NULL,
            total_tokens INTEGER NOT NULL,
            PRIMARY KEY (bucket, provider, model)
        );
        CREATE TABLE IF NOT EXISTS usage_rollup_sessions (
            bucket INTEGER NOT NULL,
            provider TEXT NOT NULL,
            session_id TEXT NOT NULL,
            PRIMARY KEY (bucket, provider, session_id)
        );",
    )?;
    if conn.prepare("SELECT purpose FROM usage_ledger LIMIT 0").is_err() {
        conn.execute_batch(
            "ALTER TABLE usage_ledger ADD COLUMN purpose TEXT NOT NULL DEFAULT 'turn';",
        )?;
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_usage_ledger_session ON usage_ledger(session_id, at);",
    )?;
    Ok(())
}

pub const PURPOSE_TURN: &str = "turn";
pub const PURPOSE_KEEP_WARM: &str = "keep_warm";

fn add_to_rollup(
    conn: &Connection,
    at: i64,
    session_id: &str,
    provider: &str,
    model: &str,
    usage: &TokenUsage,
    calls: i64,
) -> Result<(), rusqlite::Error> {
    let bucket = at - at.rem_euclid(BUCKET_SECS);
    conn.execute(
        "INSERT INTO usage_rollup (bucket, provider, model, calls, prompt_tokens,
             completion_tokens, cache_read_tokens, cache_creation_tokens, total_tokens)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT (bucket, provider, model) DO UPDATE SET
             calls = calls + excluded.calls,
             prompt_tokens = prompt_tokens + excluded.prompt_tokens,
             completion_tokens = completion_tokens + excluded.completion_tokens,
             cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens,
             cache_creation_tokens = cache_creation_tokens + excluded.cache_creation_tokens,
             total_tokens = total_tokens + excluded.total_tokens",
        params![
            bucket,
            provider,
            model,
            calls,
            usage.prompt_tokens as i64,
            usage.completion_tokens as i64,
            usage.cache_read_tokens as i64,
            usage.cache_creation_tokens as i64,
            usage.total_tokens as i64,
        ],
    )?;
    if !session_id.is_empty() {
        conn.execute(
            "INSERT OR IGNORE INTO usage_rollup_sessions (bucket, provider, session_id)
             VALUES (?1, ?2, ?3)",
            params![bucket, provider, session_id],
        )?;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageTotals {
    pub provider: String,
    pub model: String,
    pub calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub total_tokens: u64,
    pub first_at: i64,
    pub last_at: i64,
}

impl Store {
    pub fn record_usage(
        &self,
        session_id: &str,
        provider: &str,
        model: &str,
        usage: &TokenUsage,
    ) -> Result<(), StoreError> {
        self.record_usage_for(session_id, provider, model, usage, PURPOSE_TURN)
    }

    /// Record a call under `purpose`, so keep-warm spend stays apart from turns.
    pub fn record_usage_for(
        &self,
        session_id: &str,
        provider: &str,
        model: &str,
        usage: &TokenUsage,
        purpose: &str,
    ) -> Result<(), StoreError> {
        let mut usage = *usage;
        if usage.total_tokens == 0 {
            usage.total_tokens = usage.prompt_tokens.saturating_add(usage.completion_tokens);
        }
        let now = chrono::Utc::now().timestamp();
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "INSERT INTO usage_ledger (at, session_id, provider, model, prompt_tokens,
                     completion_tokens, cache_read_tokens, cache_creation_tokens, total_tokens,
                     purpose)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    now,
                    session_id,
                    provider,
                    model,
                    usage.prompt_tokens as i64,
                    usage.completion_tokens as i64,
                    usage.cache_read_tokens as i64,
                    usage.cache_creation_tokens as i64,
                    usage.total_tokens as i64,
                    purpose,
                ],
            )?;
            let ledger_id = tx.last_insert_rowid();
            add_to_rollup(&tx, now, session_id, provider, model, &usage, 1)?;
            if ledger_id % 500 == 0 {
                tx.execute(
                    "DELETE FROM usage_ledger WHERE at < ?1",
                    params![now - LEDGER_RETENTION_SECS],
                )?;
            }
            tx.commit()
        })
    }

    pub fn usage_totals(&self, since: Option<i64>) -> Result<Vec<UsageTotals>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT provider, model, SUM(calls), SUM(prompt_tokens), SUM(completion_tokens),
                        SUM(cache_read_tokens), SUM(cache_creation_tokens), SUM(total_tokens),
                        MIN(bucket), MAX(bucket)
                 FROM usage_rollup
                 WHERE bucket >= ?1
                 GROUP BY provider, model
                 ORDER BY SUM(total_tokens) DESC",
            )?;
            let rows = stmt.query_map(
                params![bucket_floor(since)],
                |row| {
                    Ok(UsageTotals {
                        provider: row.get(0)?,
                        model: row.get(1)?,
                        calls: row.get::<_, i64>(2)? as u64,
                        prompt_tokens: row.get::<_, i64>(3)? as u64,
                        completion_tokens: row.get::<_, i64>(4)? as u64,
                        cache_read_tokens: row.get::<_, i64>(5)? as u64,
                        cache_creation_tokens: row.get::<_, i64>(6)? as u64,
                        total_tokens: row.get::<_, i64>(7)? as u64,
                        first_at: row.get(8)?,
                        last_at: row.get(9)?,
                    })
                },
            )?;
            rows.collect()
        })
    }

    pub fn usage_sessions_by_provider(
        &self,
        since: Option<i64>,
    ) -> Result<Vec<(String, u64)>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT provider, COUNT(DISTINCT session_id) FROM usage_rollup_sessions
                 WHERE bucket >= ?1
                 GROUP BY provider",
            )?;
            let rows = stmt.query_map(
                params![bucket_floor(since)],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64)),
            )?;
            rows.collect()
        })
    }
}

impl Store {
    /// When the session last made a model call for a real turn (not keep-warm).
    pub fn last_turn_at(&self, session_id: &str) -> Result<Option<i64>, StoreError> {
        self.with_connection(|conn| {
            conn.query_row(
                "SELECT MAX(at) FROM usage_ledger WHERE session_id = ?1 AND purpose = ?2",
                params![session_id, PURPOSE_TURN],
                |row| row.get::<_, Option<i64>>(0),
            )
        })
    }

    /// Keep-warm calls and their tokens since `since`, per session.
    pub fn keep_warm_totals(&self, since: i64) -> Result<Vec<serde_json::Value>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT session_id, provider, model, COUNT(*), SUM(prompt_tokens),
                        SUM(completion_tokens), SUM(cache_read_tokens), SUM(cache_creation_tokens)
                 FROM usage_ledger WHERE purpose = ?1 AND at >= ?2
                 GROUP BY session_id, provider, model ORDER BY session_id",
            )?;
            let rows = stmt.query_map(params![PURPOSE_KEEP_WARM, since], |row| {
                Ok(serde_json::json!({
                    "session_id": row.get::<_, String>(0)?,
                    "provider": row.get::<_, String>(1)?,
                    "model": row.get::<_, String>(2)?,
                    "calls": row.get::<_, i64>(3)?,
                    "prompt_tokens": row.get::<_, i64>(4)?,
                    "completion_tokens": row.get::<_, i64>(5)?,
                    "cache_read_tokens": row.get::<_, i64>(6)?,
                    "cache_creation_tokens": row.get::<_, i64>(7)?,
                }))
            })?;
            rows.collect::<Result<_, _>>()
        })
    }
}

fn bucket_floor(since: Option<i64>) -> i64 {
    since
        .map(|at| at - at.rem_euclid(BUCKET_SECS))
        .unwrap_or(i64::MIN)
}

pub fn record(session_id: &str, provider: &str, model: &str, usage: &TokenUsage) {
    record_for(session_id, provider, model, usage, PURPOSE_TURN);
}

pub fn record_for(session_id: &str, provider: &str, model: &str, usage: &TokenUsage, purpose: &str) {
    if usage.prompt_tokens == 0 && usage.completion_tokens == 0 && usage.total_tokens == 0 {
        return;
    }
    if let Ok(store) = Store::open_cached(crate::store::default_db_path()) {
        let _ = store.record_usage_for(session_id, provider, model, usage, purpose);
    }
}

pub struct MeteredModel {
    inner: Box<dyn AgentModel>,
    provider: String,
    model: String,
    session_id: String,
}

impl MeteredModel {
    pub fn new(
        inner: Box<dyn AgentModel>,
        provider: impl Into<String>,
        model: impl Into<String>,
        session_id: Option<String>,
    ) -> Self {
        Self {
            inner,
            provider: provider.into(),
            model: model.into(),
            session_id: session_id.unwrap_or_default(),
        }
    }

    fn record(&self, usage: &TokenUsage) {
        record(&self.session_id, &self.provider, &self.model, usage);
    }
}

#[async_trait]
impl AgentModel for MeteredModel {
    async fn generate(
        &mut self,
        messages: &[HarnessMessage],
        tools: &[NativeToolDefinition],
        force_tool: bool,
        sink: Option<StreamHandle>,
    ) -> Result<ModelOutput, ToolError> {
        let output = self
            .inner
            .generate(messages, tools, force_tool, sink)
            .await?;
        if let Some(usage) = output.usage.as_ref() {
            self.record(usage);
        }
        Ok(output)
    }

    fn supports_images(&self) -> bool {
        self.inner.supports_images()
    }

    fn is_configured(&self) -> bool {
        self.inner.is_configured()
    }

    fn swap_reasoning_effort(&mut self, effort: Option<String>) -> Option<String> {
        self.inner.swap_reasoning_effort(effort)
    }

    fn cli_agent_profile(&self) -> Option<crate::config::InferenceProfileConfig> {
        self.inner.cli_agent_profile()
    }

    fn keep_warm_support(&self) -> Result<(), String> {
        self.inner
            .keep_warm_support()
            .map_err(|inner| crate::keep_warm::reason_for(&self.provider).map_or(inner, str::to_string))
    }

    async fn keep_warm(
        &mut self,
        messages: &[HarnessMessage],
        tools: &[NativeToolDefinition],
    ) -> Result<TokenUsage, ToolError> {
        let usage = self.inner.keep_warm(messages, tools).await?;
        record_for(&self.session_id, &self.provider, &self.model, &usage, PURPOSE_KEEP_WARM);
        Ok(usage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(read: u64) -> TokenUsage {
        TokenUsage { prompt_tokens: 50_000, completion_tokens: 1, cache_read_tokens: read, ..TokenUsage::default() }
    }

    #[test]
    fn keep_warm_calls_are_tagged_apart_from_turns() {
        let store = Store::open_in_memory().unwrap();
        store.record_usage("inbox-researcher", "anthropic", "sonnet", &usage(0)).unwrap();
        let turn_at = store.last_turn_at("inbox-researcher").unwrap().unwrap();
        store
            .record_usage_for("inbox-researcher", "anthropic", "sonnet", &usage(49_000), PURPOSE_KEEP_WARM)
            .unwrap();
        assert_eq!(store.last_turn_at("inbox-researcher").unwrap(), Some(turn_at));
        assert_eq!(store.last_turn_at("other").unwrap(), None);
        let spend = store.keep_warm_totals(0).unwrap();
        assert_eq!(spend.len(), 1);
        assert_eq!(spend[0]["calls"], 1);
        assert_eq!(spend[0]["cache_read_tokens"], 49_000);
    }

    #[test]
    fn an_old_ledger_gains_the_purpose_column() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE usage_ledger (id INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL,
             session_id TEXT NOT NULL, provider TEXT NOT NULL, model TEXT NOT NULL,
             prompt_tokens INTEGER NOT NULL, completion_tokens INTEGER NOT NULL,
             cache_read_tokens INTEGER NOT NULL, cache_creation_tokens INTEGER NOT NULL,
             total_tokens INTEGER NOT NULL);
             INSERT INTO usage_ledger VALUES (1, 10, 's', 'p', 'm', 1, 1, 0, 0, 2);",
        )
        .unwrap();
        ensure_schema(&conn).unwrap();
        ensure_schema(&conn).unwrap();
        let purpose: String = conn.query_row("SELECT purpose FROM usage_ledger", [], |r| r.get(0)).unwrap();
        assert_eq!(purpose, PURPOSE_TURN);
    }
}
