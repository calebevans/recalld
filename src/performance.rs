//! Internal opt-in performance events. No payloads, memory identities or readiness claims.
//! Enable only the `recalld::performance=trace` target. Disabled timers do not
//! sample clocks or allocate. Sink overhead remains in caller-observed latency.
//! Ordinary writer I/O errors cannot replace domain outcomes. Subscriber panics
//! are not caught: no unwind-safety guarantee is made for arbitrary subscribers.
use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
static NEXT_TRACE: AtomicU64 = AtomicU64::new(1);
use tracing::Instrument;

pub(crate) fn enabled() -> bool {
    tracing::enabled!(target: "recalld::performance", tracing::Level::TRACE)
}

pub(crate) struct Timer {
    stage: &'static str,
    start: Option<Instant>,
}
impl Timer {
    pub fn new(stage: &'static str) -> Self {
        Self {
            stage,
            start: enabled().then(Instant::now),
        }
    }
    fn end(&mut self, outcome: &'static str) {
        if let Some(start) = self.start.take() {
            tracing::trace!(target: "recalld::performance", kind = "duration",
                stage = self.stage, outcome, elapsed_ns = start.elapsed().as_nanos() as u64);
        }
    }
    pub fn outcome(mut self, success: bool) {
        self.end(if success { "ok" } else { "error" });
    }
    pub fn complete(mut self) {
        self.end("ok");
    }
}
impl Drop for Timer {
    fn drop(&mut self) {
        self.end("cancelled_error_or_unwound");
    }
}

/// Consecutive marks partition pipeline wall time, including a final residual.
/// Nested adapter/lock events must not be added again to these intervals.
pub(crate) struct Timeline {
    start: Option<Instant>,
    last: Option<Instant>,
    accounted: u64,
}
impl Timeline {
    pub fn new() -> Self {
        let start = enabled().then(Instant::now);
        Self {
            start,
            last: start,
            accounted: 0,
        }
    }
    pub fn mark(&mut self, stage: &'static str) {
        if let Some(last) = self.last {
            let now = Instant::now();
            let elapsed = now.duration_since(last).as_nanos() as u64;
            self.accounted += elapsed;
            self.last = Some(now);
            tracing::trace!(target: "recalld::performance", kind = "exclusive_pipeline",
                stage, elapsed_ns = elapsed);
        }
    }
}
impl Drop for Timeline {
    fn drop(&mut self) {
        self.mark("residual");
        if let (Some(start), Some(end)) = (self.start, self.last) {
            tracing::trace!(target: "recalld::performance", kind = "pipeline_accounting",
                elapsed_ns = end.duration_since(start).as_nanos() as u64,
                accounted_ns = self.accounted);
        }
    }
}

/// An outer request reports error explicitly; cancellation/unwind retains a
/// duration with an honestly ambiguous outcome. Errors themselves are not logged.
pub(crate) async fn measure<T, E>(
    operation: &'static str,
    record_access: bool,
    future: impl Future<Output = Result<T, E>>,
) -> Result<T, E> {
    let trace_id = if enabled() {
        NEXT_TRACE.fetch_add(1, Ordering::Relaxed)
    } else {
        0
    };
    async move {
        let mut timer = Timer::new(operation);
        let result = future.await;
        timer.end(if result.is_ok() { "ok" } else { "error" });
        result
    }
    .instrument(
        tracing::trace_span!(target: "recalld::performance", "performance_request",
        operation, record_access, trace_id, event_version = 1),
    )
    .await
}

/// Canonical receipt identities are isolated trace fields, never metric labels.
/// Do not fabricate receipts, canonical read generations or readiness barriers.
pub(crate) fn receipts(receipts: &[crate::storage::control::WriteReceipt]) {
    if enabled() {
        for receipt in receipts {
            tracing::trace!(target: "recalld::performance", kind = "committed_receipt",
                store_id = %receipt.store_id, operation_id = %receipt.operation_id,
                committed_generation = receipt.committed_generation.get());
        }
    }
}

/// Carry the request's subscriber and span into a blocking worker. No capture
/// occurs when disabled. Queue delay ends when the closure actually begins.
pub(crate) struct Worker {
    context: Option<(tracing::Dispatch, tracing::Span)>,
    queued: Timer,
}
impl Worker {
    pub fn new(stage: &'static str) -> Self {
        let context = enabled().then(|| {
            (
                tracing::dispatcher::get_default(Clone::clone),
                tracing::Span::current(),
            )
        });
        // One decision: a disabled context never starts a clock or emits on drop.
        let queued = Timer {
            stage,
            start: context.as_ref().map(|_| Instant::now()),
        };
        Self { context, queued }
    }
    pub fn run<T>(self, work: impl FnOnce() -> T) -> T {
        let Self { context, queued } = self;
        match context {
            Some((dispatch, span)) => tracing::dispatcher::with_default(&dispatch, || {
                span.in_scope(|| {
                    queued.complete();
                    work()
                })
            }),
            None => work(),
        }
    }
}

/// Release the actual guard before reporting its hold duration; never unlock
/// a borrowed FD or change the acquisition/poisoning contract.
pub(crate) struct Hold<G> {
    guard: Option<G>,
    timer: Timer,
}
impl<G> Hold<G> {
    pub fn new(stage: &'static str, guard: G) -> Self {
        Self {
            guard: Some(guard),
            timer: Timer::new(stage),
        }
    }
}
impl<G: Deref> Deref for Hold<G> {
    type Target = G::Target;
    fn deref(&self) -> &Self::Target {
        self.guard.as_ref().unwrap().deref()
    }
}
impl<G: DerefMut> DerefMut for Hold<G> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard.as_mut().unwrap().deref_mut()
    }
}
impl<G> Drop for Hold<G> {
    fn drop(&mut self) {
        drop(self.guard.take());
        self.timer.end("released");
    }
}

#[cfg(test)]
#[path = "performance_tests.rs"]
mod tests;
