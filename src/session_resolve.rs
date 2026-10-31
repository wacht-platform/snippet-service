//! Resolve the loose ways Mission Control names a session (an id in either
//! form, a bare conversation uuid, a workspace path, a title fragment, plus
//! `latest`) to one canonical id, or to the nearest candidates when it can't.

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Candidate {
    pub id: String,
    pub title: String,
    pub folder: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_folder: Option<String>,
    pub last_active: i64,
    pub status: String,
    pub settled: bool,
}

impl From<&crate::session::SessionInfo> for Candidate {
    fn from(s: &crate::session::SessionInfo) -> Self {
        Self {
            id: s.id.clone(),
            title: s.title.clone(),
            folder: s.folder.clone(),
            origin_folder: s.origin_folder.clone(),
            last_active: s.last_active,
            status: s.status.clone(),
            settled: s.settled,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    Found(String),
    Miss { reason: String, candidates: Vec<Candidate> },
}

const NEAREST: usize = 3;

fn trim_path(p: &str) -> String {
    let expanded = match p.strip_prefix("~/") {
        Some(rest) => format!("{}/{rest}", std::env::var("HOME").unwrap_or_default()),
        None => p.to_string(),
    };
    expanded.trim_end_matches('/').to_string()
}

fn in_folder(c: &Candidate, folder: &str) -> bool {
    trim_path(&c.folder) == folder || c.origin_folder.as_deref().is_some_and(|o| trim_path(o) == folder)
}

fn looks_like_uuid(s: &str) -> bool {
    s.len() == 36 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-') && s.matches('-').count() == 4
}

fn newest(mut found: Vec<&Candidate>) -> Option<&Candidate> {
    found.sort_by_key(|c| std::cmp::Reverse(c.last_active));
    found.into_iter().next()
}

fn top(mut found: Vec<&Candidate>) -> Vec<Candidate> {
    found.sort_by_key(|c| std::cmp::Reverse(c.last_active));
    found.into_iter().take(NEAREST).cloned().collect()
}

/// Pure, so the matching rules are testable without sessions on disk.
pub fn resolve(query: &str, sessions: &[Candidate]) -> Resolved {
    let words: Vec<&str> = query.split_whitespace().collect();
    let latest = words.iter().any(|w| w.eq_ignore_ascii_case("latest"));
    let rest = words
        .iter()
        .filter(|w| !w.eq_ignore_ascii_case("latest"))
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
    let raw = rest.trim().trim_start_matches("session:").to_string();
    let rest = raw.trim_matches('/').to_string();
    let awake: Vec<&Candidate> = sessions.iter().filter(|c| !c.settled).collect();
    let pool: Vec<&Candidate> = if awake.is_empty() { sessions.iter().collect() } else { awake };

    let matches: Vec<&Candidate> = if rest.is_empty() {
        if latest { pool.clone() } else { Vec::new() }
    } else if let Some(exact) = sessions.iter().find(|c| c.id == rest) {
        if latest {
            let folder = trim_path(&exact.folder);
            sessions.iter().filter(|c| in_folder(c, &folder)).collect()
        } else {
            return Resolved::Found(exact.id.clone());
        }
    } else {
        let uuid = rest.rsplit("conversations/").next().unwrap_or(&rest).trim_matches('/');
        if looks_like_uuid(uuid) {
            let suffix = format!("conversations/{uuid}");
            sessions.iter().filter(|c| c.id.ends_with(&suffix)).collect()
        } else if raw.starts_with('/') || raw.starts_with('~') {
            let folder = trim_path(&raw);
            pool.iter().copied().filter(|c| in_folder(c, &folder)).collect()
        } else {
            let needle = rest.to_lowercase();
            pool.iter()
                .copied()
                .filter(|c| c.title.to_lowercase().contains(&needle) || c.id.to_lowercase().starts_with(&needle))
                .collect()
        }
    };
    match matches.len() {
        1 => Resolved::Found(matches[0].id.clone()),
        0 => Resolved::Miss {
            reason: format!("no session matches `{}`", query.trim()),
            candidates: nearest(&rest, &pool),
        },
        _ if latest => Resolved::Found(newest(matches).map(|c| c.id.clone()).unwrap_or_default()),
        n => Resolved::Miss {
            reason: format!("`{}` matches {n} sessions; name one by id, or add `latest`", query.trim()),
            candidates: top(matches),
        },
    }
}

/// The sessions sharing the most words with the query, most recent first.
fn nearest(query: &str, pool: &[&Candidate]) -> Vec<Candidate> {
    let words: Vec<String> = query
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 1)
        .map(str::to_string)
        .collect();
    let mut scored: Vec<(usize, &Candidate)> = pool
        .iter()
        .map(|c| {
            let hay = format!("{} {} {}", c.title, c.folder, c.id).to_lowercase();
            (words.iter().filter(|w| hay.contains(w.as_str())).count(), *c)
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.last_active.cmp(&a.1.last_active)));
    scored.into_iter().take(NEAREST).map(|(_, c)| c.clone()).collect()
}

/// Sessions in `folder` that are awake and were active within `days`: the ones
/// a new session there would most likely duplicate.
pub fn recent_in_folder<'a>(folder: &str, sessions: &'a [Candidate], now: i64, days: i64) -> Vec<&'a Candidate> {
    let folder = trim_path(folder);
    let mut found: Vec<&Candidate> = sessions
        .iter()
        .filter(|c| !c.settled && in_folder(c, &folder) && now - c.last_active <= days * 86_400)
        .collect();
    found.sort_by_key(|c| std::cmp::Reverse(c.last_active));
    found
}

