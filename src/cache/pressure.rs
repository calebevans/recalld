//! Memory pressure monitoring and response.
//!
//! Provides `PressureLevel`, the `PressureMonitor` trait, platform-specific
//! implementations (macOS GCD, Linux PSI), and the pressure response loop
//! that adjusts the cache budget and triggers proactive eviction.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use crate::cache::warming::compute_priority;
use crate::model::{CachedRecord, MemoryId};

// ─── PressureLevel ──────────────────────────────────────────────────

/// System memory pressure level.
///
/// Ordered: Normal < Warning < Critical. The `PartialOrd`/`Ord` derives
/// follow variant declaration order.
///
/// Stored as `AtomicU8` for lock-free reads by the prefetch worker
/// and eviction sweeper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum PressureLevel {
    /// No pressure. Full budget, all prefetch modes active.
    Normal = 0,
    /// Moderate pressure. Budget reduced to 75%, eager-only prefetch.
    Warning = 1,
    /// Severe pressure. Budget reduced to 50%, prefetch paused entirely.
    Critical = 2,
}

impl PressureLevel {
    /// Convert from the raw u8 stored in AtomicU8.
    /// Returns Normal for any unrecognized value (defensive).
    pub fn from_u8(val: u8) -> Self {
        match val {
            0 => Self::Normal,
            1 => Self::Warning,
            2 => Self::Critical,
            _ => Self::Normal,
        }
    }
}

// ─── PressureMonitor Trait ──────────────────────────────────────────

/// Platform-specific memory pressure monitor.
///
/// Implementations listen for OS-level memory pressure events and
/// update a shared `AtomicU8` that the rest of the cache system polls.
///
/// Implementors:
/// - `MacOsPressureMonitor` -- GCD dispatch source (macOS)
/// - `LinuxPressureMonitor` -- PSI triggers via /proc/pressure/memory (Linux)
/// - `NoOpPressureMonitor`  -- always Normal (unsupported platforms, testing)
#[async_trait::async_trait]
pub trait PressureMonitor: Send + Sync + 'static {
    /// Block until the pressure level changes. Returns the new level.
    async fn wait_for_change(&self) -> PressureLevel;

    /// Poll the current pressure level without blocking.
    fn current_level(&self) -> PressureLevel;
}

