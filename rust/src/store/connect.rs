//! The store connection: WAL, foreign keys, a 5 s busy timeout (the time left when a deadline is
//! closer), `sm_fold` registered as a deterministic function (the FTS triggers call it), the
//! migrations, and the bounded wait for another writer's lock.

use std::time::{Duration, Instant};

use rusqlite::functions::FunctionFlags;
use rusqlite::{Connection, OpenFlags};

use crate::error::SmError;
use crate::py::path::PyPath;

/// A writer waits this long in all for another writer's lock, then fails.
pub const WRITE_LOCK_WAIT_S: f64 = 15.0;
const BUSY_TIMEOUT_MS: i64 = 5000;
/// A lock wait gives SQLite's busy handler at most this long per try, then reads the clock: the
/// handler counts its nominal sleeps, which run long on a busy host (`busy_reference`).
const LOCK_WAIT_SLICE_MS: i64 = 100;

/// The migrations shipped with the package, by version: `_discover_migrations()`.
const MIGRATIONS: &[(i64, &str)] = &[
    (
        1,
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/migrations/0001_init.sql"
        )),
    ),
    (
        2,
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/migrations/0002_indexes.sql"
        )),
    ),
    (
        3,
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/migrations/0003_dedupe_per_file.sql"
        )),
    ),
];

/// `sqlite3.complete_statement(sql)`.
fn complete_statement(sql: &str) -> bool {
    let Ok(c) = std::ffi::CString::new(sql) else {
        return false;
    };
    // SAFETY: sqlite3_complete reads a NUL-terminated string and keeps no pointer to it.
    unsafe { rusqlite::ffi::sqlite3_complete(c.as_ptr()) != 0 }
}

/// Whole statements, comment and blank lines before each dropped.
pub fn split_sql_statements(sql_text: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut buf = String::new();
    for line in crate::py::str::splitlines(sql_text, true) {
        let stripped = crate::py::str::strip(line);
        if buf.is_empty() && (stripped.is_empty() || stripped.starts_with("--")) {
            continue;
        }
        buf.push_str(line);
        if complete_statement(&buf) {
            statements.push(crate::py::str::strip(&buf).to_string());
            buf.clear();
        }
    }
    if !crate::py::str::strip(&buf).is_empty() {
        statements.push(crate::py::str::strip(&buf).to_string());
    }
    statements
}

/// `conn.execute(sql)`: prepare one statement and step it once, as Python's cursor does.
pub fn execute(conn: &Connection, sql: &str) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.raw_query();
    rows.next()?;
    Ok(())
}

fn is_busy(err: &rusqlite::Error) -> bool {
    matches!(err, rusqlite::Error::SqliteFailure(e, _) if e.extended_code & 0xff == rusqlite::ffi::SQLITE_BUSY)
}

fn seconds_left(deadline: Instant) -> f64 {
    deadline
        .saturating_duration_since(Instant::now())
        .as_secs_f64()
}

/// The busy timeout between lock waits: 5 s, or the time left before the deadline (0 once it
/// passed) when that is shorter.
fn busy_ms(deadline: Option<Instant>) -> i64 {
    deadline.map_or(BUSY_TIMEOUT_MS, |d| {
        BUSY_TIMEOUT_MS.min(d.saturating_duration_since(Instant::now()).as_millis() as i64)
    })
}

