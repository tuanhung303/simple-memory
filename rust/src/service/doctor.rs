use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use rusqlite::{Connection, OpenFlags};

use crate::cli::query::shlex_quote;
use crate::config::{Aliases, aliases, get_db_path, get_home, load_config, repo_roots};
use crate::core::item::Item;
use crate::core::redact::redact_for_store;
use crate::core::tags::has_retired_tag;
use crate::error::{Base, SmError};
use crate::py::path::{Errors, PyPath};
use crate::py::str::{slice, split};
use crate::py::value::PyValue;
use crate::service::backup::auto_backups;
use crate::service::import_::{
    CopyKey, OnePerSource, automemory_without_file, cfg_get, cfg_on, cfg2, path_of,
    plan_automemory, roots_of,
};
use crate::sources::automemory::SOURCE_PREFIX;
use crate::sources::handoff::parse_handoff_file;
use crate::sources::iterate;
use crate::sources::sorted_paths;
use crate::sources::tasklog::{infer_tasklog_repo, parse_tasklog_file};
use crate::store::connect::connect;
use crate::store::items::{
    CheckResult, FileRow, check_and_rebuild, file_backed, file_items_are_current, file_row_holds,
    fill_refs,
};

const TASKLOG_GLOBS: [&str; 3] = ["*/docs/tasklog", "*/*/docs/tasklog", "*/*/*/docs/tasklog"];

/// A linked worktree's `.git` file points into `.git/worktrees/`; a
/// submodule's into `modules/`.
pub fn in_worktree(path: &PyPath, root: &PyPath) -> Result<bool, SmError> {
    let mut dirs = vec![path.clone()];
    dirs.extend(path.parents());
    for d in dirs {
        let marker = d.join(".git");
        if marker.is_file() && marker.read_text(Errors::Strict)?.contains("/worktrees/") {
            return Ok(true);
        }
        if d == *root {
            return Ok(false);
        }
    }
    Ok(false)
}

fn tasklog_files(root: &PyPath) -> Result<Vec<PyPath>, SmError> {
    let mut files = Vec::new();
    for pattern in TASKLOG_GLOBS {
        for d in root.glob(pattern)? {
            if in_worktree(&d.parent().parent(), root)? {
                continue;
            }
            files.extend(d.glob("*/*.yaml")?);
        }
    }
    Ok(files)
}

/// The fix a missing `[upkeep]` folder entry names; see `is_missing_entry`.
pub const FIX_ENTRY: &str = "; fix or remove it in [upkeep] ";

/// `Path(raw).expanduser().resolve()` of an `[upkeep] <key>` entry, which must be a folder; a
/// missing one names the key and the config file to fix.
fn config_dir(raw: &PyValue, key: &str, home: Option<&str>) -> Result<PyPath, SmError> {
    let root = path_of(raw)?.expanduser()?.resolve()?;
    if !root.is_dir() {
        let cfg = get_home(home)?.join("config.toml");
        return Err(SmError::new(
            Base::Os,
            "FileNotFoundError",
            format!("upkeep.{key} entry not found: {root}{FIX_ENTRY}{key} in {cfg}"),
        ));
    }
    Ok(root)
}

/// Is this a missing `[upkeep]` folder entry? Its message says what to fix, so catch-up shows
/// it without the class name.
pub fn is_missing_entry(e: &SmError) -> bool {
    e.is(Base::Os) && e.msg.contains(FIX_ENTRY)
}

/// One line for each `tasklog_roots` entry that holds `docs/tasklog` itself: the scan looks for
/// `docs/tasklog` one to three folders below a root, so it finds none of that repo's task logs.
pub fn repo_folder_roots(cfg: &PyValue) -> Result<Vec<String>, SmError> {
    let mut lines = Vec::new();
    for raw in iterate(cfg_get(cfg_get(cfg, "upkeep")?, "tasklog_roots")?)? {
        let root = path_of(&raw)?.expanduser()?.resolve()?;
        if root.join("docs/tasklog").is_dir() {
            lines.push(format!(
                "{root} is a repo folder: tasklog_roots takes the folder above it (set \"{}\")",
                root.parent()
            ));
        }
    }
    Ok(lines)
}

