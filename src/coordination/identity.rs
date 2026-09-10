//! Durable agent identity homes, independent from execution workspaces.
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IdentityMetadata {
    pub agent_id: String,
    pub revision: u64,
    pub updated_at: String,
    pub updated_by: String,
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

    pub fn metadata_path(&self) -> PathBuf {
        self.root.join("identity.json")
    }

    pub fn tools_path(&self) -> PathBuf {
        self.root.join("tools")
    }

    pub fn ensure_layout(
        &self,
        default_identity: &str,
        updated_by: &str,
    ) -> Result<(), IdentityError> {
        std::fs::create_dir_all(self.tools_path())?;
        self.ensure_identity_metadata(default_identity, updated_by)
    }

    fn ensure_identity_metadata(
        &self,
        default_identity: &str,
        updated_by: &str,
    ) -> Result<(), IdentityError> {
        if !self.identity_path().exists() {
            atomic_write(&self.identity_path(), default_identity.as_bytes())?;
        }
        if !self.metadata_path().exists() {
            let metadata = IdentityMetadata {
                agent_id: self.agent_id.clone(),
                revision: 1,
                updated_at: unix_seconds(),
                updated_by: updated_by.to_string(),
            };
            atomic_write(
                &self.metadata_path(),
                serde_json::to_vec_pretty(&metadata)?.as_slice(),
            )?;
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

    pub fn read_metadata(&self) -> Result<IdentityMetadata, IdentityError> {
        Ok(serde_json::from_slice(&std::fs::read(
            self.metadata_path(),
        )?)?)
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

fn unix_seconds() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_independent_agent_home_and_reads_identity() {
        let root = tempfile::tempdir().unwrap();
        let home = AgentHome::new(root.path(), "rust-reviewer").unwrap();
        home.ensure_layout("# Rust reviewer\n", "system").unwrap();
        assert_eq!(home.read_identity().unwrap(), "# Rust reviewer\n");
        assert_eq!(home.read_metadata().unwrap().revision, 1);
        assert!(home.tools_path().is_dir());
    }

    #[test]
    fn rejects_path_traversal_and_unbounded_identity() {
        assert!(AgentHome::new("/tmp", "../escape").is_err());
        let root = tempfile::tempdir().unwrap();
        let home = AgentHome::new(root.path(), "worker").unwrap();
        home.ensure_layout("x", "system").unwrap();
        std::fs::write(home.identity_path(), vec![b'x'; MAX_IDENTITY_BYTES + 1]).unwrap();
        assert!(matches!(
            home.read_identity(),
            Err(IdentityError::TooLarge(_))
        ));
    }
}
