//! Per-workspace persistent memory: durable facts, pointers, and how-to
//! playbooks the agent accumulates for a folder across sessions. Stored
//! in-project under `<workspace>/.snippet/memory/`:
//!
//! ```text
//! <ws>/.snippet/memory/
//!   index.md          # always loaded into context; LLM-maintained, budget-capped
//!   entries/<id>.md   # full resources, loaded on demand via memory_read
//! ```
//!
//! The index is a lean, always-loaded pointer list; entries hold the detail.
//! Writes are atomic (temp + rename) and bounded by char budgets the caller
//! supplies (from config). Ids are sanitized to a kebab-case slug so they can't
//! escape the entries dir.

use std::fs;
use std::path::{Path, PathBuf};

const MEMORY_DIRNAME: &str = "memory";
const INDEX_FILE: &str = "index.md";
const RULES_FILE: &str = "rules.md";
const PATTERNS_FILE: &str = "patterns.md";
const ENTRIES_DIR: &str = "entries";
const MAX_ID_LEN: usize = 64;
const PATTERNS_BUDGET_CHARS: usize = 5_000;

pub fn patterns_budget() -> usize {
    PATTERNS_BUDGET_CHARS
}
/// Standing rules are injected verbatim into EVERY session, so they're hard-capped
/// small (not user-tunable) — they're directives, not a knowledge store.
const RULES_BUDGET_CHARS: usize = 2_000;

const BLOCK_HEADER: &str = "[workspace_memory]\nDurable memory built across sessions. STANDING RULES are always in force — follow them in every reply. REUSABLE PATTERNS are cross-project techniques — apply the fitting one instead of re-deriving. The REFERENCE INDEX points to fuller entries you load on demand with memory_read(id). Maintain rules with memory_rule, patterns with memory_pattern, entries with memory_write, the index with memory_index. Verify a load-bearing detail against the live code before relying on it.";

const EMPTY_BLOCK: &str = "[workspace_memory]\n(empty) — no durable memory yet. As you learn how this project is built, its conventions and gotchas, or how to do recurring tasks here, save them with memory_write(id, content) plus a pointer line via memory_index. For an always-followed directive (e.g. a user preference like 'in emails, don't use dashes'), use memory_rule(scope, content): scope='global' applies in every workspace, 'workspace' only here. Entries load on demand via memory_read(id).";

/// The hard cap on a standing-rules file (global or per-workspace).
pub fn rules_budget() -> usize {
    RULES_BUDGET_CHARS
}

/// Budgets/flags controlling the memory tools, sourced from config.
#[derive(Debug, Clone)]
pub struct MemoryLimits {
    /// Memory injected into context + read tool offered.
    pub enabled: bool,
    /// Write tools offered (main session only; lanes are read-only).
    pub writable: bool,
    pub index_budget_chars: usize,
    pub entry_budget_chars: usize,
    pub max_entries: usize,
}

impl Default for MemoryLimits {
    fn default() -> Self {
        Self {
            enabled: true,
            writable: true,
            index_budget_chars: 5_000,
            entry_budget_chars: 12_000,
            max_entries: 128,
        }
    }
}

impl MemoryLimits {
    /// Read-only view for delegated lanes: they see the memory but can't write it.
    pub fn read_only() -> Self {
        Self {
            writable: false,
            ..Self::default()
        }
    }
}

pub struct MemoryStore {
    root: PathBuf, // <ws>/.snippet/memory
}

impl MemoryStore {
    pub fn for_workspace(workspace_root: &Path) -> Self {
        Self {
            root: workspace_root.join(".snippet").join(MEMORY_DIRNAME),
        }
    }