/// `execute_waiting_for_lock(conn, sql, deadline=...)`: each try waits the time left as its busy
/// timeout, at most LOCK_WAIT_SLICE_MS; a busy answer that came back sooner is tried again until
/// the deadline, then one clear OperationalError. The busy timeout goes back to `busy_ms` whatever
/// happens.
pub fn execute_waiting_for_lock(
    conn: &Connection,
    sql: &str,
    deadline: Option<Instant>,
) -> Result<(), SmError> {
    let start = Instant::now();
    let capped = start + Duration::from_secs_f64(WRITE_LOCK_WAIT_S);
    let until = match deadline {
        Some(d) if d < capped => d,
        _ => capped,
    };
    let result = (|| loop {
        // int() truncates; a deadline already past gives max(1, negative) = 1.
        let left_ms = if Instant::now() >= until {
            1
        } else {
            ((seconds_left(until) * 1000.0) as i64).max(1)
        };
        let try_ms = left_ms.min(LOCK_WAIT_SLICE_MS);
        execute(conn, &format!("PRAGMA busy_timeout = {try_ms}"))?;
        match execute(conn, sql) {
            Ok(()) => return Ok(()),
            Err(e) if !is_busy(&e) => return Err(SmError::sqlite(&e)),
            Err(_) if Instant::now() >= until => {
                // Python: `deadline - start` in seconds, then round(x, 1) and :g.
                let waited = until.saturating_duration_since(start).as_secs_f64();
                let shown = crate::py::float::format_g(crate::py::float::round_ndigits(waited, 1)?);
                return Err(SmError::new(
                    crate::error::Base::Sqlite,
                    "OperationalError",
                    format!(
                        "database is locked: another sm process holds the write lock (waited {shown} s); try again"
                    ),
                ));
            }
            Err(_) => {}
        }
        std::thread::sleep(Duration::from_millis(25));
    })();
    let restore = execute(
        conn,
        &format!("PRAGMA busy_timeout = {}", busy_ms(deadline)),
    );
    match (result, restore) {
        (Ok(()), Err(e)) => Err(SmError::sqlite(&e)),
        (r, _) => r,
    }
}

/// `with transaction(conn, deadline=...)`: BEGIN IMMEDIATE (waiting for the lock), COMMIT after
/// `f`, ROLLBACK when `f` fails. Inside an open transaction `f` just runs.
pub fn transaction<T>(
    conn: &Connection,
    deadline: Option<Instant>,
    f: impl FnOnce(&Connection) -> Result<T, SmError>,
) -> Result<T, SmError> {
    if !conn.is_autocommit() {
        return f(conn);
    }
    execute_waiting_for_lock(conn, "BEGIN IMMEDIATE", deadline)?;
    match f(conn) {
        Ok(v) => {
            execute(conn, "COMMIT").map_err(|e| {
                rollback(conn);
                SmError::sqlite(&e)
            })?;
            Ok(v)
        }
        Err(e) => {
            rollback(conn);
            Err(e)
        }
    }
}

/// `if conn.in_transaction: conn.execute("ROLLBACK")` on the error path; the first error wins.
fn rollback(conn: &Connection) {
    if !conn.is_autocommit() {
        let _ = execute(conn, "ROLLBACK");
    }
}

fn user_version(conn: &Connection) -> Result<i64, SmError> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?)
}

/// `apply_migrations(conn, deadline=...)`: the user_version after every newer migration ran.
fn apply_migrations(conn: &Connection, deadline: Option<Instant>) -> Result<i64, SmError> {
    let Some(&(latest, _)) = MIGRATIONS.last() else {
        return Ok(0);
    };
    let current = user_version(conn)?;
    if current >= latest {
        return Ok(current);
    }
    migrate_under_lock(conn, deadline)
}

/// The locked half of `apply_migrations`: the version is read again after
/// BEGIN IMMEDIATE, since another process may have migrated between the unlocked read and the lock.
fn migrate_under_lock(conn: &Connection, deadline: Option<Instant>) -> Result<i64, SmError> {
    transaction(conn, deadline, |conn| {
        let mut current = user_version(conn)?;
        for &(version, sql) in MIGRATIONS {
            if version <= current {
                continue;
            }
            for stmt in split_sql_statements(sql) {
                execute(conn, &stmt)?;
            }
            execute(conn, &format!("PRAGMA user_version = {version}"))?;
            current = version;
        }
        Ok(current)
    })
}

/// `sm_fold(x)`: `core.fold.fold` of a TEXT or NULL argument. Python's fold raises on any other
/// type, and sqlite3 then reports "user-defined function raised exception".
fn register_fold(conn: &Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function(
        "sm_fold",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            use rusqlite::types::ValueRef;
            match ctx.get_raw(0) {
                ValueRef::Null => Ok(crate::core::fold::fold(None)),
                ValueRef::Text(b) => match std::str::from_utf8(b) {
                    Ok(s) => Ok(crate::core::fold::fold(Some(s))),
                    Err(_) => Err(rusqlite::Error::UserFunctionError(Box::new(Raised))),
                },
                _ => Err(rusqlite::Error::UserFunctionError(Box::new(Raised))),
            }
        },
    )
}