// ─── macOS Implementation: GCD FFI ─────────────────────────────────

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::ffi::c_void;
    use tokio::sync::Notify;

    // libdispatch constants from <dispatch/source.h>
    const DISPATCH_MEMORYPRESSURE_NORMAL: u64 = 0x01;
    const DISPATCH_MEMORYPRESSURE_WARN: u64 = 0x02;
    const DISPATCH_MEMORYPRESSURE_CRITICAL: u64 = 0x04;
    const QOS_CLASS_DEFAULT: isize = 0x15;

    unsafe extern "C" {
        fn dispatch_get_global_queue(identifier: isize, flags: usize) -> *mut c_void;

        fn dispatch_source_create(
            source_type: *const c_void,
            handle: usize,
            mask: u64,
            queue: *mut c_void,
        ) -> *mut c_void;

        fn dispatch_source_set_event_handler_f(
            source: *mut c_void,
            handler: extern "C" fn(*mut c_void),
        );

        fn dispatch_set_context(object: *mut c_void, context: *mut c_void);

        fn dispatch_source_get_data(source: *const c_void) -> u64;

        fn dispatch_resume(source: *mut c_void);

        static _dispatch_source_type_memorypressure: c_void;
    }

    /// Shared state passed to the GCD handler via dispatch_set_context.
    struct HandlerContext {
        level: Arc<AtomicU8>,
        notify: Arc<Notify>,
        source: *mut c_void,
    }

    // SAFETY: The dispatch source and handler context are thread-safe
    // (GCD guarantee). The AtomicU8 and Notify are Send+Sync by construction.
    unsafe impl Send for HandlerContext {}
    unsafe impl Sync for HandlerContext {}

    /// macOS memory pressure monitor using GCD dispatch sources.
    pub struct MacOsPressureMonitor {
        level: Arc<AtomicU8>,
        notify: Arc<Notify>,
        // The dispatch source is never explicitly released -- it lives
        // for the process lifetime. This field prevents the pointer from
        // being lost.
        _source: *mut c_void,
    }

    // SAFETY: The dispatch source is thread-safe (GCD guarantee).
    unsafe impl Send for MacOsPressureMonitor {}
    unsafe impl Sync for MacOsPressureMonitor {}

    impl MacOsPressureMonitor {
        /// Create and install the GCD memory pressure dispatch source.
        ///
        /// # Safety
        ///
        /// Calls into libdispatch via raw FFI. The dispatch source and
        /// handler context are leaked intentionally (process-lifetime).
        pub unsafe fn new() -> Self {
            let level = Arc::new(AtomicU8::new(PressureLevel::Normal as u8));
            let notify = Arc::new(Notify::new());

            let mask = DISPATCH_MEMORYPRESSURE_NORMAL
                | DISPATCH_MEMORYPRESSURE_WARN
                | DISPATCH_MEMORYPRESSURE_CRITICAL;

            // SAFETY: All FFI calls below are safe when called from a process
            // with a running dispatch queue (always true for a Tokio application).
            let queue = unsafe { dispatch_get_global_queue(QOS_CLASS_DEFAULT, 0) };
            let source = unsafe {
                dispatch_source_create(
                    &_dispatch_source_type_memorypressure as *const _ as *const c_void,
                    0,
                    mask,
                    queue,
                )
            };

            // Allocate the handler context on the heap and leak it.
            let ctx = Box::into_raw(Box::new(HandlerContext {
                level: level.clone(),
                notify: notify.clone(),
                source,
            }));

            unsafe {
                dispatch_set_context(source, ctx as *mut c_void);
                dispatch_source_set_event_handler_f(source, pressure_handler);
                dispatch_resume(source);
            }

            // Read initial pressure state.
            // GCD only fires on transitions, so the initial state must
            // be read separately. We conservatively start at Normal.
            let initial = Self::read_initial_pressure();
            level.store(initial as u8, Ordering::Relaxed);

            Self {
                level,
                notify,
                _source: source,
            }
        }

        /// Read current memory pressure at startup.
        ///
        /// A production implementation would call host_statistics64()
        /// via the mach crate to read the internal_pressure_level field.
        /// For now, conservatively returns Normal.
        fn read_initial_pressure() -> PressureLevel {
            PressureLevel::Normal
        }
    }

    extern "C" fn pressure_handler(context: *mut c_void) {
        // SAFETY: context is a HandlerContext* that we leaked in new().
        let ctx = unsafe { &*(context as *const HandlerContext) };

        // Read the pressure flags from the dispatch source.
        let data = unsafe { dispatch_source_get_data(ctx.source) };

        let level = if data & DISPATCH_MEMORYPRESSURE_CRITICAL != 0 {
            PressureLevel::Critical
        } else if data & DISPATCH_MEMORYPRESSURE_WARN != 0 {
            PressureLevel::Warning
        } else {
            PressureLevel::Normal
        };

        ctx.level.store(level as u8, Ordering::Relaxed);
        ctx.notify.notify_one();
    }

    #[async_trait::async_trait]
    impl PressureMonitor for MacOsPressureMonitor {
        async fn wait_for_change(&self) -> PressureLevel {
            self.notify.notified().await;
            self.current_level()
        }

        fn current_level(&self) -> PressureLevel {
            PressureLevel::from_u8(self.level.load(Ordering::Relaxed))
        }
    }
}

