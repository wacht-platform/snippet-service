//! Durable agent memory as plain files the agent reads with the shell and edits
//! with `change_files`. The daemon does the bookkeeping: it assigns ids to new
//! bullets, counts note reads, generates the table of contents, and applies the
//! reflection pass's helpful/harmful marks and deltas.
//!
//! ```text
//! <ws>/.snippet/memory/
//!   rules.md                      always-obeyed bullets:  - [r1] text
//!   learnings.md                  reusable lessons:       - [l1] (+2/-0) text
//!   notes/<section>/<id>.md       one topic per note, title + summary header
//!   notes/<section>/_section.md   one-line summary of the section
//! ~/.snippet/memory/
//!   rules.md, learnings.md        the same, for every project (ids gr1, gl1)
//! ```

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

const RULES_FILE: &str = "rules.md";
const LEARNINGS_FILE: &str = "learnings.md";
const NOTES_DIR: &str = "notes";
const SECTION_FILE: &str = "_section.md";

const MAX_ID_LEN: usize = 64;
const MAX_TITLE_CHARS: usize = 80;
const MAX_SUMMARY_CHARS: usize = 200;
const MAX_BODY_CHARS: usize = 12_000;
const MAX_BULLET_CHARS: usize = 300;
const MAX_SECTION_DEPTH: usize = 3;
const RULES_PROMPT_CHARS: usize = 2_000;
const LEARNINGS_PROMPT_CHARS: usize = 4_000;
const TOC_PROMPT_CHARS: usize = 6_000;
const SIMILAR_AT: f64 = 0.6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Rule,
    Learning,
}

impl Kind {
    fn prefix(self) -> char {
        match self {
            Kind::Rule => 'r',
            Kind::Learning => 'l',
        }
    }

