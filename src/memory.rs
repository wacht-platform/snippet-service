//! Durable agent memory as git repositories in the Agent Memory Repo format
//! (https://github.com/AgentMemoryRepo/agentmemoryrepo).
//!
//! ```text
//! ~/.snippet/memory/                          global: the user, every project
//! ~/.snippet/projects/<repo>-<id>/memory/     project: one per repository
//! ~/.snippet/agents/<agent>/memory/           agent: a specialist's own expertise
//! ~/.snippet/session-memory/<session>/        session: one conversation's working memory
//! ```
//!
//! Every repo has a short `MEMORY.md` entry point (entries, then `## Index` of
//! `[[links]]`) and topic files of one-line entries with `[key: value]`
//! metadata. Agents edit the files and commit with git themselves; the daemon
//! creates the repos and commits the writes it makes itself (reflection and
//! the end-of-turn tidy).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

pub const ENTRY_FILE: &str = "MEMORY.md";
const INDEX_HEADING: &str = "## Index";
const ENTRY_PROMPT_CHARS: usize = 4_000;
const MAX_ENTRY_CHARS: usize = 400;
const IGNORED: &[&str] = &[".lock", ".tmp-*", ".snippet/"];
pub const CONSOLIDATE_PREFIX: &str = "Consolidate:";

static GIT_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    Global,
    Project,
    Agent,
    Session,
}

impl ScopeKind {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "global" => Some(Self::Global),
            "project" => Some(Self::Project),
            "agent" => Some(Self::Agent),
            "session" => Some(Self::Session),
            _ => None,
        }
    }

    fn purpose(self) -> &'static str {
        match self {
            Self::Global => "the user: preferences, standing directives and facts that hold in every project",
            Self::Project => "this repository: where things live, how to build, test and deploy, conventions and decisions; shared by every worktree and session of the repo",
            Self::Agent => "your own expertise as this agent: techniques, checks and lessons that carry across every job you take",
            Self::Session => "this conversation: decisions made, constraints, and the state of the work, so it survives compaction and restarts",
        }
    }
}

/// One memory repository.
#[derive(Debug, Clone)]
pub struct MemoryRepo {
    pub kind: ScopeKind,
    /// Stable address: `global`, `project:<dir>`, `agent:<id>`, `session:<id>`.
    pub key: String,
    pub label: String,
    pub root: PathBuf,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryCommit {
    pub hash: String,
    pub date: i64,
    pub subject: String,
}

impl MemoryRepo {
    pub fn global() -> Self {
        Self {
            kind: ScopeKind::Global,
            key: "global".to_string(),
            label: "Global".to_string(),
            root: crate::config::snippet_home().join("memory"),
        }
    }

    pub fn project(workspace: &Path) -> Self {
        let root = project_memory_dir(workspace);
        let dir = root
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        Self::project_dir(&dir, root)
    }

    fn project_dir(dir: &str, root: PathBuf) -> Self {
        let label = dir.rsplit_once('-').map(|(name, _)| name).unwrap_or(dir).to_string();
        Self {
            kind: ScopeKind::Project,
            key: format!("project:{dir}"),
            label,
            root,
        }
    }

    pub fn agent(agent_id: &str) -> Option<Self> {
        if !safe_component(agent_id) {
            return None;
        }
        Some(Self {
            kind: ScopeKind::Agent,
            key: format!("agent:{agent_id}"),
            label: agent_id.to_string(),
            root: crate::coordination::agents_root(&crate::config::snippet_home().join("mission-control"))
                .join(agent_id)
                .join("memory"),
        })
    }

    pub fn session(session_id: &str) -> Self {
        Self {
            kind: ScopeKind::Session,
            key: format!("session:{session_id}"),
            label: session_id.to_string(),
            root: session_root().join(session_dir_name(session_id)),
        }
    }

    /// Resolve an address from [`MemoryRepo::key`].
    pub fn from_key(key: &str) -> Option<Self> {
        let (kind, rest) = key.split_once(':').unwrap_or((key, ""));
        match kind {
            "global" if rest.is_empty() => Some(Self::global()),
            "project" if safe_component(rest) => {
                let root = crate::config::snippet_home().join("projects").join(rest).join("memory");
                Some(Self::project_dir(rest, root))
            }
            "agent" => Self::agent(rest),
            "session" if !rest.is_empty() => Some(Self::session(rest)),
            _ => None,
        }
    }

