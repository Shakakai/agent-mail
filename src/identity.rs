//! ed25519 identity: one secret key per agent, stored as hex at
//! `AGENT_MAIL_SECRET_KEY` (default `~/.config/agent-mail/secret-key`, 0600).
//! The IROH NodeId derived from this key *is* the agent's address.

use std::io::Write;

use anyhow::{Context, Result, bail};
use iroh::SecretKey;

use crate::config::Paths;

/// Load the secret key, or generate and persist it if missing.
pub fn load_or_generate(paths: &Paths) -> Result<SecretKey> {
    if paths.secret_key.exists() {
        return load(paths);
    }
    let secret_key = SecretKey::generate();
    if let Some(parent) = paths.secret_key.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&paths.secret_key)
            .with_context(|| format!("creating {}", paths.secret_key.display()))?;
        f.write_all(hex::encode(secret_key.to_bytes()).as_bytes())?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&paths.secret_key, hex::encode(secret_key.to_bytes()))
            .with_context(|| format!("creating {}", paths.secret_key.display()))?;
    }
    Ok(secret_key)
}

/// Load an existing secret key; fails with guidance if `init` was not run.
pub fn load(paths: &Paths) -> Result<SecretKey> {
    if !paths.secret_key.exists() {
        bail!(
            "no identity found at {} — run `agent-mail init` first",
            paths.secret_key.display()
        );
    }
    let text = std::fs::read_to_string(&paths.secret_key)
        .with_context(|| format!("reading {}", paths.secret_key.display()))?;
    let bytes: [u8; 32] = hex::decode(text.trim())
        .context("secret key is not valid hex")?
        .try_into()
        .map_err(|_| anyhow::anyhow!("secret key must be 32 bytes"))?;
    Ok(SecretKey::from_bytes(&bytes))
}

/// The agent's NodeId as a z-base-32 string (the shareable address).
pub fn node_id(secret_key: &SecretKey) -> String {
    secret_key.public().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;

    fn tmp_paths(name: &str) -> (Paths, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("am-id-test-{name}-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&dir).unwrap();
        (
            Paths {
                config_dir: dir.clone(),
                data_dir: dir.clone(),
                secret_key: dir.join("secret-key"),
                allowed_keys: dir.join("allowed-keys.toml"),
                db: dir.join("mail.db"),
            },
            dir,
        )
    }

    #[test]
    fn generate_then_load_round_trip() {
        let (paths, dir) = tmp_paths("roundtrip");
        let sk1 = load_or_generate(&paths).unwrap();
        let sk2 = load(&paths).unwrap();
        assert_eq!(node_id(&sk1), node_id(&sk2));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&paths.secret_key).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn load_without_init_errors_with_guidance() {
        let (paths, dir) = tmp_paths("missing");
        let err = load(&paths).unwrap_err().to_string();
        assert!(err.contains("init"), "error should mention init: {err}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn load_rejects_malformed_keys() {
        let (paths, dir) = tmp_paths("badkey");
        std::fs::write(&paths.secret_key, "zzzz").unwrap();
        assert!(load(&paths).is_err());
        std::fs::write(&paths.secret_key, "aa".repeat(10)).unwrap(); // 20 bytes, not 32
        assert!(load(&paths).is_err());
        std::fs::remove_dir_all(dir).ok();
    }
}