    fn file(self) -> &'static str {
        match self {
            Kind::Rule => RULES_FILE,
            Kind::Learning => LEARNINGS_FILE,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Kind::Rule => "Rules",
            Kind::Learning => "Learnings",
        }
    }

    fn noun(self) -> &'static str {
        match self {
            Kind::Rule => "rule",
            Kind::Learning => "learning",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Bullet {
    pub id: String,
    pub section: String,
    pub helpful: u32,
    pub harmful: u32,
    pub text: String,
}

impl Bullet {
    fn counters(&self) -> String {
        if self.helpful > 0 || self.harmful > 0 {
            format!("(+{}/-{}) ", self.helpful, self.harmful)
        } else {
            String::new()
        }
    }

    fn line(&self) -> String {
        let section = if self.section.is_empty() {
            String::new()
        } else {
            format!("{}: ", self.section)
        };
        format!("- [{}] {}{section}{}", self.id, self.counters(), self.text)
    }

    fn score(&self) -> i64 {
        self.helpful as i64 - self.harmful as i64
    }
}

#[derive(Debug, Clone)]
pub struct Note {
    pub id: String,
    pub section: String,
    pub path: PathBuf,
    pub title: String,
    pub summary: String,
    pub updated: String,
    pub read: u32,
    pub helpful: u32,
    pub harmful: u32,
    pub body: String,
}

impl Note {
    fn counters(&self) -> String {
        let mut parts = Vec::new();
        if self.helpful > 0 || self.harmful > 0 {
            parts.push(format!("+{}/-{}", self.helpful, self.harmful));
        }
        if self.read > 0 {
            parts.push(format!("read {}×", self.read));
        }
        if self.harmful > self.helpful {
            parts.push("disputed".to_string());
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!(" ({})", parts.join(" · "))
        }
    }

    fn render_file(&self) -> String {
        let mut header = format!("---\ntitle: {}\nsummary: {}\n", self.title, self.summary);
        for (key, value) in [("read", self.read), ("helpful", self.helpful), ("harmful", self.harmful)] {
            if value > 0 {
                header.push_str(&format!("{key}: {value}\n"));
            }
        }
        format!("{header}---\n\n{}\n", self.body.trim_end())
    }
}

#[derive(Debug, Default, Clone)]
pub struct NoteEdit {
    pub title: Option<String>,
    pub summary: Option<String>,
    pub body: Option<String>,
    pub section: Option<String>,
}

enum Target {
    Bullet { global: bool, kind: Kind },
    Note,
}

pub struct Memory {
    workspace: PathBuf,
    global: PathBuf,
}

impl Memory {
    pub fn open(workspace_root: &Path) -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            workspace: workspace_root.join(".snippet").join("memory"),
            global: home.join(".snippet").join("memory"),
        }
    }

    fn store(&self, global: bool) -> &Path {
        if global {
            &self.global
        } else {
            &self.workspace
        }
    }

    fn notes_dir(&self) -> PathBuf {
        self.workspace.join(NOTES_DIR)
    }

    /// Give every hand-written bullet an id, writing a file back only when
    /// something changed.
    pub fn normalize(&self) {
        for global in [false, true] {
            for kind in [Kind::Rule, Kind::Learning] {
                let Ok(text) = fs::read_to_string(self.store(global).join(kind.file())) else {
                    continue;
                };
                let (bullets, assigned) = load_bullets(&text, global, kind);
                if assigned {
                    let _ = self.save_bullets(global, kind, &bullets);
                }
            }
        }
    }

    pub fn bullets(&self, global: bool, kind: Kind) -> Vec<Bullet> {
        let text = fs::read_to_string(self.store(global).join(kind.file())).unwrap_or_default();
        load_bullets(&text, global, kind).0
    }

    fn save_bullets(&self, global: bool, kind: Kind, bullets: &[Bullet]) -> Result<(), String> {
        let path = self.store(global).join(kind.file());
        if bullets.is_empty() {
            let _ = fs::remove_file(&path);
            return Ok(());
        }
        let mut out = format!("# {}\n", kind.title());
        let mut sections: Vec<&str> = Vec::new();
        for bullet in bullets {
            if !sections.contains(&bullet.section.as_str()) {
                sections.push(&bullet.section);
            }
        }
        for section in sections {
            out.push('\n');
            if !section.is_empty() {
                out.push_str(&format!("## {section}\n"));
            }
            for bullet in bullets.iter().filter(|b| b.section == section) {
                out.push_str(&format!("- [{}] {}{}\n", bullet.id, bullet.counters(), bullet.text));
            }
        }
        write_atomic(&path, &out)
    }

    pub fn notes(&self) -> Vec<Note> {
        let mut notes = Vec::new();
        collect_notes(&self.notes_dir(), "", &mut notes);
        notes.sort_by(|a, b| (&a.section, &a.id).cmp(&(&b.section, &b.id)));
        notes
    }

    fn find_note(&self, id: &str) -> Result<Note, String> {
        self.notes()
            .into_iter()
            .find(|n| n.id == id)
            .ok_or_else(|| format!("no memory note `{id}`"))
    }

    fn section_dir(&self, section: &str) -> PathBuf {
        let mut path = self.notes_dir();
        for part in section.split('/').filter(|p| !p.is_empty()) {
            path.push(part);
        }
        path
    }

    fn section_summary(&self, section: &str) -> String {
        fs::read_to_string(self.section_dir(section).join(SECTION_FILE))
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .to_string()
    }

    fn save_note(&self, note: &Note) -> Result<(), String> {
        write_atomic(&note.path, &note.render_file())
    }

    fn save_counters(&self, note: &Note) -> Result<(), String> {
        let modified = fs::metadata(&note.path).and_then(|m| m.modified()).ok();
        self.save_note(note)?;
        if let Some(time) = modified
            && let Ok(file) = fs::File::options().write(true).open(&note.path)
        {
            let _ = file.set_modified(time);
        }
        Ok(())
    }

    /// The notes among `paths`, by id.
    pub fn note_ids(&self, paths: &[PathBuf]) -> Vec<String> {
        let notes = self.notes();
        let mut ids = Vec::new();
        for path in paths {
            if let Some(note) = notes.iter().find(|n| &n.path == path)
                && !ids.contains(&note.id)
            {
                ids.push(note.id.clone());
            }
        }
        ids
    }

    /// Count one read of each note among `paths`.
    pub fn record_reads(&self, paths: &[PathBuf]) {
        let notes_dir = self.notes_dir();
        if !paths.iter().any(|p| p.starts_with(&notes_dir)) {
            return;
        }
        for mut note in self.notes().into_iter().filter(|n| paths.contains(&n.path)) {
            note.read += 1;
            let _ = self.save_counters(&note);
        }
    }

    pub fn add_note(
        &self,
        section: &str,
        id: &str,
        title: &str,
        summary: &str,
        body: &str,
    ) -> Result<String, String> {
        let section = clean_section(section)?;
        let id = clean_note_id(id)?;
        let title = one_line(title, "title", MAX_TITLE_CHARS)?;
        let summary = one_line(summary, "summary", MAX_SUMMARY_CHARS)?;
        let body = clean_body(body)?;
        let notes = self.notes();
        if let Some(existing) = notes.iter().find(|n| n.id == id) {
            return Err(format!(
                "note `{id}` already exists in {}/ — update it instead",
                existing.section
            ));
        }
        let words = tokens(&format!("{title} {summary}"));
        if let Some((note, overlap)) = notes
            .iter()
            .map(|n| (n, similarity(&words, &tokens(&format!("{} {}", n.title, n.summary)))))
            .filter(|(_, s)| *s >= SIMILAR_AT)
            .max_by(|a, b| a.1.total_cmp(&b.1))
        {
            return Err(format!(
                "too similar to note `{}` \"{}\" ({:.0}% overlap) — update that note instead",
                note.id,
                note.title,
                overlap * 100.0
            ));
        }
        let note = Note {
            path: self.section_dir(&section).join(format!("{id}.md")),
            id: id.clone(),
            section: section.clone(),
            title,
            summary,
            updated: String::new(),
            read: 0,
            helpful: 0,
            harmful: 0,
            body,
        };
        self.save_note(&note)?;
        Ok(format!("added note `{id}` in {section}/"))
    }

    pub fn update_note(&self, id: &str, edit: NoteEdit) -> Result<String, String> {
        let mut note = self.find_note(id)?;
        let old_path = note.path.clone();
        let mut changed = Vec::new();
        if let Some(title) = edit.title {
            note.title = one_line(&title, "title", MAX_TITLE_CHARS)?;
            changed.push("title");
        }
        if let Some(summary) = edit.summary {
            note.summary = one_line(&summary, "summary", MAX_SUMMARY_CHARS)?;
            changed.push("summary");
        }
        if let Some(body) = edit.body {
            note.body = clean_body(&body)?;
            changed.push("body");
        }
        if let Some(section) = edit.section {
            note.section = clean_section(&section)?;
            note.path = self.section_dir(&note.section).join(format!("{}.md", note.id));
            changed.push("section");
        }
        if changed.is_empty() {
            return Err("nothing to change — give title, summary, body or section".to_string());
        }
        self.save_note(&note)?;
        if note.path != old_path {
            let _ = fs::remove_file(&old_path);
            prune_empty_dirs(old_path.parent(), &self.notes_dir());
        }
        Ok(format!("updated note `{id}` ({})", changed.join(", ")))
    }

    pub fn set_section(&self, section: &str, summary: &str) -> Result<String, String> {
        let section = clean_section(section)?;
        let summary = one_line(summary, "summary", MAX_SUMMARY_CHARS)?;
        write_atomic(&self.section_dir(&section).join(SECTION_FILE), &format!("{summary}\n"))?;
        Ok(format!("section {section}/ summary set"))
    }

    pub fn add_bullet(
        &self,
        kind: Kind,
        global: bool,
        section: &str,
        text: &str,
    ) -> Result<String, String> {
        let text = one_line(text, kind.noun(), MAX_BULLET_CHARS)?;
        let section = clean_label(section)?;
        let words = tokens(&text);
        if let Some((bullet, overlap)) = self
            .bullets(false, kind)
            .into_iter()
            .chain(self.bullets(true, kind))
            .map(|b| {
                let s = similarity(&words, &tokens(&b.text));
                (b, s)
            })
            .filter(|(_, s)| *s >= SIMILAR_AT)
            .max_by(|a, b| a.1.total_cmp(&b.1))
        {
            return Err(format!(
                "too similar to [{}] \"{}\" ({:.0}% overlap) — update that {} instead",
                bullet.id,
                bullet.text,
                overlap * 100.0,
                kind.noun()
            ));
        }
        let mut bullets = self.bullets(global, kind);
        let id = format!("{}{}", bullet_prefix(global, kind), max_number(&bullets, global, kind) + 1);
        bullets.push(Bullet {
            id: id.clone(),
            section,
            helpful: 0,
            harmful: 0,
            text,
        });
        self.save_bullets(global, kind, &bullets)?;
        Ok(format!("added {} [{id}]", kind.noun()))
    }

    pub fn update_bullet(
        &self,
        id: &str,
        text: Option<&str>,
        section: Option<&str>,
    ) -> Result<String, String> {
        let Target::Bullet { global, kind } = classify(id) else {
            return Err(format!("`{id}` is not a rule or learning id"));
        };
        if text.is_none() && section.is_none() {
            return Err("nothing to change — give text or section".to_string());
        }
        let mut bullets = self.bullets(global, kind);
        let bullet = bullets
            .iter_mut()
            .find(|b| b.id == id)
            .ok_or_else(|| format!("no {} `{id}`", kind.noun()))?;
        if let Some(text) = text {
            bullet.text = one_line(text, kind.noun(), MAX_BULLET_CHARS)?;
        }
        if let Some(section) = section {
            bullet.section = clean_label(section)?;
        }
        self.save_bullets(global, kind, &bullets)?;
        Ok(format!("updated {} [{id}]", kind.noun()))
    }

    pub fn remove(&self, id: &str) -> Result<String, String> {
        match classify(id) {
            Target::Bullet { global, kind } => {
                let mut bullets = self.bullets(global, kind);
                let before = bullets.len();
                bullets.retain(|b| b.id != id);
                if bullets.len() == before {
                    return Err(format!("no {} `{id}`", kind.noun()));
                }
                self.save_bullets(global, kind, &bullets)?;
                Ok(format!("removed {} [{id}]", kind.noun()))
            }
            Target::Note => {
                let note = self.find_note(id)?;
                fs::remove_file(&note.path).map_err(|e| e.to_string())?;
                prune_empty_dirs(note.path.parent(), &self.notes_dir());
                Ok(format!("removed note `{id}`"))
            }
        }
    }

    pub fn mark(&self, id: &str, helpful: bool) -> Result<String, String> {
        let verdict = if helpful { "helpful" } else { "harmful" };
        match classify(id) {
            Target::Bullet { global, kind } => {
                let mut bullets = self.bullets(global, kind);
                let bullet = bullets
                    .iter_mut()
                    .find(|b| b.id == id)
                    .ok_or_else(|| format!("no {} `{id}`", kind.noun()))?;
                if helpful {
                    bullet.helpful += 1;
                } else {
                    bullet.harmful += 1;
                }
                let (h, x) = (bullet.helpful, bullet.harmful);
                if kind == Kind::Learning && x >= 2 && x > h {
                    bullets.retain(|b| b.id != id);
                    self.save_bullets(global, kind, &bullets)?;
                    return Ok(format!(
                        "marked [{id}] {verdict}; it has hurt more than helped (+{h}/-{x}), so it was removed"
                    ));
                }
                self.save_bullets(global, kind, &bullets)?;
                Ok(format!("marked [{id}] {verdict} (+{h}/-{x})"))
            }
            Target::Note => {
                let mut note = self.find_note(id)?;
                if helpful {
                    note.helpful += 1;
                } else {
                    note.harmful += 1;
                }
                self.save_counters(&note)?;
                Ok(format!("marked `{id}` {verdict} (+{}/-{})", note.helpful, note.harmful))
            }
        }
    }

    fn rules_block(&self, budget: Option<usize>) -> String {
        let rules: Vec<Bullet> = self
            .bullets(true, Kind::Rule)
            .into_iter()
            .chain(self.bullets(false, Kind::Rule))
            .collect();
        budgeted_lines(&rules, budget, "rules")
    }

    fn learnings_block(&self, budget: Option<usize>) -> String {
        let mut learnings: Vec<Bullet> = self
            .bullets(true, Kind::Learning)
            .into_iter()
            .chain(self.bullets(false, Kind::Learning))
            .collect();
        learnings.sort_by_key(|b| std::cmp::Reverse(b.score()));
        budgeted_lines(&learnings, budget, "learnings")
    }

    fn toc(&self, budget: usize) -> String {
        let notes = self.notes();
        if notes.is_empty() {
            return "(no notes yet)\n".to_string();
        }
        let mut sections: Vec<&str> = notes.iter().map(|n| n.section.as_str()).collect();
        sections.dedup();
        let render = |detail: u8| {
            let mut out = String::new();
            for section in &sections {
                let in_section: Vec<&Note> = notes.iter().filter(|n| n.section == *section).collect();
                let name = if section.is_empty() {
                    "notes/".to_string()
                } else {
                    format!("{section}/")
                };
                let summary = self.section_summary(section);
                let head = if summary.is_empty() {
                    name
                } else {
                    format!("{name} — {summary}")
                };
                if detail == 0 {
                    out.push_str(&format!("{head} ({} notes)\n", in_section.len()));
                    continue;
                }
                out.push_str(&head);
                out.push('\n');
                for note in in_section {
                    let summary = if detail == 2 && !note.summary.is_empty() {
                        format!(": {}", note.summary)
                    } else {
                        String::new()
                    };
                    out.push_str(&format!("  {} — {}{summary}{}\n", note.id, note.title, note.counters()));
                }
            }
            out
        };
        for detail in [2, 1] {
            let out = render(detail);
            if out.chars().count() <= budget {
                return out;
            }
        }
        format!("{}(too many notes to list — `ls` a section folder to see its notes)\n", render(0))
    }

    /// Everything, unabridged and with note dates: the reflection pass's view.
    pub fn full_listing(&self) -> String {
        self.normalize();
        let mut notes = String::new();
        for note in self.notes() {
            notes.push_str(&format!(
                "{} [{}/] — {}: {}{} (updated {})\n",
                note.id, note.section, note.title, note.summary, note.counters(), note.updated
            ));
        }
        format!(
            "RULES\n{}\nLEARNINGS\n{}\nNOTES\n{}",
            or_none(self.rules_block(None)),
            or_none(self.learnings_block(None)),
            or_none(notes)
        )
    }

    pub fn render_prompt(&self) -> String {
        self.normalize();
        format!(
            "[memory]\nthis project: {ws}\nevery project: {global}\n\nRULES — always obey:\n{}\nLEARNINGS — apply the ones that fit:\n{}\nNOTES — table of contents of {ws}/notes (id — title: summary):\n{}",
            or_none(self.rules_block(Some(RULES_PROMPT_CHARS))),
            or_none(self.learnings_block(Some(LEARNINGS_PROMPT_CHARS))),
            self.toc(TOC_PROMPT_CHARS),
            ws = self.workspace.display(),
            global = self.global.display(),
        )
        .trim_end()
        .to_string()
    }
}

