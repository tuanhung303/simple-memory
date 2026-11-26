//! `sm import catch-up`: the imports, retirements, backup and prune that no agent remembers to
//! run. Each part raises on failure; one failure never stops the next part, except a failed
//! backup, which skips every part that writes memory.db. Each run appends one F2 row to
//! `<home>/catchup.log`.

use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::time::Instant;

use indexmap::IndexMap;

use crate::config::{aliases, get_db_path, get_home, load_config};
use crate::error::SmError;
use crate::py::path::{Errors, PyPath};
use crate::py::str::{isdecimal, slice, split, splitlines};
use crate::py::value::PyValue;
use crate::service::backup::{backup_age_hours, run_backup};
use crate::service::capture::{Run, run_process};
use crate::service::doctor::{
    find_repo_dir, import_lag, import_lag_by_repo, is_missing_entry, tasklog_repo_dirs,
};
use crate::service::import_::{cfg_on, cfg2, float_of, run_automemory_import, run_import};
use crate::store::connect::connect;
use crate::store::items::retire_item;

pub const PRUNE_DAYS: f64 = 14.0;
pub const LOG_MAX_BYTES: u64 = 1_000_000;
pub const LOG_KEEP_ROWS: usize = 500;

pub type Counts = IndexMap<&'static str, i64>;

