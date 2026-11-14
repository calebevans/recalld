//! Generation payloads. Metadata is the only selection authority.
use super::*;
use crate::storage::fsync::fsync_dir;
use crate::storage::metadata::text_generation::{TextGeneration, generation_filename};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const GENERATION_HEADER_SIZE: u64 = 48;

fn header(store_id: Uuid, id: Uuid) -> [u8; 48] {
    let mut bytes = [0; 48];
    bytes[..4].copy_from_slice(&TEXT_LOG_MAGIC);
    bytes[4..6].copy_from_slice(&2u16.to_le_bytes());
    bytes[16..32].copy_from_slice(store_id.as_bytes());
    bytes[32..48].copy_from_slice(id.as_bytes());
    bytes
}

fn prefix_hash(file: &File, length: u64) -> Result<[u8; 32], StorageError> {
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    let mut offset = 0;
    while offset < length {
        let n = (length - offset).min(buffer.len() as u64) as usize;
        read_exact_at(file, &mut buffer[..n], offset)?;
        hash.update(&buffer[..n]);
        offset += n as u64;
    }
    Ok(hash.finalize().into())
}

impl TextStore {
    pub(crate) fn create_generation(
        dir: &Path,
        store_id: Uuid,
    ) -> Result<(Self, Uuid), StorageError> {
        let id = Uuid::new_v4();
        let path = dir.join(generation_filename(id));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.write_all(&header(store_id, id))?;
        Ok((
            Self {
                file,
                path,
                write_pos: GENERATION_HEADER_SIZE,
                header_size: GENERATION_HEADER_SIZE,
            },
            id,
        ))
    }

    /// Opens only the named regular file. Never creates a missing selection.
    pub(crate) fn open_generation(
        dir: &Path,
        descriptor: &TextGeneration,
        writable: bool,
    ) -> Result<Self, StorageError> {
        let path = dir.join(&descriptor.filename);
        crate::storage::validation::require_file(&path)?;
        let file = OpenOptions::new().read(true).write(writable).open(&path)?;
        let length = file.metadata()?.len();
        if length < descriptor.sealed_length
            || prefix_hash(&file, descriptor.sealed_length)? != descriptor.sealed_sha256
        {
            return Err(StorageError::RecoveryRequired(
                "selected text sealed-prefix mismatch".into(),
            ));
        }
        let header_size = if descriptor.format_version == 1 {
            16
        } else {
            GENERATION_HEADER_SIZE
        };
        let mut store = Self {
            file,
            path,
            write_pos: length,
            header_size,
        };
        if descriptor.format_version == 2 {
            let mut bytes = [0; 48];
            read_exact_at(&store.file, &mut bytes, 0)?;
            if bytes != header(descriptor.store_id, descriptor.id) {
                return Err(StorageError::OwnershipMismatch(
                    "selected text header identity mismatch".into(),
                ));
            }
        } else {
            store.validate_header()?;
        }
        Ok(store)
    }

    pub(crate) fn seal(
        &self,
        store_id: Uuid,
        id: Uuid,
        version: u16,
    ) -> Result<TextGeneration, StorageError> {
        self.file.sync_all()?;
        fsync_dir(self.path.parent().ok_or(StorageError::InvalidPath)?)?;
        Ok(TextGeneration {
            id,
            store_id,
            filename: generation_filename(id),
            format_version: version,
            sealed_length: self.write_pos,
            sealed_sha256: prefix_hash(&self.file, self.write_pos)?,
        })
    }

    /// The first upgrade commit preserves offsets and keeps original bytes intact.
    pub(crate) fn copy_legacy(
        &self,
        store_id: Uuid,
    ) -> Result<(Self, TextGeneration), StorageError> {
        let id = Uuid::new_v4();
        let path = self
            .path
            .parent()
            .ok_or(StorageError::InvalidPath)?
            .join(generation_filename(id));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        let mut buffer = [0; 65536];
        let mut offset = 0;
        while offset < self.write_pos {
            let n = (self.write_pos - offset).min(buffer.len() as u64) as usize;
            read_exact_at(&self.file, &mut buffer[..n], offset)?;
            file.write_all(&buffer[..n])?;
            offset += n as u64;
        }
        let copy = Self {
            file,
            path,
            write_pos: self.write_pos,
            header_size: 16,
        };
        let descriptor = copy.seal(store_id, id, 1)?;
        Ok((copy, descriptor))
    }

    pub(crate) fn sync_all(&self) -> Result<(), StorageError> {
        self.file.sync_all().map_err(StorageError::Io)
    }

    pub(crate) fn complete_entry_count(&self) -> Result<usize, StorageError> {
        let mut offset = self.header_size;
        let mut count = 0;
        while offset + 8 <= self.write_pos {
            let mut bytes = [0; 4];
            read_exact_at(&self.file, &mut bytes, offset)?;
            let length = u32::from_le_bytes(bytes);
            if length == 0
                || length > MAX_ENTRY_SIZE
                || offset + 8 + u64::from(length) > self.write_pos
            {
                break;
            }
            offset += 8 + u64::from(length);
            count += 1;
        }
        Ok(count)
    }
}

/// This file only rejects old readers. Its contents NEVER select a payload.
/// Called only after a durable metadata selection exists. An interrupted gate
/// installation leaves either old bytes or the rejection header, both recoverable.
pub(crate) fn install_legacy_gate(
    dir: &Path,
    selected: &TextGeneration,
) -> Result<(), StorageError> {
    let path = dir.join("fulltext.dat");
    let bytes = header(selected.store_id, selected.id);
    if let Ok(file) = File::open(&path) {
        let mut current = [0; 48];
        if file.metadata()?.len() == 48
            && read_exact_at(&file, &mut current, 0).is_ok()
            && current[..6] == bytes[..6]
        {
            return Ok(());
        }
    }
    crate::storage::validation::require_file(&path)?;
    let temporary = dir.join(format!(".text-gate.{}", Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, &path)?;
    fsync_dir(dir)?;
    Ok(())
}

/// Conservative startup hygiene: only self-identifying owned v2 payloads qualify.
/// Legacy copies, malformed files, symlinks and foreign stores are retained.
pub(crate) fn cleanup_orphans(dir: &Path, selected: &TextGeneration) -> Result<(), StorageError> {
    use crate::storage::metadata::text_generation::is_generation_filename;
    let mut changed = false;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name == selected.filename
            || !is_generation_filename(name)
            || !entry.file_type()?.is_file()
        {
            continue;
        }
        let id = Uuid::parse_str(&name[9..name.len() - 4])
            .map_err(|e| StorageError::Deserialize(e.to_string()))?;
        let file = File::open(entry.path())?;
        let mut bytes = [0; 48];
        if read_exact_at(&file, &mut bytes, 0).is_err() || bytes != header(selected.store_id, id) {
            continue;
        }
        std::fs::remove_file(entry.path())?;
        changed = true;
    }
    if changed {
        fsync_dir(dir)?;
    }
    Ok(())
}
