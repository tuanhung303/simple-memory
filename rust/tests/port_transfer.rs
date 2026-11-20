//! Ports of the CLI-level and cwd-scope Python transfer tests that no Rust
//! unit test plays: each Python test keeps its name (snake_case), and every sm process gets
//! HOME and SIMPLE_MEMORY_HOME in scratch. Expected values are the Python tests' literals.
//! The other transfer tests are covered by the unit tests of src/service/transfer.rs
//! (rows in the lane report).
mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{git, sm, write};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use simple_memory::core::item::Item;
use simple_memory::service::transfer::run_import_jsonl;
use simple_memory::store::connect::connect;
use simple_memory::store::items::{put_item, supersede_item};

fn sha(text: &str) -> String {
    Sha256::digest(text.trim().as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn tmpdir() -> (tempfile::TempDir, PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let p = std::fs::canonicalize(d.path()).unwrap();
    (d, p)
}

fn db(home: &Path) -> rusqlite::Connection {
    std::fs::create_dir_all(home).unwrap();
    connect(home.join("memory.db").to_str().unwrap(), None).unwrap()
}

fn put(home: &Path, id: &str, body: &str, repo: &str) {
    let it = Item {
        as_of: "2026-10-01T09:00:00+00:00".into(),
        ..Item::bare(id, repo, "fact", &format!("T {id}"), body, "assumed")
    };
    put_item(&db(home), &it.post_init(), None).unwrap();
}

fn row(id: &str, body: &str, kw: Value) -> Value {
    let mut r = json!({
        "id": id, "repo": "a", "kind": "fact", "title": format!("T {id}"), "body": body,
        "refs": "", "tags": "", "source": "", "tag": "assumed", "evidence": "",
        "as_of": "2026-10-01T09:00:00+00:00", "status": "active", "superseded_by": null,
        "content_hash": sha(body),
    });
    for (k, v) in kw.as_object().unwrap() {
        r[k] = v.clone();
    }
    r
}

fn write_rows(path: &Path, rows: &[Value]) -> PathBuf {
    write(
        path,
        &rows.iter().map(|r| format!("{r}\n")).collect::<String>(),
    );
    path.to_path_buf()
}

fn exported_ids(out: &str) -> Vec<String> {
    out.lines()
        .map(|l| {
            serde_json::from_str::<Value>(l).unwrap()["id"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

fn lines(out: &str) -> Vec<&str> {
    out.lines().collect()
}

/// `sm <args>` run from `cwd`, as a user runs it.
fn sm_in(home: &Path, cwd: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_sm"))
        .args(args)
        .current_dir(cwd)
        .env("HOME", home.parent().unwrap())
        .env("SIMPLE_MEMORY_HOME", home)
        .env_remove("SM_PARITY_NOW")
        .env_remove("SM_PARITY_SEED")
        .env_remove("SM_PARITY_TRACE")
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

#[test]
fn export_default_scope_maps_a_project_key_cwd_as_put_and_query_do() {
    let (_d, tmp) = tmpdir();
    let home = tmp.join("h");
    std::fs::create_dir_all(&home).unwrap();
    write(&home.join("config.toml"), "[repos]\nroots = [\"~/code\"]\n");
    std::fs::create_dir_all(tmp.join("code/alpha")).unwrap();
    let hooks = tmp.join("code/claude-code-hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    git(&["init", "-q", hooks.to_str().unwrap()]);
    let pad = tmp.join("u-1000/-u-ann-code-alpha/sess/scratchpad");
    std::fs::create_dir_all(&pad).unwrap();
    put(&home, "m_alpha", "in alpha", "alpha");
    put(&home, "m_hooks", "in hooks", "claude-code-hooks");
    put(&home, "m_none", "in unassigned", "unassigned");
    // a session scratchpad holds the key -u-ann-code-alpha: the repo is alpha, not unassigned
    let (code, out, err) = sm_in(&home, &pad, &["export"]);
    assert_eq!(
        (code, exported_ids(&out), err.as_str()),
        (0, vec!["m_alpha".to_string()], "")
    );
    // a folder name that only holds the -code- marker is no key: its repo comes from git
    let (code, out, err) = sm_in(&home, &hooks, &["export"]);
    assert_eq!(
        (code, exported_ids(&out), err.as_str()),
        (0, vec!["m_hooks".to_string()], "")
    );
}

#[test]
fn cli_export_repo_and_all_are_exclusive() {
    let (_d, tmp) = tmpdir();
    let (code, out, err) = sm(&tmp.join("h"), &["export", "--repo", "a", "--all"]);
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(err.contains("not allowed with argument"), "{err}");
}

#[test]
fn cli_import_jsonl_prints_counts_and_conflicts() {
    let (_d, tmp) = tmpdir();
    let home = tmp.join("h");
    put(&home, "m_diff", "stored body", "a");
    let path = write_rows(
        &tmp.join("in.jsonl"),
        &[
            row("m_diff", "file body", json!({})),
            row(
                "m_new",
                "new",
                json!({"status": "superseded", "superseded_by": "m_nowhere"}),
            ),
        ],
    );
    let (code, out, _) = sm(
        &home,
        &["import", "jsonl", path.to_str().unwrap(), "--dry-run"],
    );
    assert_eq!(code, 0);
    assert_eq!(
        lines(&out),
        [
            "import jsonl (dry-run): rows=2 added=1 unchanged=0 conflicts=1 links_restored=0 links_missing=1 \
             status_differs=0 redacted=0",
            "conflict: m_diff",
            "link_missing: m_new -> m_nowhere",
        ]
    );
    let (code, _, err) = sm(
        &home,
        &["import", "jsonl", tmp.join("none.jsonl").to_str().unwrap()],
    );
    assert_eq!(code, 1);
    assert!(err.contains("No such file"), "{err}");
}

#[test]
fn cli_export_prints_jsonl() {
    let (_d, tmp) = tmpdir();
    let home = tmp.join("h");
    put(&home, "m_a", "alpha", "a");
    put(&home, "m_b", "beta", "a");
    supersede_item(&db(&home), "m_a", "m_b").unwrap();
    let (code, out, _) = sm(&home, &["export", "--repo", "a"]);
    assert_eq!((code, exported_ids(&out)), (0, vec!["m_b".to_string()]));
    let (code, out, _) = sm(&home, &["export", "--repo", "a", "--include-history"]);
    assert_eq!(
        (code, exported_ids(&out)),
        (0, vec!["m_a".to_string(), "m_b".to_string()])
    );
}

#[test]
fn cli_import_prints_status_differs_with_a_ready_supersede_command() {
    let (_d, tmp) = tmpdir();
    let home = tmp.join("a");
    put(&home, "m_x", "old way", "a");
    put(&home, "m_k", "kept", "a");
    put(&home, "m_l", "later", "a");
    supersede_item(&db(&home), "m_k", "m_l").unwrap();
    let path = write_rows(
        &tmp.join("in.jsonl"),
        &[
            row(
                "m_x",
                "old way",
                json!({"status": "superseded", "superseded_by": "m_y"}),
            ),
            row("m_y", "new way", json!({})),
            row("m_k", "kept", json!({})),
            row("m_l", "later", json!({})),
        ],
    );
    let (code, out, _) = sm(&home, &["import", "jsonl", path.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert_eq!(
        lines(&out),
        [
            "import jsonl (applied): rows=4 added=1 unchanged=1 conflicts=0 links_restored=0 links_missing=0 \
             status_differs=2 redacted=0",
            "status_differs: m_k store=superseded file=active",
            "status_differs: m_x store=active file=superseded",
            "  run: sm supersede m_x m_y",
        ]
    );
}

#[test]
fn import_counts_and_names_the_rows_the_redactor_rewrote() {
    let (_d, tmp) = tmpdir();
    let home = tmp.join("a");
    let path = write_rows(
        &tmp.join("in.jsonl"),
        &[
            row("m_clean", "plain note", json!({})),
            row(
                "m_body",
                "deploy with --secret=db-pass-name today",
                json!({}),
            ),
            row(
                "m_title",
                "plain body",
                json!({"title": "run --secret=db-pass-name"}),
            ),
        ],
    );
    let (code, out, _) = sm(&home, &["import", "jsonl", path.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert_eq!(
        lines(&out),
        [
            "import jsonl (applied): rows=3 added=3 unchanged=0 conflicts=0 links_restored=0 links_missing=0 \
             status_differs=0 redacted=2",
            "redacted: m_body",
            "redacted: m_title",
        ]
    );
    let res = run_import_jsonl(path.to_str().unwrap(), false, home.to_str()).unwrap();
    assert_eq!(
        (res.res.unchanged, res.res.redacted),
        (3, vec!["m_body".to_string(), "m_title".to_string()])
    );
}
