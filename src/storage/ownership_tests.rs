//! Hold the fork-to-exec window open deterministically: no timing sleeps/retries.
use super::*;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

/// An unrelated child inherits descriptors but never touches storage or runs
/// Rust destructors. Its post-fork path uses only async-signal-safe syscalls.
struct ForkWindow {
    child_pid: u32,
    spawn: Option<std::thread::JoinHandle<std::io::Result<std::process::Child>>>,
    release: UnixStream,
}
impl ForkWindow {
    fn hold() -> Self {
        use std::os::unix::process::CommandExt;
        let (parent, child) = UnixStream::pair().unwrap();
        let child_fd = child.as_raw_fd();
        let spawn = std::thread::spawn(move || {
            let mut command = std::process::Command::new("/bin/true");
            unsafe {
                command.pre_exec(move || {
                    let pid = (libc::getpid() as u32).to_ne_bytes();
                    if libc::write(child_fd, pid.as_ptr().cast(), pid.len()) != pid.len() as isize {
                        libc::_exit(2);
                    }
                    let mut release = 0_u8;
                    if libc::read(child_fd, (&mut release as *mut u8).cast(), 1) != 1 {
                        libc::_exit(3);
                    }
                    Ok(())
                });
            }
            let result = command.spawn();
            drop(child);
            result
        });
        let mut window = Self {
            child_pid: 0,
            spawn: Some(spawn),
            release: parent,
        };
        window
            .release
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let mut pid = [0_u8; 4];
        window.release.read_exact(&mut pid).unwrap();
        window.child_pid = u32::from_ne_bytes(pid);
        window
    }

    fn evidence(&self, label: &str, file: &File) {
        let fd = file.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert_eq!(flags & libc::FD_CLOEXEC, libc::FD_CLOEXEC);
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::MetadataExt;
            let parent = file.metadata().unwrap();
            let path = format!("/proc/{}/fd/{fd}", self.child_pid);
            let child = std::fs::metadata(&path).unwrap();
            assert_eq!((parent.dev(), parent.ino()), (child.dev(), child.ino()));
            eprintln!(
                "FORK_EVIDENCE {label} parent={} child={} fd={fd} cloexec={flags} dev={} inode={} target={:?} fdinfo={}",
                std::process::id(),
                self.child_pid,
                parent.dev(),
                parent.ino(),
                std::fs::read_link(&path).unwrap(),
                std::fs::read_to_string(format!("/proc/{}/fdinfo/{fd}", self.child_pid)).unwrap()
            );
        }
    }
}
impl Drop for ForkWindow {
    fn drop(&mut self) {
        let _ = self.release.write_all(&[1]);
        if let Some(spawn) = self.spawn.take()
            && let Ok(Ok(mut child)) = spawn.join()
        {
            let _ = child.wait();
        }
    }
}

#[test]
fn ownership_fork_window_releases_stable_owner_when_parent_drops() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("store");
    let owner = StoreOwnership::acquire(&path).unwrap();
    assert!(
        StoreOwnership::acquire(&path).is_err(),
        "live owner must exclude contenders"
    );
    let child = ForkWindow::hold();
    child.evidence("stable-owner", &owner._file);
    drop(owner);
    let reopened = StoreOwnership::acquire(&path);
    assert!(
        reopened.is_ok(),
        "unrelated fork prolonged stable ownership: {:?}",
        reopened.err()
    );
}

#[test]
fn ownership_fork_window_releases_directory_lock_when_parent_drops() {
    let directory = tempfile::tempdir().unwrap();
    let lock = lock_directory(directory.path()).unwrap();
    assert!(
        lock_directory(directory.path()).is_err(),
        "live legacy lock must exclude contenders"
    );
    let child = ForkWindow::hold();
    child.evidence("directory-lock", &lock);
    drop(lock);
    let reopened = lock_directory(directory.path());
    assert!(
        reopened.is_ok(),
        "unrelated fork prolonged directory ownership: {:?}",
        reopened.err()
    );
}

