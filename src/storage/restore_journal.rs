//! Durable restore publication under the stable sibling ownership lock.
use super::{
    StorageError,
    fsync::fsync_dir,
    metadata::restoration::{self, CapturedSnapshot},
    ownership::{StoreOwnership, lock_directory},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Stage {
    Prepared,
    OldMoved,
    NewInstalled,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FileSeal {
    name: String,
    length: u64,
    sha256: [u8; 32],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RestoreJournal {
    version: u32,
    pub operation_id: Uuid,
    pub target: PathBuf,
    pub staged: PathBuf,
    pub previous: PathBuf,
    pub new_store_id: Uuid,
    pub captured: Option<CapturedSnapshot>,
    pub archive_sha256: [u8; 32],
    state: Stage,
    inventory: Vec<FileSeal>,
}

pub(crate) fn hash_file(path: &Path) -> Result<[u8; 32], StorageError> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = [0; 65536];
    loop {
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        hash.update(&bytes[..n]);
    }
    Ok(hash.finalize().into())
}

fn inventory(path: &Path) -> Result<Vec<FileSeal>, StorageError> {
    let mut result = Vec::new();
    let mut files = super::validation::canonical_files(path)?;
    // A restore publishes the reconciled FTS index along with canonical data.
    super::validation::require_file(&path.join("fts.db"))?;
    for name in ["fts.db-wal", "fts.db-shm"] {
        if path.join(name).try_exists()? {
            return Err(StorageError::RecoveryRequired(
                "restore FTS checkpoint is incomplete".into(),
            ));
        }
    }
    files.push(path.join("fts.db"));
    for file in files {
        result.push(FileSeal {
            name: file
                .strip_prefix(path)
                .map_err(|_| StorageError::InvalidPath)?
                .to_string_lossy()
                .into_owned(),
            length: fs::metadata(&file)?.len(),
            sha256: hash_file(&file)?,
        });
    }
    result.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(result)
}

impl RestoreJournal {
    pub(crate) fn prepared(
        owner: &StoreOwnership,
        staged: PathBuf,
        captured: Option<CapturedSnapshot>,
        new_store_id: Uuid,
        operation_id: Uuid,
        archive_sha256: [u8; 32],
    ) -> Result<Self, StorageError> {
        let previous = owner
            .path
            .parent()
            .ok_or(StorageError::InvalidPath)?
            .join(format!(
                "{}.bak-{operation_id}",
                owner.path.file_name().unwrap().to_string_lossy()
            ));
        Ok(Self {
            version: 1,
            operation_id,
            target: owner.path.clone(),
            inventory: inventory(&staged)?,
            staged,
            previous,
            new_store_id,
            captured,
            archive_sha256,
            state: Stage::Prepared,
        })
    }
    fn validate_paths(&self, owner: &StoreOwnership) -> Result<(), StorageError> {
        let parent = owner.path.parent().ok_or(StorageError::InvalidPath)?;
        let expected_previous = parent.join(format!(
            "{}.bak-{}",
            owner.path.file_name().unwrap().to_string_lossy(),
            self.operation_id
        ));
        if self.version != 1
            || self.operation_id.is_nil()
            || self.new_store_id.is_nil()
            || self.target != owner.path
            || self.previous != expected_previous
            || self.staged.parent() != Some(parent)
            || !self
                .staged
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(".recalld-restore-"))
            || self.staged == self.target
            || self.staged == self.previous
            || [&self.target, &self.staged, &self.previous]
                .iter()
                .any(|p| p.is_symlink())
        {
            return Err(StorageError::RecoveryRequired(
                "invalid restore journal identity/path/version".into(),
            ));
        }
        Ok(())
    }
    fn save(&self, owner: &StoreOwnership) -> Result<(), StorageError> {
        self.validate_paths(owner)?;
        let parent = owner.path.parent().ok_or(StorageError::InvalidPath)?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        temp.write_all(&super::control::encode(self)?)?;
        temp.as_file().sync_all()?;
        temp.persist(&owner.pending)
            .map_err(|e| StorageError::Io(e.error))?;
        fsync_dir(parent)?;
        Ok(())
    }
    fn validate_new(&self, path: &Path) -> Result<(), StorageError> {
        let capture = restoration::capture(path)?;
        let lineage = capture
            .lineage
            .ok_or_else(|| StorageError::RecoveryRequired("restored lineage missing".into()))?;
        if capture.control.store_id != self.new_store_id
            || lineage.operation_id != self.operation_id
            || lineage.archive_sha256 != self.archive_sha256
            || lineage.captured_ledger_source
                != self.captured.as_ref().map(|c| c.control.generation_ref())
            || inventory(path)? != self.inventory
        {
            return Err(StorageError::RecoveryRequired(
                "restore candidate identity/seal mismatch".into(),
            ));
        }
        Ok(())
    }
    fn validate_old(&self, path: &Path) -> Result<(), StorageError> {
        let expected = self.captured.as_ref().ok_or_else(|| {
            StorageError::RecoveryRequired("unexpected old restore target".into())
        })?;
        let actual = restoration::capture(path)?;
        if &actual != expected {
            return Err(StorageError::RecoveryRequired(
                "retained restore target changed".into(),
            ));
        }
        Ok(())
    }
}

/// Called while old/staged directory locks and stable ownership are already held.
pub(crate) fn publish(
    owner: &StoreOwnership,
    mut journal: RestoreJournal,
    mut boundary: impl FnMut(Stage) -> Result<(), StorageError>,
) -> Result<(), StorageError> {
    if owner.pending.try_exists()? {
        return Err(StorageError::RecoveryRequired(
            "restore journal already exists".into(),
        ));
    }
    journal.validate_new(&journal.staged)?;
    journal.save(owner)?;
    boundary(Stage::Prepared)?;
    finish(owner, &mut journal, &mut boundary)
}

fn finish(
    owner: &StoreOwnership,
    journal: &mut RestoreJournal,
    boundary: &mut impl FnMut(Stage) -> Result<(), StorageError>,
) -> Result<(), StorageError> {
    journal.validate_paths(owner)?;
    let parent = owner.path.parent().ok_or(StorageError::InvalidPath)?;
    if journal.target.try_exists()? {
        let actual = restoration::capture(&journal.target)?;
        if actual.control.store_id == journal.new_store_id {
            if journal.staged.try_exists()? {
                return Err(StorageError::RecoveryRequired(
                    "ambiguous duplicate restore staging".into(),
                ));
            }
            journal.validate_new(&journal.target)?;
        } else {
            journal.validate_old(&journal.target)?;
            if journal.previous.try_exists()? {
                return Err(StorageError::RecoveryRequired(
                    "ambiguous retained restore directory".into(),
                ));
            }
            journal.validate_new(&journal.staged)?;
            fs::rename(&journal.target, &journal.previous)?;
            fsync_dir(parent)?;
            journal.state = Stage::OldMoved;
            journal.save(owner)?;
            boundary(Stage::OldMoved)?;
        }
    }
    if !journal.target.try_exists()? {
        if !journal.staged.try_exists()? && journal.previous.try_exists()? {
            journal.validate_old(&journal.previous)?;
            fs::rename(&journal.previous, &journal.target)?;
            fsync_dir(parent)?;
            fs::remove_file(&owner.pending)?;
            fsync_dir(parent)?;
            return Err(StorageError::RecoveryRequired(
                "staging unavailable; retained original target restored, retry startup".into(),
            ));
        }
        if journal.captured.is_some() {
            journal.validate_old(&journal.previous)?;
        } else if journal.previous.try_exists()? {
            return Err(StorageError::RecoveryRequired(
                "unexpected previous store for fresh historical restore".into(),
            ));
        }
        journal.validate_new(&journal.staged)?;
        fs::rename(&journal.staged, &journal.target)?;
        fsync_dir(parent)?;
        journal.state = Stage::NewInstalled;
        journal.save(owner)?;
        boundary(Stage::NewInstalled)?;
    }
    if journal.captured.is_some() {
        journal.validate_old(&journal.previous)?;
    }
    journal.validate_new(&journal.target)?;
    journal.state = Stage::Complete;
    journal.save(owner)?;
    boundary(Stage::Complete)?;
    fs::remove_file(&owner.pending)?;
    fsync_dir(parent)?;
    Ok(())
}

/// Startup calls this before opening writable stores; never guesses unknown journals.
pub(crate) fn recover(owner: &StoreOwnership) -> Result<(), StorageError> {
    super::validation::require_file(&owner.pending)?;
    let mut journal: RestoreJournal = serde_json::from_slice(&fs::read(&owner.pending)?)
        .map_err(|e| StorageError::RecoveryRequired(format!("invalid restore journal: {e}")))?;
    journal.validate_paths(owner)?;
    let mut locks = Vec::new();
    for path in [&journal.target, &journal.staged, &journal.previous] {
        if path.try_exists()? {
            locks.push(lock_directory(path)?);
        }
    }
    finish(owner, &mut journal, &mut |_| Ok(()))
}