/// `tasklog_repo_dirs(roots, repo_aliases)`: repo key -> main checkout for each `docs/tasklog`
/// folder the scan finds (worktrees skipped).
pub fn tasklog_repo_dirs(
    roots: &PyValue,
    repo_aliases: &crate::config::Aliases,
    home: Option<&str>,
) -> Result<IndexMap<String, PyPath>, SmError> {
    let mut found: IndexMap<String, PyPath> = IndexMap::new();
    for raw_root in iterate(roots)? {
        let root = config_dir(&raw_root, "tasklog_roots", home)?;
        for pattern in TASKLOG_GLOBS {
            for d in root.glob(pattern)? {
                let checkout = d.parent().parent();
                if !in_worktree(&checkout, &root)? {
                    let repo = infer_tasklog_repo(d.join("x").as_str())?;
                    let key = repo_aliases.get(&repo).cloned().unwrap_or(repo);
                    found.entry(key).or_insert(checkout);
                }
            }
        }
    }
    Ok(found)
}

/// Fallback repo checkout lookup under `tasklog_roots` when `docs/tasklog` was removed from `repo`.
pub fn find_repo_dir(
    roots: &PyValue,
    repo_aliases: &Aliases,
    home: Option<&str>,
    repo: &str,
) -> Result<Option<PyPath>, SmError> {
    if repo.is_empty()
        || repo == "unassigned"
        || repo == "global"
        || repo == "."
        || repo == ".."
        || repo.contains('/')
    {
        return Ok(None);
    }
    let mut names = vec![repo.to_string()];
    let mut alias_keys: Vec<String> = repo_aliases
        .iter()
        .filter_map(|(k, v)| {
            (v == repo && k != repo && !k.is_empty() && !k.contains('/')).then(|| k.clone())
        })
        .collect();
    alias_keys.sort();
    names.extend(alias_keys);
    let mut fallback: Option<PyPath> = None;
    for raw_root in iterate(roots)? {
        let root = config_dir(&raw_root, "tasklog_roots", home)?;
        for name in &names {
            let d = root.join(name);
            if d.is_dir() && !in_worktree(&d, &root)? {
                if d.join(".git").to_os().symlink_metadata().is_ok() {
                    return Ok(Some(d));
                }
                fallback.get_or_insert(d);
            }
        }
        for pat_prefix in ["*", "*/*"] {
            for parent in root.glob(pat_prefix)? {
                if !parent.is_dir() {
                    continue;
                }
                for name in &names {
                    let d = parent.join(name);
                    if d.is_dir()
                        && d.join(".git").to_os().symlink_metadata().is_ok()
                        && !in_worktree(&d, &root)?
                    {
                        return Ok(Some(d));
                    }
                }
            }
        }
    }
    Ok(fallback)
}

/// The dict `import_lag` returns.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImportLag {
    pub tasklog_files: i64,
    pub handoff_files: i64,
    pub not_stored: Vec<String>,
    pub unreadable: Vec<String>,
    pub stored_tasks_without_file: Vec<String>,
}

/// `sorted(set(paths))`.
fn sorted_unique(paths: Vec<PyPath>) -> Vec<PyPath> {
    let mut seen = HashSet::new();
    let unique: Vec<PyPath> = paths
        .into_iter()
        .filter(|p| seen.insert(p.as_str().to_string()))
        .collect();
    sorted_paths(unique)
}

/// A task log or handoff the parser refused, as doctor lists it: an error raised while reading,
/// parsing or converting one file is that file's problem, never the end of the scan.
fn unreadable_row(f: &PyPath, e: &SmError) -> String {
    let msg = split(&e.msg).join(" ");
    format!("{f}: {}", slice(&msg, None, Some(160)))
}

/// `import_lag(conn, full_cfg)`: task logs and handoffs on disk with no stored item.
pub fn import_lag(
    conn: &Connection,
    full_cfg: &PyValue,
    home: Option<&str>,
) -> Result<ImportLag, SmError> {
    Ok(import_lag_by_repo(conn, full_cfg, home)?.0)
}