    /// The user-level store at `~/.snippet/memory/`, shared by every workspace.
    /// Used for cross-cutting standing rules (e.g. global writing preferences).
    pub fn global() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            root: home.join(".snippet").join(MEMORY_DIRNAME),
        }
    }

    fn index_path(&self) -> PathBuf {
        self.root.join(INDEX_FILE)
    }

    fn rules_path(&self) -> PathBuf {
        self.root.join(RULES_FILE)
    }

    fn patterns_path(&self) -> PathBuf {
        self.root.join(PATTERNS_FILE)
    }

    pub fn read_patterns(&self) -> String {
        fs::read_to_string(self.patterns_path()).unwrap_or_default()
    }

    pub fn add_pattern(&self, line: &str, budget: usize) -> Result<bool, String> {
        let line = line.trim();
        if line.is_empty() {
            return Err("pattern is empty".to_string());
        }
        if line.lines().count() > 1 {
            return Err("add one pattern per call (a single line: situation → approach → why)".to_string());
        }
        let current = self.read_patterns();
        if current.lines().any(|l| l.trim() == line) {
            return Ok(false);
        }
        let new_len = current.chars().count() + line.chars().count() + 1;
        if new_len > budget {
            return Err(format!(
                "patterns would be {} chars over the {budget}-char budget — consolidate first with action='replace' (merge overlapping patterns, drop the least reusable)",
                new_len - budget
            ));
        }
        if let Some(dir) = self.patterns_path().parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.patterns_path())
            .map_err(|e| e.to_string())?;
        let sep = if current.is_empty() || current.ends_with('\n') { "" } else { "\n" };
        f.write_all(format!("{sep}{line}\n").as_bytes()).map_err(|e| e.to_string())?;
        Ok(true)
    }

    pub fn write_patterns(&self, content: &str, budget: usize) -> Result<(), String> {
        let len = content.chars().count();
        if len > budget {
            return Err(format!(
                "patterns are {} chars over the {budget}-char budget — keep each pattern to a tight situation → technique → why; drop the least-reusable ones",
                len - budget
            ));
        }
        if content.trim().is_empty() {
            let _ = fs::remove_file(self.patterns_path());
            return Ok(());
        }
        write_atomic(&self.patterns_path(), content)
    }

    fn entries_dir(&self) -> PathBuf {
        self.root.join(ENTRIES_DIR)
    }

    fn entry_path(&self, id: &str) -> Result<PathBuf, String> {
        let slug = sanitize_id(id)?;
        Ok(self.entries_dir().join(format!("{slug}.md")))
    }

    /// The always-loaded index text, or "" when absent.
    pub fn read_index(&self) -> String {
        fs::read_to_string(self.index_path()).unwrap_or_default()
    }

    pub fn write_index(&self, content: &str, budget: usize) -> Result<(), String> {
        let len = content.chars().count();
        if len > budget {
            return Err(format!(
                "index is {} chars over the {budget}-char budget — drop low-value lines or shorten summaries, then save again",
                len - budget
            ));
        }
        write_atomic(&self.index_path(), content)
    }

    /// Standing rules (always-loaded directives), or "" when absent.
    pub fn read_rules(&self) -> String {
        fs::read_to_string(self.rules_path()).unwrap_or_default()
    }

    /// Replace the standing rules. Empty content clears them (removes the file).
    pub fn write_rules(&self, content: &str, budget: usize) -> Result<(), String> {
        let len = content.chars().count();
        if len > budget {
            return Err(format!(
                "rules are {} chars over the {budget}-char budget — these are always-loaded directives, keep them short and imperative",
                len - budget
            ));
        }
        if content.trim().is_empty() {
            let _ = fs::remove_file(self.rules_path());
            return Ok(());
        }
        write_atomic(&self.rules_path(), content)
    }

    pub fn read_entry(&self, id: &str) -> Result<String, String> {
        let path = self.entry_path(id)?;
        fs::read_to_string(&path).map_err(|_| format!("no memory entry `{}`", sanitize_id(id).unwrap_or_default()))
    }

    pub fn write_entry(
        &self,
        id: &str,
        content: &str,
        budget: usize,
        max_entries: usize,
    ) -> Result<(), String> {
        let path = self.entry_path(id)?;
        let len = content.chars().count();
        if len > budget {
            return Err(format!(
                "entry is {} chars over the {budget}-char budget — split it into focused entries or trim detail",
                len - budget
            ));
        }
        // Cap only applies to NEW entries; updating an existing id is always allowed.
        if !path.exists() && self.list_entries().len() >= max_entries {
            return Err(format!(
                "at the {max_entries}-entry cap — delete or consolidate an existing entry before adding a new one"
            ));
        }
        write_atomic(&path, content)
    }

    /// Sorted entry ids (file stems under `entries/`).
    pub fn list_entries(&self) -> Vec<String> {
        let mut ids: Vec<String> = match fs::read_dir(self.entries_dir()) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let p = e.path();
                    if p.extension().and_then(|x| x.to_str()) == Some("md") {
                        p.file_stem().and_then(|s| s.to_str()).map(String::from)
                    } else {
                        None
                    }
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        ids.sort();
        ids
    }

    pub fn delete_entry(&self, id: &str) -> Result<(), String> {
        let path = self.entry_path(id)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(format!("no memory entry `{}`", sanitize_id(id).unwrap_or_default()))
            }
            Err(e) => Err(e.to_string()),
        }
    }

    /// Just the reference-index portion (index text + entry manifest), budget-capped,
    /// or `None` when no index has been saved. No header — the composer adds that.
    fn index_body(&self, index_budget: usize) -> Option<String> {
        let index = self.read_index();
        let trimmed = index.trim();
        if trimmed.is_empty() {
            return None;
        }
        let body: String = if trimmed.chars().count() > index_budget {
            trimmed.chars().take(index_budget).collect::<String>() + "\n…[index trimmed to budget]"
        } else {
            trimmed.to_string()
        };
        let entries = self.list_entries();
        let manifest = if entries.is_empty() {
            String::new()
        } else {
            format!("\n\nentries (load with memory_read): {}", entries.join(", "))
        };
        Some(format!("{body}{manifest}"))
    }

    /// This workspace's index block alone (no rules), or the empty hint. Retained
    /// for direct/standalone use and tests; sessions use `render_session_memory`.
    pub fn render_for_prompt(&self, index_budget: usize) -> Option<String> {
        match self.index_body(index_budget) {
            Some(body) => Some(format!(
                "{BLOCK_HEADER}\n\nREFERENCE INDEX — load an entry with memory_read(id):\n{body}"
            )),
            None => Some(EMPTY_BLOCK.to_string()),
        }
    }
}

