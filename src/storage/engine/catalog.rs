use super::*;
use crate::storage::{
    catalog::{NamespacePolicy, boundary},
    control::{MutationContext, WriteReceipt},
    fsync::fsync_dir,
};

impl RedbStorageEngine {
    /// Replay precedes CAS, directory inspection and vector preparation. The
    /// receipt is canonical metadata evidence, never whole-store readiness.
    pub fn create_namespace_with_receipt(
        &mut self,
        ctx: &MutationContext,
        config: &NamespaceConfig,
    ) -> Result<WriteReceipt, StorageError> {
        self.ensure_available()?;
        let mut prepared = None;
        let mut payload_published = false;
        let root = &self.db_path;
        let result = self.meta_store.create_catalog(ctx, config, |id| {
            boundary("before_stage")?;
            let target = root.join(&config.name);
            // Never adopt or clean an existing directory, including broken links.
            match std::fs::symlink_metadata(&target) {
                Ok(_) => {
                    return Err(StorageError::InvalidMutation(
                        "namespace directory already exists".into(),
                    ));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            let stage = tempfile::Builder::new()
                .prefix(".recalld-catalog-")
                .tempdir_in(root)?;
            let mut vector =
                crate::storage::VectorStore::open(stage.path(), config.embedding_dim as usize)?;
            vector.sync()?;
            fsync_dir(stage.path())?;
            boundary("after_stage")?;
            boundary("before_payload_publish")?;
            rename_new_directory(stage.path(), &target)?;
            payload_published = true;
            // The final directory is deliberately never compensating-deleted:
            // a failed/uncertain commit is reconciled by reopen; unreferenced
            // final directories are quarantined by existing orphan recovery.
            fsync_dir(root)?;
            vector.relocate_path(&target);
            boundary("after_payload_publish")?;
            prepared = Some((id, vector));
            Ok(())
        });
        let receipt = result.map_err(|error| {
            if payload_published {
                self.meta_store.mark_unavailable();
            }
            error
        })?;
        if let Some((id, vector)) = prepared {
            let publish = (|| {
                boundary("before_handle_publish")?;
                self.vector_manager.install_prepared(id, vector);
                boundary("after_handle_publish")
            })();
            publish.map_err(|source| {
                self.meta_store.mark_unavailable();
                StorageError::CommittedEffectsPending {
                    operation_id: receipt.operation_id,
                    source: Box::new(source),
                }
            })?;
        }
        Ok(receipt)
    }

    pub fn update_namespace_policy_with_receipt(
        &self,
        ctx: &MutationContext,
        policy: &NamespacePolicy,
    ) -> Result<WriteReceipt, StorageError> {
        self.ensure_available()?;
        self.meta_store
            .update_namespace_policy_with_receipt(ctx, policy)
    }
}

/// Publish a prepared directory without replacing even an empty existing one.
/// The supported local platforms already provide an atomic no-replace rename.
fn rename_new_directory(source: &Path, target: &Path) -> Result<(), StorageError> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::os::unix::ffi::OsStrExt;
        let source = std::ffi::CString::new(source.as_os_str().as_bytes())
            .map_err(|_| StorageError::InvalidPath)?;
        let target = std::ffi::CString::new(target.as_os_str().as_bytes())
            .map_err(|_| StorageError::InvalidPath)?;
        // SAFETY: both C strings are NUL-terminated and live across the syscall.
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                target.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(target_os = "macos")]
        let result = unsafe {
            libc::renameatx_np(
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                target.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (source, target);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "atomic no-replace directory publication unavailable",
        )
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::catalog::FAILURE;
    use crate::storage::control::MutationOutcome;
    fn config(name: &str) -> NamespaceConfig {
        let mut c = NamespaceConfig::default_namespace(123);
        c.name = name.into();
        c.embedding_dim = 4;
        c
    }
    fn context(e: &RedbStorageEngine, key: &str, scope: MutationScope) -> MutationContext {
        let state = e.control_state().unwrap();
        MutationContext {
            store_id: Some(state.store_id),
            scope,
            idempotency_key: Some(key.into()),
            expected_generation: Some(state.committed_generation),
            contract_version: 1,
        }
    }
    fn created(r: &WriteReceipt) -> NamespaceId {
        let MutationOutcome::NamespaceCreated { namespace_id, .. } = r.outcome else {
            panic!("wrong outcome")
        };
        namespace_id
    }
    fn fault<T>(point: &'static str, f: impl FnOnce() -> T) -> T {
        FAILURE.with(|v| v.set(Some(point)));
        let result = f();
        FAILURE.with(|v| v.set(None));
        result
    }

    #[test]
    fn catalog_create_replays_before_cas_and_filesystem_and_never_reuses_ids() {
        let root = tempfile::tempdir().unwrap();
        let mut e = RedbStorageEngine::open(root.path()).unwrap();
        let ctx = context(&e, "create", MutationScope::Catalog);
        let c = config("notes");
        let receipt = e.create_namespace_with_receipt(&ctx, &c).unwrap();
        let id = created(&receipt);
        assert_eq!(
            receipt.committed_generation,
            ctx.expected_generation.unwrap().next().unwrap()
        );
        assert!(root.path().join("notes/vectors.dat").is_file());
        let mut retry = c.clone();
        retry.created_at = 456;
        retry.id = NamespaceId::new(999);
        let replay = fault("before_stage", || {
            e.create_namespace_with_receipt(&ctx, &retry)
        })
        .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.operation_id, receipt.operation_id);
        assert_eq!(replay.outcome, receipt.outcome);
        let before = e.control_state().unwrap();
        let mut other = c.clone();
        other.embedding_dim = 5;
        assert!(matches!(
            e.create_namespace_with_receipt(&ctx, &other),
            Err(StorageError::IdempotencyConflict)
        ));
        let stale = MutationContext {
            idempotency_key: Some("new".into()),
            ..ctx.clone()
        };
        assert!(matches!(
            e.create_namespace_with_receipt(&stale, &config("stale")),
            Err(StorageError::GenerationConflict { .. })
        ));
        assert!(!root.path().join("stale").exists());
        assert_eq!(e.control_state().unwrap(), before);
        e.purge_namespace(id).unwrap();
        let replacement = e.create_namespace(&config("notes")).unwrap();
        assert!(replacement.get() > id.get());
        let state = e.control_state().unwrap();
        assert_eq!(
            created(
                &fault("before_stage", || e
                    .create_namespace_with_receipt(&ctx, &retry))
                .unwrap()
            ),
            id
        );
        assert_eq!(e.control_state().unwrap(), state);
        drop(e);
        let e = RedbStorageEngine::open(root.path()).unwrap();
        assert_eq!(
            e.lookup_operation(
                receipt.store_id,
                MutationScope::Catalog,
                receipt.operation_id
            )
            .unwrap(),
            Some(receipt)
        );
    }

