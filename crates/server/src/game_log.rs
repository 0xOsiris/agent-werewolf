//! Per-game JSONL event log and end-of-game summary, kept as benchmark data.

use std::{
    io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use tokio::{fs, io::AsyncWriteExt, sync::mpsc};
use tracing::error;
use werewolf_protocol::{Phase, PlayerId, Role, Team};

/// Appends lines to `<dir>/<unix-ts>-<seed>.jsonl` from a background task.
pub struct GameLog {
    tx: mpsc::UnboundedSender<String>,
    summary_path: PathBuf,
}

impl GameLog {
    pub fn create(dir: &Path, seed: u64) -> Self {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let path = dir.join(format!("{ts}-{seed}.jsonl"));
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(write_lines(dir.to_owned(), path, rx));
        Self {
            tx,
            summary_path: dir.join(format!("{ts}-{seed}-summary.json")),
        }
    }

    pub fn append(&self, line: String) {
        // Fails only once the writer has stopped, and it logs why.
        let _ = self.tx.send(line);
    }

    pub fn summary_path(&self) -> &Path {
        &self.summary_path
    }
}

async fn write_lines(dir: PathBuf, path: PathBuf, rx: mpsc::UnboundedReceiver<String>) {
    if let Err(err) = try_write_lines(&dir, &path, rx).await {
        error!(path = %path.display(), %err, "game log write failed; no more events will be logged for this game");
    }
}

async fn try_write_lines(
    dir: &Path,
    path: &Path,
    mut rx: mpsc::UnboundedReceiver<String>,
) -> io::Result<()> {
    fs::create_dir_all(dir).await?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    while let Some(mut line) = rx.recv().await {
        line.push('\n');
        file.write_all(line.as_bytes()).await?;
        file.flush().await?;
    }
    Ok(())
}

#[derive(Serialize, Debug)]
pub struct GameSummary {
    pub seed: u64,
    pub winner: Option<Team>,
    pub days_played: u32,
    pub players: Vec<PlayerSummary>,
}

#[derive(Serialize, Debug)]
pub struct PlayerSummary {
    pub id: PlayerId,
    pub name: String,
    pub model: Option<String>,
    pub human: bool,
    pub role: Role,
    pub survived: bool,
    pub died_on: Option<Phase>,
    pub timeouts: u32,
    pub invalid_actions: u32,
    /// The sheriff's shot, on the sheriff's entry only.
    pub shot: Option<ShotSummary>,
}

#[derive(Serialize, Debug)]
pub struct ShotSummary {
    pub target: PlayerId,
    pub hit_wolf: bool,
}

pub async fn write_summary(path: &Path, summary: &GameSummary) {
    let result = async {
        let json = serde_json::to_vec_pretty(summary).map_err(io::Error::other)?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).await?;
        }
        fs::write(path, json).await
    }
    .await;
    if let Err(err) = result {
        error!(path = %path.display(), %err, "failed to write game summary");
    }
}