fn counts(pairs: &[(&'static str, i64)]) -> Counts {
    pairs.iter().copied().collect()
}

fn automemory(home: &PyPath, cfg: &PyValue, dry_run: bool) -> Result<Counts, SmError> {
    if !cfg_on(cfg, "automemory", "enabled")? {
        return Ok(counts(&[("skipped", 1)]));
    }
    let r = run_automemory_import(None, dry_run, Some(home.as_str()))?;
    Ok(counts(&[
        ("added", r.added),
        ("updated", r.updated),
        ("retired", r.retired),
        ("unchanged", r.unchanged),
        ("unreadable", r.unreadable.len() as i64),
        ("unmapped", r.unmapped.len() as i64),
    ]))
}

/// `_imports`: import each task log and handoff that `sm doctor --imports` lists as `not_stored:`
/// (a task log whose body changed since its import is one); each task log it lists as
/// `unreadable:` counts as a failed row.
fn imports(home: &PyPath, cfg: &PyValue, dry_run: bool) -> Result<Counts, SmError> {
    let conn = connect(get_db_path(Some(home.as_str()))?.as_str(), None)?;
    let lag = import_lag(&conn, cfg, Some(home.as_str()))?;
    drop(conn);
    let mut c = counts(&[
        ("not_stored", lag.not_stored.len() as i64),
        ("inserted", 0),
        ("failed_rows", lag.unreadable.len() as i64),
    ]);
    for path in &lag.not_stored {
        let kind = if path.ends_with(".yaml") {
            "tasklog"
        } else {
            "handoff"
        };
        let res = run_import(kind, path, dry_run, false, Some(home.as_str()))?;
        c["inserted"] += res.inserted;
        c["failed_rows"] += res.failed_rows.len() as i64;
    }
    Ok(c)
}

fn git(repo_dir: &PyPath, args: &[&str]) -> Vec<String> {
    let mut argv = vec![
        "git".to_string(),
        "-C".to_string(),
        repo_dir.as_str().to_string(),
    ];
    argv.extend(args.iter().map(|a| a.to_string()));
    argv
}

fn in_git_repo(repo_dir: &PyPath) -> Result<bool, SmError> {
    let has_git_marker = repo_dir.join(".git").to_os().symlink_metadata().is_ok();
    let r = Run {
        argv: git(repo_dir, &["rev-parse", "--git-dir"]),
        timeout: Some(10.0),
        text: true,
        ..Default::default()
    };
    let res = run_process(&r)?;
    if res.returncode == 0 {
        return Ok(true);
    }
    if !has_git_marker
        && res
            .stderr
            .to_ascii_lowercase()
            .contains("not a git repository")
    {
        return Ok(false);
    }
    let err = crate::py::str::strip(&res.stderr);
    Err(SmError::runtime(if err.is_empty() {
        format!(
            "git rev-parse failed in {repo_dir} with code {}",
            res.returncode
        )
    } else {
        format!("git rev-parse failed in {repo_dir}: {err}")
    }))
}

/// The checkouts of the repo; none for a folder outside git.
fn worktrees(repo_dir: &PyPath) -> Result<Vec<PyPath>, SmError> {
    if !in_git_repo(repo_dir)? {
        return Ok(Vec::new());
    }
    let r = Run {
        argv: git(repo_dir, &["worktree", "list", "--porcelain"]),
        timeout: Some(10.0),
        text: true,
        check: true,
        ..Default::default()
    };
    let out = run_process(&r)?.stdout;
    Ok(splitlines(&out, false)
        .into_iter()
        .filter_map(|l| l.strip_prefix("worktree ").map(PyPath::new))
        .collect())
}

/// The branch tips of the repo; none for a folder outside git.
fn branch_refs(repo_dir: &PyPath) -> Result<Vec<String>, SmError> {
    if !in_git_repo(repo_dir)? {
        return Ok(Vec::new());
    }
    let r = Run {
        argv: git(
            repo_dir,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/heads",
                "refs/remotes",
            ],
        ),
        timeout: Some(10.0),
        text: true,
        check: true,
        ..Default::default()
    };
    Ok(split(&run_process(&r)?.stdout)
        .into_iter()
        .map(String::from)
        .collect())
}

/// One `git cat-file --batch-check` over all refs.
fn at_a_branch_tip(repo_dir: &PyPath, refs: &[String], rel_path: &str) -> Result<bool, SmError> {
    if refs.is_empty() {
        return Ok(false);
    }
    let input: String = refs.iter().map(|r| format!("{r}:./{rel_path}\n")).collect();
    let r = Run {
        argv: git(repo_dir, &["cat-file", "--batch-check"]),
        input: Some(input),
        timeout: Some(10.0),
        text: true,
        check: true,
        ..Default::default()
    };
    let out = run_process(&r)?.stdout;
    Ok(splitlines(&out, false)
        .into_iter()
        .any(|l| split(l).get(1) == Some(&"blob")))
}

/// `_TASK_ID_RE.match(s)` for `^(\d{4})(\d{2})(\d{2})-(.+)$`: the date digits and the name.
pub fn task_id_match(s: &str) -> Option<(String, String, String, String)> {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 10 || !chars[..8].iter().all(|c| isdecimal(*c)) || chars[8] != '-' {
        return None;
    }
    let rest: String = chars[9..].iter().collect();
    let name = match rest.find('\n') {
        None => rest.as_str(),
        Some(i) if i + 1 == rest.len() => &rest[..i],
        Some(_) => return None,
    };
    if name.is_empty() {
        return None;
    }
    let g = |a: usize, b: usize| chars[a..b].iter().collect::<String>();
    Some((g(0, 4), g(4, 6), g(6, 8), name.to_string()))
}

/// `_retire_missing`: retire each active task item whose file is gone from every checkout and
/// branch of its repo.
fn retire_missing(home: &PyPath, cfg: &PyValue, dry_run: bool) -> Result<Counts, SmError> {
    let tl_roots = cfg2(cfg, "upkeep", "tasklog_roots")?;
    let al = aliases(cfg);
    let mut repo_dirs: IndexMap<String, Option<PyPath>> =
        tasklog_repo_dirs(tl_roots, &al, Some(home.as_str()))?
            .into_iter()
            .map(|(k, v)| (k, Some(v)))
            .collect();
    let mut wts: IndexMap<String, Vec<PyPath>> = IndexMap::new();
    let mut refs: IndexMap<String, Vec<String>> = IndexMap::new();
    let mut c = counts(&[
        ("candidates", 0),
        ("retired", 0),
        ("in_worktree", 0),
        ("on_branch", 0),
        ("no_folder", 0),
    ]);
    let conn = connect(get_db_path(Some(home.as_str()))?.as_str(), None)?;
    let (_, missing) = import_lag_by_repo(&conn, cfg, Some(home.as_str()))?;
    for (repo, source) in &missing {
        let dir = if let Some(cached) = repo_dirs.get(repo) {
            cached.clone()
        } else {
            let found = find_repo_dir(tl_roots, &al, Some(home.as_str()), repo)?;
            repo_dirs.insert(repo.clone(), found.clone());
            found
        };
        let mut stmt = conn
            .prepare("SELECT id FROM items WHERE repo = ? AND source = ? AND status = 'active'")?;
        let ids = stmt
            .query_map([repo, source], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for id in ids {
            c["candidates"] += 1;
            let Some(dir) = dir.clone() else {
                c["no_folder"] += 1;
                continue;
            };
            if !wts.contains_key(repo) {
                wts.insert(repo.clone(), worktrees(&dir)?);
                refs.insert(repo.clone(), branch_refs(&dir)?);
            }
            if let Some((y, m, d, rest)) =
                task_id_match(source.strip_prefix("task:").unwrap_or(source))
            {
                let name = format!("{rest}.yaml");
                let mut found = false;
                for wt in &wts[repo] {
                    if !wt
                        .join("docs/tasklog")
                        .glob(&format!("*/{name}"))?
                        .is_empty()
                    {
                        found = true;
                        break;
                    }
                }
                if found {
                    c["in_worktree"] += 1;
                    continue;
                }
                let rel = format!("docs/tasklog/{y}-{m}-{d}/{name}");
                if at_a_branch_tip(&dir, &refs[repo], &rel)? {
                    c["on_branch"] += 1;
                    continue;
                }
            }
            // a dry run counts the item as the other parts count theirs: what the run would do
            c["retired"] += if dry_run { 1 } else { retire_item(&conn, &id)? };
        }
    }
    Ok(c)
}

fn backup(home: &PyPath, cfg: &PyValue, dry_run: bool) -> Result<Counts, SmError> {
    if !get_db_path(Some(home.as_str()))?.is_file() {
        return Ok(counts(&[("made", 0), ("deleted", 0)]));
    }
    let age = backup_age_hours(Some(home.as_str()))?;
    if let Some(a) = age
        && a < float_of(cfg2(cfg, "backup", "interval_hours")?)?
    {
        return Ok(counts(&[("made", 0), ("deleted", 0)]));
    }
    if dry_run {
        return Ok(counts(&[("made", 1), ("deleted", 0)]));
    }
    let keep = crate::py::int::from_value(cfg2(cfg, "backup", "keep")?)?;
    let res = run_backup(&keep, Some(home.as_str()))?;
    Ok(counts(&[
        ("made", 1),
        ("deleted", res.deleted.len() as i64),
        ("items", res.items),
    ]))
}

fn sessions(home: &PyPath, cfg: &PyValue, dry_run: bool) -> Result<Counts, SmError> {
    if !cfg_on(cfg, "capture", "enabled")? {
        return Ok(counts(&[("skipped", 1)]));
    }
    crate::service::capture::run(home, cfg, dry_run)
}

fn upkeep(home: &PyPath, _cfg: &PyValue, dry_run: bool) -> Result<Counts, SmError> {
    let r = crate::service::upkeep::run(home, dry_run)?;
    Ok(counts(&[
        ("sessions", r.sessions),
        ("superseded", r.superseded),
        ("handoffs_without_session", r.handoffs_without_session),
    ]))
}

/// `_prune`: delete prompt-hook state files (`claude-*.json`, `agy-*`) not written for 14 days.
fn prune(home: &PyPath, _cfg: &PyValue, dry_run: bool) -> Result<Counts, SmError> {
    let hook_dir = home.join("hook");
    if !hook_dir.is_dir() {
        return Ok(counts(&[("deleted", 0)]));
    }
    let cutoff = crate::clock::now_epoch() - PRUNE_DAYS * 86400.0;
    let mut deleted = 0;
    for pattern in ["claude-*.json", "agy-*"] {
        for f in hook_dir.glob(pattern)? {
            if f.is_file() && crate::sources::mtime(&f)? < cutoff {
                if !dry_run {
                    std::fs::remove_file(f.to_os())
                        .map_err(|e| SmError::from_io(&e, Some(&f.to_os())))?;
                }
                deleted += 1;
            }
        }
    }
    Ok(counts(&[("deleted", deleted)]))
}

type Part = fn(&PyPath, &PyValue, bool) -> Result<Counts, SmError>;

/// F1 registry. The backup runs first, so a part that writes has a copy from before it.
pub const PARTS: [(&str, Part); 7] = [
    ("backup", backup),
    ("automemory", automemory),
    ("imports", imports),
    ("retire_missing", retire_missing),
    ("upkeep", upkeep),
    ("sessions", sessions),
    ("prune", prune),
];
/// Parts that write memory.db: skipped when the backup fails, so no write runs without a copy.
pub const WRITES_DB: [&str; 5] = [
    "automemory",
    "imports",
    "retire_missing",
    "upkeep",
    "sessions",
];
pub const SKIPPED_NO_BACKUP: &str = "skipped: backup failed";

pub const LOCK_HELD_MESSAGE: &str =
    "error: a catch-up holds hook/catchup.lock; run this again when it ends";

/// `catchup_lock(home)`: the non-blocking flock on hook/catchup.lock, held while the returned file
/// is open; None when another process has it. Only the CLI takes it (catch-up, and manual runs of
/// the parts that write), never a part.
pub fn catchup_lock(home: &PyPath) -> Result<Option<std::fs::File>, SmError> {
    let hook = home.join("hook");
    hook.mkdir(true, true)?;
    let path = hook.join("catchup.lock").to_os();
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o644)
        .open(&path)
        .map_err(|e| SmError::from_io(&e, Some(&path)))?;
    // SAFETY: flock on a descriptor this File owns; the lock goes when the File closes.
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(Some(f));
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        return Ok(None);
    }
    Err(SmError::from_io(&err, None))
}

