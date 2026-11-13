use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::time::Duration;

use tokio::sync::watch;

// ---------------------------------------------------------------------------
// Socket / PID path resolution
// ---------------------------------------------------------------------------

/// Resolve the daemon socket path.
///
/// Priority:
/// 1. `$RECALLD_DAEMON_SOCKET` env var
/// 2. A daemon that is already listening at one of the known locations:
///    `$XDG_RUNTIME_DIR/recalld/recalld.sock`, then
///    `/run/user/<uid>/recalld/recalld.sock` (Linux), then
///    `~/.recalld/recalld.sock`
/// 3. `$XDG_RUNTIME_DIR/recalld/recalld.sock` (Linux)
/// 4. `~/.recalld/recalld.sock` (macOS / fallback)
///
/// Step 2 is there because the processes of one user do not all see the
/// same environment. A daemon started from a login session or a systemd
/// user unit has `XDG_RUNTIME_DIR`; an MCP client that starts its servers
/// with a trimmed environment does not pass it on. Deciding from the
/// variable alone, the two looked in different places, and the client
/// went on to start a second daemon against a store the first one held
/// open. Where nothing is listening, steps 3 and 4 are what they always
/// were, so a daemon binds where it did before.
///
/// Resolved once per process. Step 2 connects to a socket to see whether
/// a daemon is listening, and [`pid_path`] and most commands ask for the
/// path more than once.
pub fn socket_path() -> PathBuf {
    static RESOLVED: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

    RESOLVED
        .get_or_init(|| {
            resolve_socket_path(
                std::env::var_os("RECALLD_DAEMON_SOCKET").map(PathBuf::from),
                runtime_dir_from(std::env::var_os("XDG_RUNTIME_DIR")),
                conventional_runtime_dir(),
                dirs::home_dir(),
                |socket| is_daemon_alive(socket).unwrap_or(false),
            )
        })
        .clone()
}

/// `XDG_RUNTIME_DIR` as a directory. An empty value counts as unset:
/// joined onto, it would name a socket relative to the working directory.
fn runtime_dir_from(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    value.filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// `/run/user/<uid>`, where `pam_systemd` points `XDG_RUNTIME_DIR` for a
/// login session.
///
/// Nobody told this process to look here, so it is only somewhere to
/// look if it is what the login manager creates. Otherwise a socket
/// found in it could be one that somebody else put there, and a client
/// would hand its memories to whoever was listening.
#[cfg(target_os = "linux")]
fn conventional_runtime_dir() -> Option<PathBuf> {
    // SAFETY: `getuid` has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let dir = PathBuf::from(format!("/run/user/{uid}"));
    is_private_dir_of(&dir, uid).then_some(dir)
}

/// Whether `dir` meets what the XDG Base Directory spec requires of a
/// runtime directory: a real directory (not a symlink to one), owned by
/// `uid`, with access mode `0700`.
#[cfg(target_os = "linux")]
fn is_private_dir_of(dir: &Path, uid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;

    std::fs::symlink_metadata(dir)
        .is_ok_and(|meta| meta.is_dir() && meta.uid() == uid && meta.mode() & 0o077 == 0)
}

#[cfg(not(target_os = "linux"))]
fn conventional_runtime_dir() -> Option<PathBuf> {
    None
}

/// [`socket_path`] with its inputs passed in, so the rules can be tested
/// without touching the process environment or a real socket.
fn resolve_socket_path(
    explicit: Option<PathBuf>,
    xdg_runtime_dir: Option<PathBuf>,
    conventional_runtime_dir: Option<PathBuf>,
    home_dir: Option<PathBuf>,
    daemon_alive: impl Fn(&Path) -> bool,
) -> PathBuf {
    if let Some(socket) = explicit {
        return socket;
    }

    let in_runtime_dir = |dir: &Path| dir.join("recalld").join("recalld.sock");
    let home_socket = home_dir
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".recalld")
        .join("recalld.sock");
    let xdg_socket = xdg_runtime_dir.as_deref().map(in_runtime_dir);
    let conventional_socket = conventional_runtime_dir
        .as_deref()
        .map(in_runtime_dir)
        .filter(|socket| Some(socket) != xdg_socket.as_ref());

    let known = [
        xdg_socket.clone(),
        conventional_socket,
        Some(home_socket.clone()),
    ];
    if let Some(live) = known
        .into_iter()
        .flatten()
        .find(|socket| daemon_alive(socket))
    {
        return live;
    }

    xdg_socket.unwrap_or(home_socket)
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

/// Writes the current process ID to the given file path.
pub fn write_pid_file(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, std::process::id().to_string())
}

/// Reads the PID from the given file, returning `None` if absent or unparseable.
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

/// Returns the current UTC time as milliseconds since the Unix epoch.
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

#[cfg(test)]
mod tests {
    use super::*;

    const RUNTIME_DIR: &str = "/run/user/1000";
    const RUNTIME_SOCKET: &str = "/run/user/1000/recalld/recalld.sock";
    const HOME_SOCKET: &str = "/home/u/.recalld/recalld.sock";

