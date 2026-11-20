use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use rusqlite::Connection;

use crate::config::{aliases, get_db_path, get_home, load_config, repo_roots};
use crate::core::redact::redact_for_store;
use crate::core::tags::has_retired_tag;
use crate::error::SmError;
use crate::py::path::{Errors, PyPath};
use crate::py::str::{slice, split};
use crate::py::value::PyValue;
use crate::service::import_::{
    alias_repos, automemory_without_file, cfg_get, cfg_on, cfg2, path_of, plan_automemory, roots_of,
};
use crate::sources::automemory::SOURCE_PREFIX;
use crate::sources::handoff::parse_handoff_file;
use crate::sources::iterate;
use crate::sources::sorted_paths;
use crate::sources::tasklog::{infer_tasklog_repo, parse_tasklog_file};
use crate::store::connect::connect;
use crate::store::items::{
    CheckResult, FileRow, check_and_rebuild, file_items_are_current, file_row_holds, fill_refs,
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

/// `Path(raw).expanduser().resolve()` of a config list entry, which must be a folder.
fn config_dir(raw: &PyValue, what: &str) -> Result<PyPath, SmError> {
    let root = path_of(raw)?.expanduser()?.resolve()?;
    if !root.is_dir() {
        return Err(SmError::new(
            crate::error::Base::Os,
            "FileNotFoundError",
            format!("{what} entry not found: {root}"),
        ));
    }
    Ok(root)
}

/// `tasklog_repo_dirs(roots, repo_aliases)`: repo key -> main checkout for each `docs/tasklog`
/// folder the scan finds (worktrees skipped).
pub fn tasklog_repo_dirs(
    roots: &PyValue,
    repo_aliases: &crate::config::Aliases,
) -> Result<IndexMap<String, PyPath>, SmError> {
    let mut found: IndexMap<String, PyPath> = IndexMap::new();
    for raw_root in iterate(roots)? {
        let root = config_dir(&raw_root, "upkeep.tasklog_roots")?;
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
pub fn import_lag(conn: &Connection, full_cfg: &PyValue) -> Result<ImportLag, SmError> {
    Ok(import_lag_by_repo(conn, full_cfg)?.0)
}

/// `import_lag`, and the (repo, source) of each active task item whose repo has no file with
/// its task id: one task id in two repos is two items, so a file in one repo does not keep the
/// other repo's item.
pub(crate) fn import_lag_by_repo(
    conn: &Connection,
    full_cfg: &PyValue,
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
                active_sources.insert((repo, source));
            }
            if status == "active" || (superseded_by.is_some() && !hidden) || has_retired_tag(&tags)
            {
                stored_hashes.insert(key);
            }
        }
    }
    let is_stored = |repo: &str, source: &str, red: &crate::core::item::Item| {
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
        let root = config_dir(&raw_root, "upkeep.tasklog_roots")?;
        tl_files.extend(tasklog_files(&root)?);
    }
    let tl_files = sorted_unique(tl_files);
    for f in &tl_files {
        let item = match parse_tasklog_file(f.as_str()) {
            Ok(it) => alias_repos(vec![it], &al).remove(0),
            Err(e) => {
                lag.unreadable.push(unreadable_row(f, &e));
                continue;
            }
        };
        seen_tasks.insert((item.repo.clone(), item.source.clone()));
        // a stored source does not make a task log stored: a hand edit keeps the source and
        // changes the body, and the import then supersedes the old item. The import stores the
        // redacted body, so compare that hash, or a body with a redacted part is listed each run.
        let red = redact_for_store(&item, None);
        if !is_stored(&item.repo, &item.source, &red) {
            lag.not_stored.push(f.as_str().to_string());
        }
    }
    let mut ho_files = Vec::new();
    for raw_dir in iterate(cfg_get(cfg, "handoff_dirs")?)? {
        let hdir = config_dir(&raw_dir, "upkeep.handoff_dirs")?;
        ho_files.extend(hdir.rglob("*.md")?);
    }
    let ho_files = sorted_unique(ho_files);
    for f in &ho_files {
        let item = match parse_handoff_file(f.as_str(), Some(&repos)) {
            Ok(it) => alias_repos(vec![it], &al).remove(0),
            Err(e) => {
                lag.unreadable.push(unreadable_row(f, &e));
                continue;
            }
        };
        if !is_stored(&item.repo, &item.source, &redact_for_store(&item, None)) {
            lag.not_stored.push(f.as_str().to_string());
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
    let cfg = if imports {
        load_config(home)?
    } else {
        PyValue::Dict(Vec::new())
    };
    let conn = connect(get_db_path(home)?.as_str(), None)?;
    let refs = if backfill_refs {
        fill_refs(&conn, dry_run)?
    } else {
        IndexMap::new()
    };
    let lag = if imports {
        let il = import_lag(&conn, &cfg)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn home_with_log(text: &[u8]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("catchup.log"), text).unwrap();
        dir
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
}
