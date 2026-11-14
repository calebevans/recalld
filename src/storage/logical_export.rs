//! Backend-neutral retained-state stream. Physical offsets/slots are relocated,
//! and unreachable text/vector/database-page bytes never enter the stream.
#[cfg(test)]
mod tests;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;

use redb::{MultimapTableHandle, ReadableTable, TableDefinition, TableHandle};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::StorageError;
use super::control::GenerationRef;
use super::offline_snapshot::{OfflineRecord, OfflineSnapshot};
use crate::model::{DecayPhase, DiskRecord, NamespaceConfig};

// A maximum-size retained text (1 MiB) can expand sixfold under JSON escaping.
// One canonical row per chunk keeps parsing and transaction sizes bounded.
const MAX_LINE: u64 = 16 * 1024 * 1024;
const CATALOG: &str = "text_generations_v1";

#[cfg(test)]
thread_local! {
    static TEST_AVAILABLE_SPACE: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

fn check_staging_space(
    parent: &Path,
    input: &Path,
    chunks: u64,
    vector_slack: u64,
) -> Result<(), StorageError> {
    // Conservative headroom, not a reservation or an exact redb growth promise.
    // Retained input/source/old stores already consume space and are never deleted.
    let required = std::fs::metadata(input)?
        .len()
        .checked_mul(2)
        .and_then(|n| {
            chunks
                .checked_mul(8192)
                .and_then(|rows| n.checked_add(rows))
        })
        .and_then(|n| n.checked_add(vector_slack))
        .and_then(|n| n.checked_add(64 * 1024 * 1024))
        .ok_or_else(|| invalid("staging space estimate overflow"))?;
    #[cfg(test)]
    let available = TEST_AVAILABLE_SPACE
        .with(|v| v.take())
        .map(Ok)
        .unwrap_or_else(|| fs2::available_space(parent))?;
    #[cfg(not(test))]
    let available = fs2::available_space(parent)?;
    if available < required {
        return Err(invalid(&format!(
            "insufficient staging space: estimated {required} bytes, available {available}"
        )));
    }
    Ok(())
}

fn checkpoint(_point: &str) {
    #[cfg(test)]
    if std::env::var("RECALLD_IMPORT_TEST_STOP").as_deref() == Ok(_point) {
        std::process::exit(76);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExportSummary {
    pub export_id: Uuid,
    pub source: GenerationRef,
    pub chunks: u64,
    pub records: u64,
    pub logical_sha256: [u8; 32],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Header {
    export_format_version: u32,
    canonical_schema_version: u32,
    export_id: Uuid,
    source: GenerationRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum TableKind {
    StrBytes,
    BytesBytes,
    StrU32,
}

// Every canonical relation is explicit. Unknown relations fail closed rather
// than being guessed derived and silently omitted. Shared snapshot validation
// supplies the schema-specific ledger/effects/ancestry checks when integrated.
fn table_kind(name: &str) -> Option<TableKind> {
    match name {
        "canonical_control"
        | "canonical_schema"
        | "removal_ledger_state"
        | "restore_lineage_v1"
        | CATALOG => Some(TableKind::StrBytes),
        "mutation_receipts"
        | "mutation_keys"
        | "mutation_effects"
        | "removal_ledger"
        | "namespace_purge_progress"
        | "mutation_batches" => Some(TableKind::BytesBytes),
        "namespace_counter" | "effects_schema" | "namespace_purge_schema" | "batch_schema" => {
            Some(TableKind::StrU32)
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LogicalMemory {
    record: Vec<u8>,
    full_text: Option<String>,
    embedding_bits: Option<Vec<u32>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "data",
    rename_all = "camelCase",
    deny_unknown_fields
)]
enum Item {
    Namespace(NamespaceConfig),
    Table { name: String, kind: TableKind },
    Row { key: Vec<u8>, value: Vec<u8> },
    Memory(LogicalMemory),
    Edge { key: Vec<u8>, value: Vec<u8> },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Chunk {
    sequence: u64,
    sha256: [u8; 32],
    item: Item,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "data",
    rename_all = "camelCase",
    deny_unknown_fields
)]
enum Line {
    Header(Header),
    Chunk(Chunk),
    Footer {
        summary: ExportSummary,
        chain_sha256: [u8; 32],
    },
}

fn invalid(message: &str) -> StorageError {
    StorageError::InvalidMutation(format!("logical export: {message}"))
}
fn encoded(value: &impl Serialize) -> Result<Vec<u8>, StorageError> {
    super::control::encode(value)
}
fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
fn chain(previous: [u8; 32], sequence: u64, bytes: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(previous);
    hash.update(sequence.to_be_bytes());
    hash.update(bytes);
    hash.finalize().into()
}
fn write_line(writer: &mut impl Write, line: &Line) -> Result<(), StorageError> {
    let bytes = encoded(line)?;
    if bytes.len() as u64 >= MAX_LINE {
        return Err(invalid("chunk exceeds maximum encoded length"));
    }
    writer.write_all(&bytes)?;
    writer.write_all(b"\n")?;
    Ok(())
}

fn logical_memory(mut item: OfflineRecord) -> Result<LogicalMemory, StorageError> {
    if item.record.version != 2 || item.record.schema.is_none() {
        return Err(invalid("migrate canonical schema before logical export"));
    }
    if item.record.phase == DecayPhase::Tombstone
        && (item.full_text.is_some()
            || !item.record.summary.is_empty()
            || !item.record.tags.is_empty())
    {
        return Err(invalid("tombstone retains removed content"));
    }
    item.record.text_offset = 0;
    item.record.text_length = 0;
    item.record.vector_slot = 0;
    if let Some(reference) = item.record.schema.as_mut().unwrap().vector.as_mut() {
        reference.slot = 0;
    }
    Ok(LogicalMemory {
        record: item.record.encode()?,
        full_text: item.full_text,
        embedding_bits: item
            .embedding
            .map(|v| v.into_iter().map(f32::to_bits).collect()),
    })
}

fn visit_items(
    snapshot: &OfflineSnapshot,
    mut visit: impl FnMut(Item) -> Result<(), StorageError>,
) -> Result<(), StorageError> {
    if snapshot.control().control_schema_version != 2 {
        return Err(invalid("migrate canonical schema before logical export"));
    }
    let read = snapshot.metadata.begin_read()?;
    for table in read.list_multimap_tables()? {
        match table.name() {
            "tag_index" => {
                read.open_multimap_table(redb::MultimapTableDefinition::<&str, &[u8]>::new(
                    "tag_index",
                ))?;
            }
            "namespace_index" => {
                read.open_multimap_table(redb::MultimapTableDefinition::<u32, &[u8]>::new(
                    "namespace_index",
                ))?;
            }
            _ => return Err(invalid("unknown canonical multimap")),
        }
    }
    let mut tables = BTreeMap::new();
    for table in read.list_tables()? {
        let name = table.name();
        if matches!(
            name,
            "memories" | "namespaces" | "indexes" | "logical_import_progress"
        ) {
            continue;
        }
        tables.insert(
            name.to_owned(),
            table_kind(name).ok_or_else(|| invalid("unknown canonical table"))?,
        );
    }
    for ns in snapshot.namespaces() {
        visit(Item::Namespace(ns.clone()))?;
    }
    for (name, kind) in tables {
        visit(Item::Table {
            name: name.clone(),
            kind,
        })?;
        match kind {
            TableKind::StrBytes => {
                for row in read
                    .open_table(TableDefinition::<&str, &[u8]>::new(&name))?
                    .iter()?
                {
                    let (key, value) = row?;
                    visit(Item::Row {
                        key: key.value().as_bytes().to_vec(),
                        value: value.value().to_vec(),
                    })?;
                }
            }
            TableKind::BytesBytes => {
                for row in read
                    .open_table(TableDefinition::<&[u8], &[u8]>::new(&name))?
                    .iter()?
                {
                    let (key, value) = row?;
                    visit(Item::Row {
                        key: key.value().to_vec(),
                        value: value.value().to_vec(),
                    })?;
                }
            }
            TableKind::StrU32 => {
                for row in read
                    .open_table(TableDefinition::<&str, u32>::new(&name))?
                    .iter()?
                {
                    let (key, value) = row?;
                    visit(Item::Row {
                        key: key.value().as_bytes().to_vec(),
                        value: value.value().to_be_bytes().to_vec(),
                    })?;
                }
            }
        }
    }
    snapshot.for_each_record(|record| visit(Item::Memory(logical_memory(record)?)))?;
    let read = snapshot.edges.begin_read()?;
    for table in read.list_tables()? {
        if !matches!(table.name(), "edges_forward" | "edges_reverse") {
            return Err(invalid("unknown edge relation"));
        }
    }
    if read.list_multimap_tables()?.next().is_some() {
        return Err(invalid("unknown edge multimap"));
    }
    for row in read.open_table(super::edges::FORWARD_EDGES)?.iter()? {
        let (key, value) = row?;
        visit(Item::Edge {
            key: key.value().to_vec(),
            value: value.value().to_vec(),
        })?;
    }
    Ok(())
}

struct LogicalHash {
    hash: Sha256,
    physical: bool,
    records: u64,
}
impl LogicalHash {
    fn new(source: GenerationRef) -> Result<Self, StorageError> {
        let mut hash = Sha256::new();
        hash.update(encoded(&source)?);
        Ok(Self {
            hash,
            physical: false,
            records: 0,
        })
    }
    fn item(&mut self, item: &Item) -> Result<(), StorageError> {
        match item {
            Item::Table { name, .. } => self.physical = name == CATALOG,
            Item::Row { .. } => {}
            Item::Memory(_) => {
                self.physical = false;
                self.records += 1;
            }
            _ => self.physical = false,
        }
        // Catalog history is retained in the transport checksum, but new text
        // selection/physical offsets are not logical fidelity requirements.
        if !self.physical {
            let bytes = encoded(item)?;
            self.hash.update((bytes.len() as u64).to_be_bytes());
            self.hash.update(bytes);
        }
        Ok(())
    }
}

/// Publishes a new export file only after the entire retained-state stream is
/// written and synced. Existing files are never overwritten.
pub fn export_store(source: &Path, output: &Path) -> Result<ExportSummary, StorageError> {
    let snapshot = OfflineSnapshot::open(source)?;
    let output = std::path::absolute(output)?;
    let parent = output
        .parent()
        .ok_or(StorageError::InvalidPath)?
        .canonicalize()?;
    let output = parent.join(output.file_name().ok_or(StorageError::InvalidPath)?);
    if output.starts_with(source.canonicalize()?) || output.try_exists()? {
        return Err(invalid("export requires a fresh file outside the source"));
    }
    let mut temporary = tempfile::NamedTempFile::new_in(&parent)?;
    let header = Header {
        export_format_version: 1,
        canonical_schema_version: 2,
        export_id: Uuid::new_v4(),
        source: snapshot.control().generation_ref(),
    };
    let mut previous = digest(&encoded(&header)?);
    let mut count = 0u64;
    let mut logical = LogicalHash::new(header.source)?;
    write_line(&mut temporary, &Line::Header(header.clone()))?;
    visit_items(&snapshot, |item| {
        count = count
            .checked_add(1)
            .ok_or_else(|| invalid("chunk count overflow"))?;
        let bytes = encoded(&item)?;
        previous = chain(previous, count, &bytes);
        logical.item(&item)?;
        write_line(
            &mut temporary,
            &Line::Chunk(Chunk {
                sequence: count,
                sha256: previous,
                item,
            }),
        )
    })?;
    let summary = ExportSummary {
        export_id: header.export_id,
        source: header.source,
        chunks: count,
        records: logical.records,
        logical_sha256: logical.hash.finalize().into(),
    };
    write_line(
        &mut temporary,
        &Line::Footer {
            summary: summary.clone(),
            chain_sha256: previous,
        },
    )?;
    temporary.as_file().sync_all()?;
    temporary
        .persist_noclobber(&output)
        .map_err(|e| StorageError::Io(e.error))?;
    super::fsync::fsync_dir(&parent)?;
    Ok(summary)
}

fn read_line(reader: &mut impl BufRead) -> Result<Option<Line>, StorageError> {
    let mut bytes = Vec::new();
    let size = reader.take(MAX_LINE + 1).read_until(b'\n', &mut bytes)?;
    if size == 0 {
        return Ok(None);
    }
    if size as u64 > MAX_LINE || bytes.last() != Some(&b'\n') {
        return Err(invalid("oversized or truncated chunk"));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| invalid("malformed or unsupported stream"))
}

#[derive(Default)]
struct Order {
    phase: u8,
    namespace: u32,
    table: String,
    key: Vec<u8>,
}
impl Order {
    fn check(&mut self, item: &Item) -> Result<(), StorageError> {
        match item {
            Item::Namespace(ns) => {
                if self.phase != 0 || ns.id.get() <= self.namespace {
                    return Err(invalid("namespace order/identity"));
                }
                ns.schema
                    .as_ref()
                    .ok_or_else(|| invalid("namespace schema missing"))?
                    .validate(ns)
                    .map_err(StorageError::InvalidMutation)?;
                crate::model::validation::validate_namespace_name(&ns.name)
                    .map_err(|_| invalid("unsafe namespace name"))?;
                self.namespace = ns.id.get();
            }
            Item::Table { name, kind } => {
                if self.phase > 1 || name <= &self.table || table_kind(name) != Some(*kind) {
                    return Err(invalid("unknown, duplicate or unordered table"));
                }
                self.phase = 1;
                self.table = name.clone();
                self.key.clear();
            }
            Item::Row { key, value } => {
                if self.phase != 1
                    || self.table.is_empty()
                    || key.is_empty()
                    || (!self.key.is_empty() && key <= &self.key)
                {
                    return Err(invalid("unordered/duplicate table row"));
                }
                match table_kind(&self.table).unwrap() {
                    TableKind::StrBytes => {
                        std::str::from_utf8(key).map_err(|_| invalid("invalid string key"))?;
                    }
                    TableKind::StrU32 => {
                        std::str::from_utf8(key).map_err(|_| invalid("invalid counter key"))?;
                        if value.len() != 4 {
                            return Err(invalid("invalid u32 value"));
                        }
                    }
                    TableKind::BytesBytes => {}
                }
                self.key = key.clone();
            }
            Item::Memory(memory) => {
                if self.phase > 2 {
                    return Err(invalid("memory after edges"));
                }
                let record = DiskRecord::from_bytes(&memory.record)?;
                if record.version != 2
                    || record.text_offset != 0
                    || record.text_length != 0
                    || record.vector_slot != 0
                {
                    return Err(invalid("nonlogical record encoding"));
                }
                if self.phase == 2 && record.id.as_slice() <= self.key.as_slice() {
                    return Err(invalid("unordered/duplicate memory"));
                }
                if memory.full_text.as_ref().is_some_and(|s| {
                    s.is_empty() || s.len() > crate::model::constants::FULL_TEXT_MAX_BYTES
                }) {
                    return Err(invalid("invalid retained text"));
                }
                match (&memory.embedding_bits, record.phase) {
                    (None, DecayPhase::Tombstone)
                        if memory.full_text.is_none()
                            && record.summary.is_empty()
                            && record.tags.is_empty() => {}
                    (Some(bits), phase)
                        if phase != DecayPhase::Tombstone
                            && !bits.is_empty()
                            && bits.len() <= 8192 =>
                    {
                        let mut hash = Sha256::new();
                        for bit in bits {
                            if !f32::from_bits(*bit).is_finite() {
                                return Err(invalid("nonfinite vector"));
                            }
                            hash.update(bit.to_le_bytes());
                        }
                        if record
                            .schema
                            .as_ref()
                            .and_then(|s| s.vector.as_ref())
                            .is_none_or(|v| {
                                v.slot != 0 || v.sha256 != <[u8; 32]>::from(hash.finalize())
                            })
                        {
                            return Err(invalid("embedding hash mismatch"));
                        }
                    }
                    _ => return Err(invalid("record/embedding ownership mismatch")),
                }
                self.phase = 2;
                self.key = record.id.to_vec();
            }
            Item::Edge { key, value } => {
                if self.phase == 3 && key <= &self.key {
                    return Err(invalid("unordered/duplicate edge"));
                }
                let edge = super::PersistedEdge::from_forward(key, value)?;
                if !edge.weight.is_finite() || !(0.0..=1.0).contains(&edge.weight) || value[4] > 1 {
                    return Err(invalid("invalid edge values"));
                }
                self.phase = 3;
                self.key = key.clone();
            }
        }
        Ok(())
    }
}

fn scan_stream(
    path: &Path,
    mut visit: impl FnMut(&Header, &Chunk, &str) -> Result<(), StorageError>,
) -> Result<(Header, ExportSummary, [u8; 32]), StorageError> {
    super::validation::require_file(path)?;
    let mut reader = BufReader::new(std::fs::File::open(path)?);
    let Some(Line::Header(header)) = read_line(&mut reader)? else {
        return Err(invalid("missing header"));
    };
    if header.export_format_version != 1
        || header.canonical_schema_version != 2
        || header.export_id.is_nil()
        || header.source.store_id.is_nil()
    {
        return Err(invalid("unsupported export version/identity"));
    }
    let mut previous = digest(&encoded(&header)?);
    let mut count = 0u64;
    let mut logical = LogicalHash::new(header.source)?;
    let mut order = Order::default();
    loop {
        match read_line(&mut reader)? {
            Some(Line::Chunk(chunk)) => {
                count = count
                    .checked_add(1)
                    .ok_or_else(|| invalid("chunk count overflow"))?;
                if chunk.sequence != count
                    || chunk.sha256 != chain(previous, count, &encoded(&chunk.item)?)
                {
                    return Err(invalid("chunk sequence/checksum mismatch"));
                }
                order.check(&chunk.item)?;
                logical.item(&chunk.item)?;
                visit(&header, &chunk, &order.table)?;
                previous = chunk.sha256;
            }
            Some(Line::Footer {
                summary,
                chain_sha256,
            }) => {
                if summary.export_id != header.export_id
                    || summary.source != header.source
                    || summary.chunks != count
                    || summary.records != logical.records
                    || summary.logical_sha256 != <[u8; 32]>::from(logical.hash.finalize())
                    || chain_sha256 != previous
                    || read_line(&mut reader)?.is_some()
                {
                    return Err(invalid("footer/fidelity mismatch or trailing input"));
                }
                return Ok((header, summary, previous));
            }
            _ => return Err(invalid("missing footer or duplicate header")),
        }
    }
}

const PROGRESS: TableDefinition<&str, &[u8]> = TableDefinition::new("logical_import_progress");
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Progress {
    version: u32,
    summary: ExportSummary,
    final_chain: [u8; 32],
    completed: u64,
    prefix: [u8; 32],
    text: super::metadata::text_generation::TextGeneration,
    validated: bool,
}
fn read_progress(db: &redb::Database) -> Result<Progress, StorageError> {
    let read = db.begin_read()?;
    let table = read.open_table(PROGRESS)?;
    let value = table
        .get("state")?
        .ok_or_else(|| invalid("missing import progress"))?;
    let p: Progress = super::control::decode(value.value())?;
    if p.version != 1
        || p.completed > p.summary.chunks
        || p.text.id.is_nil()
        || p.text.store_id != p.summary.source.store_id
        || p.text.filename != super::metadata::text_generation::generation_filename(p.text.id)
        || p.text.format_version != 2
        || p.text.sealed_length != 48
    {
        return Err(invalid("unknown or invalid import progress"));
    }
    Ok(p)
}
fn store_progress(tx: &redb::WriteTransaction, p: &Progress) -> Result<(), StorageError> {
    tx.open_table(PROGRESS)?
        .insert("state", encoded(p)?.as_slice())?;
    Ok(())
}

fn initialize_stage(
    path: &Path,
    header: &Header,
    summary: &ExportSummary,
    final_chain: [u8; 32],
) -> Result<(), StorageError> {
    let mut marker = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path.join(".recalld-importing"))?;
    marker.write_all(header.export_id.as_bytes())?;
    marker.sync_all()?;
    let (text, id) = super::TextStore::create_generation(path, header.source.store_id)?;
    let descriptor = text.seal(header.source.store_id, id, 2)?;
    super::text::generation::write_rejection_gate(path, &descriptor)?;
    let db = redb::Database::create(path.join("meta.db"))?;
    let tx = db.begin_write()?;
    tx.open_table(super::metadata::META_TABLE)?;
    tx.open_table(super::metadata::NAMESPACE_TABLE)?;
    tx.open_table(super::metadata::NAMESPACE_COUNTER)?;
    tx.open_table(TableDefinition::<&str, &[u8]>::new("indexes"))?;
    tx.open_multimap_table(redb::MultimapTableDefinition::<&str, &[u8]>::new(
        "tag_index",
    ))?;
    tx.open_multimap_table(redb::MultimapTableDefinition::<u32, &[u8]>::new(
        "namespace_index",
    ))?;
    store_progress(
        &tx,
        &Progress {
            version: 1,
            summary: summary.clone(),
            final_chain,
            completed: 0,
            prefix: digest(&encoded(header)?),
            text: descriptor,
            validated: false,
        },
    )?;
    tx.commit()?;
    drop(db);
    drop(super::EdgeStore::open(&path.join("edges.db"))?);
    super::fsync::fsync_dir(path)?;
    Ok(())
}

struct Importer {
    meta: redb::Database,
    edges: redb::Database,
    vectors: super::VectorManager,
    text: super::TextStore,
    progress: Progress,
}
impl Importer {
    fn open(
        path: &Path,
        summary: &ExportSummary,
        final_chain: [u8; 32],
    ) -> Result<Self, StorageError> {
        super::validation::require_file(&path.join("meta.db"))?;
        super::validation::require_file(&path.join("edges.db"))?;
        let meta = redb::Database::open(path.join("meta.db"))?;
        let progress = read_progress(&meta)?;
        if progress.summary != *summary || progress.final_chain != final_chain {
            return Err(invalid("staging belongs to another export/checksum"));
        }
        let text = super::TextStore::open_generation(path, &progress.text, true)?;
        let mut vectors = super::VectorManager::new(path.to_path_buf());
        {
            let read = meta.begin_read()?;
            for row in read.open_table(super::metadata::NAMESPACE_TABLE)?.iter()? {
                let (_, v) = row?;
                let ns = NamespaceConfig::decode(v.value())?;
                vectors.open_or_create(ns.id, &ns.name, ns.embedding_dim as usize)?;
            }
        }
        Ok(Self {
            meta,
            edges: redb::Database::open(path.join("edges.db"))?,
            vectors,
            text,
            progress,
        })
    }
    fn chunk(&mut self, chunk: &Chunk, current_table: &str) -> Result<(), StorageError> {
        if chunk.sequence <= self.progress.completed {
            if chunk.sequence == self.progress.completed && chunk.sha256 != self.progress.prefix {
                return Err(invalid("resume prefix differs from committed chunks"));
            }
            return Ok(());
        }
        if chunk.sequence != self.progress.completed + 1 {
            return Err(invalid("resume sequence gap"));
        }
        let tx = self.meta.begin_write()?;
        match &chunk.item {
            Item::Namespace(ns) => {
                self.vectors
                    .open_or_create(ns.id, &ns.name, ns.embedding_dim as usize)?
                    .sync()?;
                tx.open_table(super::metadata::NAMESPACE_TABLE)?
                    .insert(ns.id.get(), encoded(ns)?.as_slice())?;
            }
            Item::Table { name, kind } => match kind {
                TableKind::StrBytes => {
                    tx.open_table(TableDefinition::<&str, &[u8]>::new(name))?;
                }
                TableKind::BytesBytes => {
                    tx.open_table(TableDefinition::<&[u8], &[u8]>::new(name))?;
                }
                TableKind::StrU32 => {
                    tx.open_table(TableDefinition::<&str, u32>::new(name))?;
                }
            },
            Item::Row { key, value } => match table_kind(current_table)
                .ok_or_else(|| invalid("row without canonical table"))?
            {
                TableKind::StrBytes => {
                    tx.open_table(TableDefinition::<&str, &[u8]>::new(current_table))?
                        .insert(
                            std::str::from_utf8(key).map_err(|_| invalid("string key"))?,
                            value.as_slice(),
                        )?;
                }
                TableKind::BytesBytes => {
                    tx.open_table(TableDefinition::<&[u8], &[u8]>::new(current_table))?
                        .insert(key.as_slice(), value.as_slice())?;
                }
                TableKind::StrU32 => {
                    tx.open_table(TableDefinition::<&str, u32>::new(current_table))?
                        .insert(
                            std::str::from_utf8(key).map_err(|_| invalid("counter key"))?,
                            u32::from_be_bytes(
                                value
                                    .as_slice()
                                    .try_into()
                                    .map_err(|_| invalid("counter value"))?,
                            ),
                        )?;
                }
            },
            Item::Memory(memory) => {
                let mut record = DiskRecord::from_bytes(&memory.record)?;
                if let Some(bits) = &memory.embedding_bits {
                    let vector: Vec<_> = bits.iter().map(|b| f32::from_bits(*b)).collect();
                    let store = self
                        .vectors
                        .get_mut(crate::model::NamespaceId::new(record.namespace_id))
                        .ok_or(StorageError::NamespaceNotFound(record.namespace_id))?;
                    record.vector_slot = store.insert_vector(&vector)?;
                    store.sync()?;
                    record
                        .schema
                        .as_mut()
                        .unwrap()
                        .vector
                        .as_mut()
                        .unwrap()
                        .slot = record.vector_slot;
                }
                if let Some(text) = &memory.full_text {
                    let reference = self.text.append(text)?;
                    self.text.sync_data()?;
                    record.text_offset = reference.file_offset;
                    record.text_length = reference.length;
                }
                checkpoint("payload_synced");
                tx.open_table(super::metadata::META_TABLE)?
                    .insert(record.id.as_slice(), record.encode()?.as_slice())?;
                tx.open_multimap_table(redb::MultimapTableDefinition::<u32, &[u8]>::new(
                    "namespace_index",
                ))?
                .insert(record.namespace_id, record.id.as_slice())?;
                let mut tags = tx
                    .open_multimap_table(redb::MultimapTableDefinition::<&str, &[u8]>::new(
                        "tag_index",
                    ))?;
                for tag in &record.tags {
                    tags.insert(tag.as_str(), record.id.as_slice())?;
                }
            }
            Item::Edge { key, value } => {
                let edge = super::PersistedEdge::from_forward(key, value)?;
                let edges = self.edges.begin_write()?;
                edges
                    .open_table(super::edges::FORWARD_EDGES)?
                    .insert(key.as_slice(), value.as_slice())?;
                let mut reverse = Vec::from(edge.target.as_bytes().as_slice());
                reverse.push(edge.edge_type.as_u8());
                reverse.extend(edge.source.as_bytes());
                edges
                    .open_table(super::edges::REVERSE_EDGES)?
                    .insert(reverse.as_slice(), ())?;
                edges.commit()?;
                checkpoint("edge_committed");
            }
        }
        self.progress.completed = chunk.sequence;
        self.progress.prefix = chunk.sha256;
        store_progress(&tx, &self.progress)?;
        tx.commit()?;
        checkpoint("chunk_committed");
        Ok(())
    }
    fn select_text(self, path: &Path) -> Result<(), StorageError> {
        let descriptor = self.text.seal(
            self.progress.summary.source.store_id,
            self.progress.text.id,
            2,
        )?;
        let tx = self.meta.begin_write()?;
        {
            let mut catalog = tx.open_table(TableDefinition::<&str, &[u8]>::new(CATALOG))?;
            catalog.insert(
                descriptor.id.to_string().as_str(),
                encoded(&descriptor)?.as_slice(),
            )?;
            catalog.insert("active", encoded(&descriptor.id)?.as_slice())?;
        }
        tx.commit()?;
        drop(self);
        super::fsync::fsync_dir(path)?;
        Ok(())
    }
}

fn validate_import(path: &Path, expected: &ExportSummary) -> Result<(), StorageError> {
    let snapshot = OfflineSnapshot::open_import_stage(path)?;
    validate_logical_snapshot(&snapshot, expected)
}
fn validate_logical_snapshot(
    snapshot: &OfflineSnapshot,
    expected: &ExportSummary,
) -> Result<(), StorageError> {
    if snapshot.control().generation_ref() != expected.source {
        return Err(invalid("import changed source lineage/generation"));
    }
    let mut logical = LogicalHash::new(expected.source)?;
    visit_items(snapshot, |item| logical.item(&item))?;
    if logical.records != expected.records
        || <[u8; 32]>::from(logical.hash.finalize()) != expected.logical_sha256
    {
        return Err(invalid("import logical fidelity mismatch"));
    }
    Ok(())
}

/// Resume only a checksummed import belonging to this export ID. No existing
/// destination is overwritten; an already-complete matching import is verified.
pub fn import_store(input: &Path, destination: &Path) -> Result<ExportSummary, StorageError> {
    let mut vector_slack = 0u64;
    let (header, summary, final_chain) = scan_stream(input, |_, chunk, _| {
        if let Item::Namespace(ns) = &chunk.item {
            vector_slack = vector_slack
                .checked_add(
                    u64::from(ns.embedding_dim) * 4 * u64::from(super::vectors::GROWTH_CHUNK),
                )
                .ok_or_else(|| invalid("vector staging size overflow"))?;
        }
        Ok(())
    })?;
    let absolute = std::path::absolute(destination)?;
    let parent = absolute
        .parent()
        .ok_or(StorageError::InvalidPath)?
        .canonicalize()?;
    let target = parent.join(absolute.file_name().ok_or(StorageError::InvalidPath)?);
    let owner = super::ownership::StoreOwnership::acquire(&target)?;
    let target = &owner.path;
    if target.try_exists()? {
        let (_copy, db) = super::validation::inspect_database(&target.join("meta.db"))?;
        let p = read_progress(&db)?;
        if !p.validated || p.summary != summary || p.final_chain != final_chain {
            return Err(invalid("destination is not this completed import"));
        }
        let snapshot = OfflineSnapshot::open_owned(owner)?;
        validate_logical_snapshot(&snapshot, &summary)?;
        return Ok(summary);
    }
    check_staging_space(&parent, input, summary.chunks, vector_slack)?;
    let identity: String = Sha256::digest(target.as_os_str().as_encoded_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let stage = parent.join(format!(
        ".recalld-import-{}-{}",
        &identity[..16],
        header.export_id
    ));
    if stage.is_symlink() {
        return Err(StorageError::InvalidPath);
    }
    if !stage.try_exists()? {
        let initial = tempfile::Builder::new()
            .prefix(".recalld-import-initialize-")
            .tempdir_in(&parent)?;
        initialize_stage(initial.path(), &header, &summary, final_chain)?;
        std::fs::rename(initial.path(), &stage)?;
        super::fsync::fsync_dir(&parent)?;
    }
    checkpoint("initialized");
    let mut importer = Importer::open(&stage, &summary, final_chain)?;
    let (_, verified, verified_chain) =
        scan_stream(input, |_, chunk, table| importer.chunk(chunk, table))?;
    if verified != summary || verified_chain != final_chain {
        return Err(invalid("export changed during import"));
    }
    importer.select_text(&stage)?;
    checkpoint("selected");
    validate_import(&stage, &summary)?;
    {
        let db = redb::Database::open(stage.join("meta.db"))?;
        let mut progress = read_progress(&db)?;
        progress.validated = true;
        let tx = db.begin_write()?;
        store_progress(&tx, &progress)?;
        tx.commit()?;
    }
    let marker = stage.join(".recalld-importing");
    if marker.try_exists()? {
        std::fs::remove_file(marker)?;
    }
    for file in super::validation::canonical_files(&stage)? {
        std::fs::File::open(file)?.sync_all()?;
    }
    super::fsync::fsync_dir(&stage)?;
    checkpoint("validated");
    if target.try_exists()? {
        return Err(invalid("destination appeared during import"));
    }
    std::fs::rename(&stage, target)?;
    super::fsync::fsync_dir(&parent).map_err(|e| {
        StorageError::RecoveryRequired(format!(
            "import {} published; inspect destination before retrying: {e}",
            summary.export_id
        ))
    })?;
    checkpoint("published");
    Ok(summary)
}