/// `import_lag`, and the (repo, source) of each active task item whose repo has no file with
/// its task id: one task id in two repos is two items, so a file in one repo does not keep the
/// other repo's item.
pub(crate) fn import_lag_by_repo(
    conn: &Connection,
    full_cfg: &PyValue,
    home: Option<&str>,
) -> Result<(ImportLag, Vec<(String, String)>), SmError> {
    let cfg = cfg_get(full_cfg, "upkeep")?;
    cfg_get(full_cfg, "repo_aliases")?;
    let al = aliases(full_cfg);
    let repos = repo_roots(full_cfg)?;
    // stored: an active item, one another item replaced, or one manually retired (retired:<reason>);
    // a file-deleted retired item (no replacement, no retired:* tag) does not count, so a file that
    // comes back is imported again.
    // When (repo, source) still has an active row, only an active row with that hash counts as
    // stored, so reverting a file A -> B -> A surfaces for re-import instead of matching old A.
    // Hashes are kept by source: only a row of the same file stores a file's body, as the import
    // dedupes, never a row of another file or a manual or amended row.
    // A row another repo's item of the same source replaced (the rule before one item per repo)
    // is not stored either: the import restores it.
    // An active task log or handoff row counts only when it also holds the file's title, tags,
    // as_of, evidence and refs (`file_row_holds`), as the import keeps only such a row.
    let stored = "i.status = 'active' OR i.superseded_by IS NOT NULL OR i.tags LIKE '%retired:%'";
    let mut stmt = conn.prepare(&format!(
        "SELECT i.repo, i.source, i.status, i.superseded_by, i.tags, i.content_hash, \
         ifnull(b.source = i.source AND b.repo != i.repo, 0), i.title, i.tag, i.evidence, \
         i.as_of, i.refs FROM items i \
         LEFT JOIN items b ON b.id = i.superseded_by WHERE {stored}"
    ))?;
    let mut active_hashes: HashMap<(String, String, String), FileRow> = HashMap::new();
    let mut active_sources: HashSet<(String, String)> = HashSet::new();
    let mut stored_hashes: HashSet<(String, String, String)> = HashSet::new();
    let mut known_stored_sources: HashSet<String> = HashSet::new();
    {
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let repo: String = r.get(0)?;
            let source: String = r.get(1)?;
            let status: String = r.get(2)?;
            let superseded_by: Option<String> = r.get(3)?;
            let tags: String = r.get(4)?;
            let hash: String = r.get(5)?;
            let hidden: bool = r.get(6)?;
            let key = (repo.clone(), source.clone(), hash);
            if status == "active" {
                let row = FileRow {
                    title: r.get(7)?,
                    tags: tags.clone(),
                    tag: r.get(8)?,
                    evidence: r.get(9)?,
                    as_of: r.get(10)?,
                    refs: r.get(11)?,
                };
                active_hashes.insert(key.clone(), row);
                active_sources.insert((repo.clone(), source.clone()));
            }
            if status == "active" || (superseded_by.is_some() && !hidden) || has_retired_tag(&tags)
            {
                if repo != "unassigned" && file_backed(&source) {
                    known_stored_sources.insert(source);
                }
                stored_hashes.insert(key);
            }
        }
    }
    let is_stored = |repo: &str, source: &str, red: &crate::core::item::Item| {
        if repo == "unassigned"
            && file_backed(source)
            && known_stored_sources.contains(source)
        {
            return true;
        }
        let key = (
            repo.to_string(),
            source.to_string(),
            red.content_hash.clone(),
        );
        if active_sources.contains(&(repo.to_string(), source.to_string())) {
            active_hashes
                .get(&key)
                .is_some_and(|row| file_row_holds(row, red))
        } else {
            stored_hashes.contains(&key)
        }
    };
    let active_tasks: Vec<(String, String)> = active_sources
        .iter()
        .filter(|(_, source)| source.starts_with("task:"))
        .cloned()
        .collect();
    let mut lag = ImportLag::default();
    let mut seen_tasks: HashSet<(String, String)> = HashSet::new();
    let mut tl_files = Vec::new();
    for raw_root in iterate(cfg_get(cfg, "tasklog_roots")?)? {
        let root = config_dir(&raw_root, "tasklog_roots", home)?;
        tl_files.extend(tasklog_files(&root)?);
    }
    let tl_files = sorted_unique(tl_files);
    // the import takes one file per source, so only that file can be not stored: a copy of the
    // source elsewhere is never imported and is never listed. Each file keeps only its choice
    // key and whether it is stored; the parsed file is dropped at once, so the scan does not
    // hold every body until the choice is made.
    let mut pick: OnePerSource<bool> = OnePerSource::default();
    let mut offer = |f: &PyPath, mut item: Item| {
        if let Some(r) = al.get(&item.repo) {
            item.repo = r.clone();
        }
        // a stored source does not make a file stored: a hand edit keeps the source and changes
        // the body, and the import then supersedes the old item. The import stores the redacted
        // body, so compare that hash, or a body with a redacted part is listed each run. Only a
        // file that takes its source's place is checked: a losing copy is never listed.
        let stored = || is_stored(&item.repo, &item.source, &redact_for_store(&item, None));
        let key = CopyKey::of(f.clone(), &item);
        pick.offer(item.repo.clone(), item.source.clone(), key, stored);
    };
    for f in &tl_files {
        match parse_tasklog_file(f.as_str()) {
            Ok(it) => offer(f, it),
            Err(e) => lag.unreadable.push(unreadable_row(f, &e)),
        }
    }
    let mut ho_files = Vec::new();
    for raw_dir in iterate(cfg_get(cfg, "handoff_dirs")?)? {
        let hdir = config_dir(&raw_dir, "handoff_dirs", home)?;
        ho_files.extend(hdir.rglob("*.md")?);
    }
    let ho_files = sorted_unique(ho_files);
    for f in &ho_files {
        match parse_handoff_file(f.as_str(), Some(&repos)) {
            Ok(it) => offer(f, it),
            Err(e) => lag.unreadable.push(unreadable_row(f, &e)),
        }
    }
    for ((repo, source), key, stored) in pick.into_kept() {
        if !stored {
            lag.not_stored.push(key.path.as_str().to_string());
        }
        if source.starts_with("task:") {
            seen_tasks.insert((repo, source));
        }
    }
    lag.tasklog_files = tl_files.len() as i64;
    lag.handoff_files = ho_files.len() as i64;
    let mut gone: Vec<(String, String)> = active_tasks
        .into_iter()
        .filter(|key| !seen_tasks.contains(key))
        .collect();
    gone.sort();
    let mut sources: Vec<String> = gone.iter().map(|(_, source)| source.clone()).collect();
    sources.sort();
    sources.dedup();
    lag.stored_tasks_without_file = sources;
    Ok((lag, gone))
}

