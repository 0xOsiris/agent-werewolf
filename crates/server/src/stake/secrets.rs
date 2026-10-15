//! The per-game seed and X25519 secret, written to disk before `create` so a challenge can still be
//! proven after a restart. Nothing here is ever logged.

use std::path::{Path, PathBuf};

use rand::{
    TryRng,
    rngs::{SysError, SysRng},
};
use serde::Serialize;
use tokio::{fs, io::AsyncWriteExt};
use werewolf_verify::alloy_primitives::{Address, B256};

use super::StakeError;

/// Deliberately not `Debug`, so it can't end up in a log line.
pub struct Secrets {
    pub seed: B256,
    pub enc_secret: [u8; 32],
}

impl Secrets {
    pub fn generate() -> Result<Self, SysError> {
        let mut seed = B256::ZERO;
        let mut enc_secret = [0; 32];
        SysRng.try_fill_bytes(seed.as_mut_slice())?;
        SysRng.try_fill_bytes(&mut enc_secret)?;
        Ok(Self { seed, enc_secret })
    }
}

#[derive(Serialize)]
struct SecretFile {
    chain_id: u64,
    escrow: Address,
    seed: B256,
    enc_secret: B256,
}

/// Creates `dir`, readable only by the owner.
pub async fn ensure_dir(dir: &Path) -> Result<(), StakeError> {
    let io = |source| StakeError::Io {
        what: "creating",
        path: dir.to_owned(),
        source,
    };
    fs::create_dir_all(dir).await.map_err(io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .await
            .map_err(io)?;
    }
    Ok(())
}

/// Writes the secrets under a temporary name, since the game id is not known yet.
pub async fn write_pending(
    dir: &Path,
    chain_id: u64,
    escrow: Address,
    secrets: &Secrets,
) -> Result<PathBuf, StakeError> {
    let mut tag = [0; 8];
    SysRng.try_fill_bytes(&mut tag)?;
    let path = dir.join(format!("pending-{}.secret.json", hex::encode(tag)));
    let file = SecretFile {
        chain_id,
        escrow,
        seed: secrets.seed,
        enc_secret: B256::from(secrets.enc_secret),
    };
    write_private(&path, &serde_json::to_vec_pretty(&file)?).await?;
    Ok(path)
}

/// Gives the pending file its game's name.
pub async fn rename(from: &Path, to: &Path) -> Result<(), StakeError> {
    fs::rename(from, to).await.map_err(|source| StakeError::Io {
        what: "renaming",
        path: from.to_owned(),
        source,
    })
}

/// Writes `bytes` to a new file only the owner can read, and syncs it.
pub async fn write_private(path: &Path, bytes: &[u8]) -> Result<(), StakeError> {
    let io = |source| StakeError::Io {
        what: "writing",
        path: path.to_owned(),
        source,
    };
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).await.map_err(io)?;
    file.write_all(bytes).await.map_err(io)?;
    file.sync_all().await.map_err(io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn secrets_file_is_private_and_parses_back() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Secrets::generate().unwrap();
        let path = write_pending(dir.path(), 1, Address::ZERO, &secrets)
            .await
            .unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(json["seed"], secrets.seed.to_string());
        assert_eq!(
            json["enc_secret"],
            B256::from(secrets.enc_secret).to_string()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
