//! Shared fixtures for the ported Python tests (lane I2). Every sm process gets HOME and
//! SIMPLE_MEMORY_HOME in scratch, so no test reaches the live store.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::Connection;

/// `sm <args>` as a user runs it: (exit code, stdout, stderr). HOME is the parent of `home`.
pub fn sm(home: &Path, args: &[&str]) -> (i32, String, String) {
    sm_env(home, args, &[])
}

pub fn sm_env(home: &Path, args: &[&str], env: &[(&str, &str)]) -> (i32, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sm"));
    cmd.args(args)
        .env("HOME", home.parent().unwrap_or(home))
        .env("SIMPLE_MEMORY_HOME", home)
        .env_remove("SM_PARITY_NOW")
        .env_remove("SM_PARITY_SEED")
        .env_remove("SM_PARITY_TRACE");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn sm");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8(out.stdout).expect("utf-8 stdout"),
        String::from_utf8(out.stderr).expect("utf-8 stderr"),
    )
}

/// `sm put <args>`; the printed id.
pub fn put(home: &Path, args: &[&str]) -> String {
    let mut a = vec!["put"];
    a.extend_from_slice(args);
    let (code, out, err) = sm(home, &a);
    assert_eq!(code, 0, "put failed: {err}");
    out.trim().to_string()
}

pub fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

pub fn git(args: &[&str]) {
    let st = Command::new("git")
        .args(args)
        .env("HOME", std::env::temp_dir())
        .env_remove("GIT_DIR")
        .output()
        .expect("spawn git");
    assert!(
        st.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&st.stderr)
    );
}

pub fn open(home: &Path) -> Connection {
    simple_memory::store::connect::connect(home.join("memory.db").to_str().unwrap(), None)
        .expect("connect")
}

/// Does the store's database file or its write-ahead log hold `needle` anywhere?
pub fn db_files_hold(home: &Path, needle: &str) -> bool {
    ["memory.db", "memory.db-wal"].iter().any(|f| {
        std::fs::read(home.join(f))
            .is_ok_and(|b| b.windows(needle.len()).any(|w| w == needle.as_bytes()))
    })
}

/// A secret written with spaces in a refs value (key, value and the plain ref before them) and
/// the part of each that must not be stored.
pub const SPACED_REF_SECRETS: [(&str, &str); 3] = [
    (
        "TKT-9 aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        "wJalrXUtnFEMI",
    ),
    ("TKT-9 password: Hunter2-example-Zz9", "Hunter2-example-Zz9"),
    (
        "TKT-9 Bearer Hunter2-example-token-0001",
        "Hunter2-example-token-0001",
    ),
];

/// One text column per row.
pub fn column(home: &Path, sql: &str) -> Vec<String> {
    let conn = open(home);
    let mut stmt = conn.prepare(sql).unwrap();
    stmt.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

pub fn scratch() -> (tempfile::TempDir, PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    (d, home)
}

/// hook/catchup.lock held by another process until this value drops. A lock this test process
/// held could outlive its `drop`: a child that another test thread starts holds a copy of every
/// descriptor until it execs, the flock goes only with the last copy, and the next catch-up then
/// finds it held (stress runs on 2026-11-05 saw it held up to 181 ms after the drop). A forked
/// child opens and locks the file itself, so this process never holds the locked descriptor.
pub struct HeldLock {
    pid: libc::pid_t,
    _alive: std::os::unix::net::UnixStream,
}

pub fn hold_catchup_lock(home: &Path) -> HeldLock {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::UnixStream;
    let hook = home.join("hook");
    std::fs::create_dir_all(&hook).unwrap();
    let path = std::ffi::CString::new(hook.join("catchup.lock").as_os_str().as_bytes()).unwrap();
    let (mut ready, ready_w) = UnixStream::pair().unwrap();
    let (alive, alive_r) = UnixStream::pair().unwrap();
    // SAFETY: the child calls only async-signal-safe functions (close, open, flock, write, read,
    // _exit) on memory the fork copied.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork: {}", std::io::Error::last_os_error());
    if pid == 0 {
        unsafe {
            libc::close(ready.as_raw_fd());
            libc::close(alive.as_raw_fd());
            let fd = libc::open(
                path.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_CLOEXEC,
                0o644,
            );
            let ok = [u8::from(
                fd >= 0 && libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) == 0,
            )];
            libc::write(ready_w.as_raw_fd(), ok.as_ptr().cast(), 1);
            // Hold the lock until killed, or until the test process is gone (EOF).
            let mut b = 0u8;
            while libc::read(alive_r.as_raw_fd(), (&raw mut b).cast(), 1) != 0 {}
            libc::_exit(0);
        }
    }
    drop((ready_w, alive_r));
    let held = HeldLock { pid, _alive: alive };
    let mut ok = [0u8];
    ready.read_exact(&mut ok).expect("the lock holder answers");
    assert_eq!(ok, [1], "the lock holder could not take hook/catchup.lock");
    held
}

impl Drop for HeldLock {
    /// The lock goes when the holder has exited; waitpid returns only after that.
    fn drop(&mut self) {
        let mut status = 0;
        // SAFETY: the pid is this process's own unreaped child.
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
            libc::waitpid(self.pid, &mut status, 0);
        }
    }
}

/// The wall time SQLite's own busy handler takes to give up after `ms` on the write lock held
/// on `db`, measured in a thread beside the lock wait a test times. The handler counts nominal
/// sleeps, not wall time, so a host whose sleeps run long stretches every lock wait alike (the
/// lib twin `store::connect::busy_reference` holds the measured figures).
pub fn busy_reference(db: &Path, ms: u64) -> std::thread::JoinHandle<f64> {
    let db = db.to_path_buf();
    std::thread::spawn(move || {
        let c = Connection::open(&db).unwrap();
        c.busy_timeout(std::time::Duration::from_millis(ms))
            .unwrap();
        let started = std::time::Instant::now();
        let err = c.execute_batch("BEGIN IMMEDIATE").unwrap_err();
        assert_eq!(
            err.sqlite_error_code(),
            Some(rusqlite::ErrorCode::DatabaseBusy),
            "{err}"
        );
        started.elapsed().as_secs_f64()
    })
}
