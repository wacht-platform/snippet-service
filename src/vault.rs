//! Secret vault — named secrets the agent can USE but never SEE.
//!
//! The design keeps every secret on the harness side of the line:
//! - Values live in `~/.snippet/vault.json` (mode 0600), managed via the
//!   `snippet vault` CLI and the serve API — never through the conversation.
//! - The model only ever learns NAMES (listed in the live context). It writes
//!   `$NAME` in a bash command; the referenced values are injected as env vars
//!   into the child process at spawn — the command string never contains them.
//! - Every tool result is scrubbed at the harness choke point before it enters
//!   the conversation: each secret value is replaced with `[vault:NAME]`, so
//!   even `echo $NAME` comes back redacted (as do accidental leaks in logs,
//!   files read back, or process output).
//!
//! Honest limit: scrubbing catches values verbatim. An actively adversarial
//! script can still exfiltrate via transformation (base64 etc.) — this guards
//! against accidental context leaks, not a malicious agent with shell access.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

/// True when the shell would expand vault secret `name`: `$NAME` or `${NAME}` /
/// `${NAME:…}`. A bare identifier or a substring is not a use.
fn references_shell_var(command: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let h = command.as_bytes();
    let n = name.as_bytes();
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut from = 0;
    while let Some(rel) = command[from..].find(name) {
        let s = from + rel;
        let e = s + n.len();
        if s >= 1 && h[s - 1] == b'$' {
            let left_ok = s == 1 || !ident(h[s - 2]);
            let right_ok = e == h.len() || !ident(h[e]);
            if left_ok && right_ok {
                return true;
            }
        } else if s >= 2 && h[s - 1] == b'{' && h[s - 2] == b'$' {
            // `${NAME}` or `${NAME:-default}` / `${NAME:1:2}`
            if e < h.len() && (h[e] == b'}' || h[e] == b':') {
                return true;
            }
        }
        from = s + 1;
    }
    false
}

pub fn vault_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".snippet")
        .join("vault.json")
}

/// Minimum secret length we scrub/inject. Anything shorter would shred normal
/// text with false-positive redactions (and isn't much of a secret).
const MIN_SECRET_LEN: usize = 4;

/// True when `path` is the vault file itself — off-limits to agent tools.
/// (Defense in depth: even if read some other way, the output scrubber redacts
/// the values, whole or fragmented.)
pub fn is_protected_path(path: &std::path::Path) -> bool {
    let vault = vault_path();
    let canon = |p: &std::path::Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    canon(path) == canon(&vault)
}

/// Seed length for partial-value detection: any verbatim fragment of a secret
/// at least this long is redacted. Shorter overlaps are indistinguishable from
/// ordinary text (a 5-char coincidence is noise, not a leak) — 8 keeps false
/// positives near zero for token-shaped secrets while making fragment
/// exfiltration useless.
const PARTIAL_SEED: usize = 8;

/// Redact every span of `text` that matches a run of `value` at least
/// PARTIAL_SEED bytes long. Seeds on 8-grams of the value, then extends each
/// hit maximally in both directions so a long fragment becomes ONE marker.
fn scrub_partials(text: &str, value: &str, marker: &str) -> String {
    use std::collections::HashMap;
    let tb = text.as_bytes();
    let vb = value.as_bytes();
    let n = tb.len();
    if n < PARTIAL_SEED {
        return text.to_string();
    }
    let mut seeds: HashMap<&[u8], Vec<usize>> = HashMap::new();
    for i in 0..=vb.len() - PARTIAL_SEED {
        seeds.entry(&vb[i..i + PARTIAL_SEED]).or_default().push(i);
    }
    let mut redact = vec![false; n];
    let mut any = false;
    let mut i = 0;
    while i + PARTIAL_SEED <= n {
        if let Some(offsets) = seeds.get(&tb[i..i + PARTIAL_SEED]) {
            // Extend the best alignment as far as the bytes keep matching.
            let mut best: Option<(usize, usize)> = None;
            for &off in offsets {
                let (mut s_t, mut s_v) = (i, off);
                while s_t > 0 && s_v > 0 && tb[s_t - 1] == vb[s_v - 1] {
                    s_t -= 1;
                    s_v -= 1;
                }
                let (mut e_t, mut e_v) = (i + PARTIAL_SEED, off + PARTIAL_SEED);
                while e_t < n && e_v < vb.len() && tb[e_t] == vb[e_v] {
                    e_t += 1;
                    e_v += 1;
                }
                if best.is_none_or(|(bs, be)| e_t - s_t > be - bs) {
                    best = Some((s_t, e_t));
                }
            }
            if let Some((s, e)) = best {
                redact[s..e].fill(true);
                any = true;
                i = e;
                continue;
            }
        }
        i += 1;
    }
    if !any {
        return text.to_string();
    }
    // Rebuild: each contiguous redacted run collapses to one marker.
    let mut out: Vec<u8> = Vec::with_capacity(n);
    let mut j = 0;
    while j < n {
        if redact[j] {
            out.extend_from_slice(marker.as_bytes());
            while j < n && redact[j] {
                j += 1;
            }
        } else {
            out.push(tb[j]);
            j += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug, Default, Clone)]
pub struct Vault {
    secrets: BTreeMap<String, String>,
    /// Snippet's own credentials (the daemon token, provider keys, sign-in
    /// tokens), read from its config files. Scrubbed from everything the model
    /// sees like vault values, but never injected into commands: a script that
    /// needs one reads it from its file at run time.
    harness: Vec<(String, String)>,
}

fn harness_files() -> Vec<PathBuf> {
    let home = crate::config::snippet_home();
    ["serve.json", "serve.json.bak", "serve.token", "config.toml", "xai_auth.json", "chatgpt_auth.json"]
        .iter()
        .map(|f| home.join(f))
        .collect()
}

fn sensitive_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    k == "token"
        || k.ends_with("api_key")
        || k.ends_with("_token")
        || k.ends_with("secret")
        || k.ends_with("password")
}