fn load_bullets(text: &str, global: bool, kind: Kind) -> (Vec<Bullet>, bool) {
    let mut bullets = parse_bullets(text);
    let prefix = bullet_prefix(global, kind);
    let mut next = max_number(&bullets, global, kind);
    let mut seen = HashSet::new();
    let mut assigned = false;
    for bullet in bullets.iter_mut() {
        let owned = bullet
            .id
            .strip_prefix(&prefix)
            .is_some_and(|n| n.parse::<u32>().is_ok());
        if !owned || !seen.insert(bullet.id.clone()) {
            next += 1;
            bullet.id = format!("{prefix}{next}");
            seen.insert(bullet.id.clone());
            assigned = true;
        }
    }
    (bullets, assigned)
}

fn max_number(bullets: &[Bullet], global: bool, kind: Kind) -> u32 {
    let prefix = bullet_prefix(global, kind);
    bullets
        .iter()
        .filter_map(|b| b.id.strip_prefix(&prefix).and_then(|n| n.parse::<u32>().ok()))
        .max()
        .unwrap_or(0)
}

fn budgeted_lines(bullets: &[Bullet], budget: Option<usize>, what: &str) -> String {
    let mut out = String::new();
    let mut shown = 0;
    for bullet in bullets {
        let line = bullet.line();
        if budget.is_some_and(|b| out.chars().count() + line.chars().count() > b) {
            break;
        }
        out.push_str(&line);
        out.push('\n');
        shown += 1;
    }
    if shown < bullets.len() {
        out.push_str(&format!(
            "… {} more {what} not shown: over the length budget, merge or trim them\n",
            bullets.len() - shown
        ));
    }
    out
}

