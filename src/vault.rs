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

    /// Env vars to inject for a shell command: only the secrets it actually
    /// references (`$NAME` / `${NAME}`), so an unrelated script doesn't get the
    /// whole vault in its environment.
    pub fn env_for_command(&self, command: &str) -> Vec<(String, String)> {
        self.secrets
            .iter()
            .filter(|(name, _)| {
                command.contains(&format!("${name}")) || command.contains(&format!("${{{name}}}"))
            })
            .map(|(n, v)| (n.clone(), v.clone()))
            .collect()
    }

    /// Replace every occurrence of every secret value with `[vault:NAME]`.
    pub fn scrub_str(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (name, value) in &self.secrets {
            if value.len() >= MIN_SECRET_LEN && out.contains(value.as_str()) {
                out = out.replace(value.as_str(), &format!("[vault:{name}]"));
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