/// `sm_word(code, field, ...)`: whether the code stands as a whole word, same case, in any TEXT
/// field (`core::terms::stands_as_word`); a NULL field holds nothing. The `[scopes]`
/// filter of `store::search` calls it.
fn register_word(conn: &Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function(
        "sm_word",
        -1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let code: String = ctx.get(0)?;
            for i in 1..ctx.len() {
                if let Some(field) = ctx.get::<Option<String>>(i)?
                    && crate::core::terms::stands_as_word(&field, &code)
                {
                    return Ok(true);
                }
            }
            Ok(false)
        },
    )
}

/// `sm_epoch(as_of)`: epoch seconds as REAL, or NULL when `as_of` is NULL, non-TEXT, empty, or
/// unparseable. Registered without `SQLITE_DETERMINISTIC` because naive timestamps depend on
/// `SM_UTC_OFFSET` / `TZ`.
fn register_epoch(conn: &Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function("sm_epoch", 1, FunctionFlags::SQLITE_UTF8, |ctx| {
        use rusqlite::types::ValueRef;
        match ctx.get_raw(0) {
            ValueRef::Null => Ok(None::<f64>),
            ValueRef::Text(b) => match std::str::from_utf8(b) {
                Ok(s) => Ok(crate::core::item::parse_as_of_epoch_opt(Some(s))
                    .ok()
                    .flatten()),
                Err(_) => Ok(None),
            },
            _ => Ok(None),
        }
    })
}

#[derive(Debug)]
struct Raised;

impl std::fmt::Display for Raised {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("user-defined function raised exception")
    }
}

impl std::error::Error for Raised {}

/// `connect(db_path, deadline=...)`: the store in WAL mode with every migration applied.
pub fn connect(db_path: &str, deadline: Option<Instant>) -> Result<Connection, SmError> {
    // `str(db_path)`: a str goes to sqlite as given (no expanduser), only the parent folder is made.
    let target = db_path;
    if target != ":memory:" && !target.starts_with("file:") {
        PyPath::new(target)
            .expanduser()?
            .parent()
            .mkdir(true, true)?;
    }
    // sqlite3.connect: READWRITE | CREATE, no URI parsing (uri=False), timeout=5.0.
    let os = crate::py::io::fs_path(target);
    let opened = (|| {
        let conn = Connection::open_with_flags(
            &os,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| SmError::sqlite_open(&e, &os))?;
        conn.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS as u64))?;
        register_fold(&conn)?;
        register_word(&conn)?;
        register_epoch(&conn)?;
        execute_waiting_for_lock(&conn, "PRAGMA journal_mode = WAL", deadline)?;
        execute(&conn, "PRAGMA foreign_keys = ON")?;
        apply_migrations(&conn, deadline)?;
        Ok(conn)
    })();
    // A file SQLite cannot open, write or read as a database: say which file.
    opened.map_err(|mut e: SmError| {
        let unopenable = e.class == "DatabaseError"
            || [
                "unable to open database file",
                "attempt to write a readonly database",
            ]
            .contains(&e.msg.as_str());
        if e.is(crate::error::Base::Sqlite) && unopenable {
            e.msg = format!("cannot open {target}: {}", e.msg);
        }
        e
    })
}

