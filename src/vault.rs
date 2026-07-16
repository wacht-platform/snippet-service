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

/// True when `name` appears in `command` as a whole identifier token — bounded
/// by anything that isn't `[A-Za-z0-9_]` (or a string edge). So `DATABASE_URL`
/// matches inside `os.environ['DATABASE_URL']`, `$DATABASE_URL`, `${DATABASE_URL}`,
/// `process.env.DATABASE_URL`, or bare — but NOT inside `MY_DATABASE_URL_X`.
fn references_token(command: &str, name: &str) -> bool {
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
        let left = s == 0 || !ident(h[s - 1]);
        let right = e == h.len() || !ident(h[e]);
        if left && right {
            return true;
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
}

impl Vault {
    /// Load the vault; missing or unreadable file → empty vault (never an error
    /// on the hot path).
    pub fn load() -> Self {
        let secrets = std::fs::read_to_string(vault_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { secrets }
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
            && name.chars().next().is_some_and(|c| c.is_ascii_uppercase() || c == '_')
            && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
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
        self.secrets.insert(name.to_string(), value.trim().to_string());
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
            .filter(|(name, _)| references_token(command, name))
            .map(|(n, v)| (n.clone(), v.clone()))
            .collect()
    }

    /// Names of secrets a command references (any form). Drives the "using a
    /// secret always needs approval" gate — broader than injection needs, so a
    /// command that merely reads a secret via a language env API is still gated.
    pub fn referenced_names(&self, command: &str) -> Vec<String> {
        self.secrets
            .keys()
            .filter(|name| references_token(command, name))
            .cloned()
            .collect()
    }

    /// Replace every occurrence of every secret value — whole OR partial — with
    /// `[vault:NAME]`. Partial matters: `cut -c1-12 vault.json`, `${KEY:0:10}`,
    /// `head -c`, hexdump-adjacent tricks all emit fragments, and leaking a
    /// secret 8 chars at a time is still leaking it.
    pub fn scrub_str(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (name, value) in &self.secrets {
            if value.len() < MIN_SECRET_LEN {
                continue;
            }
            let marker = format!("[vault:{name}]");
            // Redact the raw value AND its JSON-escaped on-disk form: `cat`-ing
            // vault.json emits the escaped bytes (a value with " or \ differs from
            // the raw string), so we must catch both representations.
            let mut forms = vec![value.clone()];
            if let Ok(json) = serde_json::to_string(value) {
                // Strip the surrounding quotes serde adds → the escaped body.
                let escaped = json.trim_matches('"').to_string();
                if escaped != *value {
                    forms.push(escaped);
                }
            }
            for form in forms {
                if form.len() < MIN_SECRET_LEN {
                    continue;
                }
                if out.contains(form.as_str()) {
                    out = out.replace(form.as_str(), &marker);
                }
                if form.len() >= PARTIAL_SEED {
                    out = scrub_partials(&out, &form, &marker);
                }
            }
        }
        out
    }

    /// Recursively scrub every string inside a JSON value (tool results).
    pub fn scrub_value(&self, value: &mut Value) {
        if self.secrets.is_empty() {
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