    #[test]
    fn catalog_policy_is_typed_atomic_replayable_and_identity_preserving() {
        let root = tempfile::tempdir().unwrap();
        let mut e = RedbStorageEngine::open(root.path()).unwrap();
        let id = e.create_namespace(&config("policy")).unwrap();
        let original = e.get_namespace(id).unwrap().unwrap();
        let mut policy = NamespacePolicy::from(&original);
        policy.initial_stability = 8.;
        policy.decay_rate_multiplier = Some(0.);
        let ctx = context(&e, "policy", MutationScope::Namespace(id));
        let r = e
            .update_namespace_policy_with_receipt(&ctx, &policy)
            .unwrap();
        assert_eq!(
            r.committed_generation,
            ctx.expected_generation.unwrap().next().unwrap()
        );
        let now = e.get_namespace(id).unwrap().unwrap();
        assert_eq!(now.initial_stability, 8.);
        assert_eq!(now.id, original.id);
        assert_eq!(now.name, original.name);
        assert_eq!(now.created_at, original.created_at);
        assert_eq!(now.embedding_dim, original.embedding_dim);
        assert!(now.schema.is_none());
        assert!(
            e.update_namespace_policy_with_receipt(&ctx, &policy)
                .unwrap()
                .replayed
        );
        let mut bad = policy.clone();
        bad.initial_stability = f32::NAN;
        assert!(e.update_namespace_policy_with_receipt(&ctx, &bad).is_err());
        bad = policy.clone();
        bad.initial_stability = 9.;
        assert!(matches!(
            e.update_namespace_policy_with_receipt(&ctx, &bad),
            Err(StorageError::IdempotencyConflict)
        ));
        let next = context(&e, "failpolicy", MutationScope::Namespace(id));
        let before = e.control_state().unwrap();
        assert!(
            fault("before_metadata_commit", || e
                .update_namespace_policy_with_receipt(&next, &bad))
            .is_err()
        );
        assert_eq!(e.control_state().unwrap(), before);
        assert_eq!(e.get_namespace(id).unwrap().unwrap().initial_stability, 8.);
        for field in ["name", "embeddingDim", "schema", "lifecycleVersion"] {
            let mut json = serde_json::to_value(&policy).unwrap();
            json[field] = serde_json::json!("forbidden");
            assert!(serde_json::from_value::<NamespacePolicy>(json).is_err());
        }
        e.purge_namespace(id).unwrap();
        assert!(
            e.update_namespace_policy_with_receipt(&ctx, &policy)
                .unwrap()
                .replayed
        );
        drop(e);
        let e = RedbStorageEngine::open(root.path()).unwrap();
        assert_eq!(
            e.lookup_operation(r.store_id, ctx.scope, r.operation_id)
                .unwrap(),
            Some(r)
        );
    }