/// The dict `_automemory_lag` returns; `None` is `{"automemory_off": True}`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AutomemoryLag {
    pub automemory_files: i64,
    pub automemory_not_stored: Vec<String>,
    pub automemory_stored_without_file: Vec<String>,
    pub automemory_unmapped: Vec<String>,
    pub automemory_unreadable: Vec<String>,
}

/// Auto-memory files whose item is not current, items whose file
/// is gone; None when `[automemory] enabled = false`.
fn automemory_lag(conn: &Connection, cfg: &PyValue) -> Result<Option<AutomemoryLag>, SmError> {
    if !cfg_on(cfg, "automemory", "enabled")? {
        return Ok(None);
    }
    let root = path_of(cfg2(cfg, "automemory", "root")?)?.expanduser()?;
    let plan = plan_automemory(&root, &aliases(cfg), &roots_of(cfg)?)?;
    let current =
        file_items_are_current(conn, SOURCE_PREFIX, plan.items.iter().map(|(_, item)| item))?;
    let not_stored = plan
        .items
        .iter()
        .zip(current)
        .filter(|(_, current)| !current)
        .map(|((mf, _), _)| mf.path.as_str().to_string())
        .collect();
    Ok(Some(AutomemoryLag {
        automemory_files: plan.sources.len() as i64,
        automemory_not_stored: not_stored,
        automemory_stored_without_file: automemory_without_file(conn, &plan)?
            .into_iter()
            .map(|(_, s, _)| s)
            .collect(),
        automemory_unmapped: plan.unmapped,
        automemory_unreadable: plan.unreadable,
    }))
}

