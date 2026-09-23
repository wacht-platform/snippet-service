//! Durable agent identity homes, independent from execution workspaces.
use std::path::{Path, PathBuf};

const MAX_IDENTITY_BYTES: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("invalid agent id: {0}")]
    InvalidAgentId(String),
    #[error("identity file is too large ({0} bytes; maximum is {MAX_IDENTITY_BYTES})")]
    TooLarge(usize),
    #[error("identity home: {0}")]
    Io(#[from] std::io::Error),
    #[error("identity metadata: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentHome {
    root: PathBuf,
    agent_id: String,
}

impl AgentHome {
    pub fn new(root: impl AsRef<Path>, agent_id: &str) -> Result<Self, IdentityError> {
        validate_agent_id(agent_id)?;
        Ok(Self {
            root: root.as_ref().join(agent_id),
            agent_id: agent_id.to_string(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub fn identity_path(&self) -> PathBuf {
        self.root.join("identity.md")
    }

    pub fn tools_path(&self) -> PathBuf {
        self.root.join("tools")
    }

    /// Create the home if absent: the tools directory and `identity.md`.
    ///
    /// Idempotent — an existing identity is never overwritten, so a researched
    /// identity survives every daemon boot.
    pub fn ensure_layout(&self, default_identity: &str) -> Result<(), IdentityError> {
        std::fs::create_dir_all(self.tools_path())?;
        if !self.identity_path().exists() {
            atomic_write(&self.identity_path(), default_identity.as_bytes())?;
        }
        Ok(())
    }

    pub fn read_identity(&self) -> Result<String, IdentityError> {
        let bytes = std::fs::read(self.identity_path())?;
        if bytes.len() > MAX_IDENTITY_BYTES {
            return Err(IdentityError::TooLarge(bytes.len()));
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

fn validate_agent_id(agent_id: &str) -> Result<(), IdentityError> {
    if agent_id.is_empty()
        || agent_id == "."
        || agent_id == ".."
        || agent_id.len() > 96
        || !agent_id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        return Err(IdentityError::InvalidAgentId(agent_id.to_string()));
    }
    Ok(())
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), std::io::Error> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_independent_agent_home_and_reads_identity() {
        let root = tempfile::tempdir().unwrap();
        let home = AgentHome::new(root.path(), "rust-reviewer").unwrap();
        home.ensure_layout("# Rust reviewer\n").unwrap();
        assert_eq!(home.read_identity().unwrap(), "# Rust reviewer\n");
        assert!(home.tools_path().is_dir());
    }

    #[test]
    fn rejects_path_traversal_and_unbounded_identity() {
        assert!(AgentHome::new("/tmp", "../escape").is_err());
        let root = tempfile::tempdir().unwrap();
        let home = AgentHome::new(root.path(), "worker").unwrap();
        home.ensure_layout("x").unwrap();
        std::fs::write(home.identity_path(), vec![b'x'; MAX_IDENTITY_BYTES + 1]).unwrap();
        assert!(matches!(
            home.read_identity(),
            Err(IdentityError::TooLarge(_))
        ));
    }
}