    #[test]
    fn catalog_faults_preserve_preexisting_directories_and_reconcile_own_publication() {
        for point in [
            "before_stage",
            "after_stage",
            "before_payload_publish",
            "after_payload_publish",
            "before_metadata_commit",
            "after_metadata_commit",
            "before_handle_publish",
            "after_handle_publish",
            "commit_unknown",
        ] {
            let root = tempfile::tempdir().unwrap();
            let mut e = RedbStorageEngine::open(root.path()).unwrap();
            let ctx = context(&e, "fault", MutationScope::Catalog);
            let before = e.control_state().unwrap();
            let result = fault(point, || {
                e.create_namespace_with_receipt(&ctx, &config("fault"))
            });
            assert!(result.is_err(), "{point}");
            let committed = matches!(
                point,
                "after_metadata_commit"
                    | "before_handle_publish"
                    | "after_handle_publish"
                    | "commit_unknown"
            );
            if point == "commit_unknown" {
                assert!(matches!(
                    result,
                    Err(StorageError::CommitOutcomeUnknown { .. })
                ));
            }
            if !matches!(
                point,
                "before_stage" | "after_stage" | "before_payload_publish"
            ) {
                assert!(e.control_state().is_err(), "{point}");
            }
            drop(e);
            let mut e = RedbStorageEngine::open(root.path()).unwrap();
            let observed = e.control_state().unwrap();
            assert_eq!(
                observed.committed_generation.get(),
                before.committed_generation.get() + u64::from(committed),
                "{point}"
            );
            let replay = e
                .create_namespace_with_receipt(&ctx, &config("fault"))
                .unwrap();
            assert_eq!(replay.replayed, committed, "{point}");
            assert!(e.get_namespace(created(&replay)).unwrap().is_some());
        }
        for nonempty in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let mut e = RedbStorageEngine::open(root.path()).unwrap();
            let dir = root.path().join("existing");
            std::fs::create_dir(&dir).unwrap();
            if nonempty {
                std::fs::write(dir.join("sentinel"), b"keep").unwrap();
            }
            let before = e.control_state().unwrap();
            assert!(e.create_namespace(&config("existing")).is_err());
            assert!(dir.is_dir());
            assert_eq!(e.control_state().unwrap(), before);
            if nonempty {
                assert_eq!(std::fs::read(dir.join("sentinel")).unwrap(), b"keep");
            }
            let stage = tempfile::tempdir_in(root.path()).unwrap();
            assert!(rename_new_directory(stage.path(), &dir).is_err());
            assert!(dir.is_dir());
        }
    }

    #[test]
    fn catalog_process_child() {
        let Ok(path) = std::env::var("RECALLD_CATALOG_STORE") else {
            return;
        };
        let mut e = RedbStorageEngine::open(path).unwrap();
        let ctx = context(&e, "crash", MutationScope::Catalog);
        e.create_namespace_with_receipt(&ctx, &config("crash"))
            .unwrap();
        panic!("crash boundary not reached");
    }

    #[test]
    fn catalog_crash_boundaries_reopen_to_absent_or_receipted_namespace() {
        for point in [
            "before_stage",
            "after_stage",
            "after_payload_publish",
            "before_metadata_commit",
            "after_metadata_commit",
            "before_handle_publish",
            "after_handle_publish",
        ] {
            let root = tempfile::tempdir().unwrap();
            let e = RedbStorageEngine::open(root.path()).unwrap();
            let ctx = context(&e, "crash", MutationScope::Catalog);
            drop(e);
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "storage::engine::catalog::tests::catalog_process_child",
                    "--nocapture",
                ])
                .env("RECALLD_CATALOG_STORE", root.path())
                .env("RECALLD_CATALOG_CRASH", point)
                .output()
                .unwrap();
            assert_eq!(
                child.status.code(),
                Some(89),
                "{point}: {}",
                String::from_utf8_lossy(&child.stderr)
            );
            let mut e = RedbStorageEngine::open(root.path()).unwrap();
            let committed = matches!(
                point,
                "after_metadata_commit" | "before_handle_publish" | "after_handle_publish"
            );
            assert_eq!(
                e.get_namespace_by_name("crash").unwrap().is_some(),
                committed,
                "{point}"
            );
            let r = e
                .create_namespace_with_receipt(&ctx, &config("crash"))
                .unwrap();
            assert_eq!(r.replayed, committed, "{point}");
            let id = created(&r);
            assert!(e.vector_manager.get(id).is_some());
            assert!(e.meta_store.text_selection().unwrap().is_some());
        }
    }

    #[test]
    fn catalog_invalid_inputs_and_lineage_never_allocate() {
        let root = tempfile::tempdir().unwrap();
        let mut e = RedbStorageEngine::open(root.path()).unwrap();
        let before = e.control_state().unwrap();
        for dim in [0, 8193, u32::MAX] {
            let mut c = config("bad");
            c.embedding_dim = dim;
            assert!(e.create_namespace(&c).is_err());
        }
        let mut c = config("bad");
        c.desired_retention = f32::INFINITY;
        assert!(e.create_namespace(&c).is_err());
        c = config("bad");
        c.phase_thresholds.full_to_summary = f32::NAN;
        assert!(e.create_namespace(&c).is_err());
        let mut ctx = context(&e, "lineage", MutationScope::Catalog);
        ctx.store_id = Some(uuid::Uuid::new_v4());
        assert!(matches!(
            e.create_namespace_with_receipt(&ctx, &config("bad")),
            Err(StorageError::StoreLineageMismatch)
        ));
        assert_eq!(e.control_state().unwrap(), before);
        assert!(!root.path().join("bad").exists());
    }
}
