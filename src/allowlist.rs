//! The trust model: a TOML allowlist of peer NodeIds, gating both inbound
//! and outbound connections. The file is re-read per connection, so edits
//! take effect immediately without a daemon restart.

use std::path::Path;

use anyhow::{Context, Result, bail};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerEntry {
    pub node_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub human: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AllowListFile {
    #[serde(default)]
    pub peer: Vec<PeerEntry>,
}

#[derive(Debug, Clone)]
pub struct AllowList {
    pub path: std::path::PathBuf,
    pub entries: Vec<PeerEntry>,
}

impl AllowList {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self {
                path: path.to_path_buf(),
                entries: Vec::new(),
            });
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        #[cfg(unix)]
        {
            // Tighten legacy installs: the trust list is security-critical.
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(path) {
                let mut perms = meta.permissions();
                if perms.mode() & 0o077 != 0 {
                    perms.set_mode(0o600);
                    let _ = std::fs::set_permissions(path, perms);
                }
            }
        }
        let file: AllowListFile = toml::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            entries: file.peer,
        })
    }

    /// Load, or create an empty file if missing (used by `init`).
    pub fn load_or_create(path: &Path) -> Result<Self> {
        let list = Self::load(path)?;
        if !path.exists() {
            list.save()?;
        }
        Ok(list)
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let file = AllowListFile {
            peer: self.entries.clone(),
        };
        let text = toml::to_string_pretty(&file)?;
        // Write-then-rename so a crash mid-write cannot leave a torn file:
        // the daemon fails closed on a corrupt allowlist (rejects everyone).
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, &text).with_context(|| format!("writing {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&tmp)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&tmp, perms)?;
        }
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("writing {}", self.path.display()))
    }

    pub fn contains(&self, id: &EndpointId) -> bool {
        let s = id.to_string();
        self.entries.iter().any(|e| e.node_id == s)
    }

    /// Find a peer by exact NodeId or by name.
    pub fn resolve(&self, query: &str) -> Option<&PeerEntry> {
        self.entries
            .iter()
            .find(|e| e.node_id == query)
            .or_else(|| self.entries.iter().find(|e| e.name.as_deref() == Some(query)))
    }

    pub fn add(&mut self, entry: PeerEntry) -> Result<()> {
        if entry.node_id.parse::<EndpointId>().is_err() {
            bail!("`{}` is not a valid NodeId", entry.node_id);
        }
        if self.entries.iter().any(|e| e.node_id == entry.node_id) {
            bail!("`{}` is already in the allowlist", entry.node_id);
        }
        self.entries.push(entry);
        self.save()
    }

    pub fn remove(&mut self, query: &str) -> Result<PeerEntry> {
        let idx = self
            .entries
            .iter()
            .position(|e| e.node_id == query || e.name.as_deref() == Some(query))
            .with_context(|| format!("`{query}` is not in the allowlist"))?;
        let entry = self.entries.remove(idx);
        self.save()?;
        Ok(entry)
    }
}
