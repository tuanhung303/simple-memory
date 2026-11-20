//! Ports of the Python catch-up tests (all but `_retire_missing`'s dry run, which sits in
//! src/service/catchup.rs because that function is private) and of the catch-up tests in
//! the Python CLI auto-memory tests. Python ids are the test names; expected values are the
//! literals of the Python tests.
mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use common::{column, git, hold_catchup_lock, open, sm, write};
use serde_json::{Value, json};
use simple_memory::error::Base;
use simple_memory::py::json::{Opts, dumps};
use simple_memory::py::value::PyValue;
use simple_memory::service::backup::run_backup;
use simple_memory::service::catchup::run_catchup;
use simple_memory::service::doctor::{DoctorResult, run_doctor};
use simple_memory::service::import_::run_import;
use simple_memory::service::put::{PutArgs, run_put};

struct Env {
    _d: tempfile::TempDir,
    tmp: PathBuf,
    home: PathBuf,
    repo: PathBuf,
    am: PathBuf,
}

impl Env {
    fn h(&self) -> Option<&str> {
        self.home.to_str()
    }
}

/// The `env` fixture of the catch-up tests.
fn env() -> Env {
    let d = tempfile::tempdir().unwrap();
    let tmp = std::fs::canonicalize(d.path()).unwrap();
    let gh = tmp.join("gh");
    let repo = gh.join("r1");
    std::fs::create_dir_all(repo.join("docs/tasklog")).unwrap();
    git(&["init", "-q", repo.to_str().unwrap()]);
    let am = tmp.join("projects");
    std::fs::create_dir_all(am.join("p1/memory")).unwrap();
    write(
        &am.join("p1/s.jsonl"),
        &format!("{}\n", json!({"cwd": repo.to_str().unwrap()})),
    );
    write(&am.join("p1/memory/a.md"), "---\nname: a\n---\nAuto body\n");
    let transcripts = tmp.join("transcripts");
    std::fs::create_dir_all(&transcripts).unwrap();
    let home = tmp.join("home");
    std::fs::create_dir_all(&home).unwrap();
    write(
        &home.join("config.toml"),
        &format!(
            "[automemory]\nroot = \"{}\"\n[capture]\nroot = \"{}\"\n[upkeep]\ntasklog_roots = [\"{}\"]\nhandoff_dirs = []\n",
            am.display(),
            transcripts.display(),
            gh.display()
        ),
    );
    let e = Env {
        _d: d,
        tmp,
        home,
        repo,
        am,
    };
    put_seed(&e.home, "seed item", "r1");
    e
}

fn put_seed(home: &Path, body: &str, repo: &str) -> String {
    let a = PutArgs {
        tag: "decided",
        repo,
        home: home.to_str(),
        ..Default::default()
    };
    run_put(body, &a).unwrap().0
}

fn tasklog(path: &Path, task_id: &str) -> PathBuf {
    write(
        path,
        &format!(
            "id: {task_id}\ntitle: T {task_id}\nas_of: 2026-11-01T09:00:00+00:00\nresults:\n  - done {task_id}\n"
        ),
    );
    path.to_path_buf()
}

fn import_tasklog(e: &Env, path: &Path) {
    run_import("tasklog", path.to_str().unwrap(), false, false, e.h()).unwrap();
}

fn pv(v: &PyValue) -> Value {
    serde_json::from_str(&dumps(v, &Opts::unicode()).unwrap()).unwrap()
}

/// `sm import catch-up --quiet` in its own process, then the row it appended to catchup.log.
/// A run in this process would hold hook/catchup.lock while other test threads fork `git`: a
/// child between fork and exec holds a copy of the descriptor, and the next catch-up on that
/// home then finds the lock held.
fn catchup_in(home: &Path) -> Value {
    let before = log_rows(home);
    let (code, _, err) = sm(home, &["import", "catch-up", "--quiet"]);
    let rows = log_rows(home);
    if before.len() >= 500 && rows.len() == 500 {
        assert_eq!(
            &rows[..499],
            &before[before.len() - 499..],
            "rotated catchup.log did not append one new row"
        );
    } else {
        assert_eq!(rows.len(), before.len() + 1, "no new catchup.log row");
        assert_eq!(
            &rows[..before.len()],
            &before[..],
            "prior catchup.log rows mutated"
        );
    }
    let row = rows.into_iter().last().expect("a catchup.log row");
    let expected_code = if row["ok"] == json!(false) { 1 } else { 0 };
    assert_eq!(
        code, expected_code,
        "unexpected catch-up exit ({err}) for {row}"
    );
    row
}

fn catchup(e: &Env) -> Value {
    catchup_in(&e.home)
}

