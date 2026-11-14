//! Metadata publication admission. This lease does not certify other files,
//! derived generations, queued consumers, clocks, RNGs or behavioral caches.
use super::*;
use crate::storage::control::GenerationRef;
use crate::storage::purge::PurgeStage;
use crate::storage::work::WorkFreeze;
use std::sync::RwLockWriteGuard;
use std::sync::atomic::Ordering;

/// Covers a synchronous mutation from admission through canonical publication.
/// An uncertain return/panic retains unresolved work, in addition to the durable
/// recovery manifest. Known errors may finish: descendants have their own tickets.
pub(crate) struct MutationGuard<'a> {
    store: &'a MetadataStore,
    publication: Option<RwLockWriteGuard<'a, ()>>,
    ticket: Option<WorkTicket>,
}

impl MutationGuard<'_> {
    pub(crate) fn check(&self, store: &MetadataStore) -> Result<(), StorageError> {
        store
            .work_tracker
            .check_ticket(self.ticket.as_ref().unwrap())?;
        Ok(())
    }
}

impl Drop for MutationGuard<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.store.mark_unavailable();
        }
        // Publication must be released before the pending count reaches zero.
        drop(self.publication.take());
        let ticket = self.ticket.take().unwrap();
        if !self.store.unavailable.load(Ordering::Acquire) {
            ticket.finish();
        }
    }
}

/// A pinned metadata generation with no durable effects or active purge. This
/// is deliberately NOT shared readiness, a ReadView, or a snapshot permission.
/// Whole-store capture still requires all native writers and derived consumers
/// to participate, plus frozen clock/config/RNG/cache state.
pub struct MetadataFreeze<'a> {
    generation: GenerationRef,
    // Field order releases publication before reopening root admission.
    _publication: RwLockWriteGuard<'a, ()>,
    _admission: WorkFreeze,
}

impl MetadataFreeze<'_> {
    pub fn generation(&self) -> GenerationRef {
        self.generation
    }
}

impl MetadataStore {
    /// Clone under a brief outer storage read lock, then release that lock
    /// BEFORE close_and_drain. Workers need the outer lock to finish.
    pub fn work_tracker(&self) -> WorkTracker {
        self.work_tracker.clone()
    }

