//! Owned-core accepted work. This is not a derived-view watermark or ReadView.
//!
//! Register before enqueue, register descendants before completing their parent,
//! and finish only after all effects owned by that ticket have been published.
//! Dropping a ticket without finishing retains unresolved work until owner reopen.
use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use uuid::Uuid;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WorkError {
    #[error("work admission is closed")]
    AdmissionClosed,
    #[error("work ticket belongs to another owned core or is no longer active")]
    InvalidTicket,
    #[error("work sequence exhausted")]
    SequenceExhausted,
    #[error("work drain deadline expired: {pending} pending, {unresolved} unresolved")]
    Deadline { pending: usize, unresolved: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkStatus {
    pub admission_open: bool,
    pub pending: usize,
    pub unresolved: usize,
}

#[derive(Default)]
struct State {
    closed: bool,
    next: u64,
    // false = executing, true = abandoned/unknown completion. No payloads.
    work: BTreeMap<u64, bool>,
}

struct Inner {
    store_id: Uuid,
    state: Mutex<State>,
    changed: Condvar,
}

/// Clone this handle from the already owned MetadataStore, not a second session.
#[derive(Clone)]
pub struct WorkTracker(Arc<Inner>);

impl WorkTracker {
    pub(crate) fn new(store_id: Uuid) -> Self {
        Self(Arc::new(Inner {
            store_id,
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        }))
    }

    pub fn store_id(&self) -> Uuid {
        self.0.store_id
    }

    pub fn status(&self) -> WorkStatus {
        let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::status_locked(&state)
    }

    #[cfg(test)]
    pub(crate) fn wait_until_closed(&self) {
        let limit = Instant::now() + std::time::Duration::from_secs(5);
        let mut state = self.0.state.lock().unwrap();
        while !state.closed {
            let left = limit.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "drain never closed admission");
            (state, _) = self.0.changed.wait_timeout(state, left).unwrap();
        }
    }

    fn status_locked(state: &State) -> WorkStatus {
        WorkStatus {
            admission_open: !state.closed,
            pending: state.work.len(),
            unresolved: state.work.values().filter(|unknown| **unknown).count(),
        }
    }

    /// A root must be accepted before its worker is queued or externally staged.
    pub fn begin(&self) -> Result<WorkTicket, WorkError> {
        self.admit(None)
    }

    pub(crate) fn admit(&self, parent: Option<&WorkTicket>) -> Result<WorkTicket, WorkError> {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(parent) = parent {
            if !Arc::ptr_eq(&self.0, &parent.tracker.0)
                || state.work.get(&parent.id) != Some(&false)
            {
                return Err(WorkError::InvalidTicket);
            }
        } else if state.closed {
            return Err(WorkError::AdmissionClosed);
        }
        state.next = state
            .next
            .checked_add(1)
            .ok_or(WorkError::SequenceExhausted)?;
        let id = state.next;
        state.work.insert(id, false);
        Ok(WorkTicket {
            tracker: self.clone(),
            id,
            parent: parent.map(|p| p.id),
            finished: false,
        })
    }

    pub(crate) fn check_ticket(&self, ticket: &WorkTicket) -> Result<(), WorkError> {
        let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if Arc::ptr_eq(&self.0, &ticket.tracker.0) && state.work.get(&ticket.id) == Some(&false) {
            Ok(())
        } else {
            Err(WorkError::InvalidTicket)
        }
    }

    /// Stop new roots, then wait for the transitive fixed point. No tracker lock
    /// is held while a worker executes, while waiting, or while accessing redb.
    /// The returned lease keeps admission closed; it does not certify storage
    /// readiness. The owner must separately validate canonical and derived work.
    pub fn close_and_drain(&self, deadline: Instant) -> Result<WorkFreeze, WorkError> {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return Err(WorkError::AdmissionClosed);
        }
        state.closed = true;
        self.0.changed.notify_all();
        loop {
            let now = Instant::now();
            if now >= deadline {
                let status = Self::status_locked(&state);
                state.closed = false;
                self.0.changed.notify_all();
                return Err(WorkError::Deadline {
                    pending: status.pending,
                    unresolved: status.unresolved,
                });
            }
            if state.work.is_empty() {
                return Ok(WorkFreeze {
                    tracker: self.clone(),
                    deadline,
                });
            }
            (state, _) = self
                .0
                .changed
                .wait_timeout(state, deadline.saturating_duration_since(now))
                .unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// Ticket IDs identify in-process work only, never canonical operation IDs or
/// generations. This type intentionally cannot be cloned or serialized.
pub struct WorkTicket {
    tracker: WorkTracker,
    id: u64,
    parent: Option<u64>,
    finished: bool,
}

impl WorkTicket {
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn parent_id(&self) -> Option<u64> {
        self.parent
    }
    pub fn child(&self) -> Result<Self, WorkError> {
        self.tracker.admit(Some(self))
    }
    /// Known failure with no remaining work may finish too; this records task
    /// completion, not mutation success. Unknown/cancelled work must not finish.
    pub fn finish(mut self) {
        let mut state = self
            .tracker
            .0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state.work.remove(&self.id);
        self.finished = true;
        self.tracker.0.changed.notify_all();
    }
}

impl Drop for WorkTicket {
    fn drop(&mut self) {
        if !self.finished {
            let mut state = self
                .tracker
                .0
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(unknown) = state.work.get_mut(&self.id) {
                *unknown = true;
            }
            self.tracker.0.changed.notify_all();
        }
    }
}

/// Admission remains closed for this lease's lifetime. Canonical capture must
/// also hold the owner's publication gate; this is never itself a snapshot.
pub struct WorkFreeze {
    tracker: WorkTracker,
    deadline: Instant,
}

impl WorkFreeze {
    pub(crate) fn check(&self, tracker: &WorkTracker) -> Result<(), WorkError> {
        if !Arc::ptr_eq(&self.tracker.0, &tracker.0) {
            return Err(WorkError::InvalidTicket);
        }
        let status = tracker.status();
        if Instant::now() >= self.deadline {
            return Err(WorkError::Deadline {
                pending: status.pending,
                unresolved: status.unresolved,
            });
        }
        if status.admission_open || status.pending != 0 {
            return Err(WorkError::InvalidTicket);
        }
        Ok(())
    }
}

impl Drop for WorkFreeze {
    fn drop(&mut self) {
        let mut state = self
            .tracker
            .0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state.closed = false;
        self.tracker.0.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    fn wait_closed(tracker: &WorkTracker) {
        tracker.wait_until_closed();
    }

    #[test]
    fn accepted_descendants_drain_after_close_without_a_zero_count_gap() {
        let tracker = WorkTracker::new(Uuid::new_v4());
        let parent = tracker.begin().unwrap();
        std::thread::scope(|scope| {
            let (send, recv) = mpsc::channel();
            let closing = &tracker;
            scope.spawn(move || {
                send.send(closing.close_and_drain(deadline()).unwrap())
                    .unwrap()
            });
            wait_closed(&tracker);
            assert!(matches!(tracker.begin(), Err(WorkError::AdmissionClosed)));
            let child = parent.child().unwrap();
            assert_eq!(child.parent_id(), Some(parent.id()));
            parent.finish();
            let grandchild = child.child().unwrap();
            child.finish();
            assert_eq!(tracker.status().pending, 1);
            assert!(recv.try_recv().is_err());
            grandchild.finish();
            let frozen = recv.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(tracker.status().pending, 0);
            assert!(matches!(tracker.begin(), Err(WorkError::AdmissionClosed)));
            frozen.check(&tracker).unwrap();
            drop(frozen);
            tracker.begin().unwrap().finish();
        });
    }

    #[test]
    fn abandoned_ancestor_remains_unresolved_after_its_running_child_finishes() {
        let tracker = WorkTracker::new(Uuid::new_v4());
        let parent = tracker.begin().unwrap();
        let child = parent.child().unwrap();
        std::thread::scope(|scope| {
            let (running, started) = mpsc::channel();
            let (release, released) = mpsc::channel();
            let worker = scope.spawn(move || {
                running.send(()).unwrap();
                released.recv().unwrap();
                child.finish();
            });
            started.recv_timeout(Duration::from_secs(5)).unwrap();
            drop(parent);
            assert_eq!(tracker.status().pending, 2);
            assert_eq!(tracker.status().unresolved, 1);
            release.send(()).unwrap();
            worker.join().unwrap();
        });
        assert!(matches!(
            tracker.close_and_drain(Instant::now()),
            Err(WorkError::Deadline {
                pending: 1,
                unresolved: 1
            })
        ));
        assert!(tracker.status().admission_open);
        assert_eq!(tracker.status().unresolved, 1);
    }

    #[test]
    fn finished_parent_and_foreign_core_with_same_uuid_cannot_admit_children() {
        let id = Uuid::new_v4();
        let tracker = WorkTracker::new(id);
        let foreign = WorkTracker::new(id);
        let parent = tracker.begin().unwrap();
        assert!(matches!(
            foreign.admit(Some(&parent)),
            Err(WorkError::InvalidTicket)
        ));
        // A public caller cannot reuse the consumed parent. Exercise the runtime
        // check too, so a future internal carrier cannot resurrect its ID.
        let stale = WorkTicket {
            tracker: tracker.clone(),
            id: parent.id(),
            parent: None,
            finished: true,
        };
        parent.finish();
        assert!(matches!(stale.child(), Err(WorkError::InvalidTicket)));
        let frozen = tracker.close_and_drain(deadline()).unwrap();
        assert_eq!(frozen.check(&foreign), Err(WorkError::InvalidTicket));
    }

    #[test]
    fn timeout_reopens_then_concurrent_reclose_keeps_admission_closed() {
        let tracker = WorkTracker::new(Uuid::new_v4());
        let parent = tracker.begin().unwrap();
        assert!(matches!(
            tracker.close_and_drain(Instant::now()),
            Err(WorkError::Deadline {
                pending: 1,
                unresolved: 0
            })
        ));
        assert!(tracker.status().admission_open);
        std::thread::scope(|scope| {
            let (send, recv) = mpsc::channel();
            let closing = &tracker;
            scope.spawn(move || {
                send.send(closing.close_and_drain(deadline()).unwrap())
                    .unwrap()
            });
            wait_closed(&tracker);
            // A rejected second closer must not reopen the first closer's gate.
            assert!(matches!(
                tracker.close_and_drain(Instant::now()),
                Err(WorkError::AdmissionClosed)
            ));
            assert!(matches!(tracker.begin(), Err(WorkError::AdmissionClosed)));
            let child = parent.child().unwrap();
            parent.finish();
            assert!(recv.try_recv().is_err());
            child.finish();
            let frozen = recv.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(!tracker.status().admission_open);
            drop(frozen);
            assert!(tracker.status().admission_open);
        });
    }

    #[test]
    fn spurious_notifications_do_not_complete_pending_work_or_extend_deadline() {
        let tracker = WorkTracker::new(Uuid::new_v4());
        let parent = tracker.begin().unwrap();
        std::thread::scope(|scope| {
            let limit = Instant::now() + Duration::from_millis(500);
            let (send, recv) = mpsc::channel();
            let closing = &tracker;
            scope.spawn(move || send.send(closing.close_and_drain(limit)).unwrap());
            wait_closed(&tracker);
            // Notification without a state change is explicitly not completion.
            tracker.0.changed.notify_all();
            assert_eq!(tracker.status().pending, 1);
            assert!(matches!(
                recv.recv_timeout(Duration::from_secs(5)).unwrap(),
                Err(WorkError::Deadline {
                    pending: 1,
                    unresolved: 0
                })
            ));
            assert!(Instant::now() >= limit);
            assert!(tracker.status().admission_open);
        });
        parent.finish();
        assert!(matches!(
            tracker.close_and_drain(Instant::now() - Duration::from_secs(1)),
            Err(WorkError::Deadline {
                pending: 0,
                unresolved: 0
            })
        ));
        let expired = WorkFreeze {
            tracker: tracker.clone(),
            deadline: Instant::now(),
        };
        assert!(matches!(
            expired.check(&tracker),
            Err(WorkError::Deadline { .. })
        ));
    }

    #[test]
    fn sequence_exhaustion_never_wraps_or_registers_work() {
        let tracker = WorkTracker::new(Uuid::new_v4());
        tracker.0.state.lock().unwrap().next = u64::MAX;
        assert!(matches!(tracker.begin(), Err(WorkError::SequenceExhausted)));
        assert_eq!(tracker.status().pending, 0);
    }
}