/// Every routable session, as candidates.
pub fn routable_candidates() -> Vec<Candidate> {
    crate::session::list_routable_sessions().iter().map(Candidate::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: &str, title: &str, folder: &str, last_active: i64) -> Candidate {
        Candidate {
            id: id.into(),
            title: title.into(),
            folder: folder.into(),
            origin_folder: None,
            last_active,
            status: "idle".into(),
            settled: false,
        }
    }

    fn board() -> Vec<Candidate> {
        vec![
            c("research-beb1", "Market research", "/code/research", 10),
            c("research-beb1/conversations/275420ba-17b3-427c-8a47-bda2be9b6933", "Pricing deep dive", "/code/research", 30),
            c("launch-video-4d7d", "Launch video edit", "/code/launch-video", 20),
        ]
    }

    #[test]
    fn exact_ids_in_either_form_resolve() {
        let s = board();
        assert_eq!(resolve("research-beb1", &s), Resolved::Found("research-beb1".into()));
        let conv = "research-beb1/conversations/275420ba-17b3-427c-8a47-bda2be9b6933";
        assert_eq!(resolve(&format!("session:{conv}"), &s), Resolved::Found(conv.into()));
        assert_eq!(resolve("/conversations/275420ba-17b3-427c-8a47-bda2be9b6933", &s), Resolved::Found(conv.into()));
        assert_eq!(resolve("275420ba-17b3-427c-8a47-bda2be9b6933", &s), Resolved::Found(conv.into()));
    }

    #[test]
    fn a_folder_id_with_latest_takes_the_newest_conversation_there() {
        let conv = "research-beb1/conversations/275420ba-17b3-427c-8a47-bda2be9b6933";
        assert_eq!(resolve("research-beb1 latest", &board()), Resolved::Found(conv.into()));
    }

    #[test]
    fn a_path_or_title_fragment_resolves_and_ambiguity_lists_candidates() {
        let s = board();
        assert_eq!(resolve("/code/launch-video/", &s), Resolved::Found("launch-video-4d7d".into()));
        assert_eq!(resolve("pricing", &s), Resolved::Found(s[1].id.clone()));
        match resolve("/code/research", &s) {
            Resolved::Miss { candidates, .. } => assert_eq!(candidates.len(), 2),
            other => panic!("expected ambiguity, got {other:?}"),
        }
        assert_eq!(resolve("latest /code/research", &s), Resolved::Found(s[1].id.clone()));
    }

    #[test]
    fn a_miss_returns_the_three_nearest() {
        let mut s = board();
        s.push(c("notes-1", "Notes", "/code/notes", 5));
        match resolve("research video", &s) {
            Resolved::Miss { candidates, reason } => {
                assert!(reason.contains("no session"));
                assert_eq!(candidates.len(), 3);
                assert!(candidates.iter().all(|c| c.id != "notes-1"));
            }
            other => panic!("expected a miss, got {other:?}"),
        }
    }

    #[test]
    fn settled_sessions_lose_to_awake_ones() {
        let mut s = board();
        s[1].settled = true;
        assert_eq!(resolve("/code/research", &s), Resolved::Found("research-beb1".into()));
    }

    #[test]
    fn recent_in_folder_skips_settled_and_old() {
        let now = 10 * 86_400;
        let mut s = board();
        s[0].last_active = now - 8 * 86_400;
        s[1].last_active = now - 86_400;
        assert_eq!(recent_in_folder("/code/research/", &s, now, 7).len(), 1);
        s[1].settled = true;
        assert!(recent_in_folder("/code/research", &s, now, 7).is_empty());
    }
}