fn or_none(block: String) -> String {
    if block.trim().is_empty() {
        "(none yet)\n".to_string()
    } else {
        block
    }
}

fn bullet_prefix(global: bool, kind: Kind) -> String {
    format!("{}{}", if global { "g" } else { "" }, kind.prefix())
}

fn classify(id: &str) -> Target {
    let id = id.trim();
    let (global, rest) = match id.strip_prefix('g') {
        Some(rest) if is_bullet_id(rest) => (true, rest),
        _ => (false, id),
    };
    if is_bullet_id(rest) {
        let kind = if rest.starts_with('r') { Kind::Rule } else { Kind::Learning };
        Target::Bullet { global, kind }
    } else {
        Target::Note
    }
}

fn is_bullet_id(id: &str) -> bool {
    let mut chars = id.chars();
    matches!(chars.next(), Some('r' | 'l'))
        && !chars.as_str().is_empty()
        && chars.all(|c| c.is_ascii_digit())
}

fn clean_note_id(id: &str) -> Result<String, String> {
    let id = id.trim();
    if id.is_empty() || id.len() > MAX_ID_LEN {
        return Err(format!("note id must be 1-{MAX_ID_LEN} chars"));
    }
    if !id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
        return Err(format!("note id `{id}` must be kebab-case: lowercase letters, digits and '-'"));
    }
    if matches!(classify(id), Target::Bullet { .. }) {
        return Err(format!("`{id}` looks like a rule/learning id; pick a descriptive note id"));
    }
    Ok(id.to_string())
}

