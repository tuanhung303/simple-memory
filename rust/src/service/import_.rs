use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use rusqlite::Connection;

use crate::config::{
    Aliases, aliases, get_db_path, load_config, project_dir_repo, repo_roots, resolve_repo, roots,
};
use crate::core::item::Item;
use crate::core::redact::redact_text;
use crate::error::SmError;
use crate::py::path::PyPath;
use crate::py::value::{PyValue, type_name};
use crate::sources::automemory::{
    MemoryFile, SOURCE_PREFIX, discover, parse_memory_file, to_item, transcript_cwd,
};
use crate::sources::handoff::load_handoffs;
use crate::sources::legacy::load_legacy;
use crate::sources::proposal::load_proposals_with_rejects;
use crate::sources::tasklog::load_tasklogs;
use crate::sources::truthy;
use crate::store::connect::{connect, execute_waiting_for_lock};
use crate::store::items::{put_items_batch, retire_item, sync_source_items};

// ---------------------------------------------------------------- config values

/// `cfg[key]` on a loaded config section: a missing key is Python's KeyError.
pub(crate) fn cfg_get<'a>(v: &'a PyValue, key: &str) -> Result<&'a PyValue, SmError> {
    match v {
        PyValue::Dict(d) => crate::sources::dict_get(d, key).ok_or_else(|| SmError::key(key)),
        other => Err(SmError::type_(format!(
            "'{}' object is not subscriptable",
            type_name(other)
        ))),
    }
}

/// `cfg[a][b]`.
pub(crate) fn cfg2<'a>(cfg: &'a PyValue, a: &str, b: &str) -> Result<&'a PyValue, SmError> {
    cfg_get(cfg_get(cfg, a)?, b)
}

/// `bool(cfg[a][b])`.
pub(crate) fn cfg_on(cfg: &PyValue, a: &str, b: &str) -> Result<bool, SmError> {
    Ok(truthy(cfg2(cfg, a, b)?))
}

/// Python `float(v)` of a config value.
pub(crate) fn float_of(v: &PyValue) -> Result<f64, SmError> {
    match v {
        PyValue::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        PyValue::Float(x) => Ok(*x),
        PyValue::Int(s) => {
            let x: f64 = s
                .parse()
                .map_err(|_| SmError::overflow("int too large to convert to float"))?;
            if x.is_finite() {
                Ok(x)
            } else {
                Err(SmError::overflow("int too large to convert to float"))
            }
        }
        PyValue::Str(s) => crate::py::float::from_str(s),
        other => Err(SmError::type_(format!(
            "float() argument must be a string or a real number, not '{}'",
            type_name(other)
        ))),
    }
}

/// Python `int(v)` of a config value, as an i64 (a value past i64 is Python's OverflowError at
/// the first C call that takes it).
pub(crate) fn int_of(v: &PyValue) -> Result<i64, SmError> {
    crate::py::int::from_value(v)?
        .to_i64()
        .ok_or_else(|| SmError::overflow("Python int too large to convert to C ssize_t"))
}

/// `Path(v)` of a config value: only a str makes a path.
pub(crate) fn path_of(v: &PyValue) -> Result<PyPath, SmError> {
    match v {
        PyValue::Str(s) => Ok(PyPath::new(s)),
        other => Err(SmError::type_(format!(
            "argument should be a str or an os.PathLike object where __fspath__ returns a str, not '{}'",
            type_name(other)
        ))),
    }
}

// ---------------------------------------------------------------- run_import

/// `alias_repos(items, repo_aliases)`: each item's repo through `[repo_aliases]`.
pub fn alias_repos(mut items: Vec<Item>, repo_aliases: &Aliases) -> Vec<Item> {
    for it in &mut items {
        if let Some(r) = repo_aliases.get(&it.repo) {
            it.repo = r.clone();
        }
    }
    items
}

/// Counts per `str(attr or "")`, most first, then by key.
pub fn count_by(items: &[Item], attr: &str) -> IndexMap<String, i64> {
    let mut counts: IndexMap<String, i64> = IndexMap::new();
    for it in items {
        let key = match attr {
            "repo" => &it.repo,
            "tag" => &it.tag,
            "kind" => &it.kind,
            _ => &it.source,
        };
        *counts.entry(key.clone()).or_default() += 1;
    }
    let mut pairs: Vec<(String, i64)> = counts.into_iter().collect();
    pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    pairs.into_iter().collect()
}

