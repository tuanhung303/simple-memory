use std::time::Duration;

use rusqlite::Connection;
use rusqlite::backup::{Backup, StepResult};

use crate::clock;
use crate::config::{get_db_path, get_home};
use crate::error::{Base, SmError};
use crate::py::int::Int;
use crate::py::path::PyPath;
use crate::py::str::isdecimal;
use crate::sources::mtime;

/// The reply of `run_backup`: `{"path", "quick_check", "items", "deleted"}` in that order.
#[derive(Clone, Debug, PartialEq)]
pub struct BackupResult {
    pub path: String,
    pub quick_check: String,
    pub items: i64,
    pub deleted: Vec<String>,
}

/// `AUTO_RE.match(name)` for `^memory-\d{8}-\d{6}-auto\.db$`: `\d` is any Unicode decimal digit,
/// and `$` also matches before one final `\n`.
fn auto_re_match(name: &str) -> bool {
    let name = name.strip_suffix('\n').unwrap_or(name);
    let Some(rest) = name.strip_prefix("memory-") else {
        return false;
    };
    let Some(rest) = rest.strip_suffix("-auto.db") else {
        return false;
    };
    let chars: Vec<char> = rest.chars().collect();
    chars.len() == 15
        && chars[8] == '-'
        && chars[..8].iter().chain(&chars[9..]).all(|c| isdecimal(*c))
}

/// `auto_backups(home)`: the `*-auto.db` copies, oldest first (the name holds the time); no
/// other file counts. Sorted as `Path` sorts on posix: by the list of parts.
pub fn auto_backups(home: Option<&str>) -> Result<Vec<PyPath>, SmError> {
    let bdir = get_home(home)?.join("backups");
    if !bdir.is_dir() {
        return Ok(Vec::new());
    }
    let mut found: Vec<PyPath> = bdir
        .iterdir()?
        .into_iter()
        .filter(|p| p.is_file() && auto_re_match(p.name()))
        .collect();
    found.sort_by(|a, b| a.as_str().split('/').cmp(b.as_str().split('/')));
    Ok(found)
}

/// `backup_age_hours(home)`: hours since the newest auto copy was written, or None.
pub fn backup_age_hours(home: Option<&str>) -> Result<Option<f64>, SmError> {
    let found = auto_backups(home)?;
    let Some(newest) = found.last() else {
        return Ok(None);
    };
    Ok(Some((clock::now_epoch() - mtime(newest)?) / 3600.0))
}

fn open(path: &PyPath) -> Result<Connection, SmError> {
    let os = path.to_os();
    Connection::open(&os).map_err(|e| SmError::sqlite_open(&e, &os))
}

/// The backup, the check and the count, on two open connections (the `try` body).
fn copy_and_check(src: &Connection, out: &mut Connection) -> Result<(String, i64), SmError> {
    // `src.backup(out)`: pages=-1 copies every page in one step; on BUSY or LOCKED Python
    // sleeps 250 ms and steps again (Modules/_sqlite/connection.c `backup_impl`).
    {
        let b = Backup::new(src, out).map_err(|e| SmError::sqlite(&e))?;
        loop {
            match b.step(-1).map_err(|e| SmError::sqlite(&e))? {
                StepResult::Done => break,
                StepResult::More => {}
                _ => std::thread::sleep(Duration::from_millis(250)),
            }
        }
    }
    let check: String = out
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .map_err(|e| SmError::sqlite(&e))?;
    let items: i64 = out
        .query_row("SELECT count(*) FROM items", [], |r| r.get(0))
        .map_err(|e| SmError::sqlite(&e))?;
    Ok((check, items))
}