    fn dir(path: &str) -> Option<PathBuf> {
        Some(PathBuf::from(path))
    }

    fn nothing_alive(_: &Path) -> bool {
        false
    }

    fn alive_at(socket: &'static str) -> impl Fn(&Path) -> bool {
        move |candidate| candidate == Path::new(socket)
    }

    #[test]
    fn an_explicit_socket_wins_and_nothing_is_probed() {
        let socket = resolve_socket_path(
            dir("/tmp/custom.sock"),
            dir(RUNTIME_DIR),
            dir(RUNTIME_DIR),
            dir("/home/u"),
            |_| panic!("RECALLD_DAEMON_SOCKET is an instruction, not a hint"),
        );

        assert_eq!(socket, PathBuf::from("/tmp/custom.sock"));
    }

    /// With no daemon listening anywhere, a daemon binds where it always
    /// has. In particular the conventional runtime directory is only ever
    /// somewhere to look, never somewhere to bind: a daemon started
    /// without `XDG_RUNTIME_DIR` still goes to the home directory.
    #[test]
    fn with_no_daemon_running_the_defaults_are_unchanged() {
        assert_eq!(
            resolve_socket_path(
                None,
                dir(RUNTIME_DIR),
                dir(RUNTIME_DIR),
                dir("/home/u"),
                nothing_alive
            ),
            PathBuf::from(RUNTIME_SOCKET)
        );
        assert_eq!(
            resolve_socket_path(None, None, dir(RUNTIME_DIR), dir("/home/u"), nothing_alive),
            PathBuf::from(HOME_SOCKET)
        );
        assert_eq!(
            resolve_socket_path(None, None, None, dir("/home/u"), nothing_alive),
            PathBuf::from(HOME_SOCKET)
        );
    }

    /// The case that broke MCP clients which start their servers with a
    /// trimmed environment. The daemon was started from a login session
    /// or a systemd user unit and listens under `/run/user/<uid>`; the
    /// client has no `XDG_RUNTIME_DIR`, looked in the home directory,
    /// found nothing, and tried to start a second daemon on a store the
    /// first one holds open.
    #[test]
    fn a_client_without_xdg_runtime_dir_finds_a_daemon_started_with_it() {
        let socket = resolve_socket_path(
            None,
            None,
            dir(RUNTIME_DIR),
            dir("/home/u"),
            alive_at(RUNTIME_SOCKET),
        );

        assert_eq!(socket, PathBuf::from(RUNTIME_SOCKET));
    }

    /// The same disagreement the other way round: the daemon was
    /// auto-started by a client with no `XDG_RUNTIME_DIR`, so it listens
    /// in the home directory, and a later client that does have the
    /// variable must not go looking only in the runtime directory.
    #[test]
    fn a_client_with_xdg_runtime_dir_finds_a_daemon_started_without_it() {
        let socket = resolve_socket_path(
            None,
            dir(RUNTIME_DIR),
            dir(RUNTIME_DIR),
            dir("/home/u"),
            alive_at(HOME_SOCKET),
        );

        assert_eq!(socket, PathBuf::from(HOME_SOCKET));
    }

    #[test]
    fn a_daemon_under_a_custom_xdg_runtime_dir_is_found_before_the_conventional_one() {
        let socket = resolve_socket_path(
            None,
            dir("/custom/runtime"),
            dir(RUNTIME_DIR),
            dir("/home/u"),
            |_| true,
        );

        assert_eq!(
            socket,
            PathBuf::from("/custom/runtime/recalld/recalld.sock")
        );
    }

    /// The conventional runtime directory is trusted only in the form the
    /// login manager creates it. A looser one could hold a socket that
    /// another user planted for a client to find.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_runtime_dir_is_only_somewhere_to_look_if_it_is_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;

        // SAFETY: `getuid` has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("runtime");
        std::fs::create_dir(&dir).expect("create dir");
        let chmod = |mode| {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).expect("chmod")
        };

        chmod(0o700);
        assert!(is_private_dir_of(&dir, uid));
        assert!(
            !is_private_dir_of(&dir, uid.wrapping_add(1)),
            "owned by someone else"
        );

        chmod(0o770);
        assert!(!is_private_dir_of(&dir, uid), "writable by the group");
        chmod(0o755);
        assert!(!is_private_dir_of(&dir, uid), "open to other users");

        chmod(0o700);
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&dir, &link).expect("symlink");
        assert!(!is_private_dir_of(&link, uid), "a symlink to a directory");
        assert!(!is_private_dir_of(&tmp.path().join("missing"), uid));
    }

    #[test]
    fn an_empty_xdg_runtime_dir_counts_as_unset() {
        assert_eq!(runtime_dir_from(Some("".into())), None);
        assert_eq!(runtime_dir_from(None), None);
        assert_eq!(runtime_dir_from(Some(RUNTIME_DIR.into())), dir(RUNTIME_DIR));
    }
}