/// One part's row in the F2 log row.
#[derive(Clone, Debug, PartialEq)]
pub struct PartRow {
    pub ok: bool,
    pub ms: i64,
    pub counts: Counts,
    pub error: Option<String>,
}

/// The F2 row a run appends to catchup.log.
#[derive(Clone, Debug, PartialEq)]
pub struct CatchupRow {
    pub ts: String,
    pub ok: bool,
    pub ms: i64,
    pub parts: IndexMap<&'static str, PartRow>,
}

impl CatchupRow {
    /// The row as Python's dict, for `json.dumps(row, ensure_ascii=False)`.
    pub fn value(&self) -> PyValue {
        let s = |x: &str| PyValue::Str(x.into());
        let parts = self
            .parts
            .iter()
            .map(|(name, p)| {
                let c = p
                    .counts
                    .iter()
                    .map(|(k, v)| (s(k), PyValue::int(*v)))
                    .collect();
                let row = PyValue::Dict(vec![
                    (s("ok"), PyValue::Bool(p.ok)),
                    (s("ms"), PyValue::int(p.ms)),
                    (s("counts"), PyValue::Dict(c)),
                    (s("error"), p.error.as_deref().map_or(PyValue::None, s)),
                ]);
                (s(name), row)
            })
            .collect();
        PyValue::Dict(vec![
            (s("ts"), s(&self.ts)),
            (s("ok"), PyValue::Bool(self.ok)),
            (s("ms"), PyValue::int(self.ms)),
            (s("parts"), PyValue::Dict(parts)),
        ])
    }
}

