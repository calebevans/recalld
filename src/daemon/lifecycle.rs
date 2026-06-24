use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

// ---------------------------------------------------------------------------
// Socket / PID path resolution
// ---------------------------------------------------------------------------

/// Resolve the daemon socket path.
///
/// Priority:
/// 1. `$RECALLD_DAEMON_SOCKET` env var
/// 2. `$XDG_RUNTIME_DIR/recalld/recalld.sock` (Linux)
/// 3. `~/.recalld/recalld.sock` (macOS / fallback)
pub fn socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("RECALLD_DAEMON_SOCKET") {
        return PathBuf::from(p);
    }

    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join("recalld").join("recalld.sock");
    }

    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".recalld")
        .join("recalld.sock")
}

/// PID file path -- sibling to the socket file.
pub fn pid_path() -> PathBuf {
    let mut p = socket_path();
    p.set_file_name("recalld.pid");
    p
}

// ---------------------------------------------------------------------------
// Stale socket detection
// ---------------------------------------------------------------------------

/// Check if a daemon is actually running at the given socket path.
/// Returns `Ok(true)` if alive, `Ok(false)` if stale or not running.
pub fn is_daemon_alive(socket_path: &Path) -> std::io::Result<bool> {
    if !socket_path.exists() {
        return Ok(false);
    }

    use std::os::unix::net::UnixStream;
    match UnixStream::connect(socket_path) {
        Ok(_stream) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => Ok(false),
        Err(e) => Err(e),
    }
}

/// Remove a stale socket file. No-op if the daemon is actually alive.
pub fn cleanup_stale_socket(socket_path: &Path) -> std::io::Result<()> {
    if !is_daemon_alive(socket_path)? && socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// PID file operations
// ---------------------------------------------------------------------------

pub fn write_pid_file(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, std::process::id().to_string())
}

pub fn read_pid_file(path: &Path) -> std::io::Result<Option<u32>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(contents.trim().parse::<u32>().ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// Idle monitor
// ---------------------------------------------------------------------------

pub fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Background task that shuts down the daemon after it has been idle
/// (zero connections and no recent activity) for `idle_timeout`.
pub async fn idle_monitor(
    last_activity: Arc<AtomicI64>,
    connection_count: Arc<AtomicU32>,
    idle_timeout: Duration,
    shutdown_tx: watch::Sender<bool>,
) {
    let check_interval = Duration::from_secs(30);

    loop {
        tokio::time::sleep(check_interval).await;

        if connection_count.load(Ordering::Relaxed) > 0 {
            continue;
        }

        let last = last_activity.load(Ordering::Relaxed);
        let elapsed_ms = now_millis() - last;

        if elapsed_ms >= idle_timeout.as_millis() as i64 {
            tracing::info!(
                idle_secs = elapsed_ms / 1000,
                "idle timeout reached, initiating shutdown"
            );
            let _ = shutdown_tx.send(true);
            return;
        }
    }
}