/// `last_catchup(home)`: the last row of catchup.log (F2), or None when no catch-up has run.
pub fn last_catchup(home: Option<&str>) -> Result<Option<PyValue>, SmError> {
    let log = get_home(home)?.join("catchup.log");
    if !log.is_file() {
        return Ok(None);
    }
    let text = log.read_text(Errors::Strict)?;
    // `for line in fh: last = line`: the last line, its `\n` included
    let body = text.strip_suffix('\n').unwrap_or(&text);
    let last = body.rfind('\n').map_or(text.as_str(), |i| &text[i + 1..]);
    if last.is_empty() {
        return Ok(None);
    }
    match crate::py::json::loads(last) {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.class == "JSONDecodeError" => {
            let s = |x: &str| PyValue::Str(x.into());
            let err = PyValue::Dict(vec![(s("error"), s("last catchup.log row is not JSON"))]);
            Ok(Some(PyValue::Dict(vec![
                (s("ok"), PyValue::Bool(false)),
                (s("ts"), s("unknown")),
                (s("parts"), PyValue::Dict(vec![(s("log"), err)])),
            ])))
        }
        Err(e) => Err(e),
    }
}

/// The dict `run_doctor` returns, as the CLI reads it.
#[derive(Clone, Debug, PartialEq)]
pub struct DoctorResult {
    pub check: CheckResult,
    pub refs: IndexMap<&'static str, i64>,
    pub lag: Option<(ImportLag, Option<AutomemoryLag>, Option<PyValue>)>,
    pub dry_run: bool,
}

/// `run_doctor(rebuild_fts=..., backfill_refs=..., dry_run=..., imports=..., home=...)`.
pub fn run_doctor(
    rebuild_fts: bool,
    backfill_refs: bool,
    dry_run: bool,
    imports: bool,
    home: Option<&str>,
) -> Result<DoctorResult, SmError> {
    if dry_run && !backfill_refs {
        return Err(SmError::value("--dry-run applies to --backfill-refs only"));
    }
    let cfg = load_config(home)?;
    let conn = connect(get_db_path(home)?.as_str(), None)?;
    let refs = if backfill_refs {
        fill_refs(&conn, dry_run)?
    } else {
        IndexMap::new()
    };
    let lag = if imports {
        let il = import_lag(&conn, &cfg, home)?;
        let am = automemory_lag(&conn, &cfg)?;
        Some((il, am, last_catchup(home)?))
    } else {
        None
    };
    let check = check_and_rebuild(&conn, rebuild_fts)?;
    Ok(DoctorResult {
        check,
        refs,
        lag,
        dry_run,
    })
}

/// Does the error carry SQLite's text for a damaged file (SQLITE_CORRUPT, SQLITE_NOTADB)?
pub fn is_damaged(e: &SmError) -> bool {
    e.is(Base::Sqlite)
        && e.class == "DatabaseError"
        && ["malformed", "not a database", "vtable constructor failed"]
            .iter()
            .any(|w| e.msg.contains(w))
}

fn passes_quick_check(db: impl AsRef<std::path::Path>, flags: OpenFlags) -> bool {
    Connection::open_with_flags(db, flags)
        .and_then(|c| c.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0)))
        .is_ok_and(|v| v == "ok")
}

