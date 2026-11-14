//! Explicit offline policy cutover into a fresh destination. No serving writer.
//! Original schema publication and historical receipt identities stay immutable.
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use redb::ReadableTable;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::canonical_schema::CanonicalSchemaState;
use super::control::{
    self, ControlState, Generation, GenerationRef, MutationContext, MutationOutcome, MutationScope,
    WriteReceipt,
};
use super::metadata::{META_TABLE, NAMESPACE_TABLE};
use super::offline_snapshot::OfflineSnapshot;
use super::ownership::{StoreOwnership, lock_directory};
use super::{StorageError, fsync::fsync_dir};
use crate::decay::{
    DecayConfig,
    lifecycle::{project, resolve_policy},
};
use crate::graph::{
    RelationshipGraph,
    activation::{ActivationConfig, connection_bonus_with_strengths},
};
use crate::model::{
    DecayPhase, DiskRecord, MemoryId, NamespaceConfig, NamespaceId, schema::LifecycleVersion,
};
use crate::time::Clock;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PolicyTransition {
    pub namespace_id: NamespaceId,
    pub from: LifecycleVersion,
    pub to: LifecycleVersion,
    pub previous_revision: Generation,
    pub next_revision: Generation,
    pub previous_generation: Generation,
    pub record_count: u64,
    /// SHA-256 over sorted UUID bytes followed by each post-commit u63 token (BE).
    pub record_token_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LifecycleMigrationOptions {
    pub expected_source: GenerationRef,
    pub namespace_id: NamespaceId,
    pub expected_policy_revision: Generation,
    pub target: LifecycleVersion,
    pub idempotency_key: String,
}
impl LifecycleMigrationOptions {
    fn context(&self) -> MutationContext {
        MutationContext {
            store_id: Some(self.expected_source.store_id),
            scope: MutationScope::Namespace(self.namespace_id),
            idempotency_key: Some(self.idempotency_key.clone()),
            expected_generation: Some(self.expected_source.generation),
            contract_version: 1,
        }
    }
    fn hash(&self, destination: &Path) -> Result<[u8; 32], StorageError> {
        // Physical destination is intentional: retry reconciliation is local to
        // this cutover, not a global key promise across independently copied stores.
        let bytes = control::encode(&(
            self.namespace_id,
            self.expected_policy_revision,
            self.target,
            destination,
        ))?;
        Ok(control::request_hash(
            &self.context(),
            "lifecycle_policy_migrate",
            &[&bytes],
        ))
    }
}

fn invalid(message: &str) -> StorageError {
    StorageError::InvalidControl(message.into())
}
fn token_digest(records: &[DiskRecord], rewind: u64) -> Result<String, StorageError> {
    let mut tokens = records
        .iter()
        .map(|r| {
            let token = r
                .schema
                .as_ref()
                .ok_or_else(|| invalid("record lacks lifecycle schema"))?
                .lifecycle
                .record_generation
                .get()
                .checked_sub(rewind)
                .ok_or_else(|| invalid("lifecycle record token underflow"))?;
            Ok((r.id, token))
        })
        .collect::<Result<Vec<_>, StorageError>>()?;
    tokens.sort_unstable();
    let mut digest = Sha256::new();
    for (id, token) in tokens {
        digest.update(id);
        digest.update(token.to_be_bytes());
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Recognized in-place lifecycle operations and explicit policy migrations are
/// validated in their actual commit order. Other operation types fail closed.
/// Historical counters remain ancestry-bounded, never compared across origins.
pub(crate) fn validate_policy_history(
    read: &redb::ReadTransaction,
    control: &ControlState,
    schema: &CanonicalSchemaState,
) -> Result<(), StorageError> {
    let baseline = if schema.source.store_id == control.store_id {
        schema.publication_generation
    } else {
        Generation::default().next()?
    };
    if baseline > control.committed_generation {
        return Err(invalid("lifecycle baseline exceeds current generation"));
    }
    let keys = read
        .open_table(control::KEYS)?
        .iter()?
        .map(|row| {
            let (_, v) = row?;
            Ok(v.value().to_vec())
        })
        .collect::<Result<std::collections::HashSet<_>, StorageError>>()?;
    let mut suffix = BTreeMap::new();
    let mut policies: HashMap<u32, BTreeMap<Generation, (WriteReceipt, PolicyTransition)>> =
        HashMap::new();
    let mut native_history = Vec::new();
    for row in read.open_table(control::RECEIPTS)?.iter()? {
        let (key, value) = row?;
        let receipt = control::validate_receipt(value.value())?.receipt;
        let active =
            receipt.store_id == control.store_id && receipt.committed_generation > baseline;
        if active
            && suffix
                .insert(receipt.committed_generation, receipt.clone())
                .is_some()
        {
            return Err(invalid("duplicate lifecycle generation"));
        }
        let namespace = match &receipt.outcome {
            MutationOutcome::LifecyclePolicyMigrated { transition: t } => {
                if !keys.contains(key.value())
                    || t.namespace_id.is_unset()
                    || t.from == t.to
                    || t.next_revision != t.previous_revision.next()?
                    || receipt.committed_generation != t.previous_generation.next()?
                    || t.record_token_digest.len() != 64
                    || !t
                        .record_token_digest
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    return Err(invalid("invalid lifecycle transition evidence"));
                }
                if policies
                    .entry(t.namespace_id.get())
                    .or_default()
                    .insert(t.next_revision, (receipt.clone(), t.clone()))
                    .is_some()
                {
                    return Err(invalid("ambiguous lifecycle policy revision"));
                }
                Some(t.namespace_id)
            }
            MutationOutcome::LifecycleExposureRecorded { transition: t }
            | MutationOutcome::LifecycleFeedbackApplied { transition: t } => {
                // validate_receipt already verifies the exact numeric transition.
                native_history.push((receipt.clone(), t.clone()));
                Some(t.namespace_id)
            }
            _ if active => return Err(invalid("unrecognized operation after schema publication")),
            _ => None,
        };
        if let Some(ns) = namespace {
            if key.value()
                != control::scoped_key(
                    receipt.store_id,
                    MutationScope::Namespace(ns),
                    receipt.operation_id.as_bytes(),
                )
                || !control::permits_generation_origin(
                    read,
                    control,
                    GenerationRef {
                        store_id: receipt.store_id,
                        generation: receipt.committed_generation,
                    },
                )?
            {
                return Err(invalid("invalid lifecycle receipt origin/scope"));
            }
        }
    }
    let mut expected = baseline;
    for generation in suffix.keys() {
        expected = expected.next()?;
        if *generation != expected {
            return Err(invalid("gap in lifecycle receipt suffix"));
        }
    }
    if expected != control.committed_generation {
        return Err(invalid("missing lifecycle receipt suffix"));
    }
    // Retained historical native operations still require an explicit policy
    // selection. Their record sets need not survive a deletion-aware restore.
    for (receipt, t) in native_history {
        let version = if t.policy_revision == Generation::default() {
            LifecycleVersion::LegacyAccessV1
        } else {
            policies
                .get(&t.namespace_id.get())
                .and_then(|h| h.get(&t.policy_revision))
                .map(|(_, p)| p.to)
                .ok_or_else(|| invalid("native receipt lacks policy history"))?
        };
        if version != t.lifecycle_version {
            return Err(invalid("native receipt policy version mismatch"));
        }
        if let Some(history) = policies.get(&t.namespace_id.get()) {
            for (revision, (policy_receipt, _)) in history {
                // Only compare generations from the same origin. Ancestry
                // bounds establish the retained foreign-origin proof instead.
                if policy_receipt.store_id == receipt.store_id
                    && ((*revision <= t.policy_revision)
                        != (policy_receipt.committed_generation < receipt.committed_generation))
                {
                    return Err(invalid(
                        "native receipt contradicts policy order in its origin",
                    ));
                }
            }
        }
    }
    let mut records: HashMap<u32, Vec<DiskRecord>> = HashMap::new();
    for row in read.open_table(META_TABLE)?.iter()? {
        let (_, v) = row?;
        let r = DiskRecord::from_bytes(v.value())?;
        records.entry(r.namespace_id).or_default().push(r);
    }
    let mut current_policy = HashMap::new();
    for row in read.open_table(NAMESPACE_TABLE)?.iter()? {
        let (_, v) = row?;
        let ns = NamespaceConfig::decode(v.value())?;
        let fields = ns
            .schema
            .as_ref()
            .ok_or_else(|| invalid("namespace lacks lifecycle schema"))?;
        let mut version = LifecycleVersion::LegacyAccessV1;
        let mut revision = Generation::default();
        let mut last_in_origin = HashMap::new();
        if let Some(history) = policies.get(&ns.id.get()) {
            for (_, (receipt, t)) in history.range(..=fields.policy_revision) {
                if t.previous_revision != revision || t.from != version {
                    return Err(invalid("gap in namespace policy history"));
                }
                if last_in_origin
                    .insert(receipt.store_id, receipt.committed_generation)
                    .is_some_and(|previous| previous >= receipt.committed_generation)
                {
                    return Err(invalid(
                        "policy revisions contradict receipt order in their origin",
                    ));
                }
                revision = t.next_revision;
                version = t.to;
            }
        }
        if revision != fields.policy_revision || version != fields.lifecycle_version {
            return Err(invalid("namespace policy lacks transition evidence"));
        }
        for record in records.entry(ns.id.get()).or_default().iter() {
            let t = &record
                .schema
                .as_ref()
                .ok_or_else(|| invalid("record lacks lifecycle schema"))?
                .lifecycle;
            if t.policy_revision != revision || t.lifecycle_version != version {
                return Err(invalid("record policy does not match migration"));
            }
        }
        current_policy.insert(ns.id.get(), (version, revision));
    }
    if records.keys().any(|ns| !current_policy.contains_key(ns)) {
        return Err(invalid("record namespace missing"));
    }
    // Reverse exactly the recognized active operations. This replaces the old
    // assumption that every token increment was a namespace policy migration.
    for receipt in suffix.values().rev() {
        match &receipt.outcome {
            MutationOutcome::LifecyclePolicyMigrated { transition: t } => {
                let policy = current_policy
                    .get_mut(&t.namespace_id.get())
                    .ok_or_else(|| invalid("policy receipt namespace absent"))?;
                let rows = records.get_mut(&t.namespace_id.get()).unwrap();
                if *policy != (t.to, t.next_revision)
                    || t.record_count != rows.len() as u64
                    || t.record_token_digest != token_digest(rows, 0)?
                {
                    return Err(invalid("lifecycle affected-record proof mismatch"));
                }
                for r in rows {
                    let fields = &mut r.schema.as_mut().unwrap().lifecycle;
                    fields.record_generation = Generation::try_from(
                        fields
                            .record_generation
                            .get()
                            .checked_sub(1)
                            .ok_or_else(|| invalid("policy token underflow"))?
                            .to_string(),
                    )?;
                    fields.lifecycle_version = t.from;
                    fields.policy_revision = t.previous_revision;
                }
                *policy = (t.from, t.previous_revision);
            }
            MutationOutcome::LifecycleExposureRecorded { transition: t }
            | MutationOutcome::LifecycleFeedbackApplied { transition: t } => {
                if current_policy.get(&t.namespace_id.get())
                    != Some(&(t.lifecycle_version, t.policy_revision))
                {
                    return Err(invalid("native receipt is out of policy order"));
                }
                t.rewind(
                    records
                        .get_mut(&t.namespace_id.get())
                        .ok_or_else(|| invalid("native receipt namespace absent"))?,
                )?;
            }
            _ => return Err(invalid("unrecognized operation in lifecycle proof")),
        }
    }
    if schema.source.store_id == control.store_id
        && current_policy
            .values()
            .any(|p| *p != (LifecycleVersion::LegacyAccessV1, Generation::default()))
    {
        return Err(invalid(
            "active policy receipt has no matching namespace transition",
        ));
    }
    if schema.source.store_id == control.store_id
        && records.values().flatten().any(|r| {
            r.schema.as_ref().unwrap().lifecycle.record_generation != Generation::default()
        })
    {
        return Err(invalid("unproven record token before lifecycle suffix"));
    }
    Ok(())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LifecyclePreview {
    pub source: GenerationRef,
    pub namespace_id: NamespaceId,
    pub from: LifecycleVersion,
    pub to: LifecycleVersion,
    pub policy_revision: Generation,
    pub evaluated_at: i64,
    pub record_count: usize,
    pub full_to_summary: usize,
    pub summary_to_ghost: usize,
    pub ghost_to_delete: usize,
    pub missing_summary: usize,
}

/// Diagnostic only. Retained phase, content and all clocks remain unchanged.
/// The supplied global policy/enable flag must be the operator's intended config.
pub fn preview_lifecycle(
    source: &Path,
    options: &LifecycleMigrationOptions,
    clock: &dyn Clock,
    global: &DecayConfig,
    multiplier: f64,
    decay_enabled: bool,
) -> Result<LifecyclePreview, StorageError> {
    let snapshot = OfflineSnapshot::open(source)?;
    check_source(snapshot.control(), options)?;
    let ns = snapshot
        .namespaces()
        .iter()
        .find(|n| n.id == options.namespace_id)
        .ok_or(StorageError::NamespaceNotFound(options.namespace_id.get()))?;
    check_policy(ns, options)?;
    let policy = resolve_policy(ns, global, multiplier)
        .map_err(|e| StorageError::InvalidMutation(e.to_string()))?;
    let now = clock.now_millis();
    if chrono::DateTime::from_timestamp_millis(now).is_none() {
        return Err(StorageError::InvalidMutation(
            "unrepresentable preview clock".into(),
        ));
    }
    let mut graph = RelationshipGraph::new();
    let mut rows = Vec::new();
    let read = snapshot.metadata.begin_read()?;
    for row in read.open_table(META_TABLE)?.iter()? {
        let (_, bytes) = row?;
        let r = DiskRecord::from_bytes(bytes.value())?;
        graph
            .add_node(
                MemoryId::from_bytes(r.id),
                NamespaceId::new(r.namespace_id),
                r.phase,
                r.strength,
                r.vector_slot,
            )
            .map_err(|e| StorageError::InvalidControl(e.to_string()))?;
        if r.namespace_id == ns.id.get() {
            rows.push(r);
        }
    }
    snapshot.for_each_edge(|e| {
        graph
            .add_edge(e.source, e.target, e.edge_type, e.weight, e.auto_created)
            .map_err(|e| StorageError::InvalidControl(e.to_string()))?;
        Ok(())
    })?;
    let raw = |r: &DiskRecord, bonus| {
        let fields = &r.schema.as_ref().unwrap().lifecycle;
        project(
            r.stability,
            fields.decay_anchor_at,
            &policy,
            now.max(fields.lifecycle_updated_at),
            bonus,
        )
        .map_err(|e| StorageError::InvalidMutation(e.to_string()))
    };
    let mut donors = HashMap::new();
    for r in &rows {
        if matches!(r.phase, DecayPhase::Full | DecayPhase::Summary) {
            donors.insert(MemoryId::from_bytes(r.id), raw(r, 0.)?.strength);
        }
    }
    let mut result = LifecyclePreview {
        source: snapshot.control().generation_ref(),
        namespace_id: ns.id,
        from: ns.schema.as_ref().unwrap().lifecycle_version,
        to: options.target,
        policy_revision: options.expected_policy_revision,
        evaluated_at: now,
        record_count: rows.len(),
        full_to_summary: 0,
        summary_to_ghost: 0,
        ghost_to_delete: 0,
        missing_summary: 0,
    };
    for r in &rows {
        if !decay_enabled
            || policy.decay_rate_multiplier == 0.
            || r.is_permastore != 0
            || r.phase == DecayPhase::Tombstone
        {
            continue;
        }
        let bonus = if r.phase == DecayPhase::Ghost {
            0.
        } else {
            connection_bonus_with_strengths(
                MemoryId::from_bytes(r.id),
                &graph,
                &ActivationConfig::default(),
                &donors,
            )
        };
        let strength = raw(r, bonus)?.decay_strength;
        match r.phase {
            DecayPhase::Full if strength <= policy.config.phase_1_threshold => {
                if r.summary.is_empty() {
                    result.missing_summary += 1;
                } else {
                    result.full_to_summary += 1;
                }
            }
            DecayPhase::Summary if strength <= policy.config.phase_2_threshold => {
                result.summary_to_ghost += 1
            }
            DecayPhase::Ghost if strength <= policy.config.phase_3_threshold => {
                result.ghost_to_delete += 1
            }
            _ => {}
        }
    }
    Ok(result)
}

fn check_source(
    control: &ControlState,
    options: &LifecycleMigrationOptions,
) -> Result<(), StorageError> {
    options.context().validate()?;
    if control.control_schema_version != 2 {
        return Err(StorageError::InvalidMutation(
            "explicit canonical schema migration is required first".into(),
        ));
    }
    if control.store_id != options.expected_source.store_id {
        return Err(StorageError::StoreLineageMismatch);
    }
    if control.committed_generation != options.expected_source.generation {
        return Err(StorageError::GenerationConflict {
            expected: options.expected_source.generation.into(),
            actual: control.committed_generation.into(),
        });
    }
    Ok(())
}
fn check_policy(
    ns: &NamespaceConfig,
    options: &LifecycleMigrationOptions,
) -> Result<(), StorageError> {
    let schema = ns
        .schema
        .as_ref()
        .ok_or_else(|| invalid("namespace schema missing"))?;
    if schema.policy_revision != options.expected_policy_revision
        || schema.lifecycle_version == options.target
    {
        return Err(StorageError::InvalidMutation(
            "policy revision conflict or policy already selected".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Copied,
    BeforeCommit,
    Committed,
    Validated,
    Published,
}

pub fn migrate_lifecycle(
    source: &Path,
    destination: &Path,
    options: &LifecycleMigrationOptions,
) -> Result<WriteReceipt, StorageError> {
    migrate_at(source, destination, options, |_| Ok(()))
}

fn migrate_at(
    source: &Path,
    destination: &Path,
    options: &LifecycleMigrationOptions,
    mut boundary: impl FnMut(Stage) -> Result<(), StorageError>,
) -> Result<WriteReceipt, StorageError> {
    options.context().validate()?;
    let absolute = std::path::absolute(destination)?;
    let parent = absolute
        .parent()
        .ok_or(StorageError::InvalidPath)?
        .canonicalize()?;
    let target = parent.join(absolute.file_name().ok_or(StorageError::InvalidPath)?);
    let source = if source.exists() {
        source.canonicalize()?
    } else {
        std::path::absolute(source)?
    };
    if target.starts_with(&source) || source.starts_with(&target) {
        return Err(StorageError::InvalidPath);
    }
    let target_owner = StoreOwnership::acquire(&target)?;
    let target = &target_owner.path;
    let hash = options.hash(target)?;
    // Existing exact destination: read a disposable copy, never open it writable.
    if target.try_exists()? {
        let _lock = lock_directory(target)?;
        super::validation::canonical_files(target)?;
        let (_copy, db) = super::validation::inspect_database(&target.join("meta.db"))?;
        let tx = db.begin_write()?;
        return control::replay_or_check(&tx, &options.context(), hash)?.ok_or_else(|| {
            StorageError::InvalidMutation(
                "destination exists without this migration receipt".into(),
            )
        });
    }
    let owner = StoreOwnership::acquire(&source)?;
    let _source_lock = lock_directory(&owner.path)?;
    let files = super::validation::canonical_files(&source)?;
    let stage = tempfile::Builder::new()
        .prefix(".recalld-lifecycle-migration-")
        .tempdir_in(&parent)?;
    for file in files {
        let copied = stage.path().join(
            file.strip_prefix(&source)
                .map_err(|_| StorageError::InvalidPath)?,
        );
        std::fs::create_dir_all(copied.parent().ok_or(StorageError::InvalidPath)?)?;
        std::fs::copy(file, &copied)?;
        std::fs::File::open(&copied)?.sync_all()?;
        fsync_dir(copied.parent().unwrap())?;
    }
    boundary(Stage::Copied)?;
    let receipt = convert_staging(stage.path(), options, hash, &mut boundary)?;
    boundary(Stage::Committed)?;
    for file in super::validation::canonical_files(stage.path())? {
        std::fs::File::open(file)?.sync_all()?;
    }
    fsync_dir(stage.path())?;
    boundary(Stage::Validated)?;
    if target.try_exists()? {
        return Err(StorageError::InvalidMutation(
            "destination appeared during staging".into(),
        ));
    }
    std::fs::rename(stage.path(), target)?;
    fsync_dir(&parent).map_err(|e| {
        StorageError::RecoveryRequired(format!(
            "policy migration {} published at {}; reconcile this exact destination: {e}",
            receipt.operation_id,
            target.display()
        ))
    })?;
    boundary(Stage::Published)?;
    Ok(receipt)
}

fn convert_staging(
    path: &Path,
    options: &LifecycleMigrationOptions,
    hash: [u8; 32],
    boundary: &mut impl FnMut(Stage) -> Result<(), StorageError>,
) -> Result<WriteReceipt, StorageError> {
    let db = redb::Database::open(path.join("meta.db"))?;
    {
        let read = db.begin_read()?;
        let control =
            control::validate_existing(&read)?.ok_or_else(|| invalid("control missing"))?;
        check_source(&control, options)?;
        if super::metadata::text_generation::read_selection(&read)?.is_none() {
            return Err(invalid("lifecycle migration requires selected-text fence"));
        }
    }
    let tx = db.begin_write()?;
    if control::replay_or_check(&tx, &options.context(), hash)?.is_some() {
        return Err(StorageError::InvalidMutation(
            "source already contains this operation; reconcile its exact destination".into(),
        ));
    }
    let mut ns = {
        let table = tx.open_table(NAMESPACE_TABLE)?;
        let bytes = table
            .get(options.namespace_id.get())?
            .ok_or(StorageError::NamespaceNotFound(options.namespace_id.get()))?;
        NamespaceConfig::decode(bytes.value())?
    };
    check_policy(&ns, options)?;
    let schema = ns.schema.as_mut().unwrap();
    let from = schema.lifecycle_version;
    schema.policy_revision = schema.policy_revision.next()?;
    schema.lifecycle_version = options.target;
    let next_revision = schema.policy_revision;
    let mut records = Vec::new();
    {
        let mut table = tx.open_table(META_TABLE)?;
        for row in table.iter()? {
            let (_, bytes) = row?;
            let mut record = DiskRecord::from_bytes(bytes.value())?;
            if record.namespace_id == ns.id.get() {
                let fields = &mut record.schema.as_mut().unwrap().lifecycle;
                fields.lifecycle_version = options.target;
                fields.policy_revision = next_revision;
                fields.record_generation = fields.record_generation.next()?;
                records.push(record);
            }
        }
        for record in &records {
            table.insert(record.id.as_slice(), record.encode()?.as_slice())?;
        }
    }
    tx.open_table(NAMESPACE_TABLE)?
        .insert(ns.id.get(), control::encode(&ns)?.as_slice())?;
    let outcome = MutationOutcome::LifecyclePolicyMigrated {
        transition: PolicyTransition {
            namespace_id: ns.id,
            from,
            to: options.target,
            previous_revision: options.expected_policy_revision,
            next_revision,
            previous_generation: options.expected_source.generation,
            record_count: records.len() as u64,
            record_token_digest: token_digest(&records, 0)?,
        },
    };
    let receipt = control::record_receipt(&tx, &options.context(), hash, outcome)?;
    boundary(Stage::BeforeCommit)?;
    tx.commit()?;
    Ok(receipt)
}

#[cfg(test)]
mod tests;
