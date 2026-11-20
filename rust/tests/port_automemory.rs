//! Ports of the Python auto-memory sync tests (all but the working-directory test, which
//! sits in port_automemory_cwd.rs) and of the Python CLI auto-memory tests
//! (the catch-up ones are in port_catchup.rs). Expected values are the Python tests' literals.
mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{busy_reference, column, git, hold_catchup_lock, open, sm, write};
use serde_json::{Value, json};
use simple_memory::config::load_config;
use simple_memory::py::json::{Opts, dumps};
use simple_memory::service::doctor::{AutomemoryLag, ImportLag, run_doctor};
use simple_memory::service::import_::{AutomemoryResult, run_automemory_import};
use simple_memory::service::put::{PutArgs, run_put};
use simple_memory::service::query::{QueryArgs, run_query};
use simple_memory::service::supersede::run_supersede;
use simple_memory::store::items::{get_item, retire_item};

struct Env {
    _d: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    repo: PathBuf,
    tmp: PathBuf,
}

impl Env {
    fn h(&self) -> Option<&str> {
        self.home.to_str()
    }
}

fn memory(root: &Path, project: &str, name: &str, body: &str, title: &str, type_: &str) -> PathBuf {
    let f = root.join(project).join("memory").join(format!("{name}.md"));
    let title = if title.is_empty() { name } else { title };
    write(
        &f,
        &format!(
            "---\nname: {name}\ndescription: {title}\nmetadata:\n  type: {type_}\n  tag: decided\n  modified: 2026-10-02T01:00:00Z\n---\n{body}\n"
        ),
    );
    f
}

fn mem(e: &Env, project: &str, name: &str, body: &str) -> PathBuf {
    memory(&e.root, project, name, body, "", "project")
}

fn project(root: &Path, project: &str, cwd: Option<&Path>) {
    std::fs::create_dir_all(root.join(project).join("memory")).unwrap();
    if let Some(cwd) = cwd {
        write(
            &root.join(project).join("s1.jsonl"),
            &format!("{}\n", json!({"cwd": cwd.to_str().unwrap()})),
        );
    }
}

/// The `env` fixture of the sync tests.
fn env() -> Env {
    let d = tempfile::tempdir().unwrap();
    let tmp = std::fs::canonicalize(d.path()).unwrap();
    let repo = tmp.join("gh/alpha");
    std::fs::create_dir_all(&repo).unwrap();
    git(&["init", "-q", repo.to_str().unwrap()]);
    let root = tmp.join("projects");
    project(&root, "p-alpha", Some(&repo));
    let home = tmp.join("home");
    std::fs::create_dir_all(&home).unwrap();
    write(
        &home.join("config.toml"),
        &format!(
            "[automemory]\nroot = \"{}\"\n[upkeep]\ntasklog_roots = []\nhandoff_dirs = []\n",
            root.display()
        ),
    );
    Env {
        _d: d,
        root,
        home,
        repo,
        tmp,
    }
}

fn sync(e: &Env) -> AutomemoryResult {
    run_automemory_import(None, false, e.h()).unwrap()
}

fn counts(r: &AutomemoryResult) -> (i64, i64, i64, i64) {
    (r.added, r.updated, r.retired, r.unchanged)
}