/// `run_backup(keep=..., home=...)`: copy memory.db through the SQLite backup API, check the
/// copy, keep the newest `keep` auto copies.
pub fn run_backup(keep: &Int, home: Option<&str>) -> Result<BackupResult, SmError> {
    let below_one = match keep {
        Int::I64(k) => *k < 1,
        Int::Big(t) => t.starts_with('-'),
    };
    if below_one {
        return Err(SmError::value(format!(
            "--keep must be at least 1, got {}",
            keep.text()
        )));
    }
    let db = get_db_path(home)?;
    if !db.is_file() {
        return Err(SmError::new(
            Base::Os,
            "FileNotFoundError",
            format!("memory.db not found: {}", db.as_str()),
        ));
    }
    let bdir = get_home(home)?.join("backups");
    bdir.mkdir(true, true)?;
    let stamp = clock::now_local().format("%Y%m%d-%H%M%S");
    let dest = bdir.join(&format!("memory-{stamp}-auto.db"));
    let src = open(&db)?;
    let mut out = open(&dest)?;
    let res = copy_and_check(&src, &mut out);
    drop(out);
    drop(src);
    let (check, items) = res?;
    if check != "ok" {
        return Err(SmError::runtime(format!(
            "backup quick_check failed on {}: {check}",
            dest.as_str()
        )));
    }
    let found = auto_backups(home)?;
    let cut = match keep {
        Int::I64(k) => found
            .len()
            .saturating_sub(usize::try_from(*k).unwrap_or(usize::MAX)),
        Int::Big(_) => 0,
    };
    let mut deleted = Vec::new();
    for old in &found[..cut] {
        let os = old.to_os();
        std::fs::remove_file(&os).map_err(|e| SmError::from_io(&e, Some(&os)))?;
        deleted.push(old.as_str().to_string());
    }
    Ok(BackupResult {
        path: dest.as_str().to_string(),
        quick_check: check,
        items,
        deleted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> (tempfile::TempDir, String) {
        let t = tempfile::tempdir().expect("a temp dir");
        let h = t.path().to_str().expect("a utf-8 temp path").to_string();
        (t, h)
    }

    fn store(home: &str, sql: &str) {
        let c = Connection::open(format!("{home}/memory.db")).expect("open");
        c.execute_batch(sql).expect("seed sql");
    }

    fn names(home: &str) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(format!("{home}/backups"))
            .expect("backups")
            .map(|e| e.expect("entry").file_name().into_string().expect("utf-8"))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn copies_checks_and_keeps_the_newest_auto_copies() {
        let (_t, h) = home();
        store(
            &h,
            "CREATE TABLE items(title); INSERT INTO items VALUES ('seed item');",
        );
        let bdir = format!("{h}/backups");
        std::fs::create_dir(&bdir).unwrap();
        let olds: Vec<String> = (1..8)
            .map(|d| format!("memory-2026010{d}-000000-auto.db"))
            .collect();
        let others = [
            "memory-20260101-000000-pre-refs.db",
            "memory-20260101-000000-auto.db-wal",
            "config-x.toml",
        ];
        for n in olds.iter().map(String::as_str).chain(others) {
            std::fs::write(format!("{bdir}/{n}"), b"").unwrap();
        }
        let res = run_backup(&Int::I64(7), Some(&h)).unwrap();
        assert_eq!((res.quick_check.as_str(), res.items), ("ok", 1));
        assert_eq!(
            res.deleted,
            vec![format!("{bdir}/memory-20260101-000000-auto.db")]
        );
        let new_name = res.path.rsplit('/').next().unwrap().to_string();
        let mut want: Vec<String> = olds[1..].to_vec();
        want.extend([new_name.clone()]);
        want.extend(others.iter().map(|s| s.to_string()));
        want.sort();
        assert_eq!(names(&h), want);
        assert!(auto_re_match(&new_name), "{new_name}");
        let copy = Connection::open(&res.path).unwrap();
        let title: String = copy
            .query_row("SELECT title FROM items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(title, "seed item");
    }

    #[test]
    fn refuses_keep_below_one_and_a_missing_store() {
        let (_t, h) = home();
        let e = run_backup(&Int::I64(0), Some(&h)).unwrap_err();
        assert_eq!(
            e.python_text(),
            "ValueError: --keep must be at least 1, got 0"
        );
        let big = Int::Big(format!("-{}", "9".repeat(25)));
        let e = run_backup(&big, Some(&h)).unwrap_err();
        assert_eq!(
            e.msg,
            format!("--keep must be at least 1, got -{}", "9".repeat(25))
        );
        let e = run_backup(&Int::I64(7), Some(&h)).unwrap_err();
        assert_eq!(
            e.python_text(),
            format!("FileNotFoundError: memory.db not found: {h}/memory.db")
        );
        assert!(e.is(Base::Os));
        assert!(!std::path::Path::new(&format!("{h}/backups")).exists());
    }

    #[test]
    fn a_huge_keep_deletes_nothing_and_keep_one_leaves_only_the_newest() {
        let (_t, h) = home();
        store(&h, "CREATE TABLE items(title);");
        std::fs::create_dir(format!("{h}/backups")).unwrap();
        std::fs::write(format!("{h}/backups/memory-20200101-000000-auto.db"), b"").unwrap();
        let first = run_backup(&Int::Big("9".repeat(25)), Some(&h)).unwrap();
        assert_eq!((first.items, first.deleted.len()), (0, 0));
        let res = run_backup(&Int::I64(1), Some(&h)).unwrap();
        // The name stamps whole seconds: two calls in one second share it and the second
        // overwrites the first; across a second boundary the first is one more old copy.
        let mut old = vec![format!("{h}/backups/memory-20200101-000000-auto.db")];
        if first.path != res.path {
            old.push(first.path);
        }
        assert_eq!(res.deleted, old);
        assert_eq!(
            names(&h),
            [res.path.rsplit('/').next().unwrap().to_string()]
        );
    }

    #[test]
    fn a_copy_that_fails_quick_check_is_a_runtime_error() {
        let (_t, h) = home();
        store(
            &h,
            "PRAGMA page_size=1024; CREATE TABLE items(title); INSERT INTO items VALUES ('a');
             CREATE TABLE t(x); CREATE INDEX tx ON t(x);
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<200) INSERT INTO t SELECT i FROM n;",
        );
        let root: i64 = Connection::open(format!("{h}/memory.db"))
            .unwrap()
            .query_row(
                "SELECT rootpage FROM sqlite_master WHERE name='tx'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let path = format!("{h}/memory.db");
        let mut bytes = std::fs::read(&path).unwrap();
        let at = (root as usize - 1) * 1024;
        // Page header: interior or leaf index page; drop its cell count to 0 so the index holds no rows.
        bytes[at + 3] = 0;
        bytes[at + 4] = 0;
        std::fs::write(&path, bytes).unwrap();
        let e = run_backup(&Int::I64(7), Some(&h)).unwrap_err();
        assert_eq!(e.python_name(), "RuntimeError");
        let prefix = format!("backup quick_check failed on {h}/backups/memory-");
        assert!(e.msg.starts_with(&prefix), "{}", e.msg);
        assert!(!e.msg.ends_with(": ok"), "{}", e.msg);
    }

    #[test]
    fn auto_backups_match_unicode_digits_and_a_final_newline_and_sort_by_name() {
        let (_t, h) = home();
        let bdir = format!("{h}/backups");
        std::fs::create_dir(&bdir).unwrap();
        for n in [
            "memory-20260102-000000-auto.db",
            "memory-\u{0662}0260101-000000-auto.db",
            "memory-20260101-000000-auto.db\n",
            "memory-20260101-000000-auto.dbx",
            "memory-2026010-0000000-auto.db",
            "memory-202601010000000-auto.db",
            "memory-2026010a-000000-auto.db",
            "memory-20260101-0000000-auto.db",
            "20260101-000000-auto.db",
            "memory-20260101-000000",
            "notes.txt",
        ] {
            std::fs::write(format!("{bdir}/{n}"), b"").unwrap();
        }
        std::fs::create_dir(format!("{bdir}/memory-20260103-000000-auto.db")).unwrap();
        std::os::unix::fs::symlink("missing", format!("{bdir}/memory-20260104-000000-auto.db"))
            .unwrap();
        std::os::unix::fs::symlink(
            "notes.txt",
            format!("{bdir}/memory-20260105-000000-auto.db"),
        )
        .unwrap();
        let got: Vec<String> = auto_backups(Some(&h))
            .unwrap()
            .iter()
            .map(|p| p.name().to_string())
            .collect();
        assert_eq!(
            got,
            [
                "memory-20260101-000000-auto.db\n",
                "memory-20260102-000000-auto.db",
                "memory-20260105-000000-auto.db",
                "memory-\u{0662}0260101-000000-auto.db",
            ]
        );
    }

    #[test]
    fn backup_age_is_none_without_copies_or_with_only_other_files() {
        let (_t, h) = home();
        assert_eq!(backup_age_hours(Some(&h)).unwrap(), None);
        std::fs::create_dir(format!("{h}/backups")).unwrap();
        std::fs::write(format!("{h}/backups/notes.txt"), b"").unwrap();
        assert_eq!(backup_age_hours(Some(&h)).unwrap(), None);
    }

    /// The age counts from the newest copy by name, whose mtime is `whole - 7200.25` s (a quarter
    /// second kept): `sources::mtime` returns it to the bit, and the age is the time since it. Without the nanoseconds the age is 0.25 s too
    /// long, which falls outside the window between the two clock reads.
    #[test]
    fn backup_age_keeps_the_sub_second_part_of_the_mtime() {
        let (_t, h) = home();
        std::fs::create_dir(format!("{h}/backups")).unwrap();
        let p = format!("{h}/backups/memory-20260101-000000-auto.db");
        std::fs::write(&p, b"").unwrap();
        // Newest by name wins, though this copy was written later.
        std::fs::write(format!("{h}/backups/memory-20250101-000000-auto.db"), b"").unwrap();
        let mtime = clock::now_epoch().floor() - 7200.0 + 0.25;
        let when = std::time::UNIX_EPOCH + Duration::from_secs_f64(mtime);
        std::fs::File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(when)
            .unwrap();
        assert_eq!(crate::sources::mtime(&PyPath::new(&p)).unwrap(), mtime);
        let before = clock::now_epoch();
        let age_h = backup_age_hours(Some(&h)).unwrap().unwrap();
        let after = clock::now_epoch();
        let age_s = age_h * 3600.0;
        assert!(
            before - mtime - 1e-3 <= age_s && age_s <= after - mtime + 1e-3,
            "{age_s} not in [{}, {}]",
            before - mtime,
            after - mtime
        );
    }

    /// The source is write-locked by another connection, so the first backup step answers BUSY
    /// (the source has no busy timeout). The copy must wait 250 ms and step again until the
    /// writer commits, and then holds the writer's row. The writer commits only after the
    /// source's busy handler ran, so the lock outlasts the first step whatever the machine load.
    #[test]
    fn a_locked_source_is_retried_until_the_writer_commits() {
        use std::sync::{Mutex, mpsc};
        static BUSY: Mutex<Option<mpsc::Sender<()>>> = Mutex::new(None);
        fn on_busy(_tries: i32) -> bool {
            if let Some(tx) = BUSY.lock().unwrap().as_ref() {
                tx.send(()).unwrap();
            }
            false
        }
        let (_t, h) = home();
        store(
            &h,
            "CREATE TABLE items(title); INSERT INTO items VALUES ('seed');",
        );
        let path = format!("{h}/memory.db");
        let (locked_tx, locked_rx) = mpsc::channel();
        let (busy_tx, busy_rx) = mpsc::channel();
        *BUSY.lock().unwrap() = Some(busy_tx);
        let writer = std::thread::spawn({
            let path = path.clone();
            move || {
                let w = Connection::open(&path).unwrap();
                w.execute_batch("BEGIN EXCLUSIVE; INSERT INTO items VALUES ('late');")
                    .unwrap();
                locked_tx.send(()).unwrap();
                let met = busy_rx.recv_timeout(Duration::from_secs(5)).is_ok();
                w.execute_batch("COMMIT").unwrap();
                met
            }
        });
        locked_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut src = Connection::open(&path).unwrap();
        src.busy_handler(Some(on_busy)).unwrap();
        let mut out = Connection::open(format!("{h}/copy.db")).unwrap();
        let res = copy_and_check(&src, &mut out);
        *BUSY.lock().unwrap() = None;
        assert!(
            writer.join().unwrap(),
            "the first backup step never met the lock"
        );
        assert_eq!(res.unwrap(), ("ok".to_string(), 2));
        let mut q = out
            .prepare("SELECT title FROM items ORDER BY title")
            .unwrap();
        let titles: Vec<String> = q
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(titles, ["late", "seed"]);
    }
}