/// The dict `run_import` returns, keys in Python's order (`skipped` repeats `deduped`).
#[derive(Clone, Debug, PartialEq)]
pub struct ImportResult {
    pub kind: String,
    pub total: i64,
    pub by_repo: IndexMap<String, i64>,
    pub by_tag: IndexMap<String, i64>,
    pub dry_run: bool,
    pub applied: bool,
    pub inserted: i64,
    pub deduped: i64,
    pub superseded: i64,
    pub supersede_missing: i64,
    pub rejected: i64,
    pub unresolved_supersedes: Vec<String>,
    pub failed_rows: Vec<String>,
}

/// `run_import(source_kind, path, dry_run=..., apply=..., home=...)`.
pub fn run_import(
    source_kind: &str,
    path: &str,
    dry_run: bool,
    apply: bool,
    home: Option<&str>,
) -> Result<ImportResult, SmError> {
    let kind = crate::py::str::lower(crate::py::str::strip(source_kind));
    let mut rejected = 0i64;
    let cfg = load_config(home)?;
    let items = match kind.as_str() {
        "tasklog" => load_tasklogs(path)?,
        "handoff" => load_handoffs(path, Some(&repo_roots(&cfg)?))?,
        "legacy" => load_legacy(path, Some(&repo_roots(&cfg)?))?,
        "proposal" => {
            let (items, n) = load_proposals_with_rejects(path)?;
            rejected = n as i64;
            items
        }
        _ => {
            return Err(SmError::value(format!(
                "Unknown import source: {}",
                crate::py::str::repr(source_kind)
            )));
        }
    };
    let items = alias_repos(items, &aliases(&cfg));
    let by_repo = count_by(&items, "repo");
    let by_tag = count_by(&items, "tag");
    let should_write = !dry_run && (kind != "proposal" || apply);
    let (mut inserted, mut deduped, mut superseded, mut supersede_missing) = (0, 0, 0, 0);
    let (mut unresolved, mut failed_rows) = (Vec::new(), Vec::new());
    if should_write {
        let conn = connect(get_db_path(home)?.as_str(), None)?;
        (
            inserted,
            deduped,
            superseded,
            supersede_missing,
            unresolved,
            failed_rows,
        ) = put_items_batch(&conn, &items, kind == "proposal" && apply)?;
    }
    Ok(ImportResult {
        kind,
        total: items.len() as i64,
        by_repo,
        by_tag,
        dry_run: dry_run || !should_write,
        applied: should_write,
        inserted,
        deduped,
        superseded,
        supersede_missing,
        rejected,
        unresolved_supersedes: unresolved,
        failed_rows,
    })
}

// ---------------------------------------------------------------- auto-memory

/// The `AutomemoryPlan` dataclass. `sources` is a set: only membership and its size are used.
#[derive(Clone, Debug, PartialEq)]
pub struct AutomemoryPlan {
    pub root: PyPath,
    pub sources: HashSet<String>,
    pub items: Vec<(MemoryFile, Item)>,
    pub unreadable: Vec<String>,
    pub unmapped: Vec<String>,
    pub yaml_errors: i64,
}

/// The repo of the session cwd, else of the
/// project folder name; never global or unassigned.
fn project_repo(
    project: &PyPath,
    repo_aliases: &Aliases,
    roots: &[String],
) -> Result<Option<String>, SmError> {
    if let Some(cwd) = transcript_cwd(project)?.filter(|c| !c.is_empty()) {
        let repo = resolve_repo(Some(&cwd), repo_aliases, roots)?;
        if repo != "unassigned" && repo != "global" {
            return Ok(Some(repo));
        }
    }
    let repo = project_dir_repo(project.name(), repo_aliases, roots)?;
    Ok((repo != "unassigned" && repo != "global").then_some(repo))
}

/// `plan_automemory(root, repo_aliases, roots)`.
pub fn plan_automemory(
    root: &PyPath,
    repo_aliases: &Aliases,
    roots: &[String],
) -> Result<AutomemoryPlan, SmError> {
    let mut plan = AutomemoryPlan {
        root: root.clone(),
        sources: HashSet::new(),
        items: Vec::new(),
        unreadable: Vec::new(),
        unmapped: Vec::new(),
        yaml_errors: 0,
    };
    let mut repos: HashMap<String, Option<String>> = HashMap::new();
    for path in discover(root)? {
        let project = path.parent().parent();
        plan.sources
            .insert(format!("{SOURCE_PREFIX}{}/{}", project.name(), path.name()));
        let mf = match parse_memory_file(&path) {
            Ok(mf) => mf,
            Err(e) if e.class == "UnicodeDecodeError" => {
                plan.unreadable.push(path.as_str().to_string());
                continue;
            }
            Err(e) => return Err(e),
        };
        if !repos.contains_key(project.as_str()) {
            let r = project_repo(&project, repo_aliases, roots)?;
            repos.insert(project.as_str().to_string(), r);
        }
        let Some(repo) = repos[project.as_str()].clone() else {
            plan.unmapped.push(path.as_str().to_string());
            continue;
        };
        plan.yaml_errors += i64::from(mf.yaml_error);
        let item = to_item(&mf, &repo);
        plan.items.push((mf, item));
    }
    Ok(plan)
}