fn clean_section(section: &str) -> Result<String, String> {
    let section = section.trim().trim_matches('/');
    if section.is_empty() {
        return Err("a note needs a section, e.g. `build` or `architecture/harness`".to_string());
    }
    let parts: Vec<&str> = section.split('/').collect();
    if parts.len() > MAX_SECTION_DEPTH {
        return Err(format!("sections nest at most {MAX_SECTION_DEPTH} levels deep"));
    }
    for part in &parts {
        if part.is_empty()
            || part.starts_with('_')
            || !part.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(format!(
                "section `{section}` must be kebab-case names joined by '/', e.g. `build/ci`"
            ));
        }
    }
    Ok(parts.join("/"))
}

fn clean_label(label: &str) -> Result<String, String> {
    let label = label.trim();
    if label.contains('\n') || label.chars().count() > 40 {
        return Err("a bullet section is a short one-line label (at most 40 chars)".to_string());
    }
    Ok(label.to_string())
}

fn one_line(text: &str, what: &str, max: usize) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err(format!("{what} is empty"));
    }
    if text.contains('\n') {
        return Err(format!("{what} must be one line"));
    }
    let n = text.chars().count();
    if n > max {
        return Err(format!("{what} is {n} chars; keep it under {max}"));
    }
    Ok(text.to_string())
}