    pub(crate) fn mutation_guard(&self) -> Result<MutationGuard<'_>, StorageError> {
        self.mutation_guard_with_work(None)
    }

    pub(crate) fn mutation_guard_with_work(
        &self,
        parent: Option<&WorkTicket>,
    ) -> Result<MutationGuard<'_>, StorageError> {
        let guard = self.publication_guard(parent)?;
        // Recheck after waiting: the preceding writer may have failed.
        self.ensure_available()?;
        Ok(guard)
    }

    // Recovery and purge choose their narrower availability checks, but neither
    // may bypass admission. Register before waiting for the publication lock.
    pub(crate) fn publication_guard(
        &self,
        parent: Option<&WorkTicket>,
    ) -> Result<MutationGuard<'_>, StorageError> {
        let ticket = self.work_tracker.admit(parent)?;
        let publication = self.publication.write().unwrap_or_else(|e| e.into_inner());
        let guard = MutationGuard {
            store: self,
            publication: Some(publication),
            ticket: Some(ticket),
        };
        if self.unavailable.load(Ordering::Acquire) {
            return Err(StorageError::RecoveryRequired(
                "store handle unavailable; reopen for recovery".into(),
            ));
        }
        Ok(guard)
    }

    pub(crate) fn canonical_effects_guard(
        &self,
        parent: Option<&WorkTicket>,
    ) -> Result<MutationGuard<'_>, StorageError> {
        // Constructor replay may have canonical_pending/purge_pending set.
        self.publication_guard(parent)
    }

    /// Call only once the synchronous worker has returned a known outcome.
    /// Never use this on async cancellation or JoinError. Durable pending work
    /// remains independently checked by capture even when execution has ended.
    pub(crate) fn finish_work(&self, ticket: WorkTicket) {
        if !self.unavailable.load(Ordering::Acquire) {
            ticket.finish();
        }
    }

    pub fn freeze_metadata(
        &self,
        admission: WorkFreeze,
    ) -> Result<MetadataFreeze<'_>, StorageError> {
        admission.check(&self.work_tracker)?;
        // No unbounded wait after the drain deadline. A concurrent read can
        // make capture busy; the caller must retry within its original deadline.
        let publication = self.publication.try_write().map_err(|_| {
            StorageError::RecoveryRequired("metadata publication busy or poisoned".into())
        })?;
        self.ensure_available()?;
        // No input can be invented from a payload-free batch manifest. A
        // stopped executor is not proof that its durable children completed.
        let pending = self.pending_batch_items_checked(|| {
            admission.check(&self.work_tracker)?;
            Ok(())
        })?;
        if pending != 0 {
            return Err(StorageError::RecoveryRequired(format!(
                "{pending} durable batch items require matching caller resubmission"
            )));
        }
        let read = self.db.begin_read()?;
        let state = control::validate_existing(&read)?
            .ok_or_else(|| StorageError::InvalidControl("missing control state".into()))?;
        let effects = super::effects::validate_existing(&read)?;
        let purges = super::purge::validate_existing(&read, Some(&state))?;
        if !effects.is_empty()
            || self.derived_dirty.load(Ordering::SeqCst)
            || purges.iter().any(|p| p.stage != PurgeStage::Complete)
        {
            return Err(StorageError::RecoveryRequired(
                "durable effects or purge work pending".into(),
            ));
        }
        let generation = state.generation_ref();
        // Include the time spent checking durable manifests in the deadline.
        admission.check(&self.work_tracker)?;
        Ok(MetadataFreeze {
            generation,
            _publication: publication,
            _admission: admission,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::work::WorkError;
    use std::time::{Duration, Instant};

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    #[test]
    fn metadata_capture_blocks_actual_writers_and_checks_owned_core() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetadataStore::open(&dir.path().join("meta.db")).unwrap();
        let tracker = store.work_tracker();
        let before = store.control_state().unwrap().generation_ref();
        let freeze = store
            .freeze_metadata(tracker.close_and_drain(deadline()).unwrap())
            .unwrap();
        assert_eq!(freeze.generation(), before);
        assert!(matches!(
            store.create_namespace(&NamespaceConfig::default_namespace(0)),
            Err(StorageError::Work(WorkError::AdmissionClosed))
        ));
        assert!(matches!(
            store.purge_guard(None),
            Err(StorageError::Work(WorkError::AdmissionClosed))
        ));
        assert!(matches!(
            store.canonical_effects_guard(None),
            Err(StorageError::Work(WorkError::AdmissionClosed))
        ));
        assert!(matches!(
            store.acknowledge_rebuilt_effects_with_work(None),
            Err(StorageError::Work(WorkError::AdmissionClosed))
        ));
        assert_eq!(store.control_state().unwrap().generation_ref(), before);
        drop(freeze);
        store
            .create_namespace(&NamespaceConfig::default_namespace(0))
            .unwrap();

        let foreign = WorkTracker::new(tracker.store_id());
        assert!(matches!(
            store.freeze_metadata(foreign.close_and_drain(deadline()).unwrap()),
            Err(StorageError::Work(WorkError::InvalidTicket))
        ));
        assert!(foreign.status().admission_open);
        assert!(tracker.status().admission_open);
    }

    #[test]
    fn accepted_writer_can_publish_and_ack_after_close_before_capture() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetadataStore::open(&dir.path().join("meta.db")).unwrap();
        let tracker = store.work_tracker();
        let parent = tracker.begin().unwrap();
        let child = parent.child().unwrap();
        std::thread::scope(|scope| {
            let closing = &tracker;
            let drain = scope.spawn(move || closing.close_and_drain(deadline()).unwrap());
            tracker.wait_until_closed();
            assert!(matches!(
                store.mutation_guard(),
                Err(StorageError::Work(WorkError::AdmissionClosed))
            ));
            // These are actual mutation/recovery gates, not tracker-only children.
            drop(store.mutation_guard_with_work(Some(&child)).unwrap());
            drop(store.canonical_effects_guard(Some(&child)).unwrap());
            drop(store.purge_guard(Some(&child)).unwrap());
            store
                .acknowledge_rebuilt_effects_with_work(Some(&child))
                .unwrap();
            store.finish_work(child);
            store.finish_work(parent);
            let freeze = store.freeze_metadata(drain.join().unwrap()).unwrap();
            assert_eq!(
                freeze.generation(),
                store.control_state().unwrap().generation_ref()
            );
        });
    }

    #[test]
    fn actual_writer_waiting_for_publication_is_already_admitted() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetadataStore::open(&dir.path().join("meta.db")).unwrap();
        let tracker = store.work_tracker();
        let reading = store.publication.read().unwrap();
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                store
                    .create_namespace(&NamespaceConfig::default_namespace(0))
                    .unwrap()
            });
            let limit = deadline();
            while tracker.status().pending == 0 {
                assert!(
                    Instant::now() < limit,
                    "writer did not register before publication wait"
                );
                std::thread::park_timeout(Duration::from_millis(1));
            }
            let closing = &tracker;
            let drain = scope.spawn(move || closing.close_and_drain(deadline()).unwrap());
            tracker.wait_until_closed();
            drop(reading);
            let id = writer.join().unwrap();
            let capture = store.freeze_metadata(drain.join().unwrap()).unwrap();
            assert!(store.get_namespace(id).unwrap().is_some());
            assert_eq!(capture.generation().generation.get(), 1);
            assert_eq!(tracker.status().pending, 0);
        });
    }

    #[test]
    fn unavailable_guard_retains_unresolved_and_clean_failure_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetadataStore::open(&dir.path().join("meta.db")).unwrap();
        let tracker = store.work_tracker();
        store
            .create_namespace(&NamespaceConfig::default_namespace(0))
            .unwrap();
        assert!(
            store
                .create_namespace(&NamespaceConfig::default_namespace(0))
                .is_err()
        );
        assert_eq!(tracker.status().pending, 0);
        {
            let _guard = store.mutation_guard().unwrap();
            store.mark_unavailable();
        }
        assert_eq!(tracker.status().pending, 1);
        assert_eq!(tracker.status().unresolved, 1);
        assert!(matches!(
            tracker.close_and_drain(Instant::now()),
            Err(WorkError::Deadline {
                pending: 1,
                unresolved: 1
            })
        ));
    }

    #[test]
    fn capture_publication_contention_fails_without_reopening_another_lease() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetadataStore::open(&dir.path().join("meta.db")).unwrap();
        let tracker = store.work_tracker();
        let reading = store.publication.read().unwrap();
        assert!(
            store
                .freeze_metadata(tracker.close_and_drain(deadline()).unwrap())
                .is_err()
        );
        assert!(tracker.status().admission_open);
        drop(reading);
        let frozen = store
            .freeze_metadata(tracker.close_and_drain(deadline()).unwrap())
            .unwrap();
        assert!(matches!(
            tracker.close_and_drain(Instant::now()),
            Err(WorkError::AdmissionClosed)
        ));
        assert!(!tracker.status().admission_open);
        drop(frozen);
    }

    #[test]
    fn capture_checks_durable_purge_even_if_volatile_pending_flag_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetadataStore::open(&dir.path().join("meta.db")).unwrap();
        let mut config = NamespaceConfig::default_namespace(0);
        config.name = "drain-purge".into();
        let ns = store.create_namespace(&config).unwrap();
        store
            .begin_purge(&MutationContext::unkeyed(ns), true)
            .unwrap();
        // Deliberately corrupt only the accelerator, not the canonical manifest.
        store.purge_pending.store(false, Ordering::SeqCst);
        let tracker = store.work_tracker();
        assert_eq!(tracker.status().pending, 0);
        let admission = tracker.close_and_drain(deadline()).unwrap();
        assert!(store.freeze_metadata(admission).is_err());
        assert!(tracker.status().admission_open);
    }

    #[tokio::test]
    async fn capture_checks_durable_effects_even_if_dirty_flag_is_stale() {
        use crate::storage::insertion::InsertMemory;
        let fixture = crate::test_support::Fixture::new();
        let mut engine = fixture.storage.write().unwrap();
        let receipt = engine
            .insert_memory_with_receipt(
                &MutationContext::unkeyed(NamespaceId::new(1)),
                &InsertMemory {
                    id: None,
                    summary: "durable unfinished publication".into(),
                    full_text: None,
                    tags: vec![],
                    initial_stability: None,
                    created_at: None,
                    embedding: Some(vec![1., 0., 0., 0.]),
                    parent_id: None,
                    supersedes: None,
                },
                &[],
                123,
            )
            .unwrap();
        let store = engine.meta_store();
        store.derived_dirty.store(false, Ordering::SeqCst);
        assert_eq!(
            store.pending_effects().unwrap()[0].receipt.operation_id,
            receipt.operation_id
        );
        let tracker = store.work_tracker();
        assert_eq!(tracker.status().pending, 0);
        let admission = tracker.close_and_drain(deadline()).unwrap();
        assert!(store.freeze_metadata(admission).is_err());
        assert!(tracker.status().admission_open);
    }

    #[test]
    fn panic_retains_unresolved_work_and_poisoned_owner_cannot_capture() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetadataStore::open(&dir.path().join("meta.db")).unwrap();
        let tracker = store.work_tracker();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = store.mutation_guard().unwrap();
            panic!("injected unknown writer outcome");
        }));
        assert!(result.is_err());
        assert_eq!(tracker.status().unresolved, 1);
        assert!(store.ensure_available().is_err());
        assert!(tracker.close_and_drain(Instant::now()).is_err());
    }
}