fn collect_json(value: &Value, path: &str, out: &mut Vec<(String, String)>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let p = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                match v {
                    Value::String(s) if sensitive_key(k) => out.push((p, s.clone())),
                    _ => collect_json(v, &p, out),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_json(item, path, out);
            }
        }
        _ => {}
    }
}

fn collect_toml(value: &toml::Value, path: &str, out: &mut Vec<(String, String)>) {
    match value {
        toml::Value::Table(map) => {
            for (k, v) in map {
                let p = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                match v {
                    toml::Value::String(s) if sensitive_key(k) => out.push((p, s.clone())),
                    _ => collect_toml(v, &p, out),
                }
            }
        }
        toml::Value::Array(items) => {
            for item in items {
                collect_toml(item, path, out);
            }
        }
        _ => {}
    }
}

/// The credentials in Snippet's own config files, named for the redaction
/// marker (`[secret:serve.json token]`). Values shorter than a real secret are
/// skipped so ordinary text isn't shredded.
fn harness_secrets(files: &[(PathBuf, Vec<u8>)]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (path, bytes) in files {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("config");
        let text = String::from_utf8_lossy(bytes);
        let mut found = Vec::new();
        if name == "serve.token" {
            found.push(("value".to_string(), text.trim().to_string()));
        } else if name.ends_with(".toml") {
            if let Ok(value) = text.parse::<toml::Value>() {
                collect_toml(&value, "", &mut found);
            }
        } else if let Ok(value) = serde_json::from_str::<Value>(&text) {
            collect_json(&value, "", &mut found);
        }
        for (key, value) in found {
            if value.len() >= PARTIAL_SEED && !out.iter().any(|(_, v): &(String, String)| *v == value) {
                out.push((format!("{name} {key}"), value));
            }
        }
    }
    out
}

impl Vault {
    /// Load the vault; missing or unreadable file → empty vault (never an error
    /// on the hot path).
    pub fn load() -> Self {
        // Keyed on the file's bytes, not its mtime: a secret changed to a
        // same-length value within one timestamp tick must never be served
        // stale, or scrubbing would miss the new value. The file is small, so
        // reading it is cheap; only the parse is skipped.
        static CACHE: std::sync::Mutex<Option<(Vec<u8>, Vault)>> = std::sync::Mutex::new(None);
        let bytes = std::fs::read(vault_path()).unwrap_or_default();
        let files: Vec<(PathBuf, Vec<u8>)> = harness_files()
            .into_iter()
            .map(|p| {
                let b = std::fs::read(&p).unwrap_or_default();
                (p, b)
            })
            .collect();
        let mut key = bytes.clone();
        for (_, b) in &files {
            key.extend_from_slice(&(b.len() as u64).to_le_bytes());
            key.extend_from_slice(b);
        }
        let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((cached, vault)) = cache.as_ref() {
            if *cached == key {
                return vault.clone();
            }
        }
        let secrets = serde_json::from_slice(&bytes).unwrap_or_default();
        let vault = Self { secrets, harness: harness_secrets(&files) };
        *cache = Some((key, vault.clone()));
        vault
    }

    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    pub fn names(&self) -> Vec<String> {
        self.secrets.keys().cloned().collect()
    }

    /// Valid secret name = valid env var name, uppercase convention.
    pub fn valid_name(name: &str) -> bool {
        !name.is_empty()
            && name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
            && name
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    }