fn ms_since(t: Instant) -> i64 {
    t.elapsed().as_millis() as i64
}

/// `run_catchup(home=...)`: every part under a non-blocking lock; None when another catch-up
/// holds it.
pub fn run_catchup(home: Option<&str>) -> Result<Option<CatchupRow>, SmError> {
    let h = get_home(home)?;
    let Some(_lock) = catchup_lock(&h)? else {
        return Ok(None);
    };
    let cfg = load_config(Some(h.as_str()))?;
    let started = Instant::now();
    let mut row = CatchupRow {
        ts: crate::clock::now_utc()
            .format("%Y-%m-%dT%H:%M:%S+00:00")
            .to_string(),
        ok: true,
        ms: 0,
        parts: IndexMap::new(),
    };
    for (name, part) in PARTS {
        let t0 = Instant::now();
        let (counts, error) = if WRITES_DB.contains(&name) && !row.parts["backup"].ok {
            (Counts::new(), Some(SKIPPED_NO_BACKUP.to_string()))
        } else {
            match part(&h, &cfg, false) {
                Ok(c) => (c, None),
                Err(e) => {
                    row.ok = false;
                    let msg = split(&e.msg).join(" ");
                    let shown = if is_missing_entry(&e) {
                        msg
                    } else {
                        format!("{}: {}", e.python_name(), slice(&msg, None, Some(300)))
                    };
                    (Counts::new(), Some(shown))
                }
            }
        };
        row.parts.insert(
            name,
            PartRow {
                ok: error.is_none(),
                ms: ms_since(t0),
                counts,
                error,
            },
        );
    }
    row.ms = ms_since(started);
    let log = h.join("catchup.log");
    let line = crate::py::json::dumps(&row.value(), &crate::py::json::Opts::unicode())? + "\n";
    {
        use std::io::Write;
        let os = log.to_os();
        let mut fh = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&os)
            .map_err(|e| SmError::from_io(&e, Some(&os)))?;
        fh.write_all(&crate::py::path::encode_text(&line)?)
            .map_err(|e| SmError::from_io(&e, None))?;
    }
    let size = std::fs::metadata(log.to_os())
        .map_err(|e| SmError::from_io(&e, Some(&log.to_os())))?
        .len();
    if size > LOG_MAX_BYTES {
        keep_newest_rows(&log, LOG_KEEP_ROWS)?;
    }
    Ok(Some(row))
}