/// `automemory_without_file(conn, plan)`: active auto-memory items whose file is gone, as
/// (id, source, its memory folder still exists).
pub fn automemory_without_file(
    conn: &Connection,
    plan: &AutomemoryPlan,
) -> Result<Vec<(String, String, bool)>, SmError> {
    let mut stmt = conn.prepare(
        "SELECT id, source FROM items WHERE status = 'active' AND source LIKE ? ORDER BY rowid",
    )?;
    let rows = stmt
        .query_map([format!("{SOURCE_PREFIX}%")], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let prefix_len = SOURCE_PREFIX.chars().count() as isize;
    // the store holds each source in its redacted form
    let stored: HashSet<String> = plan
        .sources
        .iter()
        .map(|s| redact_text(s, None, None))
        .collect();
    let mut gone = Vec::new();
    for (id, source) in rows {
        if stored.contains(&source) {
            continue;
        }
        let rest = crate::py::str::slice(&source, Some(prefix_len), None);
        let project_dir = rest.split_once('/').map_or(rest, |(a, _)| a);
        let exists = plan.root.join(project_dir).join("memory").is_dir();
        gone.push((id, source, exists));
    }
    Ok(gone)
}

/// The dict `run_automemory_import` returns, keys in Python's order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AutomemoryResult {
    pub added: i64,
    pub updated: i64,
    pub retired: i64,
    pub unchanged: i64,
    pub files: i64,
    pub unreadable: Vec<String>,
    pub unmapped: Vec<String>,
    pub yaml_errors: i64,
    pub dry_run: bool,
}

/// `run_automemory_import(root=..., dry_run=..., home=...)`: sync every auto-memory file to one
/// item; a dry run does the same work and rolls it back.
pub fn run_automemory_import(
    root: Option<&str>,
    dry_run: bool,
    home: Option<&str>,
) -> Result<AutomemoryResult, SmError> {
    let cfg = load_config(home)?;
    let am_root = match root {
        Some(r) => PyPath::new(r),
        None => path_of(cfg2(&cfg, "automemory", "root")?)?,
    }
    .expanduser()?;
    let plan = plan_automemory(&am_root, &aliases(&cfg), &roots_of(&cfg)?)?;
    let mut res = AutomemoryResult::default();
    let conn = connect(get_db_path(home)?.as_str(), None)?;
    if dry_run {
        execute_waiting_for_lock(&conn, "BEGIN IMMEDIATE", None)?;
    }
    // retire first: a renamed file then inserts under its new source instead of deduping
    for (item_id, _source, folder_exists) in automemory_without_file(&conn, &plan)? {
        if folder_exists {
            res.retired += retire_item(&conn, &item_id)?;
        }
    }
    (res.added, res.updated, res.unchanged) = sync_source_items(
        &conn,
        SOURCE_PREFIX,
        plan.items.iter().map(|(_, item)| item),
    )?;
    if dry_run {
        conn.execute_batch("ROLLBACK")?;
    }
    drop(conn);
    res.files = plan.sources.len() as i64;
    res.unreadable = plan.unreadable;
    res.unmapped = plan.unmapped;
    res.yaml_errors = plan.yaml_errors;
    res.dry_run = dry_run;
    Ok(res)
}