    pub fn set(&mut self, name: &str, value: &str) -> Result<(), String> {
        if !Self::valid_name(name) {
            return Err(format!(
                "invalid name `{name}` — use an env-var style name (A-Z, 0-9, _; e.g. STRIPE_KEY)"
            ));
        }
        if value.trim().len() < MIN_SECRET_LEN {
            return Err(format!("value too short (min {MIN_SECRET_LEN} chars)"));
        }
        self.secrets
            .insert(name.to_string(), value.trim().to_string());
        self.save()
    }

    pub fn remove(&mut self, name: &str) -> Result<bool, String> {
        let existed = self.secrets.remove(name).is_some();
        if existed {
            self.save()?;
        }
        Ok(existed)
    }

    fn save(&self) -> Result<(), String> {
        let path = vault_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let body = serde_json::to_string_pretty(&self.secrets).map_err(|e| e.to_string())?;
        std::fs::write(&path, body).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// Env vars to inject for a shell command: only the secrets it references, so
    /// an unrelated script doesn't get the whole vault in its environment. A
    /// reference is the secret NAME appearing as an identifier token — this
    /// catches every access form, not just `$NAME`: `${NAME}`, Python
    /// `os.environ['NAME']`, Node `process.env.NAME`, a sourced `.env`, etc.
    pub fn env_for_command(&self, command: &str) -> Vec<(String, String)> {
        self.secrets
            .iter()
            .filter(|(name, _)| references_shell_var(command, name))
            .map(|(n, v)| (n.clone(), v.clone()))
            .collect()
    }

    /// Names of secrets a command would expand as a *shell* parameter — `$NAME`
    /// or `${NAME}` / `${NAME:…}`. Bare identifiers (`os.environ['NAME']`,
    /// comments, `echo NAME`) do not count: that was flooding approval prompts
    /// for ordinary commands that merely mentioned a similar word.
    pub fn referenced_names(&self, command: &str) -> Vec<String> {
        self.secrets
            .keys()
            .filter(|name| references_shell_var(command, name))
            .cloned()
            .collect()
    }

    /// Replace every occurrence of every secret value — whole OR partial — with
    /// `[vault:NAME]`. Partial matters: `cut -c1-12 vault.json`, `${KEY:0:10}`,
    /// `head -c`, hexdump-adjacent tricks all emit fragments, and leaking a
    /// secret 8 chars at a time is still leaking it.
    pub fn scrub_str(&self, text: &str) -> String {
        let mut entries: Vec<(String, String)> = Vec::new();
        let named = self
            .secrets
            .iter()
            .map(|(name, value)| (format!("[vault:{name}]"), value))
            .chain(self.harness.iter().map(|(name, value)| (format!("[secret:{name}]"), value)));
        for (marker, value) in named {
            if value.len() < MIN_SECRET_LEN {
                continue;
            }
            // The raw value AND its JSON-escaped on-disk form: `cat`-ing a JSON
            // file emits the escaped bytes, so both representations are caught.
            entries.push((marker.clone(), value.clone()));
            if let Ok(json) = serde_json::to_string(value) {
                let escaped = json.trim_matches('"').to_string();
                if escaped != *value && escaped.len() >= MIN_SECRET_LEN {
                    entries.push((marker, escaped));
                }
            }
        }
        // Whole values first, longest first, so one secret's fragments can't
        // chew into another that shares a run with it (JWTs share headers).
        entries.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
        let mut out = text.to_string();
        for (marker, form) in &entries {
            if out.contains(form.as_str()) {
                out = out.replace(form.as_str(), marker);
            }
        }
        for (marker, form) in &entries {
            if form.len() >= PARTIAL_SEED {
                out = scrub_partials(&out, form, marker);
            }
        }
        out
    }

    /// Recursively scrub every string inside a JSON value (tool results).
    pub fn scrub_value(&self, value: &mut Value) {
        if self.secrets.is_empty() && self.harness.is_empty() {
            return;
        }
        match value {
            Value::String(s) => {
                let scrubbed = self.scrub_str(s);
                if scrubbed != *s {
                    *s = scrubbed;
                }
            }
            Value::Array(items) => {
                for item in items {
                    self.scrub_value(item);
                }
            }
            Value::Object(map) => {
                for (_, v) in map.iter_mut() {
                    self.scrub_value(v);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_var_requires_dollar() {
        assert!(references_shell_var("echo $HELLO", "HELLO"));
        assert!(references_shell_var("echo ${HELLO}", "HELLO"));
        assert!(references_shell_var("echo ${HELLO:-x}", "HELLO"));
        assert!(!references_shell_var("echo 'hello'", "HELLO"));
        assert!(!references_shell_var("echo hello", "HELLO"));
        assert!(!references_shell_var("echo HELLO", "HELLO"));
        assert!(!references_shell_var("echo $HELLOT", "HELLO"));
        assert!(!references_shell_var("echo $XHELLO", "HELLO"));
    }
}