fn clean_body(body: &str) -> Result<String, String> {
    let body = body.trim();
    if body.is_empty() {
        return Err("note body is empty".to_string());
    }
    let n = body.chars().count();
    if n > MAX_BODY_CHARS {
        return Err(format!(
            "note body is {n} chars, over the {MAX_BODY_CHARS}-char limit — split it into focused notes"
        ));
    }
    Ok(body.to_string())
}

fn parse_bullets(text: &str) -> Vec<Bullet> {
    let mut section = String::new();
    let mut bullets = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(heading) = line.strip_prefix("## ") {
            section = heading.trim().to_string();
            continue;
        }
        let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) else {
            continue;
        };
        let (id, rest) = match rest.strip_prefix('[').and_then(|r| r.split_once(']')) {
            Some((id, rest)) if matches!(classify(id), Target::Bullet { .. }) => {
                (id.to_string(), rest.trim_start())
            }
            _ => (String::new(), rest),
        };
        let (helpful, harmful, text) = parse_counters(rest);
        if !text.is_empty() {
            bullets.push(Bullet {
                id,
                section: section.clone(),
                helpful,
                harmful,
                text,
            });
        }
    }
    bullets
}

fn parse_counters(rest: &str) -> (u32, u32, String) {
    if let Some(inner) = rest.strip_prefix("(+")
        && let Some((counts, text)) = inner.split_once(')')
        && let Some((h, x)) = counts.split_once("/-")
        && let (Ok(h), Ok(x)) = (h.parse(), x.parse())
    {
        return (h, x, text.trim().to_string());
    }
    (0, 0, rest.trim().to_string())
}