/// `cfg["repos"]["roots"]` as the str list the plan takes.
pub(crate) fn roots_of(cfg: &PyValue) -> Result<Vec<String>, SmError> {
    cfg2(cfg, "repos", "roots")?;
    roots(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn it(repo: &str, tag: &str) -> Item {
        Item::new("x", repo, "fact", "t", "b", tag)
    }

    #[test]
    fn count_by_orders_most_first_then_by_key() {
        let items = vec![it("b", "t1"), it("a", "t1"), it("b", "t2"), it("c", "t1")];
        let got: Vec<(String, i64)> = count_by(&items, "repo").into_iter().collect();
        assert_eq!(got, vec![("b".into(), 2), ("a".into(), 1), ("c".into(), 1)]);
        assert!(count_by(&[], "tag").is_empty());
    }

    #[test]
    fn alias_repos_renames_only_listed_repos() {
        let mut al = Aliases::new();
        al.insert("a-wt".into(), "a".into());
        let got: Vec<String> = alias_repos(vec![it("a-wt", "x"), it("b", "x")], &al)
            .into_iter()
            .map(|i| i.repo)
            .collect();
        assert_eq!(got, vec!["a", "b"]);
    }

    #[test]
    fn config_value_conversions_follow_python() {
        assert_eq!(float_of(&PyValue::Str(" 2.5 ".into())).unwrap(), 2.5);
        assert_eq!(float_of(&PyValue::Bool(true)).unwrap(), 1.0);
        assert_eq!(
            float_of(&PyValue::List(vec![])).unwrap_err().python_text(),
            "TypeError: float() argument must be a string or a real number, not 'list'"
        );
        assert_eq!(
            float_of(&PyValue::Str("x".into()))
                .unwrap_err()
                .python_text(),
            "ValueError: could not convert string to float: 'x'"
        );
        assert_eq!(int_of(&PyValue::Float(7.9)).unwrap(), 7);
        assert_eq!(path_of(&PyValue::int(3)).unwrap_err().class, "TypeError");
    }

    /// One git repo `gh/alpha`, its project folder with one memory file `a.md` and a config
    /// whose `[automemory] root` is that projects folder: (tempdir, home, memory folder).
    fn automemory_fixture() -> (tempfile::TempDir, String, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        let repo = t.join("gh/alpha");
        std::fs::create_dir_all(&repo).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(&repo)
                .status()
                .unwrap()
                .success()
        );
        let mem = t.join("projects/p-alpha/memory");
        std::fs::create_dir_all(&mem).unwrap();
        std::fs::write(
            t.join("projects/p-alpha/s1.jsonl"),
            format!("{{\"cwd\": \"{}\"}}\n", repo.display()),
        )
        .unwrap();
        std::fs::write(
            mem.join("a.md"),
            "---\nname: a\ndescription: a\nmetadata:\n  type: project\n  tag: decided\n  modified: 2026-10-02T01:00:00Z\n---\nBody A\n",
        )
        .unwrap();
        let home = t.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("config.toml"),
            format!(
                "[automemory]\nroot = \"{}\"\n[upkeep]\ntasklog_roots = []\nhandoff_dirs = []\n",
                t.join("projects").display()
            ),
        )
        .unwrap();
        (tmp, home.to_str().unwrap().to_string(), mem)
    }

    fn am_counts(r: &AutomemoryResult) -> (i64, i64, i64, i64) {
        (r.added, r.updated, r.retired, r.unchanged)
    }

    #[test]
    fn automemory_dry_run_writes_nothing() {
        let (_tmp, home, _mem) = automemory_fixture();
        let h = home.as_str();
        let r = run_automemory_import(None, true, Some(h)).unwrap();
        assert_eq!((r.added, r.updated, r.retired, r.unchanged), (1, 0, 0, 0));
        let conn = crate::store::connect::connect(
            crate::config::get_db_path(Some(h)).unwrap().as_str(),
            None,
        )
        .unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM items WHERE source LIKE 'automemory:%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
        let r = run_automemory_import(None, false, Some(h)).unwrap();
        assert_eq!((r.added, r.updated, r.retired, r.unchanged), (1, 0, 0, 0));
    }

    /// Another process holds the write lock: a run over unchanged files still finishes, because
    /// an unchanged file needs no write.
    #[test]
    fn automemory_unchanged_file_needs_no_write_lock() {
        let (_tmp, home, _mem) = automemory_fixture();
        let h = home.as_str();
        let r = run_automemory_import(None, false, Some(h)).unwrap();
        assert_eq!(am_counts(&r), (1, 0, 0, 0));
        let holder =
            rusqlite::Connection::open(crate::config::get_db_path(Some(h)).unwrap().as_str())
                .unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        let r = run_automemory_import(None, false, Some(h)).unwrap();
        assert_eq!(am_counts(&r), (0, 0, 0, 1));
        holder.execute_batch("ROLLBACK").unwrap();
    }

    /// A file whose body the store redacts is compared in its stored (redacted) form, so it is
    /// unchanged too and needs no write lock.
    #[test]
    fn automemory_unchanged_redacted_file_needs_no_write_lock() {
        let (_tmp, home, mem) = automemory_fixture();
        let h = home.as_str();
        std::fs::write(
            mem.join("b.md"),
            "---\nname: b\ndescription: b\nmetadata:\n  type: project\n  tag: decided\n---\nSet password=Zx9Qw8Er7Ty6 here\n",
        )
        .unwrap();
        let r = run_automemory_import(None, false, Some(h)).unwrap();
        assert_eq!(am_counts(&r), (2, 0, 0, 0));
        let db = crate::config::get_db_path(Some(h)).unwrap();
        let holder = rusqlite::Connection::open(db.as_str()).unwrap();
        let body: String = holder
            .query_row(
                "SELECT body FROM items WHERE source = 'automemory:p-alpha/b.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(body, "Set [REDACTED:key_value] here");
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        let r = run_automemory_import(None, false, Some(h)).unwrap();
        assert_eq!(am_counts(&r), (0, 0, 0, 2));
        holder.execute_batch("ROLLBACK").unwrap();
    }

    /// A file name the store redacts in the source still matches its file on the next run, so
    /// the item is neither retired nor added again.
    #[test]
    fn automemory_redacted_source_still_matches_its_file() {
        let (_tmp, home, mem) = automemory_fixture();
        let h = home.as_str();
        std::fs::write(
            mem.join("token=Zx9Qw8Er7Ty6.md"),
            "---\nname: b\ndescription: b\nmetadata:\n  type: project\n  tag: decided\n---\nBody B\n",
        )
        .unwrap();
        let r = run_automemory_import(None, false, Some(h)).unwrap();
        assert_eq!((am_counts(&r), r.files), ((2, 0, 0, 0), 2));
        let db = crate::config::get_db_path(Some(h)).unwrap();
        let conn = rusqlite::Connection::open(db.as_str()).unwrap();
        let source: String = conn
            .query_row("SELECT source FROM items WHERE body = 'Body B'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(source, "automemory:p-alpha/[REDACTED:key_value]");
        let r = run_automemory_import(None, false, Some(h)).unwrap();
        assert_eq!(am_counts(&r), (0, 0, 0, 2));
    }

    /// An edit with the same size and mtime updates the item, a retired item of an unchanged
    /// file comes back, and a deleted file retires its item.
    #[test]
    fn automemory_edit_retire_and_delete_still_sync() {
        let (_tmp, home, mem) = automemory_fixture();
        let h = home.as_str();
        let a = mem.join("a.md");
        assert_eq!(
            am_counts(&run_automemory_import(None, false, Some(h)).unwrap()),
            (1, 0, 0, 0)
        );
        let mtime = std::fs::metadata(&a).unwrap().modified().unwrap();
        let text = std::fs::read_to_string(&a)
            .unwrap()
            .replace("Body A", "Body B");
        std::fs::write(&a, text).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&a)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert_eq!(
            am_counts(&run_automemory_import(None, false, Some(h)).unwrap()),
            (0, 1, 0, 0)
        );
        let conn = crate::store::connect::connect(
            crate::config::get_db_path(Some(h)).unwrap().as_str(),
            None,
        )
        .unwrap();
        let active = |conn: &Connection| -> Vec<(String, String)> {
            let mut st = conn
                .prepare("SELECT body, status FROM items WHERE source = 'automemory:p-alpha/a.md' ORDER BY rowid")
                .unwrap();
            st.query_map([], |r| Ok((r.get(0).unwrap(), r.get(1).unwrap())))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(
            active(&conn),
            vec![
                ("Body A".to_string(), "superseded".to_string()),
                ("Body B".to_string(), "active".to_string())
            ]
        );
        conn.execute(
            "UPDATE items SET status = 'superseded', superseded_by = NULL WHERE status = 'active'",
            [],
        )
        .unwrap();
        assert_eq!(
            am_counts(&run_automemory_import(None, false, Some(h)).unwrap()),
            (1, 0, 0, 0)
        );
        assert_eq!(active(&conn).len(), 3);
        assert_eq!(
            active(&conn)[2],
            ("Body B".to_string(), "active".to_string())
        );
        std::fs::remove_file(&a).unwrap();
        assert_eq!(
            am_counts(&run_automemory_import(None, false, Some(h)).unwrap()),
            (0, 0, 1, 0)
        );
        assert_eq!(
            active(&conn)[2],
            ("Body B".to_string(), "superseded".to_string())
        );
    }

    #[test]
    fn unknown_source_kind_is_a_value_error() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_str().unwrap();
        let e = run_import("nope", "/x", false, false, Some(home)).unwrap_err();
        assert_eq!(e.python_text(), "ValueError: Unknown import source: 'nope'");
    }
}