#[test]
fn ownership_fork_window_reopens_actual_store_with_vectors() {
    use crate::storage::{RedbStorageEngine, StorageEngine};
    let directory = tempfile::tempdir().unwrap();
    let mut engine = RedbStorageEngine::open(directory.path()).unwrap();
    let mut namespace = crate::model::NamespaceConfig::default_namespace(0);
    namespace.name = "fork-fixture".into();
    namespace.embedding_dim = 4;
    engine.create_namespace(&namespace).unwrap();
    let _child = ForkWindow::hold();
    drop(engine);
    let reopened = RedbStorageEngine::open(directory.path());
    assert!(
        reopened.is_ok(),
        "unrelated fork prolonged engine ownership: {:?}",
        reopened.err()
    );
}

#[test]
fn ownership_fork_window_releases_vector_lock_independently() {
    let directory = tempfile::tempdir().unwrap();
    let vectors = crate::storage::VectorStore::open(directory.path(), 4).unwrap();
    assert!(crate::storage::VectorStore::open(directory.path(), 4).is_err());
    let _child = ForkWindow::hold();
    drop(vectors);
    let reopened = crate::storage::VectorStore::open(directory.path(), 4);
    assert!(
        reopened.is_ok(),
        "unrelated fork prolonged vector ownership: {:?}",
        reopened.err()
    );
}

#[test]
fn ownership_fork_window_redb_releases_its_own_lock() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("meta.db");
    let database = redb::Database::create(&path).unwrap();
    let _child = ForkWindow::hold();
    drop(database);
    let reopened = redb::Database::open(&path);
    assert!(
        reopened.is_ok(),
        "redb must release its independent lock: {:?}",
        reopened.err()
    );
}

fn acquire_for_test(kind: &str, path: &Path) -> Result<Box<dyn std::any::Any>, String> {
    match kind {
        "stable" => StoreOwnership::acquire(path)
            .map(|lock| Box::new(lock) as Box<dyn std::any::Any>)
            .map_err(|error| error.to_string()),
        "directory" => lock_directory(path)
            .map(|lock| Box::new(lock) as Box<dyn std::any::Any>)
            .map_err(|error| error.to_string()),
        "vector" => crate::storage::VectorStore::open(path, 4)
            .map(|lock| Box::new(lock) as Box<dyn std::any::Any>)
            .map_err(|error| error.to_string()),
        _ => panic!("unknown ownership test kind"),
    }
}

#[test]
fn ownership_exclusion_child() {
    let Ok(path) = std::env::var("RECALLD_OWNERSHIP_TEST_PATH") else {
        return;
    };
    let kind = std::env::var("RECALLD_OWNERSHIP_TEST_KIND").unwrap();
    let expected = std::env::var("RECALLD_OWNERSHIP_TEST_AVAILABLE").unwrap() == "true";
    assert_eq!(acquire_for_test(&kind, Path::new(&path)).is_ok(), expected);
}

#[test]
fn ownership_guard_preserves_process_and_alias_exclusion() {
    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let alias = root.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    for kind in ["stable", "directory", "vector"] {
        let path = real.join(kind);
        std::fs::create_dir(&path).unwrap();
        let alias = alias.join(kind);
        let held = acquire_for_test(kind, &path).unwrap();
        for contender in [&path, &alias, &path] {
            assert!(acquire_for_test(kind, contender).is_err());
        }
        let probe = |available: bool| {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "storage::ownership::tests::ownership_exclusion_child",
                    "--nocapture",
                ])
                .env("RECALLD_OWNERSHIP_TEST_PATH", &alias)
                .env("RECALLD_OWNERSHIP_TEST_KIND", kind)
                .env("RECALLD_OWNERSHIP_TEST_AVAILABLE", available.to_string())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{kind}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        probe(false);
        // Failed acquisition/destruction must not unlock the actual owner's FD.
        assert!(acquire_for_test(kind, &path).is_err());
        drop(held);
        probe(true);
        assert!(acquire_for_test(kind, &alias).is_ok());
    }
}