fn log_rows(home: &Path) -> Vec<Value> {
    match std::fs::read_to_string(home.join("catchup.log")) {
        Ok(text) => text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn doctor_imports(home: &Path) -> DoctorResult {
    run_doctor(false, false, false, true, home.to_str()).unwrap()
}

fn git_out(args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .env("HOME", std::env::temp_dir())
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn commit(repo: &Path, msg: &str) -> String {
    let r = repo.to_str().unwrap();
    git(&["-C", r, "add", "-A"]);
    git(&[
        "-C",
        r,
        "-c",
        "user.name=a",
        "-c",
        "user.email=a@example.invalid",
        "commit",
        "-q",
        "-m",
        msg,
    ]);
    git_out(&["-C", r, "rev-parse", "HEAD"])
}

fn task_rows(home: &Path) -> Vec<(String, String, Option<String>)> {
    let conn = open(home);
    let mut stmt = conn
        .prepare("SELECT id, status, superseded_by FROM items WHERE kind = 'task' ORDER BY rowid")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn states(home: &Path, sql: &str) -> BTreeMap<String, String> {
    let conn = open(home);
    let mut stmt = conn.prepare(sql).unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn set_mtime(path: &Path, t: SystemTime) {
    std::fs::File::open(path).unwrap().set_modified(t).unwrap();
}

fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn catchup_runs_every_part_and_logs_one_row() {
    let e = env();
    let t = tasklog(
        &e.repo.join("docs/tasklog/2026-11-01/090000-aaaa-t.yaml"),
        "20261101-090000-aaaa-t",
    );
    let row = catchup(&e);
    assert_eq!(row["ok"], json!(true));
    let names: Vec<&str> = row["parts"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        names,
        [
            "backup",
            "automemory",
            "imports",
            "retire_missing",
            "upkeep",
            "sessions",
            "prune"
        ]
    );
    assert_eq!(row["parts"]["automemory"]["counts"]["added"], json!(1));
    assert_eq!(
        row["parts"]["imports"]["counts"],
        json!({"not_stored": 1, "inserted": 1, "failed_rows": 0})
    );
    // before the run's writes
    assert_eq!(
        row["parts"]["backup"]["counts"],
        json!({"made": 1, "deleted": 0, "items": 1})
    );
    assert_eq!(log_rows(&e.home), vec![row.clone()]);
    assert!(row["ts"].as_str().unwrap().ends_with("+00:00"));
    // the second run finds nothing to do and makes no second backup inside the interval
    let again = catchup(&e);
    assert_eq!(
        again["parts"]["automemory"]["counts"]["unchanged"],
        json!(1)
    );
    assert_eq!(again["parts"]["imports"]["counts"]["not_stored"], json!(0));
    assert_eq!(
        again["parts"]["backup"]["counts"],
        json!({"made": 0, "deleted": 0})
    );
    assert_eq!(log_rows(&e.home).len(), 2);
    let doc = doctor_imports(&e.home);
    let (lag, am, last) = doc.lag.unwrap();
    assert_eq!(
        (
            lag.not_stored,
            am.unwrap().automemory_not_stored,
            lag.stored_tasks_without_file
        ),
        (vec![], vec![], vec![])
    );
    assert_eq!(pv(&last.unwrap()), again);
    assert!(t.is_file());
}

#[test]
fn catchup_exits_quietly_while_another_holds_the_lock() {
    let e = env();
    let held = hold_catchup_lock(&e.home);
    assert!(run_catchup(e.h()).unwrap().is_none());
    assert_eq!(
        sm(&e.home, &["import", "catch-up"]),
        (0, String::new(), String::new())
    );
    assert!(!e.home.join("catchup.log").exists());
    drop(held);
    catchup(&e);
}

#[test]
fn catchup_part_failure_is_logged_and_the_rest_still_run() {
    let e = env();
    let cfg = std::fs::read_to_string(e.home.join("config.toml")).unwrap();
    let no_root = e.tmp.join("no-root");
    write(
        &e.home.join("config.toml"),
        &cfg.replace(e.am.to_str().unwrap(), no_root.to_str().unwrap()),
    );
    let row = catchup(&e);
    assert_eq!(row["ok"], json!(false));
    let am = &row["parts"]["automemory"];
    assert_eq!(
        (am["ok"].clone(), am["counts"].clone()),
        (json!(false), json!({}))
    );
    assert_eq!(
        am["error"],
        json!(format!(
            "FileNotFoundError: automemory root not found: {}",
            no_root.display()
        ))
    );
    let others: Vec<Value> = row["parts"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(n, _)| *n != "automemory")
        .map(|(_, p)| p["ok"].clone())
        .collect();
    assert_eq!(others, vec![json!(true); 6]);
    let doc = run_doctor(false, false, false, false, e.h()).unwrap();
    assert_eq!(doc.check.integrity, "ok");
}

#[test]
fn catchup_retires_a_task_item_whose_file_is_gone_everywhere() {
    let e = env();
    let gone = tasklog(
        &e.repo.join("docs/tasklog/2026-11-01/090000-bbbb-g.yaml"),
        "20261101-090000-bbbb-g",
    );
    let kept = tasklog(
        &e.repo.join("docs/tasklog/2026-11-01/090000-cccc-w.yaml"),
        "20261101-090000-cccc-w",
    );
    import_tasklog(&e, &gone);
    import_tasklog(&e, &kept);
    std::fs::remove_file(&gone).unwrap();
    // the second task log now lives only in a linked worktree, which the doctor scan skips
    write(&e.repo.join("seed.txt"), "x\n");
    let r = e.repo.to_str().unwrap();
    git(&["-C", r, "add", "seed.txt"]);
    git(&[
        "-C",
        r,
        "-c",
        "user.name=a",
        "-c",
        "user.email=a@example.invalid",
        "commit",
        "-q",
        "-m",
        "seed",
    ]);
    let wt = e.tmp.join("r1-wt");
    git(&["-C", r, "worktree", "add", "-q", wt.to_str().unwrap()]);
    tasklog(
        &wt.join("docs/tasklog/2026-11-01/090000-cccc-w.yaml"),
        "20261101-090000-cccc-w",
    );
    std::fs::remove_file(&kept).unwrap();
    let row = catchup(&e);
    assert_eq!(
        row["parts"]["retire_missing"]["counts"],
        json!({"candidates": 2, "retired": 1, "in_worktree": 1, "on_branch": 0, "no_folder": 0})
    );
    assert_eq!(
        states(
            &e.home,
            "SELECT source, status || ':' || ifnull(superseded_by, '-') FROM items WHERE kind = 'task'"
        ),
        map(&[
            ("task:20261101-090000-bbbb-g", "superseded:-"),
            ("task:20261101-090000-cccc-w", "active:-")
        ])
    );
}

/// Two task logs with one body: each item can be amended (title, tags, refs, body), both stay
/// active and distinct, each keeps its file as `amended:<source>`, and catch-up neither undoes
/// an amendment nor imports either file again. A manual amend onto a manual body is refused.
#[test]
fn amending_two_task_items_with_one_body_keeps_both_through_catchup() {
    let e = env();
    let dir = e.repo.join("docs/tasklog/2026-11-01");
    for (name, title) in [
        ("090000-auth", "Auth fix TKT-101"),
        ("100000-bill", "Bill fix TKT-202"),
    ] {
        write(
            &dir.join(format!("{name}.yaml")),
            &format!("id: 20261101-{name}\ntitle: {title}\nresults:\n  - All tests pass\n"),
        );
    }
    catchup(&e);
    let id_of = |source: &str| {
        column(
            &e.home,
            &format!("SELECT id FROM items WHERE status = 'active' AND source = '{source}'"),
        )
        .remove(0)
    };
    let amend = |id: &str, args: &[&str]| {
        let mut argv = vec!["amend", id];
        argv.extend_from_slice(args);
        let (code, out, err) = sm(&e.home, &argv);
        // a new body may print the overlap hint for the other item; nothing else
        assert_eq!(code, 0, "{args:?}: {err}");
        assert!(
            err.is_empty() || err.contains(" shares "),
            "{args:?}: {err}"
        );
        out.trim().to_string()
    };
    let a = amend(
        &id_of("task:20261101-090000-auth"),
        &["--title", "Auth final"],
    );
    let b = amend(
        &id_of("task:20261101-100000-bill"),
        &["--title", "Bill final"],
    );
    let b = amend(&b, &["--add-tags", "reviewed"]);
    let b = amend(&b, &["--add-refs", "TKT-303"]);
    let a = amend(&a, &["--add-tags", "reviewed"]);
    let b = amend(&b, &["--body", "Results:\n- All tests pass\n- shipped"]);
    let a = amend(&a, &["--body", "Results:\n- All tests pass\n- shipped"]);
    let active = || {
        states(
            &e.home,
            "SELECT id, source || ' | ' || title || ' | ' || tags || ' | ' || body FROM items \
             WHERE status = 'active' AND kind = 'task'",
        )
    };
    let expected = map(&[
        (
            a.as_str(),
            "amended:task:20261101-090000-auth | Auth final | as_of:mtime, reviewed | Results:\n- All tests pass\n- shipped",
        ),
        (
            b.as_str(),
            "amended:task:20261101-100000-bill | Bill final | as_of:mtime, reviewed | Results:\n- All tests pass\n- shipped",
        ),
    ]);
    assert_eq!(active(), expected);
    let rows = column(&e.home, "SELECT id FROM items ORDER BY rowid").len();
    for _ in 0..2 {
        let row = catchup(&e);
        assert_eq!(
            row["parts"]["imports"]["counts"],
            json!({"not_stored": 0, "inserted": 0, "failed_rows": 0})
        );
        assert_eq!(
            row["parts"]["retire_missing"]["counts"]["retired"],
            json!(0)
        );
        let res = run_import("tasklog", dir.to_str().unwrap(), false, false, e.h()).unwrap();
        assert_eq!((res.inserted, res.deduped, res.superseded), (0, 2, 0));
        assert_eq!(active(), expected);
        assert_eq!(
            column(
                &e.home,
                "SELECT id FROM items WHERE status = 'active' AND ' ' || refs || ' ' LIKE '% TKT-303 %'",
            ),
            [b.as_str()]
        );
        assert_eq!(
            column(&e.home, "SELECT id FROM items ORDER BY rowid").len(),
            rows
        );
    }
    // the refs backfill reads an amended item's evidence (the file path) as it reads a manual
    // item's, as it did when an amend cleared the source
    assert_eq!(sm(&e.home, &["doctor", "--backfill-refs"]).0, 0);
    assert_eq!(
        column(
            &e.home,
            "SELECT id FROM items WHERE status = 'active' AND refs LIKE '%/090000-auth.yaml%'",
        ),
        [a.as_str()]
    );

    // export and import: both amended items (with the seed and auto-memory items) land in a
    // store whose manual item holds their body
    let (code, out, _) = sm(&e.home, &["export", "--repo", "r1"]);
    assert_eq!(code, 0);
    let dump = e.tmp.join("dump.jsonl");
    write(&dump, &out);
    let other = e.tmp.join("other-home");
    std::fs::create_dir_all(&other).unwrap();
    put_seed(&other, "Results:\n- All tests pass\n- shipped", "r1");
    let (code, out, _) = sm(&other, &["import", "jsonl", dump.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(out.contains(" added=4 unchanged=0 conflicts=0 "), "{out}");
    // and back: that manual item lands next to the two amended items that hold its body
    let (code, out, _) = sm(&other, &["export", "--repo", "r1"]);
    assert_eq!(code, 0);
    write(&dump, &out);
    let (code, out, _) = sm(&e.home, &["import", "jsonl", dump.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(out.contains(" added=1 unchanged=4 conflicts=0 "), "{out}");

    // the manual dedupe still holds: a manual item cannot take another manual item's body
    let m1 = put_seed(&e.home, "manual one", "r1");
    let m2 = put_seed(&e.home, "manual two", "r1");
    let (code, _, err) = sm(&e.home, &["amend", &m2, "--body", "manual one"]);
    assert_eq!(code, 1);
    assert!(
        err.contains(&format!(
            "Repo 'r1' already has an active item with the same body ({m1})"
        )),
        "{err}"
    );
}

/// One task id in two repos: catch-up keeps one active item per repo through edits, and a file
/// that moves to the other repo retires the old repo's item by the missing-file path.
#[test]
fn catchup_keeps_one_item_per_repo_for_a_shared_task_id_and_retires_a_moved_file() {
    let e = env();
    let r2 = e.tmp.join("gh/r2");
    std::fs::create_dir_all(r2.join("docs/tasklog")).unwrap();
    git(&["init", "-q", r2.to_str().unwrap()]);
    let tl = |repo: &Path, name: &str, body: &str| {
        let p = repo.join(format!("docs/tasklog/2026-11-01/{name}.yaml"));
        write(
            &p,
            &format!("id: 20261101-{name}\ntitle: T {name}\nresults:\n  - {body}\n"),
        );
        p
    };
    let active = || {
        states(
            &e.home,
            "SELECT repo || ' ' || source, title || ' ' || body FROM items \
             WHERE kind = 'task' AND status = 'active'",
        )
    };
    let imports = |row: &Value| row["parts"]["imports"]["counts"].clone();
    let retired = |row: &Value| row["parts"]["retire_missing"]["counts"].clone();
    let none_retired =
        json!({"candidates": 0, "retired": 0, "in_worktree": 0, "on_branch": 0, "no_folder": 0});
    tl(&e.repo, "090000-shared", "r1 api");
    tl(&r2, "090000-shared", "r2 client");
    let row = catchup(&e);
    assert_eq!(
        imports(&row),
        json!({"not_stored": 2, "inserted": 2, "failed_rows": 0})
    );
    assert_eq!(retired(&row), none_retired);
    let first = map(&[
        (
            "r1 task:20261101-090000-shared",
            "T 090000-shared Results:\n- r1 api",
        ),
        (
            "r2 task:20261101-090000-shared",
            "T 090000-shared Results:\n- r2 client",
        ),
    ]);
    assert_eq!(active(), first);
    let r2_id = column(
        &e.home,
        "SELECT id FROM items WHERE repo = 'r2' AND kind = 'task'",
    );
    tl(&e.repo, "090000-shared", "r1 docs");
    let row = catchup(&e);
    assert_eq!(
        imports(&row),
        json!({"not_stored": 1, "inserted": 1, "failed_rows": 0})
    );
    let mut edited = first.clone();
    edited.insert(
        "r1 task:20261101-090000-shared".into(),
        "T 090000-shared Results:\n- r1 docs".into(),
    );
    assert_eq!(active(), edited);
    assert_eq!(
        column(
            &e.home,
            "SELECT id FROM items WHERE repo = 'r2' AND kind = 'task' AND status = 'active'",
        ),
        r2_id
    );
    let again = catchup(&e);
    assert_eq!(
        imports(&again),
        json!({"not_stored": 0, "inserted": 0, "failed_rows": 0})
    );
    assert_eq!(retired(&again), none_retired);

    // a task log moves from r1 to r2: r2 stores it, r1's item is retired with no replacement
    let moved = tl(&e.repo, "100000-moved", "moved body");
    catchup(&e);
    std::fs::remove_file(&moved).unwrap();
    tl(&r2, "100000-moved", "moved body");
    let lag = doctor_imports(&e.home).lag.unwrap().0;
    assert_eq!(
        (lag.not_stored.len(), lag.stored_tasks_without_file),
        (1, vec!["task:20261101-100000-moved".to_string()])
    );
    let row = catchup(&e);
    assert_eq!(
        imports(&row),
        json!({"not_stored": 1, "inserted": 1, "failed_rows": 0})
    );
    assert_eq!(
        retired(&row),
        json!({"candidates": 1, "retired": 1, "in_worktree": 0, "on_branch": 0, "no_folder": 0})
    );
    assert_eq!(
        states(
            &e.home,
            "SELECT repo || ' ' || status, ifnull(superseded_by, '-') FROM items \
             WHERE source = 'task:20261101-100000-moved'",
        ),
        map(&[("r1 superseded", "-"), ("r2 active", "-")])
    );
    let again = catchup(&e);
    assert_eq!(
        imports(&again),
        json!({"not_stored": 0, "inserted": 0, "failed_rows": 0})
    );
    assert_eq!(retired(&again), none_retired);
    let lag = doctor_imports(&e.home).lag.unwrap().0;
    assert_eq!(
        (lag.not_stored.len(), lag.stored_tasks_without_file.len()),
        (0, 0)
    );
}

/// A checkout that lags its remote, or a branch not yet merged, lacks the file: keep the item.
/// A file deleted in a later commit is in history but at no tip: that item is retired.
#[test]
fn catchup_keeps_a_task_item_whose_file_sits_at_a_branch_tip() {
    let e = env();
    let r = e.repo.to_str().unwrap();
    git(&["-C", r, "checkout", "-q", "-b", "main"]);
    write(&e.repo.join("seed.txt"), "x\n");
    let base = commit(&e.repo, "seed");
    let local = tasklog(
        &e.repo.join("docs/tasklog/2026-11-01/090000-eeee-l.yaml"),
        "20261101-090000-eeee-l",
    );
    let remote = tasklog(
        &e.repo.join("docs/tasklog/2026-11-02/090000-ffff-r.yaml"),
        "20261102-090000-ffff-r",
    );
    let deleted = tasklog(
        &e.repo.join("docs/tasklog/2026-11-03/090000-abab-d.yaml"),
        "20261103-090000-abab-d",
    );
    for t in [&local, &remote, &deleted] {
        import_tasklog(&e, t);
    }
    // local branch "side" holds the first; a remote-tracking ref holds the second
    git(&["-C", r, "checkout", "-q", "-b", "side"]);
    std::fs::rename(&remote, e.tmp.join("remote.yaml")).unwrap();
    std::fs::rename(&deleted, e.tmp.join("deleted.yaml")).unwrap();
    commit(&e.repo, "side");
    git(&["-C", r, "checkout", "-q", "main"]); // removes the file side tracks
    std::fs::rename(e.tmp.join("remote.yaml"), &remote).unwrap();
    let tip = commit(&e.repo, "remote");
    git(&["-C", r, "update-ref", "refs/remotes/origin/main", &tip]);
    git(&["-C", r, "reset", "-q", "--hard", &base]);
    // the third is committed on main, then deleted: in history, at no tip
    std::fs::rename(e.tmp.join("deleted.yaml"), &deleted).unwrap();
    commit(&e.repo, "add deleted");
    std::fs::remove_file(&deleted).unwrap();
    commit(&e.repo, "delete");
    assert!(!(local.exists() || remote.exists() || deleted.exists()));
    let row = catchup(&e);
    assert_eq!(
        row["parts"]["retire_missing"]["counts"],
        json!({"candidates": 3, "retired": 1, "in_worktree": 0, "on_branch": 2, "no_folder": 0})
    );
    assert_eq!(
        states(
            &e.home,
            "SELECT source, status FROM items WHERE kind = 'task'"
        ),
        map(&[
            ("task:20261101-090000-eeee-l", "active"),
            ("task:20261102-090000-ffff-r", "active"),
            ("task:20261103-090000-abab-d", "superseded"),
        ])
    );
}

#[test]
fn a_retired_task_log_whose_file_comes_back_is_restored() {
    let e = env();
    let t = tasklog(
        &e.repo.join("docs/tasklog/2026-11-01/090000-acac-b.yaml"),
        "20261101-090000-acac-b",
    );
    import_tasklog(&e, &t);
    let [(item_id, _, _)] = <[_; 1]>::try_from(task_rows(&e.home)).unwrap();
    let held = std::fs::read_to_string(&t).unwrap();
    std::fs::remove_file(&t).unwrap();
    assert_eq!(
        catchup(&e)["parts"]["retire_missing"]["counts"]["retired"],
        json!(1)
    );
    assert_eq!(
        task_rows(&e.home),
        vec![(item_id.clone(), "superseded".into(), None)]
    );
    write(&t, &held); // a pull brings the file back
    assert_eq!(
        doctor_imports(&e.home).lag.unwrap().0.not_stored,
        vec![t.to_str().unwrap().to_string()]
    );
    let row = catchup(&e);
    assert_eq!(
        row["parts"]["imports"]["counts"],
        json!({"not_stored": 1, "inserted": 1, "failed_rows": 0})
    );
    assert_eq!(
        task_rows(&e.home),
        vec![(item_id.clone(), "active".into(), None)]
    ); // the same row, not a copy
    let again = catchup(&e);
    assert_eq!(
        again["parts"]["imports"]["counts"],
        json!({"not_stored": 0, "inserted": 0, "failed_rows": 0})
    );
    assert_eq!(
        again["parts"]["retire_missing"]["counts"]["candidates"],
        json!(0)
    );
    assert_eq!(task_rows(&e.home), vec![(item_id, "active".into(), None)]);
}

#[test]
fn a_retired_task_log_comes_back_when_a_manual_item_holds_its_body() {
    let e = env();
    let t = tasklog(
        &e.repo.join("docs/tasklog/2026-11-01/090000-adad-c.yaml"),
        "20261101-090000-adad-c",
    );
    import_tasklog(&e, &t);
    let [(item_id, _, _)] = <[_; 1]>::try_from(task_rows(&e.home)).unwrap();
    let held = std::fs::read_to_string(&t).unwrap();
    std::fs::remove_file(&t).unwrap();
    catchup(&e);
    let body = column(
        &e.home,
        &format!("SELECT body FROM items WHERE id = '{item_id}'"),
    )
    .remove(0);
    let other = put_seed(&e.home, &body, "r1");
    write(&t, &held);
    // a manual item does not stand in for the file: the file is not stored until it comes back
    assert_eq!(
        doctor_imports(&e.home).lag.unwrap().0.not_stored,
        [t.to_str().unwrap()]
    );
    let res = run_import("tasklog", t.to_str().unwrap(), false, false, e.h()).unwrap();
    assert_eq!((res.inserted, res.deduped), (1, 0));
    assert_eq!(
        task_rows(&e.home),
        vec![(item_id.clone(), "active".into(), None)]
    );
    assert_ne!(other, item_id);
}

#[test]
fn a_restored_task_log_supersedes_the_newer_version_of_its_file() {
    let e = env();
    let t = tasklog(
        &e.repo.join("docs/tasklog/2026-11-01/090000-aeae-v.yaml"),
        "20261101-090000-aeae-v",
    );
    import_tasklog(&e, &t);
    let [(first, _, _)] = <[_; 1]>::try_from(task_rows(&e.home)).unwrap();
    let held = std::fs::read_to_string(&t).unwrap();
    std::fs::remove_file(&t).unwrap();
    catchup(&e);
    write(&t, &format!("{held}  - edited\n"));
    import_tasklog(&e, &t);
    let [_, (second, _, _)] = <[_; 2]>::try_from(task_rows(&e.home)).unwrap();
    write(&t, &held); // the edit is reverted
    assert_eq!(
        run_import("tasklog", t.to_str().unwrap(), false, false, e.h())
            .unwrap()
            .inserted,
        1
    );
    assert_eq!(
        task_rows(&e.home),
        vec![
            (first.clone(), "active".into(), None),
            (second, "superseded".into(), Some(first))
        ]
    );
}

#[test]
fn a_task_item_another_item_replaced_is_not_restored_by_a_reimport() {
    let e = env();
    let t = tasklog(
        &e.repo.join("docs/tasklog/2026-11-01/090000-afaf-s.yaml"),
        "20261101-090000-afaf-s",
    );
    import_tasklog(&e, &t);
    let [(item_id, _, _)] = <[_; 1]>::try_from(task_rows(&e.home)).unwrap();
    let a = PutArgs {
        tag: "decided",
        repo: "r1",
        supersedes: simple_memory::core::refs::RefsArg::List(vec![item_id.clone()]),
        home: e.h(),
        ..Default::default()
    };
    let newer = run_put("a newer finding replaces the task", &a).unwrap().0;
    assert_eq!(
        doctor_imports(&e.home).lag.unwrap().0.not_stored,
        Vec::<String>::new()
    );
    let res = run_import("tasklog", t.to_str().unwrap(), false, false, e.h()).unwrap();
    assert_eq!((res.inserted, res.deduped), (0, 1));
    assert_eq!(
        task_rows(&e.home),
        vec![(item_id, "superseded".into(), Some(newer))]
    );
}

fn body_of(home: &Path, id: &str) -> String {
    column(home, &format!("SELECT body FROM items WHERE id = '{id}'")).remove(0)
}

#[test]
fn catchup_reimports_a_stored_task_log_whose_body_changed_and_supersedes_the_old_item() {
    let e = env();
    let t = tasklog(
        &e.repo.join("docs/tasklog/2026-11-01/090000-baba-e.yaml"),
        "20261101-090000-baba-e",
    );
    import_tasklog(&e, &t);
    let [(old, _, _)] = <[_; 1]>::try_from(task_rows(&e.home)).unwrap();
    let held = std::fs::read_to_string(&t).unwrap();
    write(&t, &format!("{held}  - edited by hand\n"));
    assert_eq!(
        doctor_imports(&e.home).lag.unwrap().0.not_stored,
        vec![t.to_str().unwrap().to_string()]
    );
    let row = catchup(&e);
    assert_eq!(
        row["parts"]["imports"]["counts"],
        json!({"not_stored": 1, "inserted": 1, "failed_rows": 0})
    );
    let [_, (new, _, _)] = <[_; 2]>::try_from(task_rows(&e.home)).unwrap();
    assert_eq!(
        task_rows(&e.home),
        vec![
            (old.clone(), "superseded".into(), Some(new.clone())),
            (new.clone(), "active".into(), None)
        ]
    );
    assert_eq!(
        body_of(&e.home, &new),
        "Results:\n- done 20261101-090000-baba-e\n- edited by hand"
    );
    let again = catchup(&e);
    assert_eq!(
        again["parts"]["imports"]["counts"],
        json!({"not_stored": 0, "inserted": 0, "failed_rows": 0})
    );
    assert_eq!(
        task_rows(&e.home),
        vec![
            (old, "superseded".into(), Some(new.clone())),
            (new, "active".into(), None)
        ]
    );
}

/// `sm import catch-up --quiet` at a UTC offset: the counts of its imports part.
fn catchup_at(home: &Path, offset: &str) -> Value {
    let (code, _, err) = common::sm_env(
        home,
        &["import", "catch-up", "--quiet"],
        &[("SM_UTC_OFFSET", offset)],
    );
    assert_eq!(code, 0, "{err}");
    log_rows(home).pop().unwrap()["parts"]["imports"]["counts"].clone()
}

/// Every row of the store but the auto-memory notes: id, status, superseded_by.
fn all_rows(home: &Path) -> Vec<(String, String, Option<String>)> {
    let conn = open(home);
    let mut stmt = conn
        .prepare(
            "SELECT id, status, superseded_by FROM items \
             WHERE substr(source, 1, 11) != 'automemory:' ORDER BY rowid",
        )
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// A task log imported once and not edited gets no new version from catch-up runs, whatever
/// else changed around it: its mtime, the run's UTC offset, its repo dir, a second root that
/// reaches it, refs a later step added, a redaction at the write, a retire or an amend by hand.
#[test]
fn catchup_leaves_an_unchanged_task_log_alone_in_every_state() {
    let e = env();
    let day = e.repo.join("docs/tasklog/2026-11-01");
    let file = |name: &str, text: String| {
        let p = day.join(format!("{name}.yaml"));
        write(&p, &text);
        p
    };
    let id = |n: &str| format!("20261101-090000-cafe-{n}");
    let akia = format!("AKIA{}", "Q".repeat(16));
    let files = [
        file(
            "a",
            tl_text(
                &id("a"),
                &[
                    BASE[0],
                    "state: done, blocked",
                    BASE[2],
                    "commits: [c0ffee1, abc1234]",
                ],
            ),
        ),
        // no commits: the evidence is the file path
        file("b", tl_text(&id("b"), &BASE)),
        // no as_of: the parser takes the file's mtime and tags it as_of:mtime
        file("c", tl_text(&id("c"), &[BASE[0], BASE[1]])),
        // a date alone and a time with no offset are read in the run's local zone
        file(
            "d",
            tl_text(&id("d"), &[BASE[0], BASE[1], "as_of: 2026-11-01"]),
        ),
        file(
            "e",
            tl_text(&id("e"), &[BASE[0], BASE[1], "as_of: 2026-11-01T09:00:00"]),
        ),
        // the store keeps a redacted title, tags, refs and body
        file(
            "f",
            format!(
                "id: {}\ntitle: key {akia}\nstate: done, {akia}\ncontinues: {akia}\nas_of: 2026-11-01T09:00:00+00:00\nresults:\n  - plan <private>made-up note</private> done\n",
                id("f")
            ),
        ),
        // refs a later step added; a retire by hand; an amend by hand
        file("g", tl_text(&id("g"), &BASE)),
        file("h", tl_text(&id("h"), &BASE)),
        file("i", tl_text(&id("i"), &BASE)),
        // a ref the body gives that the stored row lacks (an older ref extractor)
        file(
            "j",
            format!(
                "id: {}\ntitle: T one\nas_of: 2026-11-01T09:00:00+00:00\nresults:\n  - fixed docs/x/y.md\n",
                id("j")
            ),
        ),
    ];
    // a task log under another folder, with its repo in the file, whose dir later moves
    let moved_from = e.tmp.join("gh/m1/docs/tasklog/2026-11-01/m.yaml");
    write(
        &moved_from,
        &tl_text(&id("m"), &["repo: r1", BASE[0], BASE[1], BASE[2]]),
    );
    // a second root that reaches the same files
    let cfg = std::fs::read_to_string(e.home.join("config.toml")).unwrap();
    let gh = e.tmp.join("gh");
    write(
        &e.home.join("config.toml"),
        &cfg.replace(
            &format!("tasklog_roots = [\"{}\"]", gh.display()),
            &format!(
                "tasklog_roots = [\"{}\", \"{}\"]",
                gh.display(),
                e.tmp.display()
            ),
        ),
    );
    for f in files.iter().chain([&moved_from]) {
        sm_utc(&e.home, &["import", "tasklog", f.to_str().unwrap()]);
    }
    let item_of = |n: &str| {
        column(
            &e.home,
            &format!(
                "SELECT id FROM items WHERE source = 'task:{}' AND status = 'active'",
                id(n)
            ),
        )
        .remove(0)
    };
    let conn = open(&e.home);
    conn.execute(
        &format!(
            "UPDATE items SET refs = refs || ' made-up-extra' WHERE id = '{}'",
            item_of("g")
        ),
        [],
    )
    .unwrap();
    drop(conn);
    assert_eq!(
        column(
            &e.home,
            &format!("SELECT refs FROM items WHERE id = '{}'", item_of("j"))
        ),
        [format!("{} docs/x/y.md", id("j"))]
    );
    let conn = open(&e.home);
    conn.execute(
        &format!(
            "UPDATE items SET refs = '{}' WHERE id = '{}'",
            id("j"),
            item_of("j")
        ),
        [],
    )
    .unwrap();
    drop(conn);
    let retired = item_of("h");
    sm_utc(&e.home, &["retire", &retired, "--reason", "done-by-hand"]);
    let amended_from = item_of("i");
    let amended = sm_utc(&e.home, &["amend", &amended_from, "--title", "I amended"]);
    let amended = amended.split_whitespace().last().unwrap().to_string();
    let clean = json!({"not_stored": 0, "inserted": 0, "failed_rows": 0});
    assert_eq!(catchup_at(&e.home, "+00:00"), clean);
    let before = all_rows(&e.home);
    assert_eq!(
        before.iter().filter(|r| r.1 == "active").count(),
        // seed, a..g, i's amended row, j, m
        11
    );
    // an mtime change, a moved repo dir, two other offsets
    let later = SystemTime::now() + Duration::from_secs(3600);
    for f in &files {
        set_mtime(f, later);
    }
    let moved_to = e.tmp.join("gh/m2");
    std::fs::rename(e.tmp.join("gh/m1"), &moved_to).unwrap();
    for offset in ["-05:00", "+07:00", "+00:00"] {
        assert_eq!(catchup_at(&e.home, offset), clean, "at {offset}");
        assert_eq!(all_rows(&e.home), before, "at {offset}");
    }
    assert_eq!(
        doctor_imports(&e.home).lag.unwrap().0.not_stored,
        Vec::<String>::new()
    );
    assert_eq!(
        column(
            &e.home,
            &format!("SELECT status FROM items WHERE id = '{retired}'")
        ),
        ["superseded"]
    );
    assert_eq!(
        column(
            &e.home,
            &format!("SELECT title FROM items WHERE id = '{amended}' AND status = 'active'")
        ),
        ["I amended"]
    );
    // the amend replaced the file's own row: an edit of the file after it is not imported
    // (today's rule for a task log with no active row of its source)
    write(
        &files[8],
        &tl_text(&id("i"), &["title: I edited", BASE[1], BASE[2]]),
    );
    assert_eq!(catchup_at(&e.home, "+00:00"), clean);
    assert_eq!(all_rows(&e.home), before);
}

/// The binary at a fixed offset (stored as_of values then end in `+00:00`): exit 0, stdout.
fn sm_utc(home: &Path, args: &[&str]) -> String {
    let (code, out, err) = common::sm_env(home, args, &[("SM_UTC_OFFSET", "+00:00")]);
    assert_eq!(code, 0, "sm {args:?} failed: {err}");
    out
}

/// The `imports:` line of `sm doctor --imports`.
fn doctor_line(home: &Path) -> String {
    let out = sm_utc(home, &["doctor", "--imports"]);
    out.lines()
        .find(|l| l.starts_with("imports: "))
        .expect("an imports line")
        .to_string()
}

fn lag_line(not_stored: usize) -> String {
    format!(
        "imports: tasklog_files=1 handoff_files=0 not_stored={not_stored} unreadable=0 stored_tasks_without_file=0"
    )
}

/// The active task row: title, tags, as_of, tag, refs, evidence.
fn active_task(home: &Path) -> Vec<[String; 6]> {
    let conn = open(home);
    let mut stmt = conn
        .prepare(
            "SELECT title, tags, as_of, tag, refs, evidence FROM items \
             WHERE kind = 'task' AND status = 'active' ORDER BY rowid",
        )
        .unwrap();
    stmt.query_map([], |r| {
        Ok([
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
        ])
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

/// A task log with the results text every case keeps, and the header lines a case sets.
fn tl_text(id: &str, head: &[&str]) -> String {
    format!(
        "id: {id}\n{}\nresults:\n  - poller fixed and tested\n",
        head.join("\n")
    )
}

const BASE: [&str; 3] = [
    "title: T one",
    "state: in_progress",
    "as_of: 2026-11-01T09:00:00+00:00",
];

/// One edit that keeps the results text: the header before and after, then the active row the
/// store must hold after the import (`{f}` is the file path, `{id}` the task id).
struct Edit {
    name: &'static str,
    before: Vec<&'static str>,
    after: Vec<&'static str>,
    row: [&'static str; 6],
}

fn edits() -> Vec<Edit> {
    let base = BASE.to_vec();
    let with = |extra: &'static str| {
        let mut v = BASE.to_vec();
        v.push(extra);
        v
    };
    vec![
        Edit {
            name: "title",
            before: base.clone(),
            after: vec![BASE[1], BASE[2], "title: T renamed"],
            row: [
                "T renamed",
                "in_progress",
                "2026-11-01T09:00:00+00:00",
                "derived",
                "{id}",
                "{f}",
            ],
        },
        Edit {
            name: "state",
            before: base.clone(),
            after: vec![BASE[0], "state: done", BASE[2]],
            row: [
                "T one",
                "done",
                "2026-11-01T09:00:00+00:00",
                "derived",
                "{id}",
                "{f}",
            ],
        },
        Edit {
            name: "as_of",
            before: base.clone(),
            after: vec![BASE[0], BASE[1], "as_of: 2026-11-02T09:30:00+00:00"],
            row: [
                "T one",
                "in_progress",
                "2026-11-02T09:30:00+00:00",
                "derived",
                "{id}",
                "{f}",
            ],
        },
        Edit {
            name: "add commits",
            before: base.clone(),
            after: with("commits: [c0ffee1]"),
            row: [
                "T one",
                "in_progress",
                "2026-11-01T09:00:00+00:00",
                "executed",
                "{id} c0ffee1",
                "c0ffee1",
            ],
        },
        Edit {
            name: "change commits",
            before: with("commits: [aaaa111]"),
            after: with("commits: [bbbb222]"),
            row: [
                "T one",
                "in_progress",
                "2026-11-01T09:00:00+00:00",
                "executed",
                "{id} bbbb222",
                "bbbb222",
            ],
        },
        Edit {
            name: "drop one of two commits",
            before: with("commits: [aaaa111, bbbb222]"),
            after: with("commits: [aaaa111]"),
            row: [
                "T one",
                "in_progress",
                "2026-11-01T09:00:00+00:00",
                "executed",
                "{id} aaaa111",
                "aaaa111",
            ],
        },
        Edit {
            name: "drop every commit",
            before: with("commits: [aaaa111]"),
            after: base.clone(),
            row: [
                "T one",
                "in_progress",
                "2026-11-01T09:00:00+00:00",
                "derived",
                "{id}",
                "{f}",
            ],
        },
        Edit {
            name: "add continues",
            before: base.clone(),
            after: with("continues: 20261031-080000-cafe-p"),
            row: [
                "T one",
                "in_progress",
                "2026-11-01T09:00:00+00:00",
                "derived",
                "{id} 20261031-080000-cafe-p",
                "{f}",
            ],
        },
        Edit {
            name: "all at once",
            before: base,
            after: vec![
                "title: T renamed",
                "state: done",
                "as_of: 2026-11-02T09:30:00+00:00",
                "commits: [c0ffee1]",
            ],
            row: [
                "T renamed",
                "done",
                "2026-11-02T09:30:00+00:00",
                "executed",
                "{id} c0ffee1",
                "c0ffee1",
            ],
        },
    ]
}

/// Each edit of a stored task log that keeps its results text, imported again by
/// `sm import tasklog <dir>` and by `sm import catch-up`: doctor lists the file, the import
/// stores a new version that replaces the old item, and the next catch-up changes nothing.
#[test]
fn a_task_log_edited_outside_its_results_is_stored_again_by_import_and_catchup() {
    let mut failures: Vec<String> = Vec::new();
    for route in ["import tasklog", "catch-up"] {
        for edit in edits() {
            let e = env();
            let id = "20261101-090000-cafe-m";
            let dir = e.repo.join("docs/tasklog");
            let f = dir.join("2026-11-01/090000-cafe-m.yaml");
            write(&f, &tl_text(id, &edit.before));
            sm_utc(&e.home, &["import", "tasklog", dir.to_str().unwrap()]);
            let [(old, _, _)] = <[_; 1]>::try_from(task_rows(&e.home)).unwrap();
            write(&f, &tl_text(id, &edit.after));
            let case = format!("{route} / {}", edit.name);
            let mut check = |what: &str, got: String, want: String| {
                if got != want {
                    failures.push(format!("{case}: {what}: got {got:?}, want {want:?}"));
                }
            };
            check("doctor before", doctor_line(&e.home), lag_line(1));
            if route == "import tasklog" {
                let out = sm_utc(&e.home, &["import", "tasklog", dir.to_str().unwrap()]);
                check(
                    "import counts",
                    out.lines().last().unwrap_or("").to_string(),
                    "total=1 inserted=1 deduped=0 superseded=1 rejected=0 supersede_missing=0 failed_rows=0".into(),
                );
            } else {
                let (code, _, err) = common::sm_env(
                    &e.home,
                    &["import", "catch-up", "--quiet"],
                    &[("SM_UTC_OFFSET", "+00:00")],
                );
                assert_eq!(code, 0, "{err}");
                let row = log_rows(&e.home).pop().unwrap();
                check(
                    "catch-up counts",
                    row["parts"]["imports"]["counts"].to_string(),
                    json!({"not_stored": 1, "inserted": 1, "failed_rows": 0}).to_string(),
                );
            }
            let want: Vec<String> = edit
                .row
                .iter()
                .map(|v| v.replace("{id}", id).replace("{f}", f.to_str().unwrap()))
                .collect();
            let got: Vec<Vec<String>> = active_task(&e.home)
                .into_iter()
                .map(|r| r.to_vec())
                .collect();
            check(
                "active rows",
                format!("{got:?}"),
                format!("{:?}", vec![want]),
            );
            let rows = task_rows(&e.home);
            let new = rows.last().map(|r| r.0.clone()).unwrap_or_default();
            check(
                "task rows",
                format!("{rows:?}"),
                format!(
                    "{:?}",
                    vec![
                        (old.clone(), "superseded".to_string(), Some(new.clone())),
                        (new.clone(), "active".to_string(), None)
                    ]
                ),
            );
            check("doctor after", doctor_line(&e.home), lag_line(0));
            let again = catchup(&e);
            check(
                "next catch-up",
                again["parts"]["imports"]["counts"].to_string(),
                json!({"not_stored": 0, "inserted": 0, "failed_rows": 0}).to_string(),
            );
            check(
                "rows after the next catch-up",
                format!("{:?}", task_rows(&e.home)),
                format!("{rows:?}"),
            );
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Body A with title 1, then body B, then body A with title 2: one import makes one item active
/// and it shows title 2; the row of A with title 1 is not woken.
#[test]
fn a_reverted_body_with_new_fields_is_stored_with_the_new_fields() {
    let e = env();
    let id = "20261101-090000-cafe-r";
    let dir = e.repo.join("docs/tasklog");
    let f = dir.join("2026-11-01/r.yaml");
    let a1 = tl_text(id, &BASE);
    write(&f, &a1);
    sm_utc(&e.home, &["import", "tasklog", dir.to_str().unwrap()]);
    write(&f, &format!("{a1}  - and a second line\n"));
    sm_utc(&e.home, &["import", "tasklog", dir.to_str().unwrap()]);
    let [(a_1, _, _), (b, _, _)] = <[_; 2]>::try_from(task_rows(&e.home)).unwrap();
    write(&f, &tl_text(id, &["title: T two", BASE[1], BASE[2]]));
    assert_eq!(doctor_line(&e.home), lag_line(1));
    let out = sm_utc(&e.home, &["import", "tasklog", dir.to_str().unwrap()]);
    assert_eq!(
        out.lines().last().unwrap(),
        "total=1 inserted=1 deduped=0 superseded=1 rejected=0 supersede_missing=0 failed_rows=0"
    );
    let rows = task_rows(&e.home);
    let a_2 = rows[2].0.clone();
    assert_eq!(
        rows,
        vec![
            (a_1, "superseded".into(), Some(b.clone())),
            (b, "superseded".into(), Some(a_2.clone())),
            (a_2, "active".into(), None),
        ]
    );
    assert_eq!(
        column(
            &e.home,
            "SELECT title FROM items WHERE kind = 'task' AND status = 'active'"
        ),
        ["T two"]
    );
    assert_eq!(doctor_line(&e.home), lag_line(0));
    assert_eq!(
        catchup_at(&e.home, "+00:00"),
        json!({"not_stored": 0, "inserted": 0, "failed_rows": 0})
    );
    assert_eq!(task_rows(&e.home), rows);
}

/// Fields 2, then fields 1, then body B, then fields 2 again: the first row, which holds
/// fields 2, is made active again; no fourth row is stored.
#[test]
fn a_revert_to_earlier_fields_makes_the_row_that_holds_them_active() {
    let e = env();
    let id = "20261101-090000-cafe-w";
    let dir = e.repo.join("docs/tasklog");
    let f = dir.join("2026-11-01/w.yaml");
    let two = tl_text(id, &["title: T two", BASE[1], BASE[2]]);
    let import = |text: &str| {
        write(&f, text);
        sm_utc(&e.home, &["import", "tasklog", dir.to_str().unwrap()]);
    };
    import(&two);
    import(&tl_text(id, &BASE));
    import(&format!("{two}  - and a second line\n"));
    let ids: Vec<String> = task_rows(&e.home).into_iter().map(|r| r.0).collect();
    assert_eq!(ids.len(), 3);
    import(&two);
    assert_eq!(
        task_rows(&e.home),
        vec![
            (ids[0].clone(), "active".into(), None),
            (ids[1].clone(), "superseded".into(), Some(ids[2].clone())),
            (ids[2].clone(), "superseded".into(), Some(ids[0].clone())),
        ]
    );
}

/// The env with these handoff folders in its config.
fn with_handoff_dirs(e: &Env, dirs: &[&Path]) {
    let cfg = std::fs::read_to_string(e.home.join("config.toml")).unwrap();
    let list: Vec<String> = dirs
        .iter()
        .map(|d| format!("\"{}\"", d.display()))
        .collect();
    write(
        &e.home.join("config.toml"),
        &cfg.replace(
            "handoff_dirs = []",
            &format!("handoff_dirs = [{}]", list.join(", ")),
        ),
    );
}

/// The active handoff rows: title, tags, as_of.
fn active_handoffs(home: &Path) -> Vec<[String; 3]> {
    let conn = open(home);
    let mut stmt = conn
        .prepare(
            "SELECT title, tags, as_of FROM items \
             WHERE kind = 'handoff' AND status = 'active' ORDER BY rowid",
        )
        .unwrap();
    stmt.query_map([], |r| Ok([r.get(0)?, r.get(1)?, r.get(2)?]))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// A handoff edited in its `#` title or its `Written:` line with the same kept sections, imported
/// again by `sm import handoff <dir>` and by `sm import catch-up`: doctor lists it, the import
/// stores a new version that replaces the old item, and the next catch-up changes nothing.
#[test]
fn a_handoff_edited_outside_its_kept_sections_is_stored_again_by_import_and_catchup() {
    let before = "# H1\nWritten: 2026-11-01T09:00:00Z\n## Done\n- handed over\n";
    let edits = [
        (
            "title",
            "# H1 renamed\nWritten: 2026-11-01T09:00:00Z\n## Done\n- handed over\n",
            ["H1 renamed", "handoff", "2026-11-01T09:00:00+00:00"],
        ),
        (
            "Written",
            "# H1\nWritten: 2026-11-02T10:30:00Z\n## Done\n- handed over\n",
            ["H1", "handoff", "2026-11-02T10:30:00+00:00"],
        ),
    ];
    let lag = |n: usize| {
        format!(
            "imports: tasklog_files=0 handoff_files=1 not_stored={n} unreadable=0 stored_tasks_without_file=0"
        )
    };
    let mut failures: Vec<String> = Vec::new();
    for route in ["import handoff", "catch-up"] {
        for (name, after, row) in &edits {
            let e = env();
            let hdir = e.tmp.join("handoffs");
            let h = hdir.join("h1.md");
            write(&h, before);
            with_handoff_dirs(&e, &[&hdir]);
            sm_utc(&e.home, &["import", "handoff", hdir.to_str().unwrap()]);
            let [(old, _, _)] = <[_; 1]>::try_from(handoff_rows(&e.home)).unwrap();
            write(&h, after);
            let case = format!("{route} / {name}");
            let mut check = |what: &str, got: String, want: String| {
                if got != want {
                    failures.push(format!("{case}: {what}: got {got:?}, want {want:?}"));
                }
            };
            check("doctor before", doctor_line(&e.home), lag(1));
            if route == "import handoff" {
                let out = sm_utc(&e.home, &["import", "handoff", hdir.to_str().unwrap()]);
                check(
                    "import counts",
                    out.lines().last().unwrap_or("").to_string(),
                    "total=1 inserted=1 deduped=0 superseded=1 rejected=0 supersede_missing=0 failed_rows=0".into(),
                );
            } else {
                check(
                    "catch-up counts",
                    catchup_at(&e.home, "+00:00").to_string(),
                    json!({"not_stored": 1, "inserted": 1, "failed_rows": 0}).to_string(),
                );
            }
            let want: Vec<String> = row.iter().map(|v| v.to_string()).collect();
            let got: Vec<Vec<String>> = active_handoffs(&e.home)
                .into_iter()
                .map(|r| r.to_vec())
                .collect();
            check(
                "active rows",
                format!("{got:?}"),
                format!("{:?}", vec![want]),
            );
            let rows = handoff_rows(&e.home);
            let new = rows.last().map(|r| r.0.clone()).unwrap_or_default();
            check(
                "handoff rows",
                format!("{rows:?}"),
                format!(
                    "{:?}",
                    vec![
                        (old.clone(), "superseded".to_string(), Some(new.clone())),
                        (new.clone(), "active".to_string(), None)
                    ]
                ),
            );
            check("doctor after", doctor_line(&e.home), lag(0));
            check(
                "next catch-up",
                catchup_at(&e.home, "+07:00").to_string(),
                json!({"not_stored": 0, "inserted": 0, "failed_rows": 0}).to_string(),
            );
            check(
                "rows after the next catch-up",
                format!("{:?}", handoff_rows(&e.home)),
                format!("{rows:?}"),
            );
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A handoff imported once and not edited gets no new version from catch-up runs: an mtime
/// change (with and without a `Written:` date), the run's UTC offset, a moved handoff folder,
/// a `CWD:` line or none, a retire or an amend by hand.
#[test]
fn catchup_leaves_an_unchanged_handoff_alone_in_every_state() {
    let e = env();
    let hdir = e.tmp.join("handoffs");
    let cwd = format!("CWD: {}\n", e.repo.display());
    let doc = |title: &str, written: &str, extra: &str, done: &str| {
        format!("# {title}\n{written}{extra}## Done\n- {done}\n")
    };
    let files = [
        ("a", doc("A", "Written: 2026-11-01T09:00:00Z\n", "", "a")),
        // a Written: line the parser cannot read: the as_of is the file's mtime
        ("b", doc("B", "Written: soon\n", "", "b")),
        ("c", doc("C", "", "", "c")),
        // a date alone and a time with no offset
        ("d", doc("D", "Written: 2026-11-01\n", "", "d")),
        ("e", doc("E", "Written: 2026-11-01 09:00\n", "", "e")),
        // a CWD the repos map resolves
        ("f", doc("F", "Written: 2026-11-01T09:00:00Z\n", &cwd, "f")),
        // a retire and an amend by hand
        ("g", doc("G", "Written: 2026-11-01T09:00:00Z\n", "", "g")),
        ("h", doc("H", "Written: 2026-11-01T09:00:00Z\n", "", "h")),
    ];
    for (name, text) in &files {
        write(&hdir.join(format!("{name}.md")), text);
    }
    with_handoff_dirs(&e, &[&hdir]);
    let cfg = std::fs::read_to_string(e.home.join("config.toml")).unwrap();
    write(
        &e.home.join("config.toml"),
        &format!(
            "{cfg}[repos]\nroots = [\"{}\"]\n",
            e.tmp.join("gh").display()
        ),
    );
    sm_utc(&e.home, &["import", "handoff", hdir.to_str().unwrap()]);
    let item_of = |n: &str| {
        column(
            &e.home,
            &format!("SELECT id FROM items WHERE source = 'handoff:{n}.md' AND status = 'active'"),
        )
        .remove(0)
    };
    assert_eq!(
        column(
            &e.home,
            &format!("SELECT repo FROM items WHERE id = '{}'", item_of("f"))
        ),
        ["r1"]
    );
    let retired = item_of("g");
    sm_utc(&e.home, &["retire", &retired, "--reason", "done-by-hand"]);
    let amended = sm_utc(&e.home, &["amend", &item_of("h"), "--title", "H amended"]);
    let amended = amended.split_whitespace().last().unwrap().to_string();
    let clean = json!({"not_stored": 0, "inserted": 0, "failed_rows": 0});
    assert_eq!(catchup_at(&e.home, "+00:00"), clean);
    let before = all_rows(&e.home);
    // seed, a..f, h's amended row
    assert_eq!(before.iter().filter(|r| r.1 == "active").count(), 8);
    let later = SystemTime::now() + Duration::from_secs(3600);
    for (name, _) in &files {
        set_mtime(&hdir.join(format!("{name}.md")), later);
    }
    // the handoff folder moves, and the config follows it
    let moved = e.tmp.join("handoffs-moved");
    std::fs::rename(&hdir, &moved).unwrap();
    let cfg = std::fs::read_to_string(e.home.join("config.toml")).unwrap();
    write(
        &e.home.join("config.toml"),
        &cfg.replace(
            &format!("\"{}\"]", hdir.display()),
            &format!("\"{}\"]", moved.display()),
        ),
    );
    for offset in ["-05:00", "+07:00", "+00:00"] {
        assert_eq!(catchup_at(&e.home, offset), clean, "at {offset}");
        assert_eq!(all_rows(&e.home), before, "at {offset}");
    }
    assert_eq!(
        column(
            &e.home,
            &format!("SELECT status FROM items WHERE id = '{retired}'")
        ),
        ["superseded"]
    );
    assert_eq!(
        column(
            &e.home,
            &format!("SELECT title FROM items WHERE id = '{amended}' AND status = 'active'")
        ),
        ["H amended"]
    );
}

/// Two handoffs with one file name in two folders that map to one repo, with the same kept
/// sections and other titles: each catch-up makes the other's row active, and no row is added
/// after the first run.
#[test]
fn two_handoffs_with_one_name_and_one_repo_add_no_row_after_the_first_catchup() {
    let e = env();
    let one = e.tmp.join("ho1");
    let two = e.tmp.join("ho2");
    let cwd = format!("CWD: {}\n", e.repo.display());
    write(
        &one.join("dup.md"),
        &format!("# One\nWritten: 2026-11-01T09:00:00Z\n{cwd}## Done\n- same\n"),
    );
    write(
        &two.join("dup.md"),
        &format!("# Two\nWritten: 2026-11-01T09:00:00Z\n{cwd}## Done\n- same\n"),
    );
    with_handoff_dirs(&e, &[&one, &two]);
    let cfg = std::fs::read_to_string(e.home.join("config.toml")).unwrap();
    write(
        &e.home.join("config.toml"),
        &format!(
            "{cfg}[repos]\nroots = [\"{}\"]\n",
            e.tmp.join("gh").display()
        ),
    );
    let mut counts = Vec::new();
    let mut titles = Vec::new();
    for _ in 0..4 {
        catchup_at(&e.home, "+00:00");
        counts.push(handoff_rows(&e.home).len());
        titles.push(
            column(
                &e.home,
                "SELECT title FROM items WHERE kind = 'handoff' AND status = 'active'",
            )
            .join(","),
        );
    }
    assert_eq!(counts, [2, 2, 2, 2]);
    assert_eq!(titles, ["Two", "One", "Two", "One"]);
}

/// Two copies of one task log (one task id, one repo) that differ in their fields only: each
/// catch-up makes the other copy's row active, and after the second run no row is added.
#[test]
fn two_copies_of_a_task_log_that_differ_in_fields_add_no_row_after_the_second_catchup() {
    let e = env();
    let id = "20261101-090000-cafe-k";
    let one = e.repo.join("docs/tasklog/2026-11-01/k.yaml");
    let two = e.tmp.join("gh/r1copy/docs/tasklog/2026-11-01/k.yaml");
    write(
        &one,
        &tl_text(id, &["repo: r1", "title: T one", BASE[1], BASE[2]]),
    );
    write(
        &two,
        &tl_text(id, &["repo: r1", "title: T two", BASE[1], BASE[2]]),
    );
    let mut counts = Vec::new();
    let mut titles = Vec::new();
    for _ in 0..4 {
        catchup_at(&e.home, "+00:00");
        counts.push(task_rows(&e.home).len());
        titles.push(
            column(
                &e.home,
                "SELECT title FROM items WHERE kind = 'task' AND status = 'active'",
            )
            .join(","),
        );
    }
    // run 1 stores both copies in turn; run 2 inserts nothing new, it only switches rows
    assert_eq!(counts, [2, 2, 2, 2]);
    assert_eq!(titles, ["T two", "T one", "T two", "T one"]);
    assert_eq!(
        column(
            &e.home,
            "SELECT count(*) || '' FROM items WHERE kind = 'task' AND status = 'active'"
        ),
        ["1"]
    );
}

/// A row the previous build wrote for `commits: abc1234` (one ref per character, and the same
/// characters as evidence) is replaced by one catch-up with a row that holds the commit id.
#[test]
fn a_task_row_with_one_ref_per_commit_character_is_replaced_once() {
    let e = env();
    let id = "20261101-090000-cafe-u";
    let dir = e.repo.join("docs/tasklog");
    let f = dir.join("2026-11-01/u.yaml");
    write(
        &f,
        &tl_text(id, &[BASE[0], BASE[1], BASE[2], "commits: abc1234"]),
    );
    sm_utc(&e.home, &["import", "tasklog", dir.to_str().unwrap()]);
    let [(old, _, _)] = <[_; 1]>::try_from(task_rows(&e.home)).unwrap();
    // the refs and evidence the previous build stored for this file
    let conn = open(&e.home);
    conn.execute(
        &format!(
            "UPDATE items SET refs = '{id} a b c 1 2 3 4', evidence = 'a, b, c, 1, 2, 3, 4' \
             WHERE id = '{old}'"
        ),
        [],
    )
    .unwrap();
    drop(conn);
    assert_eq!(
        catchup_at(&e.home, "+00:00"),
        json!({"not_stored": 1, "inserted": 1, "failed_rows": 0})
    );
    let rows = task_rows(&e.home);
    let new = rows[1].0.clone();
    assert_eq!(
        rows,
        vec![
            (old, "superseded".into(), Some(new.clone())),
            (new, "active".into(), None)
        ]
    );
    assert_eq!(
        column(
            &e.home,
            "SELECT refs FROM items WHERE kind = 'task' AND status = 'active'"
        ),
        [format!("{id} abc1234")]
    );
    assert_eq!(
        catchup_at(&e.home, "+00:00"),
        json!({"not_stored": 0, "inserted": 0, "failed_rows": 0})
    );
    assert_eq!(task_rows(&e.home).len(), 2);
}

#[test]
fn catchup_retires_a_deleted_task_log_and_imports_nothing_for_it() {
    let e = env();
    let t = tasklog(
        &e.repo.join("docs/tasklog/2026-11-01/090000-baba-d.yaml"),
        "20261101-090000-baba-d",
    );
    import_tasklog(&e, &t);
    let [(id, _, _)] = <[_; 1]>::try_from(task_rows(&e.home)).unwrap();
    std::fs::remove_file(&t).unwrap();
    let row = catchup(&e);
    assert_eq!(
        row["parts"]["imports"]["counts"],
        json!({"not_stored": 0, "inserted": 0, "failed_rows": 0})
    );
    assert_eq!(
        row["parts"]["retire_missing"]["counts"],
        json!({"candidates": 1, "retired": 1, "in_worktree": 0, "on_branch": 0, "no_folder": 0})
    );
    assert_eq!(task_rows(&e.home), vec![(id, "superseded".into(), None)]);
}

#[test]
fn catchup_counts_a_malformed_edit_as_a_failed_row_and_imports_the_other_files() {
    let e = env();
    let bad = tasklog(
        &e.repo.join("docs/tasklog/2026-11-01/090000-baba-m.yaml"),
        "20261101-090000-baba-m",
    );
    let good = tasklog(
        &e.repo.join("docs/tasklog/2026-11-01/090000-baba-g.yaml"),
        "20261101-090000-baba-g",
    );
    import_tasklog(&e, &bad);
    import_tasklog(&e, &good);
    let [(bad_id, _, _), (good_id, _, _)] = <[_; 2]>::try_from(task_rows(&e.home)).unwrap();
    let bad_body = body_of(&e.home, &bad_id);
    write(&bad, "id: [unclosed\nresults: {\n");
    let held = std::fs::read_to_string(&good).unwrap();
    write(&good, &format!("{held}  - edited\n"));
    let row = catchup(&e);
    assert_eq!(row["ok"], json!(true));
    assert_eq!(row["parts"]["imports"]["ok"], json!(true));
    assert_eq!(
        row["parts"]["imports"]["counts"],
        json!({"not_stored": 1, "inserted": 1, "failed_rows": 1})
    );
    let rows = task_rows(&e.home);
    assert_eq!(rows.len(), 3);
    // the malformed file's item stays as it was: active, same body, not retired
    assert_eq!(rows[0], (bad_id.clone(), "active".into(), None));
    assert_eq!(body_of(&e.home, &bad_id), bad_body);
    assert_eq!(
        rows[1],
        (good_id, "superseded".into(), Some(rows[2].0.clone()))
    );
    assert_eq!(
        row["parts"]["retire_missing"]["counts"]["retired"],
        json!(0)
    );
}

#[test]
fn catchup_imports_a_handoff_the_doctor_lists() {
    let e = env();
    let hdir = e.tmp.join("handoffs");
    write(&hdir.join("h1.md"), "# H1\n## Done\n- handed over\n");
    let cfg = std::fs::read_to_string(e.home.join("config.toml")).unwrap();
    write(
        &e.home.join("config.toml"),
        &cfg.replace(
            "handoff_dirs = []",
            &format!("handoff_dirs = [\"{}\"]", hdir.display()),
        ),
    );
    let row = catchup(&e);
    assert_eq!(
        row["parts"]["imports"]["counts"],
        json!({"not_stored": 1, "inserted": 1, "failed_rows": 0})
    );
    assert_eq!(
        doctor_imports(&e.home).lag.unwrap().0.not_stored,
        Vec::<String>::new()
    );
}

#[test]
fn catchup_retires_nothing_in_a_repo_without_a_tasklog_folder() {
    let e = env();
    let elsewhere = tasklog(
        &e.tmp.join("x/r9/docs/tasklog/2026-11-01/1.yaml"),
        "20261101-090000-dddd-x",
    );
    import_tasklog(&e, &elsewhere);
    std::fs::remove_file(&elsewhere).unwrap();
    let row = catchup(&e);
    assert_eq!(
        row["parts"]["retire_missing"]["counts"],
        json!({"candidates": 1, "retired": 0, "in_worktree": 0, "on_branch": 0, "no_folder": 1})
    );
}

#[test]
fn catchup_retires_by_disk_alone_in_a_tasklog_folder_outside_git() {
    let e = env();
    let plain = e.tmp.join("plain");
    let cfg = std::fs::read_to_string(e.home.join("config.toml")).unwrap();
    write(
        &e.home.join("config.toml"),
        &cfg.replace(
            "tasklog_roots = [",
            &format!("tasklog_roots = [\"{}\", ", plain.display()),
        ),
    );
    let gone = tasklog(
        &plain.join("r2/docs/tasklog/2026-11-01/090000-eeee-g.yaml"),
        "20261101-090000-eeee-g",
    );
    let kept = tasklog(
        &plain.join("r2/docs/tasklog/2026-11-01/090000-ffff-k.yaml"),
        "20261101-090000-ffff-k",
    );
    import_tasklog(&e, &gone);
    import_tasklog(&e, &kept);
    std::fs::remove_file(&gone).unwrap();
    let row = catchup(&e);
    let part = &row["parts"]["retire_missing"];
    assert_eq!(
        (part["ok"].clone(), part["counts"].clone()),
        (
            json!(true),
            json!({"candidates": 1, "retired": 1, "in_worktree": 0, "on_branch": 0, "no_folder": 0})
        )
    );
    assert_eq!(row["ok"], json!(true));
    assert_eq!(
        states(
            &e.home,
            "SELECT source, status FROM items WHERE kind = 'task'"
        ),
        map(&[
            ("task:20261101-090000-eeee-g", "superseded"),
            ("task:20261101-090000-ffff-k", "active")
        ])
    );
}

#[test]
fn catchup_prune_deletes_only_old_hook_state() {
    let e = env();
    let hook = e.home.join("hook");
    std::fs::create_dir_all(&hook).unwrap();
    let old = SystemTime::now() - Duration::from_secs(15 * 86400);
    for n in [
        "claude-old.json",
        "agy-old",
        "claude-new.json",
        "agy-new",
        "claude-old.txt",
        "backoff.json",
    ] {
        write(&hook.join(n), "{}");
    }
    std::fs::create_dir(hook.join("agy-dir")).unwrap();
    for n in [
        "claude-old.json",
        "agy-old",
        "claude-old.txt",
        "backoff.json",
        "agy-dir",
    ] {
        set_mtime(&hook.join(n), old);
    }
    let row = catchup(&e);
    assert_eq!(row["parts"]["prune"]["counts"], json!({"deleted": 2}));
    let mut names: Vec<String> = std::fs::read_dir(&hook)
        .unwrap()
        .map(|d| d.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "agy-dir",
            "agy-new",
            "backoff.json",
            "catchup.lock",
            "claude-new.json",
            "claude-old.txt"
        ]
    );
}

#[test]
fn backup_copies_checks_and_keeps_the_newest_auto_copies() {
    let e = env();
    let bdir = e.home.join("backups");
    std::fs::create_dir(&bdir).unwrap();
    let olds: Vec<PathBuf> = (1..8)
        .map(|d| bdir.join(format!("memory-2026010{d}-000000-auto.db")))
        .collect();
    for f in &olds {
        write(f, "");
    }
    let others = [
        "memory-20260101-000000-pre-refs.db",
        "memory-20260101-000000-auto.db-wal",
        "config-x.toml",
    ];
    for n in others {
        write(&bdir.join(n), "");
    }
    let res = run_backup(&simple_memory::py::int::from_str("7").unwrap(), e.h()).unwrap();
    assert_eq!(
        (res.quick_check.as_str(), res.items, res.deleted.clone()),
        ("ok", 1, vec![olds[0].to_str().unwrap().to_string()])
    );
    let mut left: Vec<String> = std::fs::read_dir(&bdir)
        .unwrap()
        .map(|d| d.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    let mut want: Vec<String> = olds[1..]
        .iter()
        .map(|f| f.file_name().unwrap().to_str().unwrap().to_string())
        .collect();
    want.push(
        Path::new(&res.path)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string(),
    );
    want.extend(others.iter().map(|s| s.to_string()));
    want.sort();
    assert_eq!(left, want);
    let copy = rusqlite::Connection::open(&res.path).unwrap();
    let titles: Vec<String> = copy
        .prepare("SELECT title FROM items")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(titles, ["seed item"]);
}

#[test]
fn backup_refuses_keep_below_one_and_a_missing_store() {
    let d = tempfile::tempdir().unwrap();
    let h = d.path().to_str().unwrap();
    let err = run_backup(&simple_memory::py::int::from_str("0").unwrap(), Some(h)).unwrap_err();
    assert!(
        err.is(Base::Value) && err.msg.contains("--keep must be at least 1"),
        "{err}"
    );
    let err = run_backup(&simple_memory::py::int::from_str("7").unwrap(), Some(h)).unwrap_err();
    assert!(
        err.class == "FileNotFoundError" && err.msg.contains("memory.db not found"),
        "{err}"
    );
}

#[test]
fn catchup_on_a_new_home_skips_the_backup() {
    let d = tempfile::tempdir().unwrap();
    let tmp = std::fs::canonicalize(d.path()).unwrap();
    let home = tmp.join("home");
    let empty = tmp.join("empty");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&empty).unwrap();
    write(
        &home.join("config.toml"),
        &format!(
            "[automemory]\nroot = \"{0}\"\n[capture]\nroot = \"{0}\"\n[upkeep]\ntasklog_roots = []\nhandoff_dirs = []\n",
            empty.display()
        ),
    );
    let row = catchup_in(&home);
    assert_eq!(row["ok"], json!(true));
    assert_eq!(
        row["parts"]["backup"]["counts"],
        json!({"made": 0, "deleted": 0})
    );
}

#[test]
fn catchup_skips_automemory_and_sessions_when_off() {
    // a home with no ~/.claude: the default roots are missing, so both parts are switched off
    let d = tempfile::tempdir().unwrap();
    let tmp = std::fs::canonicalize(d.path()).unwrap();
    let home = tmp.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let gone = tmp.join("no-claude/projects");
    write(
        &home.join("config.toml"),
        &format!(
            "[automemory]\nenabled = false\nroot = \"{0}\"\n[capture]\nenabled = false\nroot = \"{0}\"\n",
            gone.display()
        ),
    );
    let row = catchup_in(&home);
    assert_eq!(
        (
            row["ok"].clone(),
            row["parts"]["automemory"]["counts"].clone(),
            row["parts"]["sessions"]["counts"].clone()
        ),
        (json!(true), json!({"skipped": 1}), json!({"skipped": 1}))
    );
    // switched on, the same missing roots fail both parts loudly
    write(
        &home.join("config.toml"),
        &format!(
            "[automemory]\nroot = \"{0}\"\n[capture]\nroot = \"{0}\"\n",
            gone.display()
        ),
    );
    let row = catchup_in(&home);
    let failed: Vec<&str> = row["parts"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, p)| p["ok"] == json!(false))
        .map(|(n, _)| n.as_str())
        .collect();
    assert_eq!(failed, ["automemory", "sessions"]);
}

/// Each row is 1,000 bytes with its newline.
fn fill_log(home: &Path, rows: usize) {
    let pad = "x".repeat(970);
    let text: String = (0..rows)
        .map(|n| format!("{:<999}\n", format!("{{\"n\": {n}, \"pad\": \"{pad}\"}}")))
        .collect();
    write(&home.join("catchup.log"), &text);
}

#[test]
fn catchup_log_over_1mb_keeps_the_newest_500_rows() {
    let e = env();
    fill_log(&e.home, 1100); // 1,100,000 bytes
    let row = catchup(&e);
    let lines = log_rows(&e.home);
    assert_eq!(lines.len(), 500);
    assert_eq!(lines[0]["n"], json!(601)); // rows 601 to 1099, then the new row
    assert_eq!(lines[499], row);
    let mut names: Vec<String> = std::fs::read_dir(&e.home)
        .unwrap()
        .map(|d| d.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with("catchup.log"))
        .collect();
    names.sort();
    assert_eq!(names, ["catchup.log"]);
}

#[test]
fn catchup_log_under_1mb_is_kept_whole() {
    let e = env();
    fill_log(&e.home, 990); // 990,000 bytes plus the new row stays under 1,000,000
    catchup(&e);
    let lines = log_rows(&e.home);
    assert_eq!(lines.len(), 991);
    assert_eq!(lines[0]["n"], json!(0));
}

#[test]
fn doctor_reports_a_last_catchup_row_that_is_not_json_as_a_failed_catchup() {
    let e = env();
    write(
        &e.home.join("catchup.log"),
        "{\"ok\": true, \"ts\": \"2026-11-01T00:00:00+00:00\", \"parts\": {}}\n{\"ts\": \"2026-1",
    );
    let doc = doctor_imports(&e.home);
    let last = pv(&doc.lag.unwrap().2.unwrap());
    assert_eq!(
        last,
        json!({"ok": false, "ts": "unknown", "parts": {"log": {"error": "last catchup.log row is not JSON"}}})
    );
    assert_eq!(doc.check.integrity, "ok");
}

// ---------------------------------------------------------------- CLI auto-memory

/// The `home` fixture of the CLI tests: a repo, one auto-memory file, an empty
/// transcript root, no task log roots.
fn cli_env() -> Env {
    let d = tempfile::tempdir().unwrap();
    let tmp = std::fs::canonicalize(d.path()).unwrap();
    let repo = tmp.join("gh/alpha");
    std::fs::create_dir_all(&repo).unwrap();
    git(&["init", "-q", repo.to_str().unwrap()]);
    let am = tmp.join("projects");
    std::fs::create_dir_all(am.join("p1/memory")).unwrap();
    write(
        &am.join("p1/s.jsonl"),
        &format!("{}\n", json!({"cwd": repo.to_str().unwrap()})),
    );
    write(
        &am.join("p1/memory/a.md"),
        "---\nname: a\ndescription: Title A\n---\nBody A\n",
    );
    let transcripts = tmp.join("transcripts");
    std::fs::create_dir_all(&transcripts).unwrap();
    let home = tmp.join("home");
    std::fs::create_dir_all(&home).unwrap();
    write(
        &home.join("config.toml"),
        &format!(
            "[automemory]\nroot = \"{}\"\n[capture]\nroot = \"{}\"\n[upkeep]\ntasklog_roots = []\nhandoff_dirs = []\n",
            am.display(),
            transcripts.display()
        ),
    );
    Env {
        _d: d,
        tmp,
        home,
        repo,
        am,
    }
}

#[test]
fn cli_catch_up_quiet_and_lock_held() {
    let e = cli_env();
    let held = hold_catchup_lock(&e.home);
    assert_eq!(
        sm(&e.home, &["import", "catch-up"]),
        (0, String::new(), String::new())
    );
    drop(held);
    assert_eq!(
        sm(&e.home, &["import", "catch-up", "--quiet"]),
        (0, String::new(), String::new())
    );
    let (code, out, _) = sm(&e.home, &["import", "catch-up"]);
    assert_eq!(code, 0);
    let out: Vec<&str> = out.lines().collect();
    // the first run found no store to copy
    assert_eq!(out[0], "backup: ok made=1 deleted=0 items=1");
    assert_eq!(
        out[1],
        "automemory: ok added=0 updated=0 retired=0 unchanged=1 unreadable=0 unmapped=0"
    );
}

#[test]
fn cli_catch_up_failure_exits_1_and_doctor_shows_it() {
    let e = cli_env();
    let cfg = std::fs::read_to_string(e.home.join("config.toml")).unwrap();
    let gone = e.tmp.join("gone");
    write(
        &e.home.join("config.toml"),
        &cfg.replace(e.am.to_str().unwrap(), gone.to_str().unwrap()),
    );
    assert_eq!(
        sm(&e.home, &["import", "catch-up", "--quiet"]),
        (1, String::new(), String::new())
    );
    write(&e.home.join("config.toml"), &cfg);
    let (code, out, _) = sm(&e.home, &["doctor", "--imports"]);
    assert_eq!(code, 0);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines[2],
        "automemory: files=1 not_stored=1 stored_without_file=0 unmapped=0 unreadable=0"
    );
    assert_eq!(
        lines[3],
        format!("not_stored: {}", e.am.join("p1/memory/a.md").display())
    );
    assert!(
        lines[4].starts_with("last_catchup: {\"ts\": "),
        "{}",
        lines[4]
    );
    assert_eq!(
        lines[5],
        format!(
            "catchup_error: automemory: FileNotFoundError: automemory root not found: {}",
            gone.display()
        )
    );
}

#[test]
fn cli_catch_up_skips_every_writing_part_when_the_backup_fails() {
    let e = cli_env();
    sm(
        &e.home,
        &["put", "seed item", "--tag", "decided", "--repo", "alpha"],
    );
    write(&e.home.join("backups"), "x\n"); // mkdir of the backup folder fails
    let (code, out, _) = sm(&e.home, &["import", "catch-up"]);
    assert_eq!(code, 1);
    let lines: Vec<&str> = out.lines().collect();
    assert!(
        lines[0].starts_with("backup: error: FileExistsError: "),
        "{}",
        lines[0]
    );
    assert_eq!(
        lines[1..],
        [
            "automemory: error: skipped: backup failed",
            "imports: error: skipped: backup failed",
            "retire_missing: error: skipped: backup failed",
            "upkeep: error: skipped: backup failed",
            "sessions: error: skipped: backup failed",
            "prune: ok deleted=0",
        ]
    );
    let row = log_rows(&e.home).remove(0);
    assert_eq!(row["ok"], json!(false));
    let mut upkeep_part = row["parts"]["upkeep"].as_object().unwrap().clone();
    upkeep_part.remove("ms");
    assert_eq!(
        Value::Object(upkeep_part),
        json!({"ok": false, "counts": {}, "error": "skipped: backup failed"})
    );
    let (code, out, _) = sm(&e.home, &["query", "Body A", "--repo", "alpha"]);
    assert_eq!(code, 0);
    assert!(!out.contains("Title A")); // the auto-memory file was not imported
}

/// The real race: two processes; the one that loses the lock prints nothing and exits 0. A stub
/// `git` first on PATH makes the winner's retire step take 6 s while it holds the lock, so the
/// second process (started at once) finds the lock held. (Python holds the lock by patching
/// flock until the other process has tried; Rust has no such hook.)
#[test]
fn cli_two_catch_ups_started_together_log_one_row() {
    let e = cli_env();
    let cfg = std::fs::read_to_string(e.home.join("config.toml")).unwrap();
    let gh = e.tmp.join("gh");
    write(
        &e.home.join("config.toml"),
        &cfg.replace(
            "tasklog_roots = []",
            &format!("tasklog_roots = [\"{}\"]", gh.display()),
        ),
    );
    // a task item whose file is gone, so retire_missing runs git
    let t = tasklog(
        &e.repo.join("docs/tasklog/2026-11-01/090000-aaaa-t.yaml"),
        "20261101-090000-aaaa-t",
    );
    run_import(
        "tasklog",
        t.to_str().unwrap(),
        false,
        false,
        e.home.to_str(),
    )
    .unwrap();
    std::fs::remove_file(&t).unwrap();
    let real_git = String::from_utf8(Command::new("which").arg("git").output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_string();
    let bin = e.tmp.join("bin");
    let stub = bin.join("git");
    write(
        &stub,
        &format!("#!/bin/sh\nsleep 6\nexec {real_git} \"$@\"\n"),
    );
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let spawn = || {
        Command::new(env!("CARGO_BIN_EXE_sm"))
            .args(["import", "catch-up", "--quiet"])
            .env("HOME", e.tmp.as_path())
            .env("SIMPLE_MEMORY_HOME", &e.home)
            .env("PATH", &path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let procs = [spawn(), spawn()];
    let results: Vec<(Option<i32>, Vec<u8>, Vec<u8>)> = procs
        .into_iter()
        .map(|p| {
            let o = p.wait_with_output().unwrap();
            (o.status.code(), o.stdout, o.stderr)
        })
        .collect();
    assert_eq!(
        results,
        vec![(Some(0), vec![], vec![]), (Some(0), vec![], vec![])]
    );
    assert_eq!(log_rows(&e.home).len(), 1);
}

fn handoff_rows(home: &Path) -> Vec<(String, String, Option<String>)> {
    let conn = open(home);
    let mut stmt = conn
        .prepare(
            "SELECT id, status, superseded_by FROM items WHERE kind = 'handoff' ORDER BY rowid",
        )
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn catchup_reimports_a_stored_handoff_whose_body_changed_and_supersedes_the_old() {
    let e = env();
    let hdir = e.tmp.join("handoffs");
    let hpath = hdir.join("20261101-090000-00000000-0000-4000-8000-000000000001.md");
    write(
        &hpath,
        "# HANDOFF: H1\nWritten: 2026-11-01T09:00:00+00:00\n## Recap\n- initial recap <private>secret note</private>\n",
    );
    let cfg = std::fs::read_to_string(e.home.join("config.toml")).unwrap();
    write(
        &e.home.join("config.toml"),
        &cfg.replace(
            "handoff_dirs = []",
            &format!("handoff_dirs = [\"{}\"]", hdir.display()),
        ),
    );
    let first = catchup(&e);
    assert_eq!(
        first["parts"]["imports"]["counts"],
        json!({"not_stored": 1, "inserted": 1, "failed_rows": 0})
    );
    // Unchanged redacted handoff is not re-imported on the next run.
    let idle = catchup(&e);
    assert_eq!(
        idle["parts"]["imports"]["counts"],
        json!({"not_stored": 0, "inserted": 0, "failed_rows": 0})
    );
    let [(old_id, _, _)] = <[_; 1]>::try_from(handoff_rows(&e.home)).unwrap();

    write(
        &hpath,
        "# HANDOFF: H1\nWritten: 2026-11-01T09:00:00+00:00\n## Recap\n- edited recap by hand\n",
    );
    assert_eq!(
        doctor_imports(&e.home).lag.unwrap().0.not_stored,
        vec![hpath.to_str().unwrap().to_string()]
    );
    let second = catchup(&e);
    assert_eq!(
        second["parts"]["imports"]["counts"],
        json!({"not_stored": 1, "inserted": 1, "failed_rows": 0})
    );
    let [_, (new_id, _, _)] = <[_; 2]>::try_from(handoff_rows(&e.home)).unwrap();
    assert_eq!(
        handoff_rows(&e.home),
        vec![
            (old_id, "superseded".into(), Some(new_id.clone())),
            (new_id, "active".into(), None),
        ]
    );
    let third = catchup(&e);
    assert_eq!(
        third["parts"]["imports"]["counts"],
        json!({"not_stored": 0, "inserted": 0, "failed_rows": 0})
    );
}

#[test]
fn catchup_restores_reverted_tasklog_and_handoff_without_waking_superseded_peers() {
    let e = env();
    let t = tasklog(
        &e.repo.join("docs/tasklog/2026-11-01/090000-baba-v.yaml"),
        "20261101-090000-baba-v",
    );
    let body_a = std::fs::read_to_string(&t).unwrap();
    import_tasklog(&e, &t);
    let [(id_a, _, _)] = <[_; 1]>::try_from(task_rows(&e.home)).unwrap();

    // Edit A -> B and catch-up.
    write(&t, &format!("{body_a}  - variant B\n"));
    let r_b = catchup(&e);
    assert_eq!(
        r_b["parts"]["imports"]["counts"],
        json!({"not_stored": 1, "inserted": 1, "failed_rows": 0})
    );
    let [_, (id_b, _, _)] = <[_; 2]>::try_from(task_rows(&e.home)).unwrap();

    // Revert file B -> A: doctor lists it, catch-up restores id_a active and supersedes id_b.
    write(&t, &body_a);
    assert_eq!(
        doctor_imports(&e.home).lag.unwrap().0.not_stored,
        vec![t.to_str().unwrap().to_string()]
    );
    let r_a = catchup(&e);
    assert_eq!(
        r_a["parts"]["imports"]["counts"],
        json!({"not_stored": 1, "inserted": 1, "failed_rows": 0})
    );
    assert_eq!(
        task_rows(&e.home),
        vec![
            (id_a.clone(), "active".into(), None),
            (id_b, "superseded".into(), Some(id_a)),
        ]
    );
    let steady = catchup(&e);
    assert_eq!(
        steady["parts"]["imports"]["counts"],
        json!({"not_stored": 0, "inserted": 0, "failed_rows": 0})
    );

    // Guard: a handoff superseded by a newer handoff in the same session (via upkeep) has no
    // active item of its own source and MUST NOT be listed as not_stored or resurrected.
    let hdir = e.tmp.join("handoffs");
    let h1 = hdir.join("20261101-090000-00000000-0000-4000-8000-000000000002.md");
    let h2 = hdir.join("20261101-100000-00000000-0000-4000-8000-000000000002.md");
    write(
        &h1,
        "# HANDOFF: S1\nWritten: 2026-11-01T09:00:00+00:00\n## Recap\n- older session handoff\n",
    );
    write(
        &h2,
        "# HANDOFF: S2\nWritten: 2026-11-01T10:00:00+00:00\n## Recap\n- newer session handoff\n",
    );
    let cfg = std::fs::read_to_string(e.home.join("config.toml")).unwrap();
    write(
        &e.home.join("config.toml"),
        &cfg.replace(
            "handoff_dirs = []",
            &format!("handoff_dirs = [\"{}\"]", hdir.display()),
        ),
    );
    let r_ho = catchup(&e);
    assert_eq!(
        (
            r_ho["parts"]["imports"]["counts"]["inserted"].clone(),
            r_ho["parts"]["upkeep"]["counts"]["superseded"].clone()
        ),
        (json!(2), json!(1))
    );
    let [(id_h1, _, _), (id_h2, _, _)] = <[_; 2]>::try_from(handoff_rows(&e.home)).unwrap();
    assert_eq!(
        handoff_rows(&e.home),
        vec![
            (id_h1.clone(), "superseded".into(), Some(id_h2.clone())),
            (id_h2.clone(), "active".into(), None),
        ]
    );
    assert_eq!(
        doctor_imports(&e.home).lag.unwrap().0.not_stored,
        Vec::<String>::new()
    );
    let imp_again = run_import("handoff", hdir.to_str().unwrap(), false, false, e.h()).unwrap();
    assert_eq!((imp_again.inserted, imp_again.deduped), (0, 2));
    assert_eq!(
        handoff_rows(&e.home),
        vec![
            (id_h1.clone(), "superseded".into(), Some(id_h2.clone())),
            (id_h2.clone(), "active".into(), None),
        ]
    );

    // Revert handoff h2 A -> B -> A: restores id_h2 active while id_h1 stays superseded.
    let h2_a = std::fs::read_to_string(&h2).unwrap();
    write(
        &h2,
        "# HANDOFF: S2\nWritten: 2026-11-01T10:00:00+00:00\n## Recap\n- variant B handoff\n",
    );
    let r_hb = catchup(&e);
    assert_eq!(
        r_hb["parts"]["imports"]["counts"],
        json!({"not_stored": 1, "inserted": 1, "failed_rows": 0})
    );
    let [_, _, (id_h2_b, _, _)] = <[_; 3]>::try_from(handoff_rows(&e.home)).unwrap();
    write(&h2, &h2_a);
    let r_ha = catchup(&e);
    assert_eq!(
        r_ha["parts"]["imports"]["counts"],
        json!({"not_stored": 1, "inserted": 1, "failed_rows": 0})
    );
    assert_eq!(
        handoff_rows(&e.home),
        vec![
            (id_h1, "superseded".into(), Some(id_h2.clone())),
            (id_h2.clone(), "active".into(), None),
            (id_h2_b, "superseded".into(), Some(id_h2)),
        ]
    );
}
