//! File checkpoints taken before any mutating action.
//!
//! Each `(session, turn)` has a manifest and content-addressed backups. This
//! makes `/undo` and per-turn rollback reliable even when several files changed.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::paths::{atomic_write, path_component};
use crate::session::Session;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointEntry {
    pub original_path: PathBuf,
    pub existed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointManifest {
    pub version: u32,
    pub session_id: String,
    pub turn: u64,
    pub created_at: DateTime<Utc>,
    pub entries: Vec<CheckpointEntry>,
}

#[derive(Debug, Clone, Default)]
pub struct RestoreReport {
    pub restored: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
    pub errors: Vec<String>,
}

impl RestoreReport {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.restored.is_empty() && self.removed.is_empty() && self.errors.is_empty()
    }

    #[must_use]
    pub fn render(&self) -> String {
        let mut lines = Vec::new();
        for path in &self.restored {
            lines.push(format!("restored {}", path.display()));
        }
        for path in &self.removed {
            lines.push(format!("removed  {}", path.display()));
        }
        for error in &self.errors {
            lines.push(format!("error: {error}"));
        }
        if lines.is_empty() {
            "no files needed restoration".into()
        } else {
            lines.join("\n")
        }
    }
}

#[derive(Debug, Clone)]
pub struct Checkpointer {
    base: PathBuf,
    enabled: bool,
}

impl Checkpointer {
    #[must_use]
    pub fn new(base: PathBuf, enabled: bool) -> Self {
        Self { base, enabled }
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    fn turn_dir(&self, session_id: &str, turn: u64) -> PathBuf {
        self.base.join(session_id).join(format!("turn-{turn}"))
    }

    fn manifest_path(&self, session_id: &str, turn: u64) -> PathBuf {
        self.turn_dir(session_id, turn).join("manifest.json")
    }

    fn load_manifest(&self, session_id: &str, turn: u64) -> Result<Option<CheckpointManifest>> {
        let path = self.manifest_path(session_id, turn);
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("could not read checkpoint {}", path.display()))?;
        let mut manifest: CheckpointManifest = serde_json::from_str(&text)
            .with_context(|| format!("invalid checkpoint manifest {}", path.display()))?;
        manifest
            .entries
            .sort_by(|a, b| a.original_path.cmp(&b.original_path));
        Ok(Some(manifest))
    }

    fn save_manifest(&self, manifest: &CheckpointManifest) -> Result<()> {
        let path = self.manifest_path(&manifest.session_id, manifest.turn);
        let body = serde_json::to_vec_pretty(manifest)
            .context("could not serialize checkpoint manifest")?;
        atomic_write(&path, &body)
    }