fn collect_notes(dir: &Path, section: &str, out: &mut Vec<Note>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name.starts_with('_') {
            continue;
        }
        if path.is_dir() {
            let child = if section.is_empty() {
                name
            } else {
                format!("{section}/{name}")
            };
            collect_notes(&path, &child, out);
        } else if let Some(id) = name.strip_suffix(".md")
            && let Ok(text) = fs::read_to_string(&path)
        {
            let updated = entry
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| chrono::DateTime::<chrono::Local>::from(t).format("%Y-%m-%d").to_string())
                .unwrap_or_default();
            out.push(parse_note(id, section, path, updated, &text));
        }
    }
}

fn parse_note(id: &str, section: &str, path: PathBuf, updated: String, text: &str) -> Note {
    let mut note = Note {
        id: id.to_string(),
        section: section.to_string(),
        path,
        title: String::new(),
        summary: String::new(),
        updated,
        read: 0,
        helpful: 0,
        harmful: 0,
        body: text.trim().to_string(),
    };
    if let Some(rest) = text.trim_start().strip_prefix("---\n")
        && let Some((header, body)) = rest.split_once("\n---")
    {
        for line in header.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim().to_string();
            match key.trim() {
                "title" => note.title = value,
                "summary" => note.summary = value,
                "read" => note.read = value.parse().unwrap_or(0),
                "helpful" => note.helpful = value.parse().unwrap_or(0),
                "harmful" => note.harmful = value.parse().unwrap_or(0),
                _ => {}
            }
        }
        note.body = body.trim().to_string();
    }
    if note.title.is_empty() {
        note.title = note
            .body
            .lines()
            .map(|l| l.trim_start_matches('#').trim())
            .find(|l| !l.is_empty())
            .unwrap_or(id)
            .chars()
            .take(MAX_TITLE_CHARS)
            .collect();
    }
    note
}

fn prune_empty_dirs(mut dir: Option<&Path>, stop: &Path) {
    while let Some(d) = dir {
        if d == stop || !d.starts_with(stop) {
            return;
        }
        let has_notes = fs::read_dir(d)
            .map(|rd| {
                rd.flatten().any(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    e.path().is_dir() || (name.ends_with(".md") && !name.starts_with('_'))
                })
            })
            .unwrap_or(true);
        if has_notes {
            return;
        }
        let _ = fs::remove_file(d.join(SECTION_FILE));
        if fs::remove_dir(d).is_err() {
            return;
        }
        dir = d.parent();
    }
}

/// Markdown files a shell command names, resolved against `cwd`.
pub fn markdown_paths_in_command(command: &str, cwd: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let separators = |c: char| c.is_whitespace() || matches!(c, ';' | '|' | '&' | '<' | '>' | '(' | ')');
    for word in command.split(separators) {
        let word = word.trim_matches(|c| c == '\'' || c == '"');
        if !word.ends_with(".md") {
            continue;
        }
        let expanded = match word.strip_prefix("~/") {
            Some(rest) => std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(rest),
            None => PathBuf::from(word),
        };
        let path = normalize_path(&if expanded.is_absolute() { expanded } else { cwd.join(expanded) });
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "into", "when", "then", "than", "are",
    "was", "were", "has", "have", "use", "using", "not", "don", "you", "your", "its", "all", "any",
    "can", "should", "must", "always", "never",
];

fn tokens(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3 && !STOPWORDS.contains(w))
        .map(str::to_string)
        .collect()
}

fn similarity(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let shared = a.intersection(b).count() as f64;
    shared / a.union(b).count() as f64
}

fn write_atomic(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut tmp = path.to_path_buf();
    tmp.set_file_name(format!(
        ".{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("memory")
    ));
    fs::write(&tmp, content).map_err(|e| e.to_string())?;
    fs::rename(&tmp, path).map_err(|e| e.to_string())
}
