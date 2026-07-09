//! Agent Skills (the SKILL.md open standard). A skill is a folder with a
//! `SKILL.md`: YAML frontmatter (`name`, `description`) plus a markdown body.
//! Scanned roots, in priority order (earlier wins on a name clash):
//! `~/.snippet/skills/`, `~/.claude/skills/`, `~/.codex/skills/` — the format
//! is shared across Claude Code and Codex, so skills installed for either work
//! here unchanged (deduped by name).
//!
//! Progressive disclosure: skills are NOT preloaded into context — the agent
//! finds them on demand with `search_skills` (level 1: name + description),
//! loads a body with `skill(name)` (level 2), and reads/runs bundled files with
//! the normal read_file / bash tools (level 3).

use std::path::{Path, PathBuf};

pub struct Skill {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
    pub path: PathBuf, // SKILL.md
    pub disable_model_invocation: bool,
}

/// snippet's own skills root (also where skill-management writes would go).
pub fn skills_root() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".snippet").join("skills")
}

/// All roots scanned for skills, priority order — earlier wins on a name clash.
/// Claude Code's and Codex's global skill folders use the same SKILL.md
/// standard, so anything installed for them just works here.
pub fn skills_roots() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    vec![
        skills_root(),
        home.join(".claude").join("skills"),
        home.join(".codex").join("skills"),
    ]
}

/// Discover every skill across all roots, deduped by name (first root wins).
pub fn discover() -> Vec<Skill> {
    let mut out: Vec<Skill> = Vec::new();
    for root in skills_roots() {
        for sk in discover_in(&root) {
            if !out.iter().any(|e| e.name == sk.name) {
                out.push(sk);
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Like [`discover`] but against an explicit root (used in tests).
pub fn discover_in(root: &Path) -> Vec<Skill> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for e in entries.flatten() {
        if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        // Skip hidden dirs (e.g. Codex's ~/.codex/skills/.system tree).
        if e.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let dir = e.path();
        let path = dir.join("SKILL.md");
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let (mut name, description, disable) = parse_frontmatter(&text);
        if name.is_empty() {
            // default to the folder name, like the open standard
            name = dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        }
        if name.is_empty() || description.is_empty() {
            continue;
        }
        out.push(Skill { name, description, dir, path, disable_model_invocation: disable });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Pull `name`, `description`, `disable-model-invocation` out of the leading
/// `---` YAML frontmatter. We only need a few scalars, so a small line parser
/// avoids a YAML dependency.
fn parse_frontmatter(text: &str) -> (String, String, bool) {
    let mut name = String::new();
    let mut description = String::new();
    let mut disable = false;
    let Some(rest) = text.strip_prefix("---") else {
        return (name, description, disable);
    };
    let Some(end) = rest.find("\n---") else {
        return (name, description, disable);
    };
    for line in rest[..end].lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("name:") {
            name = unquote(v);
        } else if let Some(v) = line.strip_prefix("description:") {
            description = unquote(v);
        } else if let Some(v) = line.strip_prefix("disable-model-invocation:") {
            disable = v.trim() == "true";
        }
    }
    (name, description, disable)
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    s.strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .or_else(|| s.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')))
        .unwrap_or(s)
        .to_string()
}

/// Find skills relevant to `query`, ranked. Skills are NOT preloaded into the
/// agent's context — it discovers them on demand via this, so context stays lean
/// no matter how many skills exist. Matches over name + description (weighted) and
/// the SKILL.md body. Returns (name, description) pairs, best first; when nothing
/// matches (or the query is blank) it returns all skills, so discovery never dead-ends.
pub fn search(query: &str) -> Vec<(String, String)> {
    rank(discover(), query)
}

/// Like [`search`] but against an explicit root (used in tests).
pub fn search_in(root: &Path, query: &str) -> Vec<(String, String)> {
    rank(discover_in(root), query)
}

/// Score + order a skill set for a query (shared by search / search_in).
fn rank(skills: Vec<Skill>, query: &str) -> Vec<(String, String)> {
    let terms: Vec<String> = query
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| s.len() > 1)
        .map(|s| s.to_string())
        .collect();
    let mut scored: Vec<(usize, String, String)> = Vec::new();
    for sk in skills {
        if sk.disable_model_invocation {
            continue;
        }
        let meta = format!("{} {}", sk.name, sk.description).to_lowercase();
        let body = std::fs::read_to_string(&sk.path).map(|t| t.to_lowercase()).unwrap_or_default();
        let mut score = 0usize;
        for t in &terms {
            if meta.contains(t) {
                score += 3; // name/description hits weigh more than body hits
            } else if body.contains(t) {
                score += 1;
            }
        }
        scored.push((score, sk.name, sk.description));
    }
    if scored.iter().any(|(s, _, _)| *s > 0) {
        scored.retain(|(s, _, _)| *s > 0);
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored.truncate(20);
    scored.into_iter().map(|(_, n, d)| (n, d)).collect()
}

/// Load a skill's instructions (body, frontmatter stripped) plus a listing of its
/// bundled files (level 2+). Used by the `skill` tool.
pub fn load(name: &str) -> Option<(Skill, String, Vec<String>)> {
    load_skill(discover().into_iter().find(|s| s.name == name)?)
}

/// Like [`load`] but against an explicit root (used in tests).
pub fn load_in(root: &Path, name: &str) -> Option<(Skill, String, Vec<String>)> {
    load_skill(discover_in(root).into_iter().find(|s| s.name == name)?)
}

fn load_skill(sk: Skill) -> Option<(Skill, String, Vec<String>)> {
    let raw = std::fs::read_to_string(&sk.path).ok()?;
    let body = strip_frontmatter(&raw);
    let files = bundled_files(&sk.dir);
    Some((sk, body, files))
}

fn strip_frontmatter(text: &str) -> String {
    if let Some(rest) = text.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            return rest[end + 4..].trim_start().to_string();
        }
    }
    text.trim_start().to_string()
}

/// Every file in the skill folder except SKILL.md, relative to the folder — so the
/// agent knows what references/scripts it can read or run.
fn bundled_files(dir: &Path) -> Vec<String> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                stack.push(p);
            } else if p.file_name().map(|n| n != "SKILL.md").unwrap_or(true) {
                // Absolute paths, so the agent can read_file / bash them directly.
                files.push(p.display().to_string());
            }
        }
    }
    files.sort();
    files
}