// ─── Linux Implementation: PSI Triggers ─────────────────────────────

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use tokio::sync::Notify;

    /// PSI threshold configuration.
    const WARNING_TRIGGER: &str = "some 150000 1000000"; // 15% some-stall in 1s
    const CRITICAL_TRIGGER: &str = "full 50000 1000000"; // 5% full-stall in 1s

    /// Polling interval for reading PSI avg10 values.
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

    /// Linux memory pressure monitor using PSI triggers.
    pub struct LinuxPressureMonitor {
        level: Arc<AtomicU8>,
        notify: Arc<Notify>,
        // Trigger file descriptors -- kept open for the process lifetime.
        _warning_trigger: Option<std::fs::File>,
        _critical_trigger: Option<std::fs::File>,
    }

    impl LinuxPressureMonitor {
        /// Create the PSI monitor.
        ///
        /// Sets up triggers if the kernel supports them. Falls back to
        /// polling if trigger registration fails (older kernels,
        /// cgroup-only namespaces, insufficient privileges).
        pub fn new() -> Self {
            let level = Arc::new(AtomicU8::new(PressureLevel::Normal as u8));
            let notify = Arc::new(Notify::new());

            let warning_trigger = Self::setup_trigger(WARNING_TRIGGER)
                .map_err(|e| {
                    tracing::warn!("PSI warning trigger setup failed: {}", e);
                    e
                })
                .ok();

            let critical_trigger = Self::setup_trigger(CRITICAL_TRIGGER)
                .map_err(|e| {
                    tracing::warn!("PSI critical trigger setup failed: {}", e);
                    e
                })
                .ok();

            // Read initial state.
            if let Ok(initial) = Self::read_psi_level() {
                level.store(initial as u8, Ordering::Relaxed);
            }

            Self {
                level,
                notify,
                _warning_trigger: warning_trigger,
                _critical_trigger: critical_trigger,
            }
        }

        /// Register a PSI trigger by writing the threshold string to
        /// /proc/pressure/memory.
        fn setup_trigger(trigger: &str) -> std::io::Result<std::fs::File> {
            use std::io::Write;

            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/proc/pressure/memory")?;
            file.write_all(trigger.as_bytes())?;
            Ok(file)
        }

        /// Read current PSI levels from /proc/pressure/memory.
        ///
        /// Thresholds:
        /// - full avg10 >= 5.0  -> Critical
        /// - some avg10 >= 15.0 -> Warning
        /// - otherwise          -> Normal
        fn read_psi_level() -> std::io::Result<PressureLevel> {
            let content = std::fs::read_to_string("/proc/pressure/memory")?;
            let mut some_avg10: f64 = 0.0;
            let mut full_avg10: f64 = 0.0;

            for line in content.lines() {
                if line.starts_with("some") {
                    some_avg10 = Self::parse_avg10(line);
                } else if line.starts_with("full") {
                    full_avg10 = Self::parse_avg10(line);
                }
            }

            let level = if full_avg10 >= 5.0 {
                PressureLevel::Critical
            } else if some_avg10 >= 15.0 {
                PressureLevel::Warning
            } else {
                PressureLevel::Normal
            };

            Ok(level)
        }

        /// Parse the avg10 value from a PSI line.
        fn parse_avg10(line: &str) -> f64 {
            line.split_whitespace()
                .find(|s| s.starts_with("avg10="))
                .and_then(|s| s.strip_prefix("avg10="))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0)
        }
    }

    #[async_trait::async_trait]
    impl PressureMonitor for LinuxPressureMonitor {
        async fn wait_for_change(&self) -> PressureLevel {
            // Poll /proc/pressure/memory at a fixed interval.
            // A production implementation could use AsyncFd with
            // POLLPRI on the trigger file descriptors for truly
            // event-driven notification.
            loop {
                tokio::time::sleep(POLL_INTERVAL).await;

                if let Ok(new_level) = Self::read_psi_level() {
                    let old = self.level.swap(new_level as u8, Ordering::Relaxed);
                    if old != new_level as u8 {
                        self.notify.notify_one();
                        return new_level;
                    }
                }
            }
        }

        fn current_level(&self) -> PressureLevel {
            PressureLevel::from_u8(self.level.load(Ordering::Relaxed))
        }
    }
}

// ─── No-Op Implementation ───────────────────────────────────────────

/// Stub monitor that always reports Normal pressure.
///
/// Used on unsupported platforms and in unit tests where real OS
/// pressure events are not available.
pub struct NoOpPressureMonitor;