    /// Capture a file immediately before its first mutation in this turn.
    pub fn snapshot(&self, session_id: &str, turn: u64, path: &Path) -> Result<bool> {
        if !self.enabled {
            return Ok(false);
        }
        let mut manifest =
            self.load_manifest(session_id, turn)?
                .unwrap_or_else(|| CheckpointManifest {
                    version: 1,
                    session_id: session_id.to_string(),
                    turn,
                    created_at: Utc::now(),
                    entries: Vec::new(),
                });
        if manifest
            .entries
            .iter()
            .any(|entry| entry.original_path == path)
        {
            return Ok(false);
        }

        let existed = path.exists();
        let mut backup = None;
        let mut mode = None;
        if existed {
            if !path.is_file() {
                anyhow::bail!("refusing to checkpoint non-file {}", path.display());
            }
            let bytes = std::fs::read(path)
                .with_context(|| format!("could not snapshot {}", path.display()))?;
            let hash = short_hash(path);
            let backup_name = format!("{}.{hash}.bak", path_component(path));
            let backup_path = self
                .turn_dir(session_id, turn)
                .join("backups")
                .join(&backup_name);
            atomic_write(&backup_path, &bytes)?;
            backup = Some(backup_name);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                mode = std::fs::metadata(path).ok().map(|m| m.permissions().mode());
            }
        }
        manifest.entries.push(CheckpointEntry {
            original_path: path.to_path_buf(),
            existed,
            backup,
            mode,
        });
        self.save_manifest(&manifest)?;
        Ok(true)
    }

    /// Restore all captured files for a turn, newest entry first. Newly created
    /// files are removed. The checkpoint is retained only if restoration partly
    /// failed.
    pub fn restore_turn(&self, session_id: &str, turn: u64) -> Result<RestoreReport> {
        let Some(manifest) = self.load_manifest(session_id, turn)? else {
            return Ok(RestoreReport::default());
        };
        let mut report = RestoreReport::default();
        for entry in manifest.entries.iter().rev() {
            let result = if entry.existed {
                self.restore_entry(session_id, turn, entry)
                    .map(|()| report.restored.push(entry.original_path.clone()))
            } else if entry.original_path.exists() {
                std::fs::remove_file(&entry.original_path)
                    .with_context(|| format!("could not remove {}", entry.original_path.display()))
                    .map(|()| report.removed.push(entry.original_path.clone()))
            } else {
                Ok(())
            };
            if let Err(error) = result {
                report.errors.push(format!("{error:#}"));
            }
        }
        if report.errors.is_empty() {
            let dir = self.turn_dir(session_id, turn);
            if dir.exists() {
                std::fs::remove_dir_all(&dir).ok();
            }
        }
        Ok(report)
    }

    fn restore_entry(&self, session_id: &str, turn: u64, entry: &CheckpointEntry) -> Result<()> {
        let backup = entry
            .backup
            .as_ref()
            .context("checkpoint entry has no backup")?;
        let path = self.turn_dir(session_id, turn).join("backups").join(backup);
        let bytes = std::fs::read(&path)
            .with_context(|| format!("could not read backup {}", path.display()))?;
        atomic_write(&entry.original_path, &bytes)?;
        #[cfg(unix)]
        if let Some(mode) = entry.mode {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&entry.original_path, std::fs::Permissions::from_mode(mode))
                .ok();
        }
        Ok(())
    }

    pub fn undo_session_turn(&self, session: &mut Session) -> Result<(Option<u64>, RestoreReport)> {
        let Some(turn) = session.turns.last().map(|turn| turn.number) else {
            return Ok((None, RestoreReport::default()));
        };
        let report = self.restore_turn(&session.id, turn)?;
        if report.errors.is_empty() {
            session.undo_last_turn();
        }
        Ok((Some(turn), report))
    }

    pub fn purge_session(&self, session_id: &str) {
        let dir = self.base.join(session_id);
        if dir.exists() {
            std::fs::remove_dir_all(dir).ok();
        }
    }

    /// Remove old turn directories, keeping the most recent `keep` turns plus
    /// any directory that cannot be parsed.
    pub fn cleanup_session(&self, session_id: &str, keep: usize) {
        let dir = self.base.join(session_id);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        let mut turns: Vec<(u64, PathBuf)> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                let number = name.strip_prefix("turn-")?.parse::<u64>().ok()?;
                Some((number, entry.path()))
            })
            .collect();
        turns.sort_by_key(|(number, _)| *number);
        let excess = turns.len().saturating_sub(keep);
        for (_, path) in turns.into_iter().take(excess) {
            std::fs::remove_dir_all(path).ok();
        }
    }

    #[must_use]
    pub fn list_turns(&self, session_id: &str) -> Vec<u64> {
        let dir = self.base.join(session_id);
        let mut turns = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if let Some(number) = name.strip_prefix("turn-").and_then(|n| n.parse().ok()) {
                    turns.push(number);
                }
            }
        }
        turns.sort_unstable();
        turns
    }
}

fn short_hash(path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.as_os_str().as_encoded_bytes());
    let hash = hasher.finalize();
    hex::encode(&hash[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_modified_file_and_removes_created_file() {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let modified = work.join("modified.txt");
        let created = work.join("created.txt");
        std::fs::write(&modified, "original\n").unwrap();
        let checkpointer = Checkpointer::new(root.path().join("checkpoints"), true);
        checkpointer.snapshot("session", 1, &modified).unwrap();
        checkpointer.snapshot("session", 1, &created).unwrap();
        std::fs::write(&modified, "changed\n").unwrap();
        std::fs::write(&created, "new\n").unwrap();
        let report = checkpointer.restore_turn("session", 1).unwrap();
        assert_eq!(std::fs::read_to_string(&modified).unwrap(), "original\n");
        assert!(!created.exists());
        assert!(report.errors.is_empty());
        assert_eq!(report.restored.len(), 1);
        assert_eq!(report.removed.len(), 1);
    }

    #[test]
    fn snapshots_only_once_per_turn() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("f");
        std::fs::write(&file, "a").unwrap();
        let checkpoint = Checkpointer::new(root.path().join("cp"), true);
        assert!(checkpoint.snapshot("s", 1, &file).unwrap());
        assert!(!checkpoint.snapshot("s", 1, &file).unwrap());
        let manifest = checkpoint.load_manifest("s", 1).unwrap().unwrap();
        assert_eq!(manifest.entries.len(), 1);
    }
}