/// Rewrite the log with its newest rows through a temp file.
pub fn keep_newest_rows(log: &PyPath, keep: usize) -> Result<(), SmError> {
    let text = log.read_text(Errors::Strict)?;
    let rows = splitlines(&text, true);
    let rows = &rows[rows.len().saturating_sub(keep)..];
    let tmp = log.with_name(&format!("{}.tmp", log.name()))?;
    let bytes = crate::py::path::encode_text(&rows.concat())?;
    std::fs::write(tmp.to_os(), bytes).map_err(|e| SmError::from_io(&e, Some(&tmp.to_os())))?;
    std::fs::rename(tmp.to_os(), log.to_os()).map_err(|e| {
        SmError::os(
            e.raw_os_error().unwrap_or(0),
            Some(&tmp.to_os()),
            Some(&log.to_os()),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_id_match_follows_the_pattern() {
        let m = |s: &str| task_id_match(s);
        assert_eq!(
            m("20260102-fix-x"),
            Some(("2026".into(), "01".into(), "02".into(), "fix-x".into()))
        );
        assert_eq!(
            m("2026010٣-a\n"),
            Some(("2026".into(), "01".into(), "0٣".into(), "a".into()))
        );
        assert_eq!(m("20260102-"), None);
        assert_eq!(m("20260102-a\nb"), None);
        assert_eq!(m("2026-01-02-a"), None);
        // Each value below is what Python 3.14 `_TASK_ID_RE.match` gives.
        assert_eq!(m("20260102-a\n\n"), None);
        assert_eq!(m("202601021-x"), None);
        assert_eq!(m("20260102_x"), None);
        assert_eq!(m(""), None);
    }

    #[test]
    fn keep_newest_rows_keeps_the_tail() {
        let dir = tempfile::tempdir().unwrap();
        let log = PyPath::new(dir.path().join("catchup.log").to_str().unwrap());
        std::fs::write(log.to_os(), "a\nb\r\nc\n").unwrap();
        keep_newest_rows(&log, 2).unwrap();
        assert_eq!(std::fs::read_to_string(log.to_os()).unwrap(), "b\nc\n");
        keep_newest_rows(&log, 5).unwrap();
        assert_eq!(std::fs::read_to_string(log.to_os()).unwrap(), "b\nc\n");
    }

    #[test]
    fn the_lock_is_exclusive_until_released_and_its_file_mode_is_0644() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let home = PyPath::new(d.path().join("sm").to_str().unwrap());
        let held = catchup_lock(&home).unwrap();
        assert!(held.is_some());
        let mode = std::fs::metadata(home.join("hook/catchup.lock").to_os())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode & 0o133, 0, "mode {mode:o} is 0o644 under a umask");
        assert!(catchup_lock(&home).unwrap().is_none());
        drop(held);
        // Under the parallel suite a sibling test that starts a process can hold a copy of the
        // descriptor for a moment, so the free lock may take a few tries to come back.
        let again = (0..100).find_map(|_| {
            let l = catchup_lock(&home).unwrap();
            if l.is_none() {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            l
        });
        assert!(again.is_some());
    }

    #[test]
    fn prune_deletes_hook_state_older_than_fourteen_days_only() {
        let dir = tempfile::tempdir().unwrap();
        let home = PyPath::new(dir.path().to_str().unwrap());
        let hook = dir.path().join("hook");
        std::fs::create_dir_all(&hook).unwrap();
        let now = std::time::SystemTime::now();
        let day = std::time::Duration::from_secs(86_400);
        for (name, age) in [
            ("claude-new.json", day * 13),
            ("claude-old.json", day * 15),
            ("agy-old", day * 15),
            ("other-old.json", day * 15),
        ] {
            let f = std::fs::File::create(hook.join(name)).unwrap();
            f.set_modified(now - age).unwrap();
        }
        assert_eq!(prune(&home, &PyValue::None, true).unwrap()["deleted"], 2);
        assert_eq!(std::fs::read_dir(&hook).unwrap().count(), 4);
        assert_eq!(prune(&home, &PyValue::None, false).unwrap()["deleted"], 2);
        let mut left: Vec<String> = std::fs::read_dir(&hook)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["claude-new.json", "other-old.json"]);
    }

    #[test]
    fn a_failed_backup_skips_every_part_that_writes_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let home = PyPath::new(dir.path().to_str().unwrap());
        std::fs::write(dir.path().join("memory.db"), "not a database").unwrap();
        let row = run_catchup(Some(home.as_str())).unwrap().unwrap();
        assert!(!row.parts["backup"].ok);
        for (name, _) in PARTS {
            let skipped = row.parts[name].error.as_deref() == Some(SKIPPED_NO_BACKUP);
            assert_eq!(skipped, WRITES_DB.contains(&name), "{name}");
        }
    }

    #[test]
    fn a_held_lock_skips_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let home = PyPath::new(dir.path().to_str().unwrap());
        let _held = catchup_lock(&home).unwrap();
        assert_eq!(run_catchup(Some(home.as_str())).unwrap(), None);
        assert!(!home.join("catchup.log").to_os().exists());
    }

    #[test]
    fn retire_missing_dry_run_counts_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = std::fs::canonicalize(dir.path()).unwrap();
        let gh = tmp.join("gh");
        let repo = gh.join("r1");
        std::fs::create_dir_all(repo.join("docs/tasklog/2026-11-01")).unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&repo)
            .env("HOME", &tmp)
            .output()
            .unwrap();
        assert!(init.status.success());
        let home = tmp.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("config.toml"),
            format!(
                "[upkeep]\ntasklog_roots = [\"{}\"]\nhandoff_dirs = []\n",
                gh.display()
            ),
        )
        .unwrap();
        let t = repo.join("docs/tasklog/2026-11-01/090000-baba-y.yaml");
        let id = "20261101-090000-baba-y";
        std::fs::write(&t, format!("id: {id}\ntitle: T {id}\nas_of: 2026-11-01T09:00:00+00:00\nresults:\n  - done {id}\n")).unwrap();
        let h = home.to_str().unwrap();
        run_import("tasklog", t.to_str().unwrap(), false, false, Some(h)).unwrap();
        std::fs::remove_file(&t).unwrap();
        let task_rows = || -> Vec<(String, Option<String>)> {
            let conn = connect(&home.join("memory.db").to_string_lossy(), None).unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT status, superseded_by FROM items WHERE kind = 'task' ORDER BY rowid",
                )
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        let cfg = load_config(Some(h)).unwrap();
        let hp = PyPath::new(h);
        let want = |retired| {
            counts(&[
                ("candidates", 1),
                ("retired", retired),
                ("in_worktree", 0),
                ("on_branch", 0),
                ("no_folder", 0),
            ])
        };
        assert_eq!(retire_missing(&hp, &cfg, true).unwrap(), want(1));
        assert_eq!(task_rows(), vec![("active".to_string(), None)]);
        assert_eq!(retire_missing(&hp, &cfg, false).unwrap()["retired"], 1);
        assert_eq!(task_rows(), vec![("superseded".to_string(), None)]);
    }

    #[test]
    fn retire_missing_retires_when_docs_tasklog_folder_was_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = std::fs::canonicalize(dir.path()).unwrap();
        let gh = tmp.join("gh");
        let repo = gh.join("r1");
        let tl_dir = repo.join("docs/tasklog/2026-11-01");
        std::fs::create_dir_all(&tl_dir).unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&repo)
            .env("HOME", &tmp)
            .output()
            .unwrap();
        assert!(init.status.success());
        let home = tmp.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("config.toml"),
            format!(
                "[upkeep]\ntasklog_roots = [\"{}\"]\nhandoff_dirs = []\n",
                gh.display()
            ),
        )
        .unwrap();
        let t = tl_dir.join("090000-baba-y.yaml");
        let id = "20261101-090000-baba-y";
        std::fs::write(
            &t,
            format!(
                "id: {id}\ntitle: T {id}\nas_of: 2026-11-01T09:00:00+00:00\nresults:\n  - done\n"
            ),
        )
        .unwrap();
        let h = home.to_str().unwrap();
        run_import("tasklog", t.to_str().unwrap(), false, false, Some(h)).unwrap();
        std::fs::remove_dir_all(repo.join("docs/tasklog")).unwrap();
        let cfg = load_config(Some(h)).unwrap();
        let c = retire_missing(&PyPath::new(h), &cfg, false).unwrap();
        assert_eq!((c["candidates"], c["retired"], c["no_folder"]), (1, 1, 0));
    }

    #[test]
    fn retire_missing_keeps_task_on_branch_when_docs_tasklog_folder_was_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = std::fs::canonicalize(dir.path()).unwrap();
        let gh = tmp.join("gh");
        let repo = gh.join("r1");
        let tl_dir = repo.join("docs/tasklog/2026-11-01");
        std::fs::create_dir_all(&tl_dir).unwrap();
        let git_run = |args: &[&str]| {
            let o = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .env("HOME", &tmp)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(
                o.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&o.stderr)
            );
        };
        git_run(&["init", "-q"]);
        let t = tl_dir.join("090000-baba-y.yaml");
        let id = "20261101-090000-baba-y";
        std::fs::write(
            &t,
            format!(
                "id: {id}\ntitle: T {id}\nas_of: 2026-11-01T09:00:00+00:00\nresults:\n  - done\n"
            ),
        )
        .unwrap();
        git_run(&["add", "."]);
        git_run(&["commit", "-qm", "keep on branch"]);
        git_run(&["branch", "topic"]);
        let home = tmp.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("config.toml"),
            format!(
                "[upkeep]\ntasklog_roots = [\"{}\"]\nhandoff_dirs = []\n",
                gh.display()
            ),
        )
        .unwrap();
        let h = home.to_str().unwrap();
        run_import("tasklog", t.to_str().unwrap(), false, false, Some(h)).unwrap();
        std::fs::remove_dir_all(repo.join("docs/tasklog")).unwrap();
        git_run(&["add", "-A"]);
        git_run(&["commit", "-qm", "drop docs/tasklog on main"]);
        let cfg = load_config(Some(h)).unwrap();
        let c = retire_missing(&PyPath::new(h), &cfg, false).unwrap();
        assert_eq!(
            (
                c["candidates"],
                c["retired"],
                c["on_branch"],
                c["no_folder"]
            ),
            (1, 0, 1, 0)
        );
    }

    #[test]
    fn retire_missing_fails_closed_when_git_errors_with_dot_git_present() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = std::fs::canonicalize(dir.path()).unwrap();
        let gh = tmp.join("gh");
        let repo = gh.join("r1");
        let tl_dir = repo.join("docs/tasklog/2026-11-01");
        std::fs::create_dir_all(&tl_dir).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap(); // broken .git with no HEAD/objects
        let home = tmp.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("config.toml"),
            format!(
                "[upkeep]\ntasklog_roots = [\"{}\"]\nhandoff_dirs = []\n",
                gh.display()
            ),
        )
        .unwrap();
        let t = tl_dir.join("090000-baba-y.yaml");
        let id = "20261101-090000-baba-y";
        std::fs::write(
            &t,
            format!(
                "id: {id}\ntitle: T {id}\nas_of: 2026-11-01T09:00:00+00:00\nresults:\n  - done\n"
            ),
        )
        .unwrap();
        let h = home.to_str().unwrap();
        run_import("tasklog", t.to_str().unwrap(), false, false, Some(h)).unwrap();
        std::fs::remove_dir_all(repo.join("docs/tasklog")).unwrap();
        let cfg = load_config(Some(h)).unwrap();
        let err = retire_missing(&PyPath::new(h), &cfg, false).unwrap_err();
        assert!(err.msg.contains("git rev-parse failed"), "{}", err.msg);
        let conn = connect(&home.join("memory.db").to_string_lossy(), None).unwrap();
        let status: String = conn
            .query_row("SELECT status FROM items WHERE kind = 'task'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(status, "active");
    }

    #[test]
    fn retire_missing_prefers_nested_git_repo_over_plain_container_dir() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = std::fs::canonicalize(dir.path()).unwrap();
        let gh = tmp.join("gh");
        // Plain container `gh/acme` holds the actual git repo `gh/acme/acme`.
        let repo = gh.join("acme/acme");
        let tl_dir = repo.join("docs/tasklog/2026-11-01");
        std::fs::create_dir_all(&tl_dir).unwrap();
        let git_run = |cwd: &std::path::Path, args: &[&str]| {
            let o = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .env("HOME", &tmp)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(
                o.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&o.stderr)
            );
        };
        git_run(&repo, &["init", "-q"]);
        let t = tl_dir.join("090000-baba-y.yaml");
        let id = "20261101-090000-baba-y";
        std::fs::write(
            &t,
            format!(
                "id: {id}\ntitle: T {id}\nas_of: 2026-11-01T09:00:00+00:00\nresults:\n  - done\n"
            ),
        )
        .unwrap();
        git_run(&repo, &["add", "."]);
        git_run(&repo, &["commit", "-qm", "keep on branch"]);
        git_run(&repo, &["branch", "topic"]);
        let home = tmp.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("config.toml"),
            format!(
                "[upkeep]\ntasklog_roots = [\"{}\"]\nhandoff_dirs = []\n",
                gh.display()
            ),
        )
        .unwrap();
        let h = home.to_str().unwrap();
        run_import("tasklog", t.to_str().unwrap(), false, false, Some(h)).unwrap();
        std::fs::remove_dir_all(repo.join("docs/tasklog")).unwrap();
        git_run(&repo, &["add", "-A"]);
        git_run(&repo, &["commit", "-qm", "drop docs/tasklog on main"]);
        let cfg = load_config(Some(h)).unwrap();
        let c = retire_missing(&PyPath::new(h), &cfg, false).unwrap();
        assert_eq!(
            (
                c["candidates"],
                c["retired"],
                c["on_branch"],
                c["no_folder"]
            ),
            (1, 0, 1, 0)
        );
    }
}