#[async_trait::async_trait]
impl PressureMonitor for NoOpPressureMonitor {
    async fn wait_for_change(&self) -> PressureLevel {
        // Never returns -- pressure never changes on a no-op monitor.
        std::future::pending().await
    }

    fn current_level(&self) -> PressureLevel {
        PressureLevel::Normal
    }
}

// ─── Platform Detection & Factory ───────────────────────────────────

/// Create the appropriate PressureMonitor for the current platform.
///
/// Returns a boxed trait object. The caller stores it and spawns the
/// pressure monitor loop.
pub fn create_pressure_monitor() -> Box<dyn PressureMonitor> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: GCD FFI is safe when called from a process with
        // a running dispatch queue (always true for a Tokio application).
        unsafe { Box::new(macos::MacOsPressureMonitor::new()) }
    }

    #[cfg(target_os = "linux")]
    {
        Box::new(linux::LinuxPressureMonitor::new())
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        tracing::warn!("no memory pressure monitor for this platform -- using no-op");
        Box::new(NoOpPressureMonitor)
    }
}

// ─── Pressure Response Loop ─────────────────────────────────────────

/// Background task that listens for pressure changes and adjusts
/// the cache accordingly.
///
/// Response actions per level:
/// - Normal:   100% budget, all prefetch modes active.
/// - Warning:  75% budget, eager-only prefetch.
/// - Critical: 50% budget, prefetch paused, proactive eviction.
pub async fn start_pressure_monitor(
    monitor: Box<dyn PressureMonitor>,
    pressure_level: Arc<AtomicU8>,
    effective_budget: Arc<AtomicU64>,
    configured_budget: u64,
    cache: moka::future::Cache<MemoryId, Arc<CachedRecord>>,
) {
    loop {
        let new_level = monitor.wait_for_change().await;
        let old_level =
            PressureLevel::from_u8(pressure_level.swap(new_level as u8, Ordering::Relaxed));

        if old_level == new_level {
            continue; // Spurious wake.
        }

        tracing::warn!(
            "memory pressure changed: {:?} -> {:?}",
            old_level,
            new_level,
        );

        // Adjust effective budget.
        let budget_fraction = match new_level {
            PressureLevel::Normal => 1.0_f64,
            PressureLevel::Warning => 0.75,
            PressureLevel::Critical => 0.50,
        };
        let new_budget = (configured_budget as f64 * budget_fraction) as u64;
        effective_budget.store(new_budget, Ordering::Relaxed);

        // Proactive eviction if budget shrank.
        if new_level > old_level {
            evict_to_target(&cache, new_budget).await;
        }

        metrics::gauge!("cache.pressure_level").set(new_level as u8 as f64);
        metrics::gauge!("cache.effective_budget_bytes").set(new_budget as f64);
    }
}

/// Evict lowest-priority entries until weighted_size <= target_bytes.
///
/// Eviction order: ascending priority score (least valuable first).
/// Uses `compute_priority()` from the warming module.
pub async fn evict_to_target(
    cache: &moka::future::Cache<MemoryId, Arc<CachedRecord>>,
    target_bytes: u64,
) {
    // Flush pending maintenance so weighted_size() is accurate.
    cache.run_pending_tasks().await;

    let current = cache.weighted_size();
    if current <= target_bytes {
        return;
    }

    let to_evict_bytes = current - target_bytes;
    tracing::info!(
        "pressure eviction: need to free {} bytes (current={}, target={})",
        to_evict_bytes,
        current,
        target_bytes,
    );

    // Collect and sort candidates by priority ascending (evict lowest first).
    let mut candidates: Vec<(MemoryId, f32)> = cache
        .iter()
        .map(|(id, record)| (*id, compute_priority(&record)))
        .collect();
    candidates.sort_unstable_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

    let mut evicted = 0u64;
    for (id, _priority) in candidates {
        if cache.weighted_size() <= target_bytes {
            break;
        }
        cache.invalidate(&id).await;
        evicted += 1;
    }

    cache.run_pending_tasks().await;

    tracing::info!(
        "pressure eviction complete: evicted {} entries, new size={} bytes",
        evicted,
        cache.weighted_size(),
    );
}