/// What to do after an `is_damaged` error: the newest backup that passes `quick_check` and the
/// two steps that put it in place, or that there is none. Nothing when memory.db passes
/// `quick_check`, since the error then came from another file (`sm import legacy <db>`).
pub fn restore_steps(home: Option<&str>) -> Result<Vec<String>, SmError> {
    let h = get_home(home)?;
    let db = h.join("memory.db");
    if passes_quick_check(db.to_os(), OpenFlags::SQLITE_OPEN_READ_ONLY) {
        return Ok(Vec::new());
    }
    let (found, bdir) = (auto_backups(home)?, h.join("backups"));
    // A copy is in WAL mode, so a plain read-only open leaves -wal and -shm files beside it;
    // `immutable=1` reads the file alone and writes nothing.
    let ro = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    let uri = |p: &PyPath| {
        let path = p
            .as_str()
            .replace('%', "%25")
            .replace('?', "%3f")
            .replace('#', "%23");
        format!("file:{path}?mode=ro&immutable=1")
    };
    let good: Vec<&PyPath> = found
        .iter()
        .rev()
        .filter(|p| passes_quick_check(uri(p), ro))
        .collect();
    // an empty copy restores nothing, so an older copy that holds items wins over it
    let holds_items = |p: &PyPath| {
        Connection::open_with_flags(uri(p), ro)
            .and_then(|c| c.query_row("SELECT count(*) FROM items", [], |r| r.get::<_, i64>(0)))
            .is_ok_and(|n| n > 0)
    };
    let full = good.iter().copied().find(|p| holds_items(p));
    let Some(backup) = full.or(good.first().copied()) else {
        return Ok(vec![if found.is_empty() {
            format!("{db} looks damaged and {bdir} holds no backup to restore")
        } else {
            format!("{db} looks damaged and no backup in {bdir} passes quick_check")
        }]);
    };
    let (q, aside) = (|p: &PyPath| shlex_quote(p.as_str()), h.join("damaged"));
    // name the item rule only when it passed over a newer empty copy
    let which = if good.first() == Some(&backup) {
        "the newest backup that passes quick_check"
    } else {
        "the newest backup that passes quick_check and holds items"
    };
    Ok(vec![
        format!("{db} looks damaged; to restore {which}, {backup}:"),
        format!(
            "  1. mkdir -p {} && mv {}/memory.db* {}/",
            q(&aside),
            q(&h),
            q(&aside)
        ),
        format!("  2. cp {} {}", q(backup), q(&db)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home_with_log(text: &[u8]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("catchup.log"), text).unwrap();
        dir
    }

    #[test]
    fn restore_steps_skip_an_empty_backup_when_an_older_one_holds_items() {
        let dir = tempfile::tempdir().unwrap();
        let h = dir.path();
        std::fs::write(h.join("memory.db"), b"not a database at all").unwrap();
        std::fs::create_dir(h.join("backups")).unwrap();
        let make = |name: &str, rows: i64| {
            let c = Connection::open(h.join("backups").join(name)).unwrap();
            c.execute_batch("CREATE TABLE items (id TEXT)").unwrap();
            for i in 0..rows {
                c.execute("INSERT INTO items VALUES (?1)", [i.to_string()])
                    .unwrap();
            }
        };
        make("memory-20261101-000000-auto.db", 3);
        make("memory-20261102-000000-auto.db", 0);
        let steps = restore_steps(h.to_str()).unwrap();
        assert!(steps[0].ends_with("memory-20261101-000000-auto.db:"), "{steps:?}");
        assert!(steps[0].contains("holds items"), "{steps:?}");
        // every backup empty: the newest that passes quick_check, as before
        std::fs::remove_file(h.join("backups/memory-20261101-000000-auto.db")).unwrap();
        let steps = restore_steps(h.to_str()).unwrap();
        assert!(steps[0].ends_with("memory-20261102-000000-auto.db:"), "{steps:?}");
        assert!(!steps[0].contains("holds items"), "{steps:?}");
    }

    #[test]
    fn last_catchup_reads_the_last_line() {
        let dir = home_with_log(b"{\"ok\": true}\n{\"ok\": false, \"ts\": \"t\"}\n");
        let v = last_catchup(dir.path().to_str()).unwrap().unwrap();
        assert_eq!(
            crate::py::json::dumps(&v, &Default::default()).unwrap(),
            "{\"ok\": false, \"ts\": \"t\"}"
        );
    }

    #[test]
    fn last_catchup_of_a_bad_row_or_no_log() {
        let dir = home_with_log(b"{\"ok\": true}\nnot json\n");
        let v = last_catchup(dir.path().to_str()).unwrap().unwrap();
        assert_eq!(
            crate::py::json::dumps(&v, &Default::default()).unwrap(),
            "{\"ok\": false, \"ts\": \"unknown\", \"parts\": {\"log\": {\"error\": \"last catchup.log row is not JSON\"}}}"
        );
        let empty = home_with_log(b"");
        assert_eq!(last_catchup(empty.path().to_str()).unwrap(), None);
        let none = tempfile::tempdir().unwrap();
        assert_eq!(last_catchup(none.path().to_str()).unwrap(), None);
    }

    #[test]
    fn dry_run_needs_backfill_refs() {
        let dir = tempfile::tempdir().unwrap();
        let e = run_doctor(false, false, true, false, dir.path().to_str()).unwrap_err();
        assert_eq!(
            e.python_text(),
            "ValueError: --dry-run applies to --backfill-refs only"
        );
    }

    #[test]
    fn worktree_marker_is_found_up_to_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = PyPath::new(dir.path().to_str().unwrap());
        let wt = root.join("wt");
        std::fs::create_dir_all(wt.join("docs").to_os()).unwrap();
        std::fs::write(wt.join(".git").to_os(), "gitdir: /x/.git/worktrees/wt\n").unwrap();
        assert!(in_worktree(&wt.join("docs"), &root).unwrap());
        std::fs::write(wt.join(".git").to_os(), "gitdir: /x/.git/modules/wt\n").unwrap();
        assert!(!in_worktree(&wt.join("docs"), &root).unwrap());
    }

    #[test]
    fn find_repo_dir_handles_nested_worktree_aliases_and_plain_container() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = std::fs::canonicalize(dir.path()).unwrap();
        let gh = tmp.join("gh");
        let gh2 = tmp.join("gh2");
        std::fs::create_dir_all(&gh).unwrap();
        std::fs::create_dir_all(&gh2).unwrap();

        // Plain container dir `gh/acme` collides with real nested git repo `gh/acme/acme/.git`.
        let nested = gh.join("acme/acme");
        std::fs::create_dir_all(nested.join(".git")).unwrap();

        // Linked worktree directly under `gh/beta` must be skipped in favor of `gh/org/group/beta/.git`.
        let wt_beta = gh.join("beta");
        std::fs::create_dir_all(&wt_beta).unwrap();
        std::fs::write(wt_beta.join(".git"), "gitdir: /x/.git/worktrees/beta\n").unwrap();
        let deep_beta = gh.join("org/group/beta");
        std::fs::create_dir_all(deep_beta.join(".git")).unwrap();

        // Alias `old-gamma` under `gh/team/old-gamma/.git` maps to canonical `gamma`.
        let alias_gamma = gh.join("team/old-gamma");
        std::fs::create_dir_all(alias_gamma.join(".git")).unwrap();

        // Non-git fallback still works when no `.git` repo exists anywhere.
        let plain = gh.join("plain-only");
        std::fs::create_dir_all(&plain).unwrap();

        // Plain dir in earlier root does not shadow `.git` repo in later root.
        let shadow = gh.join("shadow");
        std::fs::create_dir_all(&shadow).unwrap();
        let real_shadow = gh2.join("shadow");
        std::fs::create_dir_all(real_shadow.join(".git")).unwrap();

        let roots = PyValue::List(vec![
            PyValue::Str(gh.to_string_lossy().into_owned()),
            PyValue::Str(gh2.to_string_lossy().into_owned()),
        ]);
        let mut al = Aliases::new();
        al.insert("old-gamma".into(), "gamma".into());

        assert_eq!(
            find_repo_dir(&roots, &al, None, "acme").unwrap().unwrap().to_os(),
            nested
        );
        assert_eq!(
            find_repo_dir(&roots, &al, None, "beta").unwrap().unwrap().to_os(),
            deep_beta
        );
        assert_eq!(
            find_repo_dir(&roots, &al, None, "gamma").unwrap().unwrap().to_os(),
            alias_gamma
        );
        assert_eq!(
            find_repo_dir(&roots, &al, None, "shadow").unwrap().unwrap().to_os(),
            real_shadow
        );
        assert_eq!(
            find_repo_dir(&roots, &al, None, "plain-only").unwrap().unwrap().to_os(),
            plain
        );
        assert_eq!(find_repo_dir(&roots, &al, None, "bad[name").unwrap(), None);
    }
}