    /// Every memory repo that exists on this machine.
    pub fn all() -> Vec<Self> {
        let mut out = Vec::new();
        let global = Self::global();
        if global.root.is_dir() {
            out.push(global);
        }
        let home = crate::config::snippet_home();
        for entry in read_dirs(&home.join("projects")) {
            let root = entry.join("memory");
            if root.is_dir() {
                let dir = entry.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                out.push(Self::project_dir(&dir, root));
            }
        }
        let agents = crate::coordination::agents_root(&home.join("mission-control"));
        for entry in read_dirs(&agents) {
            if entry.join("memory").is_dir()
                && let Some(repo) = entry.file_name().and_then(|n| Self::agent(&n.to_string_lossy()))
            {
                out.push(repo);
            }
        }
        for entry in read_dirs(&session_root()) {
            if let Some(name) = entry.file_name() {
                let id = session_id_from_dir(&name.to_string_lossy());
                out.push(Self::session(&id));
            }
        }
        out
    }

    pub fn exists(&self) -> bool {
        self.root.join(".git").exists() && self.root.join(ENTRY_FILE).is_file()
    }

    /// Create the repo (migrating the old layout once) if it is not one yet.
    pub fn ensure(&self) -> Result<(), String> {
        let _guard = GIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if self.exists() {
            return self.ignore_scratch();
        }
        fs::create_dir_all(&self.root).map_err(|e| format!("create {}: {e}", self.root.display()))?;
        if !self.root.join(ENTRY_FILE).is_file() {
            write_atomic(&self.root.join(ENTRY_FILE), &format!("# {}\n\n{INDEX_HEADING}\n", self.title()))?;
        }
        self.ignore_scratch()?;
        if !self.root.join(".git").exists() {
            self.git(&["init", "-q", "-b", "main"])?;
        }
        self.git(&["config", "user.name", "Snippet"])?;
        self.git(&["config", "user.email", "memory@snippet.local"])?;
        self.git(&["config", "commit.gpgsign", "false"])?;
        self.git(&["add", "-A"])?;
        let _ = self.git(&["commit", "-q", "-m", "Create memory repo"]);
        Ok(())
    }

    /// Keep lock files, temp files and the scratch `.snippet/` folder (shell
    /// logs written by agents working in the repo) out of history.
    fn ignore_scratch(&self) -> Result<(), String> {
        let path = self.root.join(".gitignore");
        let text = fs::read_to_string(&path).unwrap_or_default();
        let missing: Vec<&str> = IGNORED
            .iter()
            .copied()
            .filter(|rule| !text.lines().any(|l| l.trim() == *rule))
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        let mut updated = text.trim_end().to_string();
        for rule in missing {
            if !updated.is_empty() {
                updated.push('\n');
            }
            updated.push_str(rule);
        }
        updated.push('\n');
        write_atomic(&path, &updated)
    }

    fn title(&self) -> String {
        match self.kind {
            ScopeKind::Global => "Memory: global".to_string(),
            ScopeKind::Project => format!("Memory: project {}", self.label),
            ScopeKind::Agent => format!("Memory: agent {}", self.label),
            ScopeKind::Session => "Memory: session".to_string(),
        }
    }

    pub fn entry_point(&self) -> String {
        fs::read_to_string(self.root.join(ENTRY_FILE)).unwrap_or_default()
    }