/// Global + this-workspace standing rules, joined (global first), or `None`.
fn combined_rules(workspace_root: &Path) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let g = MemoryStore::global().read_rules();
    if !g.trim().is_empty() {
        parts.push(g.trim().to_string());
    }
    let w = MemoryStore::for_workspace(workspace_root).read_rules();
    if !w.trim().is_empty() {
        parts.push(w.trim().to_string());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

/// The full `[workspace_memory]` block injected into a session's system prefix:
/// always-on STANDING RULES (global + workspace) followed by the on-demand
/// REFERENCE INDEX. Returns the empty hint when there's nothing at all yet.
pub fn render_session_memory(workspace_root: &Path, index_budget: usize) -> Option<String> {
    let rules = combined_rules(workspace_root);
    let patterns = {
        let p = MemoryStore::global().read_patterns();
        (!p.trim().is_empty()).then(|| p.trim().to_string())
    };
    let index = MemoryStore::for_workspace(workspace_root).index_body(index_budget);
    if rules.is_none() && patterns.is_none() && index.is_none() {
        return Some(EMPTY_BLOCK.to_string());
    }
    let mut out = String::from(BLOCK_HEADER);
    if let Some(r) = rules {
        out.push_str("\n\nSTANDING RULES — always follow these, in every reply (global + this folder):\n");
        out.push_str(&r);
    }
    if let Some(p) = patterns {
        out.push_str("\n\nREUSABLE PATTERNS — techniques learned across projects; apply the fitting one instead of re-deriving, adapt as needed:\n");
        out.push_str(&p);
    }
    if let Some(i) = index {
        out.push_str("\n\nREFERENCE INDEX — load an entry with memory_read(id):\n");
        out.push_str(&i);
    }
    Some(out)
}

/// Reduce an id to a safe kebab-case slug; reject anything that could escape the
/// entries dir (slashes, dots, `..`) or is empty/too long.
fn sanitize_id(id: &str) -> Result<String, String> {
    let id = id.trim();
    if id.is_empty() {
        return Err("memory id is empty".to_string());
    }
    if id.len() > MAX_ID_LEN {
        return Err(format!("memory id too long (max {MAX_ID_LEN} chars)"));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err(
            "memory id must be kebab-case: lowercase letters, digits, '-' or '_' only".to_string(),
        );
    }
    Ok(id.to_string())
}

/// Write `content` to `path` atomically (temp file in the same dir + rename),
/// creating parent dirs. Mirrors the persist_state pattern in harness.rs.
fn write_atomic(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut tmp = path.to_path_buf();
    let name = format!(
        "{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("memory")
    );
    tmp.set_file_name(name);
    fs::write(&tmp, content).map_err(|e| e.to_string())?;
    fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    Ok(())
}