fn rows(e: &Env) -> Vec<(String, String, Option<String>, String)> {
    let conn = open(&e.home);
    let mut stmt = conn
        .prepare("SELECT source, status, superseded_by, title FROM items WHERE source LIKE 'automemory:%' ORDER BY rowid")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn s(x: &str) -> String {
    x.to_string()
}

fn lag(e: &Env) -> (ImportLag, AutomemoryLag) {
    let (il, am, _) = run_doctor(false, false, false, true, e.h())
        .unwrap()
        .lag
        .unwrap();
    (il, am.unwrap())
}

fn put_in(e: &Env, body: &str) -> String {
    let a = PutArgs {
        tag: "decided",
        repo: "alpha",
        home: e.h(),
        ..Default::default()
    };
    run_put(body, &a).unwrap().0
}

fn query_ids(e: &Env, text: &str) -> Vec<simple_memory::core::item::Item> {
    let a = QueryArgs {
        repo: "alpha",
        home: e.h(),
        ..Default::default()
    };
    run_query(text, &a, None).unwrap()
}

#[test]
fn sync_twice_is_idempotent_and_files_under_the_cwd_repo() {
    let e = env();
    mem(&e, "p-alpha", "a", "Body A");
    memory(&e.root, "p-alpha", "b", "Body B", "", "feedback");
    assert_eq!(counts(&sync(&e)), (2, 0, 0, 0));
    assert_eq!(counts(&sync(&e)), (0, 0, 0, 2));
    let items = query_ids(&e, "Body B");
    let first = &items[0];
    let want_as_of =
        simple_memory::core::item::validate_as_of(Some("2026-10-02T01:00:00Z"), false, false)
            .unwrap();
    assert_eq!(
        (
            first.repo.as_str(),
            first.kind.as_str(),
            first.tag.as_str(),
            first.as_of.as_str()
        ),
        ("alpha", "decision", "decided", want_as_of.as_str())
    );
}

#[test]
fn sync_dry_run_writes_nothing() {
    let e = env();
    mem(&e, "p-alpha", "a", "Body A");
    assert_eq!(
        counts(&run_automemory_import(None, true, e.h()).unwrap()),
        (1, 0, 0, 0)
    );
    assert_eq!(rows(&e), vec![]);
}

/// Python patches WRITE_LOCK_WAIT_S to 1.0; Rust has a constant (15.0, store/connect.rs:17), so
/// this waits the whole cap. Python run with the constant unpatched: see the report.
#[test]
fn sync_dry_run_under_a_held_write_lock_gives_up_at_the_cap() {
    let e = env();
    mem(&e, "p-alpha", "a", "Body A");
    drop(open(&e.home));
    let db = e.home.join("memory.db");
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let held_db = db.clone();
    let writer = std::thread::spawn(move || {
        let other = rusqlite::Connection::open(held_db).unwrap();
        other.execute_batch("BEGIN IMMEDIATE").unwrap();
        held_tx.send(()).unwrap();
        // Held until the test sends or drops `release_tx`, at most 60 s: a 25 s hold ran out
        // before the 15 s wait on a host whose sleeps run long (21 to 26 s on macos-latest),
        // and with no limit a broken cap would hang the test instead of failing it.
        let _ = release_rx.recv_timeout(Duration::from_secs(60));
        other.execute_batch("COMMIT").unwrap();
    });
    held_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let reference = busy_reference(&db, 15_000);
    let started = Instant::now();
    let err = run_automemory_import(None, true, e.h()).unwrap_err();
    let waited = started.elapsed().as_secs_f64();
    let reference = reference.join().unwrap();
    release_tx.send(()).unwrap();
    writer.join().unwrap();
    assert_eq!(
        format!("{}: {}", err.class, err.msg),
        "OperationalError: database is locked: another sm process holds the write lock (waited 15 s); try again"
    );
    // The 15 s cap plus 5 s, with the busy handler's stretch on this host (busy_reference).
    assert!(
        (14.9..reference + 5.0).contains(&waited),
        "{waited} {reference}"
    );
}

#[test]
fn sync_never_restores_an_item_an_agent_superseded() {
    let e = env();
    mem(&e, "p-alpha", "a", "Body A");
    sync(&e);
    let old = query_ids(&e, "Body A")[0].id.clone();
    let fix = put_in(&e, "Body A corrected");
    run_supersede(&old, &fix, e.h()).unwrap();
    assert_eq!(counts(&sync(&e)), (0, 0, 0, 1));
    assert_eq!(
        rows(&e),
        vec![(
            s("automemory:p-alpha/a.md"),
            s("superseded"),
            Some(fix),
            s("a")
        )]
    );
    // the doctor agrees: an unchanged file is stored even though its item is superseded
    assert_eq!(lag(&e).1.automemory_not_stored, Vec::<String>::new());
}

#[test]
fn sync_title_only_edit_supersedes_the_old_item_first() {
    let e = env();
    let f = mem(&e, "p-alpha", "a", "Same body");
    sync(&e);
    let text = std::fs::read_to_string(&f).unwrap();
    write(
        &f,
        &text.replace("description: a", "description: A retitled"),
    );
    assert_eq!(counts(&sync(&e)), (0, 1, 0, 0));
    let r = rows(&e);
    let new_id = query_ids(&e, "Same body")[0].id.clone();
    assert_eq!(
        r.iter()
            .map(|x| (x.1.clone(), x.3.clone()))
            .collect::<Vec<_>>(),
        vec![(s("superseded"), s("a")), (s("active"), s("A retitled"))]
    );
    assert_eq!(r[0].2, Some(new_id));
}

#[test]
fn sync_body_held_by_another_source_counts_unchanged() {
    let e = env();
    let f = mem(&e, "p-alpha", "a", "First body");
    sync(&e);
    let other = put_in(&e, "Shared body");
    let text = std::fs::read_to_string(&f).unwrap();
    write(&f, &text.replace("First body", "Shared body"));
    assert_eq!(counts(&sync(&e)), (0, 0, 0, 1));
    assert_eq!(
        rows(&e),
        vec![(
            s("automemory:p-alpha/a.md"),
            s("superseded"),
            Some(other),
            s("a")
        )]
    );
    assert_eq!(counts(&sync(&e)), (0, 0, 0, 1));
}

#[test]
fn sync_rename_retires_then_adds_and_settles() {
    let e = env();
    let f = mem(&e, "p-alpha", "a", "Moving body");
    sync(&e);
    std::fs::rename(&f, f.with_file_name("b.md")).unwrap();
    assert_eq!(counts(&sync(&e)), (1, 0, 1, 0));
    assert_eq!(counts(&sync(&e)), (0, 0, 0, 1));
    let r: Vec<_> = rows(&e).into_iter().map(|x| (x.0, x.1, x.2)).collect();
    assert_eq!(
        r,
        vec![
            (s("automemory:p-alpha/a.md"), s("superseded"), None),
            (s("automemory:p-alpha/b.md"), s("active"), None),
        ]
    );
}

#[test]
fn sync_same_body_in_two_projects_keeps_one_item() {
    let e = env();
    project(&e.root, "p-alpha2", Some(&e.repo));
    mem(&e, "p-alpha", "a", "Twin body");
    mem(&e, "p-alpha2", "a", "Twin body");
    assert_eq!(counts(&sync(&e)), (1, 0, 0, 1));
    assert_eq!(counts(&sync(&e)), (0, 0, 0, 2));
}

#[test]
fn sync_deleted_file_comes_back() {
    let e = env();
    let f = mem(&e, "p-alpha", "a", "Returning body");
    let text = std::fs::read_to_string(&f).unwrap();
    sync(&e);
    std::fs::remove_file(&f).unwrap();
    assert_eq!(counts(&sync(&e)), (0, 0, 1, 0));
    write(&f, &text);
    assert_eq!(counts(&sync(&e)), (1, 0, 0, 0));
    assert_eq!(counts(&sync(&e)), (0, 0, 0, 1));
}

#[test]
fn sync_gone_memory_folder_retires_nothing_and_doctor_lists_it() {
    let e = env();
    project(&e.root, "p-gone", Some(&e.repo));
    mem(&e, "p-gone", "g", "Gone body");
    sync(&e);
    std::fs::remove_file(e.root.join("p-gone/memory/g.md")).unwrap();
    std::fs::remove_dir(e.root.join("p-gone/memory")).unwrap();
    assert_eq!(counts(&sync(&e)), (0, 0, 0, 0));
    assert_eq!(
        lag(&e).1.automemory_stored_without_file,
        vec![s("automemory:p-gone/g.md")]
    );
}

/// The Claude Code project folder name of a cwd.
fn key(path: &Path) -> String {
    path.to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn roots_config(e: &Env, code: &Path) {
    let cfg = e.home.join("config.toml");
    let text = std::fs::read_to_string(&cfg).unwrap();
    write(
        &cfg,
        &format!("{text}[repos]\nroots = [\"{}\"]\n", code.display()),
    );
}

/// `sm <args>` run from `cwd`, as `monkeypatch.chdir` runs the Python test.
fn sm_in(e: &Env, cwd: &Path, args: &[&str]) -> (i32, String, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_sm"))
        .args(args)
        .current_dir(cwd)
        .env("HOME", &e.tmp)
        .env("SIMPLE_MEMORY_HOME", &e.home)
        .output()
        .unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

/// Python calls run_automemory_import after `monkeypatch.chdir`; Rust runs the CLI from that
/// folder (a process-wide chdir would reach the other tests of this file).
#[test]
fn sync_unmapped_project_is_skipped_never_global() {
    let e = env();
    // sm may run from a folder whose path holds a project key (a session scratchpad)
    let code = e.tmp.join("code");
    std::fs::create_dir_all(code.join("alpha")).unwrap();
    roots_config(&e, &code);
    let keyed_cwd = e.tmp.join(key(&code.join("alpha-scratch")));
    std::fs::create_dir_all(&keyed_cwd).unwrap();
    // no transcript cwd and no key under a repo root: no repo, so the file is skipped
    project(&e.root, "-private-var-x", None);
    let f = memory(
        &e.root,
        "-private-var-x",
        "u",
        "Client contact body",
        "",
        "user",
    );
    // a cwd outside any git repo resolves to unassigned: skipped too
    let nogit = e.tmp.join("nogit");
    std::fs::create_dir_all(&nogit).unwrap();
    project(&e.root, "-private-var-y", Some(&nogit));
    let g = memory(&e.root, "-private-var-y", "v", "Other body", "", "project");
    let (code_, out, _) = sm_in(&e, &keyed_cwd, &["import", "automemory"]);
    assert_eq!(code_, 0);
    let (f, g) = (f.display(), g.display());
    assert_eq!(
        lines(&out),
        [
            "import automemory (applied): files=2 added=0 updated=0 retired=0 unchanged=0 unreadable=0 unmapped=2 yaml_errors=0".to_string(),
            format!("unmapped: {f}"),
            format!("unmapped: {g}"),
        ]
    );
    assert_eq!(rows(&e), vec![]);
    let (code_, out, _) = sm_in(&e, &keyed_cwd, &["doctor", "--imports"]);
    assert_eq!(code_, 0);
    assert!(
        lines(&out).ends_with(&[
            format!("unmapped: {f}").as_str(),
            format!("unmapped: {g}").as_str(),
            "last_catchup: none"
        ]),
        "{out}"
    );
}

#[test]
fn sync_project_key_maps_when_no_transcript() {
    let e = env();
    let code = e.tmp.join("code");
    std::fs::create_dir_all(code.join("beta")).unwrap();
    roots_config(&e, &code);
    project(&e.root, &key(&code.join("beta")), None);
    mem(&e, &key(&code.join("beta")), "k", "Key body");
    // a key naming no folder under a repo root is unmapped, not unassigned
    project(&e.root, &key(&code.join("zeta")), None);
    let z = mem(&e, &key(&code.join("zeta")), "z", "Zeta body");
    let res = sync(&e);
    assert_eq!(
        (res.added, res.unmapped),
        (1, vec![z.to_str().unwrap().to_string()])
    );
    let a = QueryArgs {
        all_repos: true,
        home: e.h(),
        ..Default::default()
    };
    let repos: Vec<String> = run_query("Key body", &a, None)
        .unwrap()
        .into_iter()
        .map(|i| i.repo)
        .collect();
    assert_eq!(repos, ["beta"]);
}

#[test]
fn sync_unreadable_file_is_listed_and_not_retired() {
    let e = env();
    let f = mem(&e, "p-alpha", "a", "Readable body");
    sync(&e);
    std::fs::write(&f, b"caf\xe9\n").unwrap();
    let res = sync(&e);
    assert_eq!(
        (res.unreadable.clone(), counts(&res)),
        (vec![f.to_str().unwrap().to_string()], (0, 0, 0, 0))
    );
    assert_eq!(
        rows(&e).iter().map(|r| r.1.clone()).collect::<Vec<_>>(),
        ["active"]
    );
    assert_eq!(
        lag(&e).1.automemory_unreadable,
        vec![f.to_str().unwrap().to_string()]
    );
}

#[test]
fn doctor_lists_a_changed_file_that_was_not_synced() {
    let e = env();
    let f = mem(&e, "p-alpha", "a", "Old body");
    sync(&e);
    assert_eq!(lag(&e).1.automemory_not_stored, Vec::<String>::new());
    let text = std::fs::read_to_string(&f).unwrap();
    write(&f, &text.replace("Old body", "New body"));
    assert_eq!(
        lag(&e).1.automemory_not_stored,
        vec![f.to_str().unwrap().to_string()]
    );
}

#[test]
fn sync_missing_root_is_an_error() {
    let e = env();
    let nope = e.tmp.join("nope");
    let err = run_automemory_import(nope.to_str(), false, e.h()).unwrap_err();
    assert!(
        err.class == "FileNotFoundError" && err.msg.contains("automemory root not found"),
        "{err}"
    );
}

#[test]
fn config_merges_automemory_and_backup_sections() {
    let d = tempfile::tempdir().unwrap();
    let h = d.path().to_str().unwrap();
    let cfg = |h| {
        serde_json::from_str::<Value>(
            &dumps(&load_config(Some(h)).unwrap(), &Opts::unicode()).unwrap(),
        )
        .unwrap()
    };
    assert_eq!(cfg(h)["backup"], json!({"interval_hours": 24, "keep": 7}));
    write(
        &d.path().join("config.toml"),
        "[automemory]\nroot = \"/x\"\n[backup]\nkeep = 3\n",
    );
    let c = cfg(h);
    assert_eq!(
        (c["automemory"]["root"].clone(), c["backup"].clone()),
        (json!("/x"), json!({"interval_hours": 24, "keep": 3}))
    );
}

#[test]
fn retire_item_leaves_a_superseded_item_alone() {
    let e = env();
    let a = put_in(&e, "Item one");
    let b = put_in(&e, "Item two");
    run_supersede(&a, &b, e.h()).unwrap();
    let conn = open(&e.home);
    assert_eq!(retire_item(&conn, &a).unwrap(), 0);
    let got = get_item(&conn, &a).unwrap().unwrap();
    assert_eq!(
        (got.status, got.superseded_by),
        (s("superseded"), Some(b.clone()))
    );
    assert_eq!(retire_item(&conn, &b).unwrap(), 1);
    let got = get_item(&conn, &b).unwrap().unwrap();
    assert_eq!((got.status, got.superseded_by), (s("superseded"), None));
}

#[test]
fn file_with_a_secret_is_stored_redacted_and_stays_current() {
    let e = env();
    let key = format!("sk-ant-{}", "a1B2c3D4".repeat(5));
    mem(
        &e,
        "p-alpha",
        "token-note",
        &format!("The staging deploy token is {key} today."),
    );
    assert_eq!(counts(&sync(&e)), (1, 0, 0, 0));
    assert_eq!(counts(&sync(&e)), (0, 0, 0, 1));
    assert_eq!(lag(&e).1.automemory_not_stored, Vec::<String>::new());
    let bodies = column(
        &e.home,
        "SELECT body FROM items WHERE source LIKE 'automemory:%'",
    );
    assert_eq!(bodies.len(), 1);
    assert!(
        !bodies[0].contains(&key) && bodies[0].contains("[REDACTED:sk_key]"),
        "{}",
        bodies[0]
    );
}

// ---------------------------------------------------------------- CLI auto-memory

/// The `home` fixture of the CLI tests.
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
        root: am,
        home,
        repo,
        tmp,
    }
}

fn lines(out: &str) -> Vec<&str> {
    out.lines().collect()
}

#[test]
fn cli_import_automemory_prints_counts_then_zero() {
    let e = cli_env();
    let (code, out, _) = sm(&e.home, &["import", "automemory", "--dry-run"]);
    assert_eq!(code, 0);
    assert_eq!(
        lines(&out),
        [
            "import automemory (dry-run): files=1 added=1 updated=0 retired=0 unchanged=0 unreadable=0 unmapped=0 yaml_errors=0"
        ]
    );
    assert_eq!(sm(&e.home, &["import", "automemory"]).0, 0);
    let (code, out, _) = sm(&e.home, &["import", "automemory"]);
    assert_eq!(code, 0);
    assert!(
        out.starts_with(
            "import automemory (applied): files=1 added=0 updated=0 retired=0 unchanged=1"
        ),
        "{out}"
    );
}

#[test]
fn cli_import_automemory_missing_root_fails() {
    let e = cli_env();
    let nope = e.tmp.join("nope");
    let (code, _, err) = sm(
        &e.home,
        &["import", "automemory", "--root", nope.to_str().unwrap()],
    );
    assert_eq!(code, 1);
    assert_eq!(
        err.trim(),
        format!("error: automemory root not found: {}", nope.display())
    );
}

#[test]
fn cli_show_and_history_label_a_retired_item() {
    let e = cli_env();
    sm(&e.home, &["import", "automemory"]);
    std::fs::remove_file(e.root.join("p1/memory/a.md")).unwrap();
    sm(&e.home, &["import", "automemory"]);
    let (code, out, _) = sm(
        &e.home,
        &["query", "Body A", "--include-history", "--repo", "alpha"],
    );
    assert_eq!(code, 0);
    let header = out.lines().next().unwrap();
    assert!(
        header.ends_with("source=automemory:p1/a.md retired: file deleted"),
        "{header}"
    );
    let item_id = header.split(']').next().unwrap()[1..].to_string();
    let (code, out, _) = sm(&e.home, &["show", &item_id]);
    assert_eq!(code, 0);
    let l = lines(&out);
    let at = l.iter().position(|x| *x == "status: superseded").unwrap();
    assert_eq!(l[at + 1], "retired: file deleted");
}

#[test]
fn cli_doctor_prints_the_rest_of_its_report_when_the_last_catch_up_row_is_not_json() {
    let e = cli_env();
    sm(&e.home, &["import", "automemory"]);
    write(&e.home.join("catchup.log"), "not json\n");
    let (code, out, _) = sm(&e.home, &["doctor", "--imports"]);
    assert_eq!(code, 0);
    assert_eq!(
        lines(&out)[2..],
        [
            "automemory: files=1 not_stored=0 stored_without_file=0 unmapped=0 unreadable=0",
            "last_catchup: {\"ok\": false, \"ts\": \"unknown\", \"parts\": {\"log\": {\"error\": \"last catchup.log row is not JSON\"}}}",
            "catchup_error: log: last catchup.log row is not JSON",
        ]
    );
}

#[test]
fn cli_doctor_without_catch_up_says_none() {
    let e = cli_env();
    sm(&e.home, &["import", "automemory"]);
    let (code, out, _) = sm(&e.home, &["doctor", "--imports"]);
    assert_eq!(code, 0);
    assert_eq!(
        lines(&out)[2..],
        [
            "automemory: files=1 not_stored=0 stored_without_file=0 unmapped=0 unreadable=0",
            "last_catchup: none"
        ]
    );
}

#[test]
fn cli_backup_prints_path_and_deletions() {
    let e = cli_env();
    sm(&e.home, &["import", "automemory"]);
    let old = e.home.join("backups/memory-20260101-000000-auto.db");
    write(&old, "");
    let (code, out, _) = sm(&e.home, &["backup", "--keep", "1"]);
    assert_eq!(code, 0);
    let l = lines(&out);
    assert!(
        l[0].starts_with(&format!(
            "backup: {}",
            e.home.join("backups/memory-").display()
        )),
        "{}",
        l[0]
    );
    assert!(
        l[0].ends_with("-auto.db quick_check=ok items=1"),
        "{}",
        l[0]
    );
    assert_eq!(l[1..], [format!("deleted: {}", old.display())]);
}

const LOCK_HELD: &str = "error: a catch-up holds hook/catchup.lock; run this again when it ends\n";

#[test]
fn cli_manual_writing_runs_exit_1_while_a_catch_up_holds_the_lock() {
    let e = cli_env();
    let held = hold_catchup_lock(&e.home);
    for argv in [
        &["import", "automemory"][..],
        &["import", "sessions"],
        &["upkeep"],
    ] {
        assert_eq!(
            sm(&e.home, argv),
            (1, String::new(), LOCK_HELD.to_string()),
            "{argv:?}"
        );
    }
    // nothing ran, so no store was even created
    assert!(!e.home.join("memory.db").exists());
    // dry runs and the report write nothing, so they run under a held lock
    for argv in [
        &["import", "automemory", "--dry-run"][..],
        &["import", "sessions", "--dry-run"],
        &["upkeep", "--dry-run"],
        &["upkeep", "--report"],
    ] {
        let (code, _, err) = sm(&e.home, argv);
        assert_eq!((code, err.as_str()), (0, ""), "{argv:?}");
    }
    drop(held);
    let (code, out, _) = sm(&e.home, &["import", "automemory"]);
    assert_eq!(code, 0);
    assert!(
        out.starts_with("import automemory (applied): files=1 added=1 "),
        "{out}"
    );
    assert_eq!(sm(&e.home, &["import", "sessions"]).0, 0);
    let (code, _, err) = sm(&e.home, &["upkeep"]);
    assert_eq!((code, err.as_str()), (0, ""));
}