/// The wall time SQLite's own busy handler takes to give up after `ms` on a held write lock,
/// measured in a thread beside the lock wait a test times. The handler counts its nominal sleeps,
/// not wall time (sqlite3.c `sqliteDefaultBusyCallback`), so on a host whose sleeps run long every
/// lock wait stretches alike: on the macos-latest runner Python 3.15.2's sqlite3 took 1.70 s for
/// 1000 ms and 16.98 s for 12000 ms, on ubuntu-latest 1.00 s and 12.02 s.
/// A bound of `cap + margin` on a lock wait is `reference + margin` with this reference.
#[cfg(test)]
pub(crate) fn busy_reference(db: &str, ms: u64) -> std::thread::JoinHandle<f64> {
    let db = db.to_string();
    std::thread::spawn(move || {
        let c = Connection::open(&db).unwrap();
        c.busy_timeout(Duration::from_millis(ms)).unwrap();
        let started = Instant::now();
        let err = execute(&c, "BEGIN IMMEDIATE").unwrap_err();
        assert!(is_busy(&err), "{err}");
        started.elapsed().as_secs_f64()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pragma(conn: &Connection, name: &str) -> String {
        conn.query_row(&format!("PRAGMA {name}"), [], |r| {
            r.get_ref(0).map(|v| match v {
                rusqlite::types::ValueRef::Integer(i) => i.to_string(),
                rusqlite::types::ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
                _ => String::new(),
            })
        })
        .unwrap()
    }

    #[test]
    fn sm_fold_is_deterministic_so_an_index_can_use_it() {
        // SQLite refuses a non-deterministic function in an index expression.
        let conn = connect(":memory:", None).unwrap();
        conn.execute_batch("CREATE TABLE t (x TEXT); CREATE INDEX t_fold ON t (sm_fold(x));")
            .unwrap();
    }

    #[test]
    fn sm_epoch_is_non_deterministic_and_handles_edge_inputs() {
        let conn = connect(":memory:", None).unwrap();
        // Without SQLITE_DETERMINISTIC, SQLite refuses sm_epoch in an expression index.
        let idx_err = conn
            .execute_batch("CREATE TABLE t (x TEXT); CREATE INDEX t_ep ON t (sm_epoch(x));")
            .unwrap_err();
        assert!(
            idx_err.to_string().contains("non-deterministic"),
            "{idx_err}"
        );
        let q = |sql: &str| -> Option<f64> { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(q("SELECT sm_epoch('1970-01-01T00:00:00Z')"), Some(0.0));
        assert_eq!(q("SELECT sm_epoch('1969-12-31T23:59:59Z')"), Some(-1.0));
        assert_eq!(
            q("SELECT sm_epoch('2026-06-15T12:00:00+02:00')"),
            Some(1_781_517_600.0)
        );
        assert_eq!(q("SELECT sm_epoch(NULL)"), None);
        assert_eq!(q("SELECT sm_epoch('')"), None);
        assert_eq!(q("SELECT sm_epoch('not-a-timestamp')"), None);
        assert_eq!(q("SELECT sm_epoch('0001-01-01T00:00+08:00')"), None);
        assert_eq!(q("SELECT sm_epoch(12345)"), None);
        assert_eq!(q("SELECT sm_epoch(x'ff')"), None);
    }

    #[test]
    fn connect_pragmas_and_migrations() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("sub/dir/memory.db");
        let conn = connect(db.to_str().unwrap(), None).unwrap();
        assert_eq!(pragma(&conn, "journal_mode").to_lowercase(), "wal");
        assert_eq!(pragma(&conn, "busy_timeout"), "5000");
        assert_eq!(pragma(&conn, "foreign_keys"), "1");
        assert_eq!(pragma(&conn, "user_version"), "3");
        drop(conn);
        // A second connect finds user_version 3 and runs no migration.
        let conn = connect(db.to_str().unwrap(), None).unwrap();
        assert_eq!(apply_migrations(&conn, None).unwrap(), 3);
    }

    // Two processes open a new store at once. The loser read user_version 0
    // before the winner migrated, then gets the lock; with no re-read it runs 0001 again and fails
    // with `table items already exists`.
    #[test]
    fn a_stale_user_version_read_under_the_lock_runs_no_migration() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("memory.db");
        let loser = Connection::open(&db).unwrap();
        assert_eq!(user_version(&loser).unwrap(), 0);
        let winner = connect(db.to_str().unwrap(), None).unwrap();
        winner
            .execute(
                "INSERT INTO items (id, repo, kind, title, body, tag, as_of, content_hash) \
                 VALUES ('m_1', 'r', 'fact', 'T', 'b', 'decided', '2026-01-01T00:00:00+00:00', 'h')",
                [],
            )
            .unwrap();
        assert_eq!(migrate_under_lock(&loser, None).unwrap(), 3);
        assert!(loser.is_autocommit());
        assert_eq!(
            loser
                .query_row("SELECT id FROM items", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "m_1"
        );
        assert_eq!(user_version(&loser).unwrap(), 3);
    }

    #[test]
    fn fold_runs_in_the_insert_trigger() {
        let conn = connect(":memory:", None).unwrap();
        conn.execute_batch(
            "INSERT INTO items (id, repo, kind, title, body, tag, as_of, content_hash, refs, tags)
             VALUES ('m_1', 'r', 'fact', 'Đorđe café', 'Crème BRÛLÉE', 'decided', 'x', 'h', 'A-1', 'tíc')",
        )
        .unwrap();
        let row: (String, String, String, String) = conn
            .query_row("SELECT refs, title, tags, body FROM items_fold", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .unwrap();
        // fold lowers and strips marks; đ becomes d (core/fold.rs).
        assert_eq!(
            row,
            (
                "a-1".into(),
                "dorde cafe".into(),
                "tic".into(),
                "creme brulee".into()
            )
        );
        let raw: String = conn
            .query_row("SELECT title FROM items_raw", [], |r| r.get(0))
            .unwrap();
        assert_eq!(raw, "Đorđe café");
        let err = conn
            .query_row("SELECT sm_fold(1)", [], |r| r.get::<_, String>(0))
            .unwrap_err();
        assert_eq!(
            SmError::sqlite(&err).python_text(),
            "OperationalError: user-defined function raised exception"
        );
        assert_eq!(
            conn.query_row("SELECT sm_fold(NULL)", [], |r| r.get::<_, String>(0))
                .unwrap(),
            ""
        );
        let err = conn
            .query_row("SELECT sm_fold(x'ff')", [], |r| r.get::<_, String>(0))
            .unwrap_err();
        assert_eq!(
            SmError::sqlite(&err).msg,
            "user-defined function raised exception"
        );
    }

    #[test]
    fn split_keeps_triggers_whole_and_drops_comments() {
        let sql = "-- head\n\nCREATE TABLE t (x);\nCREATE TRIGGER a AFTER INSERT ON t BEGIN\n  SELECT 1;\nEND;\n  -- tail\nSELECT 2";
        assert_eq!(
            split_sql_statements(sql),
            [
                "CREATE TABLE t (x);",
                "CREATE TRIGGER a AFTER INSERT ON t BEGIN\n  SELECT 1;\nEND;",
                "SELECT 2"
            ]
        );
        assert_eq!(split_sql_statements(MIGRATIONS[0].1).len(), 7);
        assert_eq!(split_sql_statements(MIGRATIONS[1].1).len(), 3);
    }

    fn hold_lock(
        db: &str,
        secs: f64,
    ) -> (std::thread::JoinHandle<()>, std::sync::mpsc::Receiver<()>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let db = db.to_string();
        let h = std::thread::spawn(move || {
            let c = Connection::open(&db).unwrap();
            c.execute_batch("BEGIN IMMEDIATE").unwrap();
            tx.send(()).unwrap();
            std::thread::sleep(Duration::from_secs_f64(secs));
            c.execute_batch("ROLLBACK").unwrap();
        });
        (h, rx)
    }

    #[test]
    fn transaction_gives_up_on_a_held_write_lock_at_the_deadline() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("lock.db").to_string_lossy().into_owned();
        let conn = connect(&db, None).unwrap();
        // The holder outlasts both waits, even when each runs twice its nominal 1 s.
        let (h, rx) = hold_lock(&db, 4.0);
        rx.recv().unwrap();
        let reference = busy_reference(&db, 1000);
        let started = Instant::now();
        let err =
            transaction(&conn, Some(started + Duration::from_secs(1)), |_| Ok(())).unwrap_err();
        let waited = started.elapsed().as_secs_f64();
        let reference = reference.join().unwrap();
        assert_eq!(
            err.python_text(),
            "OperationalError: database is locked: another sm process holds the write lock (waited 1 s); try again"
        );
        // Python bounds 0.9 <= waited < 2.0, the 1 s deadline plus 1 s, on a host where 1 s of
        // busy handler takes 1 s.
        assert!(
            (0.9..reference + 1.0).contains(&waited),
            "{waited} {reference}"
        );
        // Restored to busy_ms of the passed deadline; the last try set it to 1.
        assert_eq!(pragma(&conn, "busy_timeout"), "0");
        h.join().unwrap();
    }

    #[test]
    fn connect_with_a_deadline_waits_only_until_it_for_a_new_store_lock() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("new.db").to_string_lossy().into_owned();
        let holder = Connection::open(&db).unwrap();
        holder
            .execute_batch("CREATE TABLE t (x); BEGIN IMMEDIATE")
            .unwrap();
        let reference = busy_reference(&db, 1000);
        let started = Instant::now();
        let err = connect(&db, Some(started + Duration::from_millis(1000))).unwrap_err();
        let waited = started.elapsed().as_secs_f64();
        let reference = reference.join().unwrap();
        holder.execute_batch("ROLLBACK").unwrap();
        assert!(
            err.msg.starts_with(
                "database is locked: another sm process holds the write lock (waited "
            ),
            "{}",
            err.msg
        );
        // Python: 0.9 <= waited < 2.0; see busy_reference.
        assert!(
            (0.9..reference + 1.0).contains(&waited),
            "{waited} {reference}"
        );
    }

    #[test]
    fn connect_with_a_deadline_keeps_only_the_time_left_as_its_busy_timeout() {
        // Every read after connect waits at most this long for a lock. A lock that blocks a WAL
        // reader after its connect (an EXCLUSIVE locking-mode holder) cannot start while the
        // reader is open, so the pragma is what a test can observe.
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("m.db").to_string_lossy().into_owned();
        drop(connect(&db, None).unwrap());
        let late = connect(&db, Some(Instant::now())).unwrap();
        assert_eq!(pragma(&late, "busy_timeout"), "0");
        let conn = connect(&db, Some(Instant::now() + Duration::from_millis(1000))).unwrap();
        let ms: i64 = pragma(&conn, "busy_timeout").parse().unwrap();
        assert!((900..=1000).contains(&ms), "{ms}");
        let far = connect(&db, Some(Instant::now() + Duration::from_secs(60))).unwrap();
        assert_eq!(pragma(&far, "busy_timeout"), "5000");
    }

    #[test]
    fn transaction_waits_for_a_lock_that_clears_and_rolls_back_on_error() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("lock.db").to_string_lossy().into_owned();
        let conn = connect(&db, None).unwrap();
        let (h, rx) = hold_lock(&db, 0.5);
        rx.recv().unwrap();
        let started = Instant::now();
        transaction(&conn, Some(started + Duration::from_secs(3)), |c| {
            Ok(execute(c, "CREATE TABLE t (x)")?)
        })
        .unwrap();
        let waited = started.elapsed().as_secs_f64();
        assert!((0.4..2.0).contains(&waited), "{waited}");
        h.join().unwrap();
        let err = transaction(&conn, None, |c| {
            execute(c, "INSERT INTO t VALUES (1)")?;
            Err::<(), _>(SmError::value("boom"))
        })
        .unwrap_err();
        assert_eq!(err.python_text(), "ValueError: boom");
        assert!(conn.is_autocommit());
        assert_eq!(
            conn.query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        // Inside an open transaction the body just runs.
        execute(&conn, "BEGIN").unwrap();
        transaction(&conn, None, |c| Ok(execute(c, "INSERT INTO t VALUES (2)")?)).unwrap();
        assert!(!conn.is_autocommit());
    }

    #[test]
    fn lock_wait_raises_a_non_busy_error_at_once_and_restores_the_busy_timeout() {
        let d = tempfile::tempdir().unwrap();
        let conn = connect(d.path().join("m.db").to_str().unwrap(), None).unwrap();
        execute(&conn, "BEGIN").unwrap();
        let started = Instant::now();
        let err = execute_waiting_for_lock(&conn, "BEGIN IMMEDIATE", None).unwrap_err();
        assert_eq!(
            err.python_text(),
            "OperationalError: cannot start a transaction within a transaction"
        );
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(pragma(&conn, "busy_timeout"), "5000");
    }

    #[test]
    fn connect_errors() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("f");
        std::fs::write(&file, "x").unwrap();
        let err = connect(&format!("{}/x/memory.db", file.display()), None).unwrap_err();
        assert_eq!(
            err.python_text(),
            format!(
                "NotADirectoryError: [Errno 20] Not a directory: '{}/x'",
                file.display()
            )
        );
        let err = connect(&format!("{}/memory.db", file.display()), None).unwrap_err();
        assert_eq!(
            err.python_text(),
            format!(
                "FileExistsError: [Errno 17] File exists: '{}'",
                file.display()
            )
        );
        let err = connect(d.path().to_str().unwrap(), None).unwrap_err();
        assert_eq!(
            err.python_text(),
            format!(
                "OperationalError: cannot open {}: unable to open database file",
                d.path().display()
            )
        );
        let bad = d.path().join("bad.db");
        std::fs::write(&bad, "not a database, only text that is long enough").unwrap();
        let err = connect(bad.to_str().unwrap(), None).unwrap_err();
        assert_eq!(
            err.python_text(),
            format!(
                "DatabaseError: cannot open {}: file is not a database",
                bad.display()
            )
        );
    }
}