    pub fn git(&self, args: &[&str]) -> Result<String, String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .output()
            .map_err(|e| format!("git: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).to_string())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    /// Commit everything changed in the repo. Returns whether a commit was made.
    pub fn commit_all(&self, message: &str) -> Result<bool, String> {
        let _guard = GIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if self.git(&["status", "--porcelain"])?.trim().is_empty() {
            return Ok(false);
        }
        self.git(&["add", "-A"])?;
        self.git(&["commit", "-q", "-m", message])?;
        Ok(true)
    }

    pub fn history(&self, limit: usize) -> Vec<MemoryCommit> {
        let limit = format!("-{limit}");
        self.git(&["log", &limit, "--format=%H%x1f%ct%x1f%s"])
            .unwrap_or_default()
            .lines()
            .filter_map(|line| {
                let mut parts = line.split('\x1f');
                Some(MemoryCommit {
                    hash: parts.next()?.to_string(),
                    date: parts.next()?.parse().ok()?,
                    subject: parts.next().unwrap_or("").to_string(),
                })
            })
            .collect()
    }

    /// Files in the repo, relative to its root, sorted.
    pub fn files(&self) -> Vec<String> {
        let mut out = Vec::new();
        collect_files(&self.root, &self.root, &mut out);
        out.sort();
        out
    }

    pub fn read_file(&self, rel: &str) -> Result<String, String> {
        let path = self.resolve(rel)?;
        fs::read_to_string(&path).map_err(|e| format!("read {rel}: {e}"))
    }

    fn resolve(&self, rel: &str) -> Result<PathBuf, String> {
        let rel = rel.trim().trim_start_matches('/');
        if rel.is_empty()
            || rel.split('/').any(|p| p.is_empty() || p == "." || p == ".." || p == ".git")
        {
            return Err(format!("invalid memory path: {rel}"));
        }
        Ok(self.root.join(rel))
    }

    /// Unix time of the last consolidation commit, if any.
    pub fn last_consolidated(&self) -> Option<i64> {
        self.git(&["log", "-1", "--format=%ct", "--grep", &format!("^{CONSOLIDATE_PREFIX}")])
            .ok()
            .and_then(|s| s.trim().parse().ok())
    }

    /// Commits since the last consolidation that were not consolidations themselves, and the
    /// time of the newest.
    pub fn activity_since_consolidation(&self) -> (usize, Option<i64>) {
        let since = self.last_consolidated().map(|t| format!("--since=@{}", t + 1));
        let mut args = vec!["log", "--format=%ct%x1f%s"];
        if let Some(since) = since.as_deref() {
            args.push(since);
        }
        let log = self.git(&args).unwrap_or_default();
        let mut count = 0;
        let mut newest = None;
        for line in log.lines() {
            let (time, subject) = line.split_once('\x1f').unwrap_or((line, ""));
            if subject.starts_with(CONSOLIDATE_PREFIX) || subject == "Create memory repo" {
                continue;
            }
            count += 1;
            let t: Option<i64> = time.parse().ok();
            newest = newest.max(t);
        }
        (count, newest)
    }

    /// Add one entry to `file` (creating it, and linking it from the index,
    /// when it is new). Used by the reflection pass; agents edit directly.
    pub fn add_entry(&self, file: &str, entry: &str) -> Result<String, String> {
        let entry = normalize_entry(entry)?;
        let file = topic_file(file)?;
        let path = self.resolve(&file)?;
        let existing = fs::read_to_string(&path).unwrap_or_default();
        if existing.lines().any(|l| l.trim() == entry) {
            return Ok(format!("{file}: already has it"));
        }
        let updated = if file == ENTRY_FILE {
            insert_before_index(&existing, &entry)
        } else if existing.trim().is_empty() {
            format!("# {}\n\n{entry}\n", heading_for(&file))
        } else {
            format!("{}\n{entry}\n", existing.trim_end())
        };
        write_atomic(&path, &updated)?;
        if file != ENTRY_FILE {
            self.link_from_index(&file)?;
        }
        Ok(format!("{file}: added {}", clip(&entry, 80)))
    }

    /// Replace the entry in `file` that contains `matching` with `entry`.
    pub fn replace_entry(&self, file: &str, matching: &str, entry: &str) -> Result<String, String> {
        let entry = normalize_entry(entry)?;
        let file = topic_file(file)?;
        self.rewrite_line(&file, matching, Some(&entry))
    }

    pub fn remove_entry(&self, file: &str, matching: &str) -> Result<String, String> {
        let file = topic_file(file)?;
        self.rewrite_line(&file, matching, None)
    }

    fn rewrite_line(&self, file: &str, matching: &str, with: Option<&str>) -> Result<String, String> {
        let matching = matching.trim();
        if matching.len() < 8 {
            return Err("match text is too short to identify one entry".to_string());
        }
        let path = self.resolve(file)?;
        let text = fs::read_to_string(&path).map_err(|e| format!("read {file}: {e}"))?;
        let hits: Vec<usize> = text
            .lines()
            .enumerate()
            .filter(|(_, l)| l.trim_start().starts_with("- ") && l.contains(matching))
            .map(|(i, _)| i)
            .collect();
        let [hit] = hits.as_slice() else {
            return Err(format!("{} entries in {file} match", hits.len()));
        };
        let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
        match with {
            Some(entry) => lines[*hit] = entry.to_string(),
            None => {
                lines.remove(*hit);
            }
        }
        write_atomic(&path, &(lines.join("\n") + "\n"))?;
        Ok(match with {
            Some(entry) => format!("{file}: replaced with {}", clip(entry, 80)),
            None => format!("{file}: removed {}", clip(matching, 80)),
        })
    }

    /// After a turn: link every topic file nothing links to from the index,
    /// then commit whatever the session left uncommitted. Returns the files it
    /// linked and whether it committed.
    pub fn tidy(&self) -> Result<(Vec<String>, bool), String> {
        if !self.exists() {
            return Ok((Vec::new(), false));
        }
        let files = self.files();
        let text: String = files
            .iter()
            .filter(|f| f.ends_with(".md"))
            .map(|f| self.read_file(f).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");
        let mut linked = Vec::new();
        for file in files.iter().filter(|f| f.ends_with(".md") && f.as_str() != ENTRY_FILE) {
            if !text.contains(&format!("[[{}]]", file.trim_end_matches(".md"))) {
                self.link_from_index(file)?;
                linked.push(file.clone());
            }
        }
        let committed = self.commit_all(if linked.is_empty() {
            "Remember edits left uncommitted"
        } else {
            "Remember edits and link new topic files"
        })?;
        Ok((linked, committed))
    }

    fn link_from_index(&self, file: &str) -> Result<(), String> {
        let title = self
            .read_file(file)
            .ok()
            .and_then(|t| t.lines().find_map(|l| l.strip_prefix("# ").map(|s| s.trim().to_string())))
            .filter(|t| !t.is_empty());
        let link = format!("[[{}]]", file.trim_end_matches(".md"));
        let path = self.root.join(ENTRY_FILE);
        let text = fs::read_to_string(&path).unwrap_or_default();
        if text.contains(&link) {
            return Ok(());
        }
        let line = match title {
            Some(title) => format!("- {link} — {title}"),
            None => format!("- {link}"),
        };
        let updated = if text.contains(INDEX_HEADING) {
            format!("{}\n{line}\n", text.trim_end())
        } else {
            format!("{}\n\n{INDEX_HEADING}\n{line}\n", text.trim_end())
        };
        write_atomic(&path, &updated)
    }
}

/// The repos a session reads, most general first. Each is created on demand.
pub fn scopes_for(workspace: Option<&Path>, agent: Option<&str>, session: Option<&str>) -> Vec<MemoryRepo> {
    let mut repos = vec![MemoryRepo::global()];
    if let Some(workspace) = workspace {
        repos.push(MemoryRepo::project(workspace));
    }
    if let Some(repo) = agent.and_then(MemoryRepo::agent) {
        repos.push(repo);
    }
    if let Some(session) = session.filter(|s| !s.trim().is_empty()) {
        repos.push(MemoryRepo::session(session));
    }
    repos.retain(|repo| repo.ensure().is_ok());
    repos
}

/// The `[memory]` block appended to the system prompt.
pub fn render_prompt(repos: &[MemoryRepo]) -> String {
    let mut out = String::from("[memory]\n");
    for repo in repos {
        let scope = match repo.kind {
            ScopeKind::Global => "Global".to_string(),
            ScopeKind::Project => format!("Project ({})", repo.label),
            ScopeKind::Agent => format!("Agent ({})", repo.label),
            ScopeKind::Session => "Session".to_string(),
        };
        out.push_str(&format!(
            "\n### {scope} memory — {}\nHolds {}.\n\n{}\n",
            repo.root.display(),
            repo.kind.purpose(),
            clip(repo.entry_point().trim(), ENTRY_PROMPT_CHARS)
        ));
    }
    out.trim_end().to_string()
}

/// The project memory folder for a workspace, keyed by its repository so every
/// linked worktree and subfolder of one repository shares it. The key is the
/// main checkout (or a bare repository's git dir); outside git it is the
/// workspace itself.
fn project_memory_dir(workspace: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    let project = repository_root(workspace);
    let name: String = project
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let digest = Sha256::digest(project.to_string_lossy().as_bytes());
    let id: String = digest.iter().take(5).map(|b| format!("{b:02x}")).collect();
    let name = if name.is_empty() { "project".to_string() } else { name };
    crate::config::snippet_home()
        .join("projects")
        .join(format!("{name}-{id}"))
        .join("memory")
}

fn repository_root(workspace: &Path) -> PathBuf {
    let workspace = workspace.canonicalize().unwrap_or_else(|_| workspace.to_path_buf());
    let common = Command::new("git")
        .arg("-C")
        .arg(&workspace)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|dir| !dir.is_empty());
    let Some(common) = common else {
        return workspace;
    };
    let common = workspace.join(common);
    let common = common.canonicalize().unwrap_or(common);
    match common.parent() {
        Some(main) if common.file_name().is_some_and(|n| n == ".git") => main.to_path_buf(),
        _ => common,
    }
}

fn session_root() -> PathBuf {
    crate::config::snippet_home().join("session-memory")
}

fn session_dir_name(session_id: &str) -> String {
    session_id
        .chars()
        .map(|c| match c {
            '/' => '~',
            c if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') => c,
            _ => '_',
        })
        .collect()
}

fn session_id_from_dir(dir: &str) -> String {
    dir.replace('~', "/")
}

/// Remove a session's memory when the session itself is deleted.
pub fn delete_session_memory(session_id: &str) {
    let _ = fs::remove_dir_all(MemoryRepo::session(session_id).root);
}

fn safe_component(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn read_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
        .unwrap_or_default();
    out.sort();
    out
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            collect_files(root, &path, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push(rel.to_string_lossy().to_string());
        }
    }
}

fn topic_file(file: &str) -> Result<String, String> {
    let file = file.trim().trim_start_matches('/');
    let file = if file.is_empty() { ENTRY_FILE.to_string() } else { file.to_string() };
    let file = if file.ends_with(".md") { file } else { format!("{file}.md") };
    if file.split('/').any(|p| p.is_empty() || p == "." || p == ".." || p.starts_with('.')) {
        return Err(format!("invalid memory file: {file}"));
    }
    Ok(file)
}

fn heading_for(file: &str) -> String {
    let stem = file.trim_end_matches(".md").rsplit('/').next().unwrap_or(file);
    let words = stem.replace(['-', '_'], " ");
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => "Notes".to_string(),
    }
}

fn normalize_entry(entry: &str) -> Result<String, String> {
    let text = entry.trim().trim_start_matches("- ").trim();
    if text.is_empty() {
        return Err("entry is empty".to_string());
    }
    if text.contains('\n') {
        return Err("an entry is one line".to_string());
    }
    if text.chars().count() > MAX_ENTRY_CHARS {
        return Err(format!("entry is longer than {MAX_ENTRY_CHARS} characters; move detail to a topic file"));
    }
    Ok(format!("- {text}"))
}

fn insert_before_index(text: &str, entry: &str) -> String {
    match text.find(INDEX_HEADING) {
        Some(at) => {
            let (head, tail) = text.split_at(at);
            format!("{}\n{entry}\n\n{tail}", head.trim_end())
        }
        None => format!("{}\n{entry}\n", text.trim_end()),
    }
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    format!("{cut}…")
}

fn write_atomic(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_file_name(format!(
        ".tmp-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("memory")
    ));
    fs::write(&tmp, content).map_err(|e| e.to_string())?;
    fs::rename(&tmp, path).map_err(|e| e.to_string())
}
