//! Ports of the Python import and doctor tests of the service, CLI, CLI setup, config and CLI
//! redact test files. The Python
//! tests keep their names (snake_case). Expected values are the Python tests' literals.
mod common;

use std::path::{Path, PathBuf};

use common::{open, scratch, sm, write};
use serde_json::json;
use simple_memory::config::Aliases;
use simple_memory::core::item::Item;
use simple_memory::error::{Base, SmError};
use simple_memory::py::value::PyValue;
use simple_memory::service::doctor::{DoctorResult, run_doctor, tasklog_repo_dirs};
use simple_memory::service::import_::{ImportResult, run_import};
use simple_memory::service::put::{PutArgs, run_put};
use simple_memory::service::query::{QueryArgs, run_query};
use simple_memory::service::show::run_show_many;
use simple_memory::service::supersede::run_supersede;
use simple_memory::store::items::put_item;

/// `run_show(item_id, home=...)`: the item, or the KeyError Python raises for an id not stored.
fn run_show(item_id: &str, home: Option<&str>) -> Result<Item, SmError> {
    let (found, _) = run_show_many(&[item_id], home)?;
    found
        .into_iter()
        .next()
        .ok_or_else(|| SmError::not_found(item_id))
}

fn tmpdir() -> (tempfile::TempDir, PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let p = std::fs::canonicalize(d.path()).unwrap();
    (d, p)
}

fn s(x: &str) -> String {
    x.to_string()
}

fn put(home: &Path, body: &str, tag: &str, repo: &str) -> String {
    let a = PutArgs {
        tag,
        repo,
        home: home.to_str(),
        ..Default::default()
    };
    run_put(body, &a).unwrap().0
}

fn import(home: &Path, kind: &str, path: &Path, dry_run: bool, apply: bool) -> ImportResult {
    run_import(kind, path.to_str().unwrap(), dry_run, apply, home.to_str()).unwrap()
}

fn query(home: &Path, text: &str, repo: &str) -> Vec<Item> {
    run_query(
        text,
        &QueryArgs {
            repo,
            home: home.to_str(),
            ..Default::default()
        },
        None,
    )
    .unwrap()
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

fn doctor_imports(home: &Path) -> DoctorResult {
    run_doctor(false, false, false, true, home.to_str()).unwrap()
}

fn show_refs(home: &Path, id: &str) -> String {
    run_show(id, home.to_str()).unwrap().refs
}

/// An auto-memory root with no memory files, so `doctor --imports` never reads ~/.claude.
fn empty_dir(tmp: &Path) -> PathBuf {
    let d = tmp.join("am-root");
    std::fs::create_dir_all(&d).unwrap();
    d
}

// ---------------------------------------------------------------- service

#[test]
fn service_put_query_show_supersede_doctor() {
    let (_d, tmp) = tmpdir();
    let h = tmp.to_str();
    let err = run_put(
        "Missing tag text",
        &PutArgs {
            tag: "",
            home: h,
            repo: "anvil",
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(
        err.is(Base::Value) && err.msg.contains("Evidence tag is required"),
        "{err}"
    );
    let a = PutArgs {
        tag: "verified",
        evidence: "",
        home: h,
        repo: "anvil",
        ..Default::default()
    };
    let err = run_put("Verified without evidence", &a).unwrap_err();
    assert!(
        err.is(Base::Value) && err.msg.contains("needs non-empty evidence"),
        "{err}"
    );

    let body = "R5 queue lag café schema sync";
    let a = PutArgs {
        tag: "verified",
        evidence: "pytest -q",
        refs: "R5",
        repo: "anvil",
        home: h,
        ..Default::default()
    };
    let (id1, created1) = run_put(body, &a).unwrap();
    assert!(created1);
    let a = PutArgs {
        tag: "verified",
        evidence: "pytest -q",
        repo: "anvil",
        home: h,
        ..Default::default()
    };
    let (id1_dup, created_dup) = run_put(body, &a).unwrap();
    assert!(!created_dup);
    assert_eq!(id1_dup, id1);

    let shown = run_show(&id1, h).unwrap();
    assert_eq!(
        (shown.id.as_str(), shown.tag.as_str()),
        (id1.as_str(), "verified")
    );
    let hits: Vec<String> = query(&tmp, "cafe", "anvil")
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(hits, std::slice::from_ref(&id1));

    let a = PutArgs {
        tag: "executed",
        evidence: "commit 1234567",
        refs: "R5",
        repo: "anvil",
        home: h,
        ..Default::default()
    };
    let (id2, _) = run_put("R5 replacement fix after refactor", &a).unwrap();
    run_supersede(&id1, &id2, h).unwrap();
    let after: Vec<String> = query(&tmp, "R5", "anvil")
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(after, [id2]);

    let doc = run_doctor(true, false, false, false, h).unwrap();
    assert_eq!(
        (
            doc.check.integrity.as_str(),
            doc.check.active,
            doc.check.superseded
        ),
        ("ok", 1, 1)
    );
}

#[test]
fn service_import_proposal_idempotent_and_supersedes() {
    let (_d, tmp) = tmpdir();
    let id_old = put(&tmp, "Old legacy fact to be superseded", "decided", "anvil");
    let prop_file = tmp.join("proposals.jsonl");
    let row = json!({
        "repo": "anvil", "kind": "decision", "title": "Merged fact",
        "body": "Consolidated fact replacing old legacy entry.", "refs": ["R9"], "tags": ["cleanup"],
        "tag": "verified", "evidence": "legacy:memories:1", "as_of": "2026-10-25T12:00:00+00:00",
        "source": "legacy:memories", "supersedes": [id_old, "legacy:memories:missing_99"],
    });
    write(&prop_file, &format!("{row}\n"));

    let dry = import(&tmp, "proposal", &prop_file, true, false);
    assert_eq!((dry.total, dry.inserted), (1, 0));
    assert_eq!(run_show(&id_old, tmp.to_str()).unwrap().status, "active");

    let r1 = import(&tmp, "proposal", &prop_file, false, true);
    assert_eq!(
        (
            r1.inserted,
            r1.deduped,
            r1.superseded,
            r1.supersede_missing,
            r1.rejected
        ),
        (1, 0, 1, 1, 0)
    );
    assert_eq!(
        run_show(&id_old, tmp.to_str()).unwrap().status,
        "superseded"
    );

    let r2 = import(&tmp, "proposal", &prop_file, false, true);
    assert_eq!(
        (r2.inserted, r2.deduped, r2.supersede_missing, r2.rejected),
        (0, 1, 1, 0)
    );
}

/// An item no file backs dedupes by its body alone: a proposal whose body the repo already
/// holds under another title is that item, not a new version of it.
#[test]
fn a_proposal_with_a_stored_body_and_another_title_is_deduped() {
    let (_d, tmp) = tmpdir();
    let id_old = put(&tmp, "One shared proposal body", "decided", "anvil");
    let prop_file = tmp.join("proposals.jsonl");
    let row = json!({
        "repo": "anvil", "kind": "decision", "title": "Another title",
        "body": "One shared proposal body", "tag": "verified", "evidence": "run 1",
        "as_of": "2026-10-25T12:00:00+00:00", "source": "prop:1",
    });
    write(&prop_file, &format!("{row}\n"));
    let r = import(&tmp, "proposal", &prop_file, false, true);
    assert_eq!((r.inserted, r.deduped, r.superseded), (0, 1, 0));
    assert_eq!(run_show(&id_old, tmp.to_str()).unwrap().status, "active");
}

#[test]
fn service_put_validations_and_query_limit() {
    let (_d, tmp) = tmpdir();
    let h = tmp.to_str();
    let a = PutArgs {
        tag: "decided",
        kind: "not-a-kind",
        home: h,
        repo: "anvil",
        ..Default::default()
    };
    assert!(
        run_put("Body", &a)
            .unwrap_err()
            .msg
            .contains("Invalid kind")
    );
    let a = PutArgs {
        tag: "decided",
        as_of: "not-a-date",
        home: h,
        repo: "anvil",
        ..Default::default()
    };
    assert!(
        run_put("Body", &a)
            .unwrap_err()
            .msg
            .contains("Invalid ISO 8601")
    );
    for limit in [0, -2] {
        let a = QueryArgs {
            limit,
            home: h,
            repo: "anvil",
            ..Default::default()
        };
        let err = run_query("Body", &a, None).unwrap_err();
        assert!(err.is(Base::Value) && err.msg.contains("limit"), "{err}");
    }
}

fn tl_text(id: &str, title: &str, as_of: &str, extra: &str, result: &str) -> String {
    format!(
        "id: {id}\ntitle: {title}\nstate: done\nrepo: anvil\nas_of: {as_of}\n{extra}results:\n  - {result}\n"
    )
}

/// A secret with `/` in a task log is redacted in the body, and the importer's path ref of it
/// is dropped with it; the other refs of the same line stay.
#[test]
fn tasklog_import_drops_refs_of_a_redacted_secret() {
    let (_d, tmp) = tmpdir();
    let home = tmp.join("home");
    let tl = tmp.join("anvil/docs/tasklog/2026-11-01/090000-cred.yaml");
    write(
        &tl,
        &tl_text(
            "20261101-090000-aabb0011-cred-0001",
            "Rotate creds",
            "2026-11-01T09:00:00+00:00",
            "",
            "set aws_secret_access_key=Zx9Qw8Er7Ty6/Uu5Ii4Oo3Pp/2Aa1Ss0Dd9Ff8Gg7 per docs/run.md",
        ),
    );
    assert_eq!(import(&home, "tasklog", &tl, false, false).inserted, 1);
    let db = open(&home);
    let (body, refs): (String, String) = db
        .query_row("SELECT body, refs FROM items", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(body, "Results:\n- set [REDACTED:key_value] per docs/run.md");
    assert_eq!(refs, "20261101-090000-aabb0011-cred-0001 docs/run.md");
    let in_fold: i64 = db
        .query_row(
            "SELECT count(*) FROM items_fold WHERE items_fold MATCH '\"Uu5Ii4Oo3Pp\"*'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(in_fold, 0);
}

/// Two commits that each write `password: value` share the key word; the stored refs keep
/// neither value.
#[test]
fn tasklog_import_drops_both_values_of_commits_that_share_a_key_word() {
    let (_d, tmp) = tmpdir();
    let home = tmp.join("home");
    let tl = tmp.join("anvil/docs/tasklog/2026-11-01/090000-pairs.yaml");
    write(
        &tl,
        &tl_text(
            "20261101-090000-aabb0022-pairs-0001",
            "Rotate pairs",
            "2026-11-01T09:00:00+00:00",
            "commits: [\"db password: Hunter2-example-Zz9\", \"redis password: Hunter2-example-Yy8\"]\n",
            "poller fixed",
        ),
    );
    assert_eq!(import(&home, "tasklog", &tl, false, false).inserted, 1);
    let refs: String = open(&home)
        .query_row("SELECT refs FROM items", [], |r| r.get(0))
        .unwrap();
    assert_eq!(refs, "20261101-090000-aabb0022-pairs-0001 db redis");
    assert!(!common::db_files_hold_twice_secret(&home));
}

#[test]
fn service_tasklog_and_handoff_reimport_and_superseded_by() {
    let (_d, tmp) = tmpdir();
    let home = tmp.join("home");
    let tl_dir = tmp.join("anvil/docs/tasklog/2026-10-25");
    let a_id = "20261025-010000-aaaa1111-task-a-0001";
    let b_id = "20261025-020000-bbbb2222-task-b-0002";
    let tl_a = tl_dir.join("010000-aaaa1111-task-a-0001.yaml");
    write(
        &tl_a,
        &tl_text(
            a_id,
            "Task A v1",
            "2026-10-25T10:00:00+00:00",
            &format!("superseded_by: {b_id}\n"),
            "Alpha draft outcome",
        ),
    );
    // Order 1: Import only tl_a first; target task-b is not imported yet, so tl_a stays active
    let res1 = import(&home, "tasklog", &tl_a, false, false);
    assert_eq!(
        (
            res1.inserted,
            res1.superseded,
            res1.unresolved_supersedes.len()
        ),
        (1, 0, 1)
    );

    // Import ONLY tl_b next (single file, not directory): tl_a is resolved and superseded by tl_b
    let tl_b = tl_dir.join("020000-bbbb2222-task-b-0002.yaml");
    write(
        &tl_b,
        &tl_text(
            b_id,
            "Task B v1",
            "2026-10-25T11:00:00+00:00",
            "commits:\n  - 1234567\n",
            "Replaced Task A",
        ),
    );
    let res2 = import(&home, "tasklog", &tl_b, false, false);
    assert_eq!((res2.inserted, res2.deduped, res2.superseded), (1, 0, 1));
    assert_eq!(res2.unresolved_supersedes, Vec::<String>::new());
    assert!(query(&home, "Alpha draft", "anvil").is_empty());

    // Order 2: Import replacement tl_d first, then superseded tl_c second
    let d_id = "20261025-040000-dddd4444-task-d-0004";
    let tl_d = tl_dir.join("040000-dddd4444-task-d-0004.yaml");
    write(
        &tl_d,
        &tl_text(
            d_id,
            "Task D v1",
            "2026-10-25T14:00:00+00:00",
            "",
            "Replacement outcome D",
        ),
    );
    let res_d = import(&home, "tasklog", &tl_d, false, false);
    assert_eq!((res_d.inserted, res_d.superseded), (1, 0));
    let tl_c = tl_dir.join("030000-cccc3333-task-c-0003.yaml");
    write(
        &tl_c,
        &tl_text(
            "20261025-030000-cccc3333-task-c-0003",
            "Task C v1",
            "2026-10-25T13:00:00+00:00",
            &format!("superseded_by: {d_id}\n"),
            "Obsolete gamma outcome",
        ),
    );
    let res_c = import(&home, "tasklog", &tl_c, false, false);
    assert_eq!((res_c.inserted, res_c.superseded), (1, 1));
    assert_eq!(res_c.unresolved_supersedes, Vec::<String>::new());
    assert!(query(&home, "Obsolete gamma", "anvil").is_empty());

    // Now edit tl_b and re-import: new item with same source task:<id> supersedes active one
    write(
        &tl_b,
        &tl_text(
            b_id,
            "Task B v2",
            "2026-10-25T12:00:00+00:00",
            "commits:\n  - 1234567\n  - 89abcdef\n",
            "Replaced Task A with updated results",
        ),
    );
    let res3 = import(&home, "tasklog", &tl_b, false, false);
    assert_eq!((res3.inserted, res3.superseded), (1, 1));
    let active_b = query(&home, "Replaced", "anvil");
    assert_eq!(active_b.len(), 1);
    assert_eq!(active_b[0].title, "Task B v2");
}

#[test]
fn service_apply_deduped_supersedes_and_failed_row_isolation() {
    let (_d, tmp) = tmpdir();
    let h = tmp.to_str();
    let id_keep = put(
        &tmp,
        "Canonical body kept verbatim after merge.",
        "decided",
        "anvil",
    );
    let id_other = put(
        &tmp,
        "Redundant duplicate phrasing to supersede.",
        "decided",
        "anvil",
    );
    let id_already_sup_old = put(&tmp, "Already superseded target A.", "decided", "anvil");
    let id_already_sup_new = put(&tmp, "Replacement for target A.", "decided", "anvil");
    run_supersede(&id_already_sup_old, &id_already_sup_new, h).unwrap();

    let row1 = json!({
        "repo": "anvil", "kind": "decision", "title": "Merged canonical",
        "body": "Canonical body kept verbatim after merge.", "refs": [], "tags": ["merge"], "tag": "decided",
        "evidence": "", "as_of": "2026-10-25T12:00:00+00:00", "source": "legacy:memories",
        "supersedes": [id_keep, id_other],
    });
    let a = PutArgs {
        tag: "decided",
        source: "task:old-task-1",
        repo: "anvil",
        home: h,
        ..Default::default()
    };
    let id_sup_task = run_put("Superseded task body in store.", &a).unwrap().0;
    run_supersede(&id_sup_task, &id_already_sup_new, h).unwrap();

    let row2 = json!({
        "repo": "anvil", "kind": "task", "title": "Failing proposal row",
        "body": "Superseded task body in store.", "refs": [], "tags": [], "tag": "decided", "evidence": "",
        "as_of": "2026-10-25T12:05:00+00:00", "source": "task:old-task-1", "supersedes": [id_already_sup_old],
    });
    let row3 = json!({
        "repo": "anvil", "kind": "fact", "title": "Brand new fact",
        "body": "Third row in the same file still applies cleanly.", "refs": [], "tags": [], "tag": "decided",
        "evidence": "", "as_of": "2026-10-25T12:10:00+00:00", "source": "legacy:memories", "supersedes": [],
    });
    let prop_file = tmp.join("mixed_proposals.jsonl");
    write(&prop_file, &format!("{row1}\n{row2}\n{row3}\n"));

    let res = import(&tmp, "proposal", &prop_file, false, true);
    assert_eq!(
        (
            res.deduped,
            res.inserted,
            res.superseded,
            res.failed_rows.len()
        ),
        (1, 1, 1, 1)
    );
    assert_eq!(run_show(&id_keep, h).unwrap().status, "active");
    let other = run_show(&id_other, h).unwrap();
    assert_eq!(
        (other.status, other.superseded_by),
        (s("superseded"), Some(id_keep))
    );
    assert_eq!(query(&tmp, "Third row in the same file", "anvil").len(), 1);
}

#[test]
fn service_tasklog_supersede_chains_b_a_c_and_cycles() {
    let (_d, tmp) = tmpdir();
    let home = tmp.join("home_chain");
    let tl_dir = tmp.join("anvil/docs/tasklog/2026-10-25");
    // File names sorted alphabetically load in order: 01_B, 02_A, 03_C. Chain is A -> B -> C
    let files = [
        (
            "01_b",
            "task-B",
            "Task B middle",
            "11:00:00",
            Some("task-C"),
            "Middle step B",
        ),
        (
            "02_a",
            "task-A",
            "Task A start",
            "10:00:00",
            Some("task-B"),
            "Initial step A",
        ),
        (
            "03_c",
            "task-C",
            "Task C final",
            "12:00:00",
            None,
            "Final step C",
        ),
        // Cycle: D -> E -> D, plus missing target: F -> MISSING
        (
            "04_d",
            "task-D",
            "Task D cycle1",
            "13:00:00",
            Some("task-E"),
            "Cycle D",
        ),
        (
            "05_e",
            "task-E",
            "Task E cycle2",
            "14:00:00",
            Some("task-D"),
            "Cycle E",
        ),
        (
            "06_f",
            "task-F",
            "Task F missing target",
            "15:00:00",
            Some("task-MISSING"),
            "Missing target F",
        ),
    ];
    for (file, id, title, time, sup, result) in files {
        let extra = sup
            .map(|t| format!("superseded_by: {t}\n"))
            .unwrap_or_default();
        write(
            &tl_dir.join(format!("{file}.yaml")),
            &tl_text(
                id,
                title,
                &format!("2026-10-25T{time}+00:00"),
                &extra,
                result,
            ),
        );
    }
    let res = import(&home, "tasklog", &tl_dir, false, false);
    assert_eq!((res.inserted, res.superseded), (6, 2));
    let mut unresolved = res.unresolved_supersedes;
    unresolved.sort();
    assert_eq!(
        unresolved,
        [
            "task:task-D -> task-E",
            "task:task-E -> task-D",
            "task:task-F -> task-MISSING"
        ]
    );

    let a = QueryArgs {
        repo: "anvil",
        include_history: true,
        limit: 10,
        home: home.to_str(),
        ..Default::default()
    };
    let all = run_query("step OR Cycle OR Missing", &a, None).unwrap();
    let by = |src: &str| {
        all.iter()
            .find(|i| i.source == src)
            .unwrap_or_else(|| panic!("no item for {src}"))
            .clone()
    };
    let (item_a, item_b, item_c) = (by("task:task-A"), by("task:task-B"), by("task:task-C"));
    assert_eq!(
        (item_a.status.as_str(), item_a.superseded_by),
        ("superseded", Some(item_b.id.clone()))
    );
    assert_eq!(
        (item_b.status.as_str(), item_b.superseded_by),
        ("superseded", Some(item_c.id.clone()))
    );
    assert_eq!(item_c.status, "active");
    for src in ["task:task-D", "task:task-E", "task:task-F"] {
        assert_eq!(by(src).status, "active", "{src}");
    }
}

#[test]
fn service_put_takes_refs_from_evidence() {
    let (_d, tmp) = tmpdir();
    let a = PutArgs {
        tag: "decided",
        evidence: "session 0a1b2c3d; tasklog 20261101-101010-0a1b2c3d-made-up-slug-9f9f; commit ab12cd3",
        refs: "R4 ab12cd3",
        repo: "anvil",
        home: tmp.to_str(),
        ..Default::default()
    };
    let item_id = run_put("Rows may differ by 30 cents", &a).unwrap().0;
    assert_eq!(
        show_refs(&tmp, &item_id),
        "R4 ab12cd3 20261101-101010-0a1b2c3d-made-up-slug-9f9f 0a1b2c3d"
    );
    let a = PutArgs {
        tag: "decided",
        evidence: "Mia in chat",
        repo: "anvil",
        home: tmp.to_str(),
        ..Default::default()
    };
    let plain_id = run_put("Mia prefers short replies", &a).unwrap().0;
    assert_eq!(show_refs(&tmp, &plain_id), "");
}

fn seed(home: &Path, items: Vec<Item>) {
    let conn = open(home);
    for it in items {
        put_item(&conn, &it.post_init(), None).unwrap();
    }
}

#[test]
fn service_doctor_backfills_derivable_refs_once() {
    let (_d, tmp) = tmpdir();
    let h = tmp.to_str();
    seed(
        &tmp,
        vec![
            // an agent put from before put read refs: body names a ticket, evidence a commit
            Item {
                evidence: s("commit ab12cd3"),
                as_of: s("2026-10-01"),
                ..Item::bare(
                    "m_A",
                    "anvil",
                    "fact",
                    "Poll note",
                    "R7 poll waits",
                    "decided",
                )
            },
            // a handoff: its evidence is the file path, which must not become a ref
            Item {
                evidence: s("/x/tmp/handoffs/p/h1.md"),
                source: s("handoff:h1.md"),
                as_of: s("2026-10-02"),
                ..Item::bare(
                    "m_B",
                    "anvil",
                    "handoff",
                    "Handoff",
                    "Done: tabs",
                    "derived",
                )
            },
            // a legacy row with nothing a ref could come from
            Item {
                source: s("legacy:memories"),
                as_of: s("2026-09-01"),
                ..Item::bare(
                    "legacy:memories:fact.a",
                    "unassigned",
                    "fact",
                    "Pref",
                    "Mia prefers short replies",
                    "n/a",
                )
            },
        ],
    );
    let counts = |d: &DoctorResult| {
        (
            d.refs["refs_empty_before"],
            d.refs["refs_changed"],
            d.refs["refs_empty_after"],
        )
    };
    let dry = run_doctor(false, true, true, false, h).unwrap();
    assert_eq!(counts(&dry), (3, 1, 2));
    assert_eq!(show_refs(&tmp, "m_A"), "");

    let real = run_doctor(false, true, false, false, h).unwrap();
    assert_eq!(counts(&real), (3, 1, 2));
    assert_eq!(show_refs(&tmp, "m_A"), "R7 ab12cd3");
    assert_eq!(show_refs(&tmp, "m_B"), "");
    let ids: Vec<String> = query(&tmp, "ab12cd3", "anvil")
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(ids, ["m_A"]);

    let again = run_doctor(false, true, false, false, h).unwrap();
    assert_eq!(counts(&again), (2, 0, 2));
}

#[test]
fn service_doctor_dry_run_needs_a_write_flag() {
    let (_d, tmp) = tmpdir();
    let err = run_doctor(false, false, true, false, tmp.to_str()).unwrap_err();
    assert!(
        err.is(Base::Value) && err.msg.contains("--dry-run"),
        "{err}"
    );
}

#[test]
fn service_doctor_imports_lists_files_not_stored() {
    let (_d, tmp) = tmpdir();
    let home = tmp.join("home");
    let root = tmp.join("gh");
    let stored = tasklog(
        &root.join("r1/docs/tasklog/2026-11-01/a.yaml"),
        "20261101-090000-0000000a-a-aaaa",
    );
    let missing = tasklog(
        &root.join("r1/docs/tasklog/2026-11-02/b.yaml"),
        "20261102-090000-0000000b-b-bbbb",
    );
    let nested = tasklog(
        &root.join("r1/sub/docs/tasklog/2026-11-02/n.yaml"),
        "20261102-090000-0000000c-n-cccc",
    );
    write(&root.join("r1/mod/.git"), "gitdir: ../.git/modules/mod\n");
    let sub = tasklog(
        &root.join("r1/mod/docs/tasklog/2026-11-02/s.yaml"),
        "20261102-090000-0000000f-s-ffff",
    );
    write(
        &root.join("r1-wt/.git"),
        "gitdir: /x/r1/.git/worktrees/r1-wt\n",
    );
    tasklog(
        &root.join("r1-wt/docs/tasklog/2026-11-02/w.yaml"),
        "20261102-090000-0000000d-w-dddd",
    );
    let handoffs = tmp.join("handoffs/p");
    write(&handoffs.join("h1.md"), "# H1\n## Done\n- x\n");
    write(&handoffs.join("h2.md"), "# H2\n## Done\n- y\n");
    // same kept body as h1 in the same repo: another file, so it gets its own item
    write(&handoffs.join("h3.md"), "# H3 retitled\n## Done\n- x\n");
    std::fs::create_dir_all(&home).unwrap();
    write(
        &home.join("config.toml"),
        &format!(
            "[automemory]\nroot = \"{}\"\n[upkeep]\ntasklog_roots = [\"{}\"]\nhandoff_dirs = [\"{}\"]\n",
            empty_dir(&tmp).display(),
            root.display(),
            tmp.join("handoffs").display()
        ),
    );
    import(&home, "tasklog", &stored, false, false);
    // another id, same body in the same repo: another file, so it gets its own item
    let same_body = root.join("r1/docs/tasklog/2026-11-01/a2.yaml");
    let text = std::fs::read_to_string(&stored).unwrap();
    write(
        &same_body,
        &text
            .replacen("0000000a-a-aaaa", "00000002-a-2222", 1)
            .replacen("title: T", "title: Retitled", 1),
    );
    let res = import(&home, "tasklog", &same_body, false, false);
    assert_eq!((res.inserted, res.deduped), (1, 0));
    import(&home, "handoff", &handoffs.join("h1.md"), false, false);
    let res = import(&home, "handoff", &handoffs.join("h3.md"), false, false);
    assert_eq!((res.inserted, res.deduped), (1, 0));
    let gone = tasklog(
        &tmp.join("elsewhere/docs/tasklog/2026-10-01/g.yaml"),
        "20261001-090000-0000000e-g-eeee",
    );
    import(&home, "tasklog", &gone, false, false);
    // a superseded task item whose file is gone is history, not a gap
    let old = tasklog(
        &tmp.join("elsewhere/docs/tasklog/2026-09-01/o.yaml"),
        "20260901-090000-00000001-o-1111",
    );
    import(&home, "tasklog", &old, false, false);
    let all = QueryArgs {
        all_repos: true,
        home: home.to_str(),
        ..Default::default()
    };
    let old_id = run_query("20260901-090000-00000001-o-1111", &all, None).unwrap()[0]
        .id
        .clone();
    let gone_id = run_query("20261001-090000-0000000e-g-eeee", &all, None).unwrap()[0]
        .id
        .clone();
    run_supersede(&old_id, &gone_id, home.to_str()).unwrap();

    let (lag, _, _) = doctor_imports(&home).lag.unwrap();
    assert_eq!(lag.tasklog_files, 5);
    let resolved = |p: &Path| {
        std::fs::canonicalize(p)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(
        lag.not_stored,
        [
            resolved(&missing),
            resolved(&sub),
            resolved(&nested),
            resolved(&handoffs.join("h2.md"))
        ]
    );
    assert_eq!(lag.handoff_files, 3);
    assert_eq!(
        lag.stored_tasks_without_file,
        ["task:20261001-090000-0000000e-g-eeee"]
    );
}

#[test]
fn service_import_stores_each_file_whose_body_another_file_holds() {
    let (_d, tmp) = tmpdir();
    let home = tmp.join("home");
    let root = tmp.join("gh");
    let tl = |name: &str, id: &str, title: &str, results: &str| {
        let p = root.join(format!("r1/docs/tasklog/2026-11-01/{name}.yaml"));
        write(
            &p,
            &format!("id: {id}\ntitle: {title}\nresults:\n{results}"),
        );
        p
    };
    let tl_a = tl(
        "a",
        "20261101-090000-aaaa",
        "Auth fix TKT-101",
        "  - tests pass\n",
    );
    let tl_b = tl(
        "b",
        "20261101-100000-bbbb",
        "Billing fix TKT-202",
        "  - tests pass\n",
    );
    let handoffs = tmp.join("handoffs/r1");
    write(
        &handoffs.join("ha.md"),
        "# HANDOFF: alpha\n\n## Done\n- tests pass\n",
    );
    write(
        &handoffs.join("hb.md"),
        "# HANDOFF: beta TKT-909\n\n## Done\n- tests pass\n",
    );
    std::fs::create_dir_all(&home).unwrap();
    write(
        &home.join("config.toml"),
        &format!(
            "[automemory]\nroot = \"{}\"\n[upkeep]\ntasklog_roots = [\"{}\"]\nhandoff_dirs = [\"{}\"]\n",
            empty_dir(&tmp).display(),
            root.display(),
            tmp.join("handoffs").display()
        ),
    );
    let active = || {
        common::column(
            &home,
            "SELECT title FROM items WHERE status = 'active' ORDER BY title",
        )
    };
    let not_stored = || {
        let lag = doctor_imports(&home).lag.unwrap().0.not_stored;
        lag.iter()
            .map(|p| {
                Path::new(p)
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string()
            })
            .collect::<Vec<_>>()
    };
    import(&home, "tasklog", &tl_a, false, false);
    import(&home, "handoff", &handoffs.join("ha.md"), false, false);
    // only another file holds b's and hb's body: they are not stored
    assert_eq!(not_stored(), ["b.yaml", "hb.md"]);

    let res = import(&home, "tasklog", &tl_b, false, false);
    assert_eq!((res.inserted, res.deduped), (1, 0));
    let res = import(&home, "handoff", &handoffs.join("hb.md"), false, false);
    assert_eq!((res.inserted, res.deduped), (1, 0));
    assert_eq!(
        active(),
        [
            "Auth fix TKT-101",
            "Billing fix TKT-202",
            "alpha",
            "beta TKT-909"
        ]
    );
    assert_eq!(not_stored(), Vec::<String>::new());
    // a second import of the same files is a no-op
    let res = import(&home, "tasklog", &root, false, false);
    assert_eq!((res.inserted, res.deduped, res.superseded), (0, 2, 0));
    let res = import(&home, "handoff", &tmp.join("handoffs"), false, false);
    assert_eq!((res.inserted, res.deduped, res.superseded), (0, 2, 0));

    // a's edit supersedes a's own item only; b keeps the old body
    tl(
        "a",
        "20261101-090000-aaaa",
        "Auth fix TKT-101",
        "  - tests pass\n  - shipped\n",
    );
    let res = import(&home, "tasklog", &root, false, false);
    assert_eq!((res.inserted, res.deduped, res.superseded), (1, 1, 1));
    assert_eq!(not_stored(), Vec::<String>::new());
    let b_rows = common::column(
        &home,
        "SELECT status || ' ' || body FROM items WHERE source = 'task:20261101-100000-bbbb'",
    );
    assert_eq!(b_rows, ["active Results:\n- tests pass"]);

    // b first stored after a's edit: a new file with the old body of a
    let tl_c = tl(
        "c",
        "20261101-110000-cccc",
        "Cache fix TKT-303",
        "  - tests pass\n",
    );
    assert_eq!(not_stored(), ["c.yaml"]);
    let res = import(&home, "tasklog", &tl_c, false, false);
    assert_eq!((res.inserted, res.deduped), (1, 0));
    assert_eq!(not_stored(), Vec::<String>::new());

    // a title amend of b keeps c, the other file with b's body
    let b_id = &common::column(
        &home,
        "SELECT id FROM items WHERE status = 'active' AND source = 'task:20261101-100000-bbbb'",
    )[0];
    let (code, _, err) = sm(&home, &["amend", b_id, "--title", "Billing fix TKT-202 v2"]);
    assert_eq!((code, err.as_str()), (0, ""));
    // export, then import into a fresh home: both files with the same body come through
    let (code, out, _) = sm(&home, &["export", "--all"]);
    assert_eq!(code, 0);
    let dump = tmp.join("dump.jsonl");
    write(&dump, &out);
    let home2 = tmp.join("home2");
    let (code, _, err) = sm(&home2, &["import", "jsonl", dump.to_str().unwrap()]);
    assert_eq!((code, err.as_str()), (0, ""));
    let titles = "SELECT title FROM items WHERE status = 'active' ORDER BY title";
    assert_eq!(
        common::column(&home2, titles),
        [
            "Auth fix TKT-101",
            "Billing fix TKT-202 v2",
            "Cache fix TKT-303",
            "alpha",
            "beta TKT-909"
        ]
    );
    // a manually retired file item stays retired, though no other file item holds its body now
    let c_id = &common::column(
        &home,
        "SELECT id FROM items WHERE status = 'active' AND source = 'task:20261101-110000-cccc'",
    )[0];
    assert_eq!(sm(&home, &["retire", c_id, "--reason", "obsolete"]).0, 0);
    let res = import(&home, "tasklog", &root, false, false);
    assert_eq!((res.inserted, res.deduped), (0, 3));
    assert_eq!(not_stored(), Vec::<String>::new());
}

/// A task id or a handoff file name in two repos gives one item per repo: a file supersedes
/// only its own repo's items, with different or equal bodies.
#[test]
fn service_import_keeps_one_item_per_repo_for_a_shared_task_id_or_handoff_name() {
    for same_body in [false, true] {
        let (_d, tmp) = tmpdir();
        let home = tmp.join("home");
        let root = tmp.join("gh");
        let hdir = tmp.join("handoffs");
        std::fs::create_dir_all(&home).unwrap();
        write(
            &home.join("config.toml"),
            &format!(
                "[automemory]\nroot = \"{}\"\n[repos]\nroots = [\"{}\"]\n[upkeep]\ntasklog_roots = [\"{}\"]\nhandoff_dirs = [\"{}\"]\n",
                empty_dir(&tmp).display(),
                root.display(),
                root.display(),
                hdir.display()
            ),
        );
        let tl = |repo: &str, extra: &str| {
            let p = root.join(format!("{repo}/docs/tasklog/2026-11-01/shared.yaml"));
            let result = if same_body { "tests pass" } else { repo };
            write(
                &p,
                &format!(
                    "id: 20261101-090000-shared\ntitle: Shared {repo}\nresults:\n  - {result}\n{extra}"
                ),
            );
            p
        };
        // each handoff folder holds one file name; its CWD line names the repo
        let ho = |repo: &str, extra: &str| {
            let p = hdir.join(format!("{repo}-session/HANDOFF.md"));
            let done = if same_body { "tests pass" } else { repo };
            write(
                &p,
                &format!(
                    "# HANDOFF: shared {repo}\nCWD: {}\n\n## Done\n- {done}\n{extra}",
                    root.join(repo).display()
                ),
            );
            p
        };
        let active = || {
            common::column(
                &home,
                "SELECT repo || ' ' || source || ' ' || title FROM items WHERE status = 'active' \
                 ORDER BY repo, source",
            )
        };
        let both = [
            "r1 handoff:HANDOFF.md shared r1",
            "r1 task:20261101-090000-shared Shared r1",
            "r2 handoff:HANDOFF.md shared r2",
            "r2 task:20261101-090000-shared Shared r2",
        ];
        let lag = || {
            let l = doctor_imports(&home).lag.unwrap().0;
            (l.not_stored.len(), l.stored_tasks_without_file.len())
        };
        let (a1, a2, h1, h2) = (tl("r1", ""), tl("r2", ""), ho("r1", ""), ho("r2", ""));
        for (kind, p) in [
            ("tasklog", &a1),
            ("tasklog", &a2),
            ("handoff", &h1),
            ("handoff", &h2),
        ] {
            let res = import(&home, kind, p, false, false);
            assert_eq!(
                (res.inserted, res.deduped, res.superseded),
                (1, 0, 0),
                "{kind} {p:?} same_body={same_body}"
            );
        }
        assert_eq!(active(), both, "same_body={same_body}");
        assert_eq!(lag(), (0, 0));
        // a second run of every file stores nothing and supersedes nothing
        for (kind, p) in [
            ("tasklog", &a1),
            ("tasklog", &a2),
            ("handoff", &h1),
            ("handoff", &h2),
        ] {
            let res = import(&home, kind, p, false, false);
            assert_eq!((res.inserted, res.deduped, res.superseded), (0, 1, 0));
        }
        // an edit of either repo's file supersedes only that repo's item
        let (r1_task, r1_ho) = (
            common::column(
                &home,
                "SELECT id FROM items WHERE repo = 'r1' AND source LIKE 'task:%'",
            ),
            common::column(
                &home,
                "SELECT id FROM items WHERE repo = 'r1' AND source LIKE 'handoff:%'",
            ),
        );
        tl("r1", "  - edited\n");
        ho("r1", "- edited\n");
        assert_eq!(lag(), (2, 0));
        for (kind, p) in [("tasklog", &a1), ("handoff", &h1)] {
            let res = import(&home, kind, p, false, false);
            assert_eq!((res.inserted, res.superseded), (1, 1), "{kind}");
        }
        assert_eq!(active(), both);
        let old = common::column(
            &home,
            "SELECT o.id || ' ' || n.repo FROM items o JOIN items n ON n.id = o.superseded_by \
             WHERE o.status = 'superseded' ORDER BY o.rowid",
        );
        assert_eq!(
            old,
            [format!("{} r1", r1_task[0]), format!("{} r1", r1_ho[0])]
        );
        tl("r2", "  - edited\n");
        ho("r2", "- edited\n");
        for (kind, p) in [("tasklog", &a2), ("handoff", &h2)] {
            let res = import(&home, kind, p, false, false);
            assert_eq!((res.inserted, res.superseded), (1, 1), "{kind}");
        }
        assert_eq!(active(), both);
        // r1's files go back to their first bodies: their first items come back, r2's stay
        let r2_ids = common::column(
            &home,
            "SELECT id FROM items WHERE repo = 'r2' AND status = 'active' ORDER BY source",
        );
        tl("r1", "");
        ho("r1", "");
        for (kind, p) in [("tasklog", &a1), ("handoff", &h1)] {
            let res = import(&home, kind, p, false, false);
            assert_eq!((res.inserted, res.superseded), (1, 1), "{kind}");
        }
        assert_eq!(
            common::column(
                &home,
                "SELECT id FROM items WHERE status = 'active' ORDER BY repo, source",
            ),
            [&r1_ho[0], &r1_task[0], &r2_ids[0], &r2_ids[1]].map(String::as_str)
        );
        assert_eq!(lag(), (0, 0));
        for (kind, p) in [
            ("tasklog", &a1),
            ("tasklog", &a2),
            ("handoff", &h1),
            ("handoff", &h2),
        ] {
            let res = import(&home, kind, p, false, false);
            assert_eq!((res.inserted, res.superseded), (0, 0));
        }
        // a plain query in each repo finds that repo's task log and handoff
        for repo in ["r1", "r2"] {
            let mut titles: Vec<String> = query(&home, "shared", repo)
                .into_iter()
                .map(|i| i.title)
                .collect();
            titles.sort();
            assert_eq!(titles, [format!("Shared {repo}"), format!("shared {repo}")]);
        }
    }
}

/// The state the old cross-repo rule left: r1's task item superseded by r2's item of the same
/// source. The next import of r1's file restores r1's row (no new row), the doctor lists the
/// file until then, and a second run does nothing. An amended row, a manual supersede by an
/// item of another source, and a file whose body changed since are left as they are.
#[test]
fn service_import_restores_a_task_item_another_repo_hid() {
    let (_d, tmp) = tmpdir();
    let home = tmp.join("home");
    let root = tmp.join("gh");
    std::fs::create_dir_all(&home).unwrap();
    write(
        &home.join("config.toml"),
        &format!(
            "[automemory]\nroot = \"{}\"\n[upkeep]\ntasklog_roots = [\"{}\"]\nhandoff_dirs = []\n",
            empty_dir(&tmp).display(),
            root.display()
        ),
    );
    let tl = |repo: &str, name: &str, result: &str| {
        let p = root.join(format!("{repo}/docs/tasklog/2026-11-01/{name}.yaml"));
        write(
            &p,
            &format!("id: 20261101-{name}\ntitle: T {repo} {name}\nresults:\n  - {result}\n"),
        );
        p
    };
    let id_of = |repo: &str, name: &str| {
        common::column(
            &home,
            &format!(
                "SELECT id FROM items WHERE repo = '{repo}' AND source = 'task:20261101-{name}' \
                 ORDER BY rowid"
            ),
        )
        .remove(0)
    };
    let id_of_new = |repo: &str| {
        common::column(
            &home,
            &format!(
                "SELECT id FROM items WHERE repo = '{repo}' AND source = 'task:20261101-090000-plain' \
                 ORDER BY rowid DESC"
            ),
        )
        .remove(0)
    };
    // the old rule's result, made by hand: r1's row superseded by r2's row of its source
    let hide = |name: &str| {
        let (r1, r2) = (id_of("r1", name), id_of("r2", name));
        common::open(&home)
            .execute(
                "UPDATE items SET status = 'superseded', superseded_by = ?1 WHERE id = ?2",
                [&r2, &r1],
            )
            .unwrap();
        r1
    };
    let not_stored = || {
        let mut names: Vec<String> = doctor_imports(&home)
            .lag
            .unwrap()
            .0
            .not_stored
            .iter()
            .map(|p| {
                let p = Path::new(p);
                let repo = p.ancestors().nth(4).unwrap().file_name().unwrap();
                format!(
                    "{}/{}",
                    repo.to_str().unwrap(),
                    p.file_name().unwrap().to_str().unwrap()
                )
            })
            .collect();
        names.sort();
        names
    };
    let state = |id: &str| {
        common::column(
            &home,
            &format!(
                "SELECT status || ' ' || ifnull(superseded_by, '-') FROM items WHERE id = '{id}'"
            ),
        )
        .remove(0)
    };
    for n in [
        "090000-plain",
        "100000-amended",
        "110000-manual",
        "120000-edited",
    ] {
        tl("r1", n, &format!("r1 {n}"));
        tl("r2", n, &format!("r2 {n}"));
    }
    assert_eq!(import(&home, "tasklog", &root, false, false).inserted, 8);
    // plain: hidden by r2, then r2 edits its file, so r1's row points at a superseded r2 row
    let plain = hide("090000-plain");
    let r2_plain = id_of("r2", "090000-plain");
    let r2_file = tl("r2", "090000-plain", "r2 plain edited");
    let res = import(&home, "tasklog", &r2_file, false, false);
    assert_eq!((res.inserted, res.superseded), (1, 1));
    assert_eq!(state(&r2_plain), format!("superseded {}", id_of_new("r2")));
    // amended: an amend by hand supersedes r1's row; that stays
    let amended = id_of("r1", "100000-amended");
    let (code, out, _) = sm(&home, &["amend", &amended, "--title", "T amended by hand"]);
    assert_eq!(code, 0);
    let amended_row = out.trim().to_string();
    // manual: a manual item of r2 (another source) supersedes r1's row by hand; that stays
    let manual_case = id_of("r1", "110000-manual");
    let manual = put(&home, "manual note on the manual case", "decided", "r2");
    assert_eq!(sm(&home, &["supersede", &manual_case, &manual]).0, 0);
    // edited: hidden by r2, then r1's file changes: the import inserts a new row
    let edited = hide("120000-edited");
    let r2_edited = id_of("r2", "120000-edited");
    tl("r1", "120000-edited", "r1 edited again");

    assert_eq!(
        not_stored(),
        ["r1/090000-plain.yaml", "r1/120000-edited.yaml"]
    );
    let res = import(&home, "tasklog", &root.join("r1"), false, false);
    assert_eq!((res.inserted, res.deduped, res.superseded), (2, 2, 0));
    assert_eq!(not_stored(), Vec::<String>::new());
    let res = import(&home, "tasklog", &root.join("r1"), false, false);
    assert_eq!((res.inserted, res.deduped, res.superseded), (0, 4, 0));
    assert_eq!(state(&plain), "active -");
    assert_eq!(state(&amended), format!("superseded {amended_row}"));
    assert_eq!(state(&manual_case), format!("superseded {manual}"));
    assert_eq!(state(&edited), format!("superseded {r2_edited}"));
    assert_eq!(
        common::column(
            &home,
            "SELECT repo || ' ' || source FROM items WHERE status = 'active' ORDER BY repo, source",
        ),
        [
            "r1 amended:task:20261101-100000-amended",
            "r1 task:20261101-090000-plain",
            "r1 task:20261101-120000-edited",
            "r2 ",
            "r2 task:20261101-090000-plain",
            "r2 task:20261101-100000-amended",
            "r2 task:20261101-110000-manual",
            "r2 task:20261101-120000-edited",
        ]
    );
}

/// A row that a newer version of the same file in the SAME repo replaced is not restored through
/// the other-repo rule: A -> B in one repo, B retired by hand (the source has no active row), the
/// file back to A. The retire stays: A is deduped against its superseded row, nothing comes back,
/// and the doctor lists nothing.
#[test]
fn service_import_does_not_restore_a_row_its_own_repo_replaced() {
    let (_d, tmp) = tmpdir();
    let home = tmp.join("home");
    let root = tmp.join("gh");
    std::fs::create_dir_all(&home).unwrap();
    write(
        &home.join("config.toml"),
        &format!(
            "[automemory]\nroot = \"{}\"\n[upkeep]\ntasklog_roots = [\"{}\"]\nhandoff_dirs = []\n",
            empty_dir(&tmp).display(),
            root.display()
        ),
    );
    let file = root.join("r1/docs/tasklog/2026-11-01/090000-same.yaml");
    let body = |result: &str| {
        write(
            &file,
            &format!("id: 20261101-090000-same\ntitle: T same\nresults:\n  - {result}\n"),
        );
    };
    let counts = || {
        let res = import(&home, "tasklog", &file, false, false);
        (res.inserted, res.deduped, res.superseded)
    };
    let states = || {
        common::column(
            &home,
            "SELECT status || ' ' || ifnull(superseded_by, '-') FROM items ORDER BY rowid",
        )
    };
    body("body A");
    assert_eq!(counts(), (1, 0, 0));
    body("body B");
    assert_eq!(counts(), (1, 0, 1));
    let ids = common::column(&home, "SELECT id FROM items ORDER BY rowid");
    assert_eq!(sm(&home, &["retire", &ids[1], "--reason", "by hand"]).0, 0);
    assert_eq!(
        states(),
        [format!("superseded {}", ids[1]), s("superseded -")]
    );
    body("body A");
    assert_eq!(counts(), (0, 1, 0));
    assert_eq!(
        states(),
        [format!("superseded {}", ids[1]), s("superseded -")]
    );
    assert_eq!(counts(), (0, 1, 0));
    assert_eq!(
        doctor_imports(&home).lag.unwrap().0.not_stored,
        Vec::<String>::new()
    );
}

#[test]
fn service_import_stores_a_file_whose_body_a_manual_item_holds() {
    let (_d, tmp) = tmpdir();
    let home = tmp.join("home");
    let root = tmp.join("gh");
    let p = root.join("r1/docs/tasklog/2026-11-01/a.yaml");
    write(
        &p,
        "id: 20261101-090000-aaaa\ntitle: Auth fix\nresults:\n  - tests pass\n",
    );
    std::fs::create_dir_all(&home).unwrap();
    write(
        &home.join("config.toml"),
        &format!(
            "[automemory]\nroot = \"{}\"\n[upkeep]\ntasklog_roots = [\"{}\"]\nhandoff_dirs = []\n",
            empty_dir(&tmp).display(),
            root.display()
        ),
    );
    let not_stored = || doctor_imports(&home).lag.unwrap().0.not_stored.len();
    let manual = put(&home, "Results:\n- tests pass", "decided", "r1");
    assert_eq!(not_stored(), 1);
    let res = import(&home, "tasklog", &p, false, false);
    assert_eq!((res.inserted, res.deduped), (1, 0));
    let active = "SELECT source FROM items WHERE status = 'active' ORDER BY source";
    assert_eq!(
        common::column(&home, active),
        ["", "task:20261101-090000-aaaa"]
    );
    assert_eq!(not_stored(), 0);
    // a manual put of that body still returns an active row that holds it
    assert_eq!(
        put(&home, "Results:\n- tests pass", "decided", "r1"),
        manual
    );
    // an amend keeps the file item in a slot of its own (amended:<source>), so the manual item
    // with its body does not block it
    let file_id = &common::column(
        &home,
        "SELECT id FROM items WHERE status = 'active' AND source != ''",
    )[0];
    let (code, _, err) = sm(&home, &["amend", file_id, "--title", "Auth fix v2"]);
    assert_eq!((code, err.as_str()), (0, ""));
    assert_eq!(
        common::column(&home, active),
        ["", "amended:task:20261101-090000-aaaa"]
    );
    assert_eq!(not_stored(), 0);
}

/// A store with task log root `gh` (repo r1), no auto-memory files and an empty transcript
/// root for the catch-up's sessions part; (home, task log dir).
fn amend_store(tmp: &Path) -> (PathBuf, PathBuf) {
    let home = tmp.join("home");
    let root = tmp.join("gh");
    let dir = root.join("r1/docs/tasklog/2026-11-01");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(tmp.join(".claude/projects")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    write(
        &home.join("config.toml"),
        &format!(
            "[automemory]\nroot = \"{}\"\n[upkeep]\ntasklog_roots = [\"{}\"]\nhandoff_dirs = []\n",
            empty_dir(tmp).display(),
            root.display()
        ),
    );
    (home, dir)
}

/// A task log in `dir` whose stored body is "Results:\n- {result}".
fn amend_tasklog(dir: &Path, name: &str, id: &str, result: &str) -> PathBuf {
    let p = dir.join(format!("{name}.yaml"));
    write(
        &p,
        &format!("id: {id}\ntitle: T {name}\nresults:\n  - {result}\n"),
    );
    p
}

/// `sm amend <args>`, which must work (stderr may hold a near-duplicate notice); the new id.
fn amend_ok(home: &Path, args: &[&str]) -> String {
    let mut a = vec!["amend"];
    a.extend_from_slice(args);
    let (code, out, err) = sm(home, &a);
    assert_eq!(code, 0, "amend {args:?}: {err}");
    assert!(!err.contains("error") && !err.contains("UNIQUE"), "{err}");
    out.trim().to_string()
}

/// A title-only, a tag-only and a refs-only amend of `id`, each of which must work; the last id.
fn amend_each_field(home: &Path, id: &str) -> String {
    let mut id = id.to_string();
    for args in [
        ["--title", "Renamed by amend"],
        ["--add-tags", "amend-tag"],
        ["--add-refs", "TKT-909"],
    ] {
        id = amend_ok(home, &[&id, args[0], args[1]]);
    }
    let row = &common::column(
        home,
        &format!("SELECT title || '|' || tags || '|' || refs FROM items WHERE id = '{id}'"),
    )[0];
    assert!(row.starts_with("Renamed by amend|"), "{row}");
    assert!(
        row.contains("amend-tag") && row.contains("TKT-909"),
        "{row}"
    );
    id
}

/// Two catch-ups that store, supersede and retire nothing.
fn catchup_changes_nothing(home: &Path) {
    let rows = "SELECT id || ' ' || status || ' ' || coalesce(superseded_by, '-') FROM items \
                ORDER BY rowid";
    let before = common::column(home, rows);
    for _ in 0..2 {
        let (code, _, err) = sm(home, &["import", "catch-up", "--quiet"]);
        let log = std::fs::read_to_string(home.join("catchup.log")).unwrap();
        let row: serde_json::Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
        assert_eq!((code, err.as_str()), (0, ""), "{row}");
        assert_eq!(
            row["parts"]["imports"]["counts"],
            json!({"not_stored": 0, "inserted": 0, "failed_rows": 0}),
            "{row}"
        );
        assert_eq!(common::column(home, rows), before);
    }
}

const ACTIVE_SOURCE_TITLE: &str =
    "SELECT source || ' ' || title FROM items WHERE status = 'active' ORDER BY source, title";

/// Route A: a manual item, then a task log with its body (both active); the manual item still
/// takes a title, tag or refs amend, and a second manual item still cannot take its body.
#[test]
fn service_amend_of_a_manual_item_whose_body_a_file_item_holds() {
    let (_d, tmp) = tmpdir();
    let (home, dir) = amend_store(&tmp);
    let manual = put(&home, "Results:\n- tests pass", "assumed", "r1");
    let p = amend_tasklog(&dir, "a", "20261101-090000-aaaa", "tests pass");
    let res = import(&home, "tasklog", &p, false, false);
    assert_eq!((res.inserted, res.deduped), (1, 0));
    let last = amend_each_field(&home, &manual);
    assert_eq!(
        common::column(&home, ACTIVE_SOURCE_TITLE),
        [" Renamed by amend", "task:20261101-090000-aaaa T a"]
    );
    // the manual dedupe still holds, and the error names the manual item, not the file's
    let other = put(&home, "other notes", "assumed", "r1");
    let (code, _, err) = sm(
        &home,
        &["amend", &other, "--body", "Results:\n- tests pass"],
    );
    assert_eq!(code, 1);
    assert!(
        err.contains(&format!(
            "Repo 'r1' already has an active item with the same body ({last}); use sm supersede {other} {last}"
        )),
        "{err}"
    );
    assert!(!err.contains("UNIQUE"), "{err}");
    catchup_changes_nothing(&home);
}

/// Route B: a store from 0.6.0, whose amend gave a task log item the source '', next to the
/// item the file now has; the 0.6.0 row still takes a title, tag or refs amend.
#[test]
fn service_amend_of_a_row_amended_by_0_6_0_next_to_its_file_item() {
    let (_d, tmp) = tmpdir();
    let (home, dir) = amend_store(&tmp);
    amend_tasklog(&dir, "a", "20261101-090000-aaaa", "tests pass");
    import(&home, "tasklog", &dir, false, false);
    let task = common::column(&home, "SELECT id FROM items WHERE status = 'active'")[0].clone();
    common::open(&home)
        .execute(
            "INSERT INTO items (id, repo, kind, title, body, refs, tags, source, tag, evidence, \
             as_of, status, superseded_by, content_hash) \
             SELECT 'm_amended_by_060', repo, kind, 'Amended in 0.6.0', body, refs, tags, '', tag, \
             evidence, as_of, 'active', NULL, content_hash FROM items WHERE id = ?1",
            [&task],
        )
        .unwrap();
    amend_each_field(&home, "m_amended_by_060");
    assert_eq!(
        common::column(&home, ACTIVE_SOURCE_TITLE),
        [" Renamed by amend", "task:20261101-090000-aaaa T a"]
    );
    catchup_changes_nothing(&home);
}

/// Route C: a file item amended to a manual item's body leaves the manual item free to amend; a
/// manual item amended to a file item's body leaves the file item stored.
#[test]
fn service_amend_next_to_an_item_of_another_slot_with_its_body() {
    let (_d, tmp) = tmpdir();
    let (home, dir) = amend_store(&tmp);
    amend_tasklog(&dir, "a", "20261101-090000-aaaa", "tests pass");
    amend_tasklog(&dir, "b", "20261101-100000-bbbb", "billing shipped");
    import(&home, "tasklog", &dir, false, false);
    let file_a = common::column(
        &home,
        "SELECT id FROM items WHERE source = 'task:20261101-090000-aaaa'",
    )[0]
    .clone();
    let manual = put(&home, "deploy steps for TKT-101", "assumed", "r1");
    let amended_a = amend_ok(&home, &[&file_a, "--body", "deploy steps for TKT-101"]);
    amend_each_field(&home, &manual);
    amend_ok(&home, &[&amended_a, "--add-tags", "y"]);
    // the other way: a manual item amended to the body file b's item holds; b's item stays
    let notes = put(&home, "draft notes", "assumed", "r1");
    amend_ok(&home, &[&notes, "--body", "Results:\n- billing shipped"]);
    assert_eq!(
        common::column(&home, ACTIVE_SOURCE_TITLE),
        [
            " Renamed by amend",
            // the default title follows the new body's first line
            " Results:",
            "amended:task:20261101-090000-aaaa T a",
            "task:20261101-100000-bbbb T b"
        ]
    );
    catchup_changes_nothing(&home);
}

/// Every row's id, repo, source, status and successor, in insert order.
const ALL_ROWS: &str = "SELECT id || ' ' || repo || ' [' || source || '] ' || status || ' ' || \
                        coalesce(superseded_by, '-') FROM items ORDER BY rowid";

/// A refused amend: exit 1, the supersede hint that names `holder`, no raw index error, and no
/// row stored, superseded or changed.
fn assert_amend_refused(home: &Path, args: &[&str], repo: &str, old: &str, holder: &str) {
    let before = common::column(home, ALL_ROWS);
    let mut a = vec!["amend"];
    a.extend_from_slice(args);
    let (code, out, err) = sm(home, &a);
    assert_eq!(code, 1, "{args:?}: {out} {err}");
    assert!(
        err.contains(&format!(
            "Repo '{repo}' already has an active item with the same body ({holder}); use sm supersede {old} {holder}"
        )),
        "{err}"
    );
    assert!(
        !out.contains("UNIQUE") && !err.contains("UNIQUE"),
        "{out} {err}"
    );
    assert_eq!(common::column(home, ALL_ROWS), before);
}

/// The id of the one active item of `repo` with `source`.
fn active_id(home: &Path, repo: &str, source: &str) -> String {
    common::column(
        home,
        &format!(
            "SELECT id FROM items WHERE status = 'active' AND repo = '{repo}' AND source = '{source}'"
        ),
    )[0]
    .clone()
}

/// An amended file item holds the slot amended:<source> of its repo. After the file changes, the
/// importer stores the new text as a row of the file's own source; amending that row to the
/// amended row's body would take the held slot, so it is refused with the supersede hint.
#[test]
fn service_amend_into_a_taken_amended_slot_is_refused() {
    let (_d, tmp) = tmpdir();
    let (home, dir) = amend_store(&tmp);
    let p = amend_tasklog(&dir, "a", "20261101-090000-aaaa", "tests pass");
    import(&home, "tasklog", &p, false, false);
    let file_id = active_id(&home, "r1", "task:20261101-090000-aaaa");
    let amended = amend_ok(&home, &[&file_id, "--title", "Renamed by amend"]);
    amend_tasklog(&dir, "a", "20261101-090000-aaaa", "tests pass again");
    assert_eq!(import(&home, "tasklog", &p, false, false).inserted, 1);
    let newer = active_id(&home, "r1", "task:20261101-090000-aaaa");
    assert_amend_refused(
        &home,
        &[&newer, "--body", "Results:\n- tests pass"],
        "r1",
        &newer,
        &amended,
    );
    // the same row still takes an amend that leaves the held body alone
    amend_ok(&home, &[&newer, "--add-tags", "y"]);
    assert_eq!(
        common::column(&home, ACTIVE_SOURCE_TITLE),
        [
            "amended:task:20261101-090000-aaaa Renamed by amend",
            "amended:task:20261101-090000-aaaa T a"
        ]
    );
    catchup_changes_nothing(&home);
}

/// `amend --repo` moves a file item to another repo, where its new slot amended:<source> is
/// either free (the amend works and the importer then stores nothing) or held by an item of
/// that repo with the same body (refused with the supersede hint).
#[test]
fn service_amend_to_another_repo_follows_the_slot_of_the_new_repo() {
    let (_d, tmp) = tmpdir();
    let (home, dir_r1) = amend_store(&tmp);
    let dir_r2 = tmp.join("gh/r2/docs/tasklog/2026-11-01");
    std::fs::create_dir_all(&dir_r2).unwrap();
    amend_tasklog(&dir_r1, "a", "20261101-090000-aaaa", "tests pass");
    amend_tasklog(&dir_r1, "b", "20261101-100000-bbbb", "billing shipped");
    amend_tasklog(&dir_r2, "b", "20261101-100000-bbbb", "billing shipped");
    import(&home, "tasklog", &dir_r1, false, false);
    import(&home, "tasklog", &dir_r2, false, false);
    let b = "task:20261101-100000-bbbb";

    // slot free in r2: the amend works, the row is in r2, and the file's repo r1 is not refilled
    let moved = amend_ok(
        &home,
        &[
            &active_id(&home, "r1", "task:20261101-090000-aaaa"),
            "--repo",
            "r2",
        ],
    );
    assert_eq!(
        common::column(
            &home,
            &format!(
                "SELECT repo || ' [' || source || '] ' || status FROM items WHERE id = '{moved}'"
            )
        ),
        ["r2 [amended:task:20261101-090000-aaaa] active"]
    );
    catchup_changes_nothing(&home);

    // slot held in r2: r2's own copy of file b was amended, so it holds amended:<b>
    let held = amend_ok(
        &home,
        &[&active_id(&home, "r2", b), "--title", "Renamed by amend"],
    );
    let r1_b = active_id(&home, "r1", b);
    assert_amend_refused(&home, &[&r1_b, "--repo", "r2"], "r2", &r1_b, &held);
    assert_eq!(active_id(&home, "r1", b), r1_b);
}

#[test]
fn service_import_stores_a_file_whose_body_an_amended_item_holds() {
    let (_d, tmp) = tmpdir();
    let home = tmp.join("home");
    let root = tmp.join("gh");
    let dir = root.join("r1/docs/tasklog/2026-11-01");
    let tl = |name: &str, id: &str, title: &str| {
        write(
            &dir.join(format!("{name}.yaml")),
            &format!("id: {id}\ntitle: {title}\nresults:\n  - tests pass\n"),
        );
    };
    tl("a", "20261101-090000-aaaa", "Auth fix TKT-101");
    std::fs::create_dir_all(&home).unwrap();
    write(
        &home.join("config.toml"),
        &format!(
            "[automemory]\nroot = \"{}\"\n[upkeep]\ntasklog_roots = [\"{}\"]\nhandoff_dirs = []\n",
            empty_dir(&tmp).display(),
            root.display()
        ),
    );
    let not_stored = || {
        let lag = doctor_imports(&home).lag.unwrap().0.not_stored;
        lag.iter()
            .map(|p| {
                Path::new(p)
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string()
            })
            .collect::<Vec<_>>()
    };
    let active = || {
        common::column(
            &home,
            "SELECT source || ' ' || title FROM items WHERE status = 'active' ORDER BY title",
        )
    };
    import(&home, "tasklog", &root, false, false);
    let a_id = &common::column(&home, "SELECT id FROM items WHERE status = 'active'")[0];
    let (code, out, err) = sm(&home, &["amend", a_id, "--title", "Auth fix renamed"]);
    assert_eq!((code, err.as_str()), (0, ""));
    let renamed = out.trim().to_string();
    // the amended row's source is amended:task:...; a's own superseded row keeps a's file stored
    assert_eq!(not_stored(), Vec::<String>::new());
    tl("b", "20261101-100000-bbbb", "Billing fix TKT-202");
    assert_eq!(not_stored(), ["b.yaml"]);
    let res = import(&home, "tasklog", &root, false, false);
    assert_eq!((res.inserted, res.deduped), (1, 1));
    assert_eq!(
        active(),
        [
            "amended:task:20261101-090000-aaaa Auth fix renamed",
            "task:20261101-100000-bbbb Billing fix TKT-202"
        ]
    );
    assert_eq!(not_stored(), Vec::<String>::new());
    // an amend that changes the body: neither file is imported again
    let (code, _, _) = sm(&home, &["amend", &renamed, "--body", "tests pass, shipped"]);
    assert_eq!(code, 0);
    let res = import(&home, "tasklog", &root, false, false);
    assert_eq!((res.inserted, res.deduped, res.superseded), (0, 2, 0));
    assert_eq!(
        active(),
        [
            "amended:task:20261101-090000-aaaa Auth fix renamed",
            "task:20261101-100000-bbbb Billing fix TKT-202"
        ]
    );
    assert_eq!(not_stored(), Vec::<String>::new());
}

#[test]
fn service_doctor_imports_fails_on_missing_root() {
    let (_d, tmp) = tmpdir();
    let h = tmp.to_str();
    write(
        &tmp.join("config.toml"),
        &format!(
            "[upkeep]\ntasklog_roots = [\"{}\"]\nhandoff_dirs = []\n",
            tmp.join("nope").display()
        ),
    );
    let err = run_doctor(false, false, false, true, h).unwrap_err();
    assert!(err.class == "FileNotFoundError", "{err}");
    assert!(
        err.msg
            .starts_with("upkeep.tasklog_roots entry not found: ")
            && err.msg.ends_with(&format!(
                "nope; fix or remove it in [upkeep] tasklog_roots in {}/config.toml",
                tmp.display()
            )),
        "{err}"
    );
    write(
        &tmp.join("config.toml"),
        &format!(
            "[upkeep]\ntasklog_roots = []\nhandoff_dirs = [\"{}\"]\n",
            tmp.join("gone").display()
        ),
    );
    let err = run_doctor(false, false, false, true, h).unwrap_err();
    assert!(err.class == "FileNotFoundError", "{err}");
    assert!(
        err.msg.starts_with("upkeep.handoff_dirs entry not found: ")
            && err.msg.ends_with(&format!(
                "gone; fix or remove it in [upkeep] handoff_dirs in {}/config.toml",
                tmp.display()
            )),
        "{err}"
    );
}

#[test]
fn service_doctor_imports_scans_a_root_that_sits_in_a_worktree() {
    let (_d, tmp) = tmpdir();
    write(&tmp.join("wt/.git"), "gitdir: /x/.git/worktrees/wt\n");
    let f = tasklog(
        &tmp.join("wt/gh/r1/docs/tasklog/2026-11-01/a.yaml"),
        "20261101-090000-0000000a-a-aaaa",
    );
    write(
        &tmp.join("config.toml"),
        &format!(
            "[automemory]\nroot = \"{}\"\n[upkeep]\ntasklog_roots = [\"{}\"]\nhandoff_dirs = []\n",
            empty_dir(&tmp).display(),
            tmp.join("wt/gh").display()
        ),
    );
    let (lag, _, _) = doctor_imports(&tmp).lag.unwrap();
    assert_eq!(
        lag.not_stored,
        [std::fs::canonicalize(&f).unwrap().to_str().unwrap()]
    );
}

#[test]
fn service_doctor_backfill_keeps_old_refs_and_adds_new() {
    let (_d, tmp) = tmpdir();
    seed(
        &tmp,
        vec![Item {
            refs: s("X1 abc1234"),
            evidence: s("commit def5678"),
            as_of: s("2026-10-01"),
            ..Item::bare("m_C", "anvil", "fact", "Note", "R7 poll waits", "decided")
        }],
    );
    let doc = run_doctor(false, true, false, false, tmp.to_str()).unwrap();
    assert_eq!(
        (
            doc.refs["refs_empty_before"],
            doc.refs["refs_changed"],
            doc.refs["refs_empty_after"]
        ),
        (0, 1, 0)
    );
    assert_eq!(show_refs(&tmp, "m_C"), "X1 abc1234 R7 def5678");
}

// ---------------------------------------------------------------- CLI

#[test]
fn cli_put_query_show_supersede_doctor() {
    let (_d, home) = scratch();
    let (code, _, err) = sm(&home, &["put", "No tag given"]);
    assert_eq!(code, 1);
    assert!(err.contains("Evidence tag is required"), "{err}");
    let (code, _, err) = sm(&home, &["put", "--tag", "verified", "No evidence given"]);
    assert_eq!(code, 1);
    assert!(err.contains("needs non-empty evidence"), "{err}");

    let body1 = "R6 stays reverted: narrowing prune_stale() loses old catches";
    let (code, out, _) = sm(
        &home,
        &[
            "put", "--kind", "decision", "--tag", "decided", "--refs", "R6", "--repo", "anvil",
            body1,
        ],
    );
    assert_eq!(code, 0);
    let id1 = out.trim().to_string();
    assert!(id1.starts_with("m_"));
    let (code, out, _) = sm(&home, &["query", "R6", "--repo", "anvil"]);
    assert_eq!(code, 0);
    assert!(
        out.contains(&id1) && out.contains("repo=anvil") && out.contains("tag=decided"),
        "{out}"
    );
    let (code, out, _) = sm(&home, &["show", &id1]);
    assert_eq!(code, 0);
    assert!(
        out.contains(&format!("id: {id1}")) && out.contains("R6 stays reverted"),
        "{out}"
    );

    let args = [
        "put",
        "--kind",
        "decision",
        "--tag",
        "verified",
        "--evidence",
        "pytest -q",
        "--repo",
        "anvil",
        "R6 updated decision with full recall check",
    ];
    let (code, out, _) = sm(&home, &args);
    assert_eq!(code, 0);
    let id2 = out.trim().to_string();
    let (code, out, _) = sm(&home, &["supersede", &id1, &id2]);
    assert_eq!(code, 0);
    assert_eq!(out.trim(), format!("{id1} -> {id2}"));
    let (code, out, _) = sm(&home, &["doctor", "--rebuild-fts"]);
    assert_eq!(code, 0);
    assert!(
        out.contains("integrity=ok") && out.contains("rebuilt_fts=True"),
        "{out}"
    );
}

#[test]
fn cli_import_tasklog_and_reimport_dedupe() {
    let (_d, tmp) = tmpdir();
    let home = tmp.join("sm_home");
    let tl_file = tmp.join("sample.yaml");
    write(
        &tl_file,
        "id: 20261025-120000-11223344-cli-import-test-abcd\ntitle: CLI import test\nstate: done\nrepo: simple-memory\n\
         as_of: 2026-10-25T12:00:00+00:00\ncommits:\n  - 1122334\nresults:\n  - Verified CLI import dedupe\n",
    );
    let f = tl_file.to_str().unwrap();
    let (code, out, _) = sm(&home, &["import", "tasklog", f, "--dry-run"]);
    assert_eq!(code, 0);
    assert!(out.contains("total=1 inserted=0"), "{out}");
    assert!(!home.join("memory.db").exists());
    let (code, out, _) = sm(&home, &["import", "tasklog", f]);
    assert_eq!(code, 0);
    assert!(
        out.contains("total=1 inserted=1 deduped=0 superseded=0 rejected=0"),
        "{out}"
    );
    let (code, out, _) = sm(&home, &["import", "tasklog", f]);
    assert_eq!(code, 0);
    assert!(
        out.contains("total=1 inserted=0 deduped=1 superseded=0 rejected=0"),
        "{out}"
    );
}

#[test]
fn cli_supersedes_typo_dash_query_budget_and_one_line_errors() {
    let (_d, tmp) = tmpdir();
    let home = tmp.join("sm_home");
    // Item 3: sm put --supersedes m_TYPO exits non-zero and writes nothing
    let body = "Should not be written when supersedes target is missing";
    let (code, _, err) = sm(
        &home,
        &[
            "put",
            "--tag",
            "decided",
            "--repo",
            "anvil",
            "--supersedes",
            "m_TYPO",
            body,
        ],
    );
    assert_eq!(code, 1);
    assert!(err.contains("m_TYPO"), "{err}");
    let (code, out, _) = sm(&home, &["doctor"]);
    assert_eq!(code, 0);
    assert!(out.contains("total=0"), "{out}");
    // Item 16: --limit <= 0 rejected
    let (code, _, err) = sm(&home, &["query", "anything", "--limit", "0"]);
    assert_eq!(code, 1);
    assert!(err.to_lowercase().contains("limit"), "{err}");
    // Item 17 and 18: leading-dash query with `--` and <= 2400 char cap with even trimming
    for i in 0..4 {
        let title = format!("Long item {i}");
        let body = format!("Flag -R6 detail {i}: {}", "word ".repeat(220));
        let (code, _, err) = sm(
            &home,
            &[
                "put", "--tag", "decided", "--repo", "anvil", "--refs", "R6", "--title", &title,
                &body,
            ],
        );
        assert_eq!(code, 0, "{err}");
    }
    let (code, out, _) = sm(&home, &["query", "--repo", "anvil", "--", "-R6"]);
    assert_eq!(code, 0);
    assert!(out.chars().count() <= 2400, "{}", out.chars().count());
    let lengths: Vec<usize> = out
        .lines()
        .filter(|l| l.starts_with("  "))
        .map(|l| l.chars().count())
        .collect();
    assert_eq!(lengths.len(), 4);
    assert!(
        lengths.iter().max().unwrap() - lengths.iter().min().unwrap() <= 5,
        "{lengths:?}"
    );
    // Item 19: malformed YAML prints a one-line message to stderr and exits 1
    let bad_yaml = tmp.join("bad.yaml");
    write(&bad_yaml, "id: [unclosed\n  broken: :\n");
    let (code, _, err) = sm(&home, &["import", "tasklog", bad_yaml.to_str().unwrap()]);
    assert_eq!(code, 1);
    assert!(
        !err.trim().contains('\n') && err.starts_with("error:"),
        "{err}"
    );
}

#[test]
fn cli_query_header_drops_the_ref_that_repeats_the_source() {
    let (_d, home) = scratch();
    let tl = home.parent().unwrap().join("t.yaml");
    write(
        &tl,
        "id: 20261101-090000-aabbccdd-tab-cleanup-1a2b\ntitle: Tab cleanup\nrepo: anvil\nas_of: 2026-11-01T09:00:00+00:00\n\
         commits:\n  - abc1234\nresults:\n  - Poll closes its tab\n",
    );
    assert_eq!(sm(&home, &["import", "tasklog", tl.to_str().unwrap()]).0, 0);
    let (code, out, _) = sm(&home, &["query", "tab cleanup", "--repo", "anvil"]);
    assert_eq!(code, 0);
    let header = out.lines().next().unwrap();
    assert!(
        header.ends_with("source=task:20261101-090000-aabbccdd-tab-cleanup-1a2b refs=abc1234"),
        "{header}"
    );
}

#[test]
fn cli_doctor_backfill_refs_prints_counts() {
    let (_d, home) = scratch();
    assert_eq!(
        sm(
            &home,
            &[
                "put",
                "--tag",
                "decided",
                "--repo",
                "anvil",
                "Mia prefers short replies"
            ]
        )
        .0,
        0
    );
    let (code, out, _) = sm(&home, &["doctor", "--backfill-refs", "--dry-run"]);
    assert_eq!(code, 0);
    assert_eq!(
        out.lines().nth(1).unwrap(),
        "backfill_refs (dry-run): empty_before=1 changed=0 empty_after=1"
    );
    let (code, _, err) = sm(&home, &["doctor", "--dry-run"]);
    assert_eq!(code, 1);
    assert_eq!(
        err.trim(),
        "error: --dry-run applies to --backfill-refs only"
    );
}

#[test]
fn cli_doctor_imports_prints_lag_and_unreadable() {
    let (_d, tmp) = tmpdir();
    let home = tmp.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let bad = tmp.join("gh/r1/docs/tasklog/2026-11-01/bad.yaml");
    write(&bad, "id: [unclosed\n");
    write(&tmp.join("ho/h9.md"), "# H9\n");
    write(
        &home.join("config.toml"),
        &format!(
            "[automemory]\nroot = \"{}\"\n[upkeep]\ntasklog_roots = [\"{}\"]\nhandoff_dirs = [\"{}\"]\n",
            empty_dir(&tmp).display(),
            tmp.join("gh").display(),
            tmp.join("ho").display()
        ),
    );
    let (code, out, _) = sm(&home, &["doctor", "--imports"]);
    assert_eq!(code, 0);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines[1],
        "imports: tasklog_files=1 handoff_files=1 not_stored=1 unreadable=1 stored_tasks_without_file=0"
    );
    assert_eq!(
        lines[2],
        format!("not_stored: {}", tmp.join("ho/h9.md").display())
    );
    assert!(
        lines[3].starts_with(&format!("unreadable: {}: Malformed YAML", bad.display())),
        "{}",
        lines[3]
    );
}

// ---------------------------------------------------------------- CLI setup

#[test]
fn doctor_imports_on_a_fresh_home_says_automemory_is_off_and_exits_0() {
    let (_d, tmp) = tmpdir();
    let stranger = tmp.join(".simple-memory");
    assert_eq!(sm(&stranger, &["init"]).0, 0);
    let (code, out, _) = sm(&stranger, &["doctor", "--imports"]);
    assert_eq!(code, 0);
    assert_eq!(
        out.lines().collect::<Vec<_>>(),
        [
            "integrity=ok rebuilt_fts=False total=0 active=0 superseded=0",
            "imports: tasklog_files=0 handoff_files=0 not_stored=0 unreadable=0 stored_tasks_without_file=0",
            "automemory: off ([automemory] enabled = false)",
            "last_catchup: none",
        ]
    );
}

#[test]
fn doctor_imports_still_fails_on_a_missing_root_when_automemory_is_on() {
    let (_d, tmp) = tmpdir();
    let stranger = tmp.join(".simple-memory");
    assert_eq!(sm(&stranger, &["init"]).0, 0);
    let cfg = stranger.join("config.toml");
    let text = std::fs::read_to_string(&cfg).unwrap();
    assert!(text.contains("enabled = false  # no"));
    write(
        &cfg,
        &text.replace("enabled = false  # no", "enabled = true  # no"),
    );
    let (code, _, err) = sm(&stranger, &["doctor", "--imports"]);
    assert_eq!(code, 1);
    assert_eq!(
        err,
        format!(
            "error: automemory root not found: {}\n",
            tmp.join(".claude/projects").display()
        )
    );
}

// ---------------------------------------------------------------- config

#[test]
fn imports_and_doctor_alias_the_repo_a_key_or_folder_names() {
    let (_d, tmp) = tmpdir();
    let code = tmp.join("code");
    std::fs::create_dir_all(code.join("proj")).unwrap();
    let tasklogs = code.join("proj-wt/docs/tasklog/2026-11-01");
    for task_id in ["20261101-090000-t1", "20261101-090000-t2"] {
        // same body, another file: each gets its own item
        write(
            &tasklogs.join(format!("{task_id}.yaml")),
            &format!(
                "id: {task_id}\ntitle: T\nas_of: 2026-11-01T09:00:00+00:00\nresults:\n  - same result\n"
            ),
        );
    }
    let key: String = code
        .join("proj-wt")
        .to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let handoffs = tmp.join("handoffs").join(key);
    for name in ["a.md", "b.md"] {
        // same body, another file: each gets its own item
        write(
            &handoffs.join(name),
            "# Handoff\n\n## Done\n- shipped the alias rule\n",
        );
    }
    let home = tmp.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(tmp.join("none")).unwrap();
    write(
        &home.join("config.toml"),
        &format!(
            "[repos]\nroots = [\"{0}\"]\n[repo_aliases]\n\"proj-wt\" = \"proj\"\n[upkeep]\ntasklog_roots = [\"{0}\"]\nhandoff_dirs = [\"{1}\"]\n[automemory]\nroot = \"{2}\"\n",
            code.display(),
            tmp.join("handoffs").display(),
            tmp.join("none").display()
        ),
    );
    let res = import(&home, "handoff", &tmp.join("handoffs"), false, false);
    assert_eq!(
        (
            res.inserted,
            res.deduped,
            res.by_repo.get("proj").copied(),
            res.by_repo.len()
        ),
        (2, 0, Some(2), 1)
    );
    let res = import(
        &home,
        "tasklog",
        &code.join("proj-wt/docs/tasklog"),
        false,
        false,
    );
    assert_eq!(
        (
            res.inserted,
            res.deduped,
            res.by_repo.get("proj").copied(),
            res.by_repo.len()
        ),
        (2, 0, Some(2), 1)
    );
    // doctor parses each file with the same roots and aliases, so an imported file counts as stored
    assert_eq!(
        doctor_imports(&home).lag.unwrap().0.not_stored,
        Vec::<String>::new()
    );
    let aliases: Aliases = [(s("proj-wt"), s("proj"))].into_iter().collect();
    let dirs = tasklog_repo_dirs(
        &PyValue::List(vec![PyValue::Str(s(code.to_str().unwrap()))]),
        &aliases,
        home.to_str(),
    )
    .unwrap();
    assert_eq!(dirs.len(), 1);
    assert_eq!(
        dirs["proj"].as_str(),
        code.join("proj-wt").to_str().unwrap()
    );
}

// ---------------------------------------------------------------- CLI redact

#[test]
fn cli_import_and_put_store_redacted_text() {
    // Made-up values in the shape each kind has; none is a real credential or id.
    let fake_sk = format!("sk-ant-api03-{}", "Abc123xyz_".repeat(4));
    let fake_gh = format!("ghp_{}", "a1B2c3D4e5".repeat(4));
    let fake_sha = "0123456789abcdef0123456789abcdef01234567";
    let fake_short_sha = "1a2b3c4";
    let fake_task_id = "20261101-090000-aabbccdd-redact-test-1a2b";
    let (_d, tmp) = tmpdir();
    let home = tmp.join("home");
    let tl = tmp.join("t.yaml");
    write(
        &tl,
        &format!(
            "id: {fake_task_id}\ntitle: Redact test\nrepo: a\nas_of: 2026-11-01T09:00:00+00:00\ncommits:\n  - {fake_short_sha}\nresults:\n  - Set api key {fake_sk} in the env\n  - Note <private>gross margin 17 percent</private> done\n"
        ),
    );
    let f = tl.to_str().unwrap();
    assert_eq!(sm(&home, &["import", "tasklog", f]).0, 0);
    let (code, out, _) = sm(&home, &["query", "redact test", "--repo", "a"]);
    assert_eq!(code, 0);
    let item_id = out
        .split(']')
        .next()
        .unwrap()
        .trim_start_matches('[')
        .to_string();
    let (code, shown, _) = sm(&home, &["show", &item_id]);
    assert_eq!(code, 0);
    assert!(
        shown.contains("[REDACTED:sk_key]") && shown.contains("[REDACTED:private]"),
        "{shown}"
    );
    assert!(
        !shown.contains(&fake_sk) && !shown.contains("gross margin"),
        "{shown}"
    );
    assert!(
        shown.contains(&format!("source: task:{fake_task_id}")),
        "{shown}"
    );
    assert!(
        shown
            .split("refs: ")
            .nth(1)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .contains(fake_short_sha),
        "{shown}"
    );

    // a second import of the same file dedupes on the redacted hash
    let (code, out, _) = sm(&home, &["import", "tasklog", f]);
    assert_eq!(code, 0);
    assert!(out.contains("inserted=0 deduped=1"), "{out}");

    let body = format!("Token {fake_gh} <private>secret plan</private> for {fake_sha}");
    let (code, out, _) = sm(&home, &["put", "--repo", "a", "--tag", "assumed", &body]);
    assert_eq!(code, 0);
    let put_id = out.trim().to_string();
    let (code, shown_put, _) = sm(&home, &["show", &put_id]);
    assert_eq!(code, 0);
    assert!(
        shown_put.trim_end().ends_with(&format!(
            "Token [REDACTED:github_token] [REDACTED:private] for {fake_sha}"
        )),
        "{shown_put}"
    );
    assert!(!shown_put.contains("secret plan"), "{shown_put}");
}

// ---------------------------------------------------------------- alias expansion

/// A task log and an auto-memory front matter whose aliases nest 9 wide and `depth` deep
/// (`b<i>` holds 9 aliases of `b<i-1>`), each next to a normal file.
fn alias_bomb_lines(depth: usize) -> String {
    let mut t = "a: &a [\"x\",\"x\",\"x\",\"x\",\"x\",\"x\",\"x\",\"x\",\"x\"]\n".to_string();
    let mut prev = "a".to_string();
    for i in 1..=depth {
        t.push_str(&format!(
            "b{i}: &b{i} [{}]\n",
            vec![format!("*{prev}"); 9].join(",")
        ));
        prev = format!("b{i}");
    }
    t.push_str(&format!("results:\n  - *{prev}\n"));
    t
}

const TOO_MANY: &str = "YAML document expands to too many nodes (its aliases copy more than 750000 nodes and scalar bytes)";

/// `sm <args>` as `common::sm` runs it, killed after `secs`: a run past the limit fails the
/// test instead of hanging the test run.
fn sm_within(home: &Path, args: &[&str], secs: u64) -> (i32, String, String) {
    sm_within_env(home, args, secs, &[])
}

fn sm_within_env(
    home: &Path,
    args: &[&str],
    secs: u64,
    env: &[(&str, &str)],
) -> (i32, String, String) {
    use std::io::Read;
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_sm"))
        .args(args)
        .env("HOME", home.parent().unwrap())
        .env("SIMPLE_MEMORY_HOME", home)
        .env_remove("SM_PARITY_NOW")
        .env_remove("SM_PARITY_SEED")
        .env_remove("SM_PARITY_TRACE")
        .envs(env.iter().copied())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn sm");
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if started.elapsed() > std::time::Duration::from_secs(secs) {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("sm {args:?} ran past {secs} s");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let (mut out, mut err) = (String::new(), String::new());
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    (status.code().unwrap_or(-1), out, err)
}

/// A task log and an auto-memory note that hold `yaml`, each next to a normal file, are
/// refused like any malformed YAML file: a one-line error for the file itself (`<path>:
/// Malformed YAML: ` then `problem`), a row in `unreadable:` and a failed row in a catch-up,
/// the normal task log stored; the auto-memory front matter falls back to its known keys line
/// by line. Each command must end within 20 s.
fn refused_yaml_and_the_next_file_imported(yaml: &str, problem: &str) {
    let (_d, tmp) = tmpdir();
    let home = tmp.join("home");
    std::fs::create_dir_all(tmp.join(".claude/projects")).unwrap();
    let repo = tmp.join("gh/r1");
    std::fs::create_dir_all(&repo).unwrap();
    common::git(&["init", "-q", repo.to_str().unwrap()]);
    let day = repo.join("docs/tasklog/2026-11-01");
    let bad = day.join("090500-bad.yaml");
    write(
        &bad,
        &format!("id: 20261101-090500-bad\ntitle: bad\n{yaml}"),
    );
    write(
        &day.join("090600-good.yaml"),
        "id: 20261101-090600-good\ntitle: Good one\nstate: done\nas_of: 2026-11-01T09:06:00+00:00\nresults:\n  - normal result line\n",
    );
    let am = tmp.join("projects");
    write(
        &am.join("p-alpha/s1.jsonl"),
        &format!("{}\n", json!({"cwd": repo.to_str().unwrap()})),
    );
    write(
        &am.join("p-alpha/memory/bad-note.md"),
        &format!(
            "---\nname: bad-note\ndescription: Bad note\ntype: project\n{yaml}---\nbad body\n"
        ),
    );
    write(
        &am.join("p-alpha/memory/good-note.md"),
        "---\nname: good-note\ndescription: Good note\ntype: project\n---\nnormal note body\n",
    );
    std::fs::create_dir_all(tmp.join("ho")).unwrap();
    write(
        &home.join("config.toml"),
        &format!(
            "[automemory]\nroot = \"{}\"\n[upkeep]\ntasklog_roots = [\"{}\"]\nhandoff_dirs = [\"{}\"]\n",
            am.display(),
            tmp.join("gh").display(),
            tmp.join("ho").display()
        ),
    );

    let (code, _, err) = sm_within(&home, &["import", "tasklog", bad.to_str().unwrap()], 20);
    let msg = format!("{}: Malformed YAML: {problem}", bad.display());
    assert_eq!(
        (code, err.as_str()),
        (1, format!("error: {msg}\n").as_str())
    );

    let (code, out, err) = sm_within(&home, &["doctor", "--imports"], 20);
    assert_eq!((code, err.as_str()), (0, ""));
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines[1],
        "imports: tasklog_files=2 handoff_files=0 not_stored=1 unreadable=1 stored_tasks_without_file=0"
    );
    assert!(
        lines[3].starts_with(&format!("unreadable: {}: Malformed YAML: ", bad.display())),
        "{}",
        lines[3]
    );

    let (code, out, err) = sm_within(&home, &["import", "automemory"], 20);
    assert_eq!((code, err.as_str()), (0, ""));
    assert_eq!(
        out.lines().next().unwrap(),
        "import automemory (applied): files=2 added=2 updated=0 retired=0 unchanged=0 unreadable=0 unmapped=0 yaml_errors=1"
    );

    let (code, out, err) = sm_within(&home, &["import", "catch-up"], 20);
    assert_eq!(
        (code, err.as_str()),
        (
            0,
            "catch-up: 1 task log or handoff not imported; sm doctor --imports lists it\n"
        )
    );
    assert!(
        !out.contains("Traceback") && !out.contains("error"),
        "{out}"
    );
    let rows = common::column(&home, "SELECT source FROM items ORDER BY source");
    assert_eq!(
        rows,
        [
            "automemory:p-alpha/bad-note.md",
            "automemory:p-alpha/good-note.md",
            "task:20261101-090600-good"
        ]
    );
}

#[test]
fn alias_bomb_depth_5_is_refused_and_the_next_file_imported() {
    refused_yaml_and_the_next_file_imported(&alias_bomb_lines(5), TOO_MANY);
}

#[test]
fn alias_bomb_depth_7_is_refused_and_the_next_file_imported() {
    refused_yaml_and_the_next_file_imported(&alias_bomb_lines(7), TOO_MANY);
}

/// A `=` value key whose value leads back to its own mapping (PyYAML 6.0.3 recurses until
/// RecursionError, which no caller catches) is a YAML error at that mapping.
#[test]
fn a_value_key_that_leads_back_to_its_mapping_is_refused_and_the_next_file_imported() {
    refused_yaml_and_the_next_file_imported(
        "v: !!str &a {=: *a}\nresults:\n  - x\n",
        "found a value key that leads back to its own mapping at line 3, column 4: v: !!str &a {=: *a} ^",
    );
}

/// A value nested past the loader's depth limits ends in RecursionError, which PyYAML raises
/// too; for one file it is malformed YAML, not the end of every import.
#[test]
fn a_value_nested_too_deep_is_refused_and_the_next_file_imported() {
    let nest = format!(
        "v: {}{}\nresults:\n  - x\n",
        "[".repeat(492),
        "]".repeat(492)
    );
    refused_yaml_and_the_next_file_imported(&nest, "maximum recursion depth exceeded");
    let mut chain = "junk:\n- &a0 [x]\n".to_string();
    for i in 1..998 {
        chain.push_str(&format!("- &a{i} [*a{}]\n", i - 1));
    }
    chain.push_str("results:\n  - x\n");
    refused_yaml_and_the_next_file_imported(&chain, "maximum recursion depth exceeded");
}

/// An impossible date or time is a ValueError of the loader (PyYAML 6.0.3 gives the same text);
/// for one file it is malformed YAML, not the end of every import.
#[test]
fn an_impossible_date_or_time_is_refused_and_the_next_file_imported() {
    for (value, problem) in [
        ("2026-13-01", "month must be in 1..12, not 13"),
        (
            "2026-02-30",
            "day 30 must be in range 1..28 for month 2 in year 2026",
        ),
        ("2026-11-01T25:00:00", "hour must be in 0..23, not 25"),
    ] {
        refused_yaml_and_the_next_file_imported(
            &format!("when: {value}\nresults:\n  - x\n"),
            problem,
        );
    }
}

// ---------------------------------------------------------------- one file's value errors

/// A local zone east of UTC: 0001-01-01 00:00 local is in year 0 in UTC.
const EAST_OF_UTC: [(&str, &str); 2] = [("SM_UTC_OFFSET", "+07:00"), ("TZ", "Asia/Bangkok")];

struct OddFiles {
    _d: tempfile::TempDir,
    home: PathBuf,
    day: PathBuf,
    odd_task: String,
    odd_note: String,
}

/// A git repo whose task log folder holds `odd.yaml` (with `task_extra`) next to a normal task
/// log, and a project folder whose memory holds `odd-note.md` (with `note_extra` in its front
/// matter) next to a normal note.
fn odd_files(task_extra: &str, note_extra: &str) -> OddFiles {
    let (d, tmp) = tmpdir();
    let home = tmp.join("home");
    std::fs::create_dir_all(tmp.join(".claude/projects")).unwrap();
    let repo = tmp.join("gh/r1");
    std::fs::create_dir_all(&repo).unwrap();
    common::git(&["init", "-q", repo.to_str().unwrap()]);
    let day = repo.join("docs/tasklog/2026-11-01");
    let odd_task = day.join("090500-odd.yaml");
    write(
        &odd_task,
        &format!("id: 20261101-090500-odd\ntitle: odd\n{task_extra}results:\n  - x\n"),
    );
    write(
        &day.join("090600-good.yaml"),
        "id: 20261101-090600-good\ntitle: Good one\nstate: done\nas_of: 2026-11-01T09:06:00+00:00\nresults:\n  - normal result line\n",
    );
    let am = tmp.join("projects");
    write(
        &am.join("p-alpha/s1.jsonl"),
        &format!("{}\n", json!({"cwd": repo.to_str().unwrap()})),
    );
    let odd_note = am.join("p-alpha/memory/odd-note.md");
    write(
        &odd_note,
        &format!("---\nname: odd-note\ndescription: Odd note\n{note_extra}---\nodd body\n"),
    );
    write(
        &am.join("p-alpha/memory/good-note.md"),
        "---\nname: good-note\ndescription: Good note\ntype: project\n---\nnormal note body\n",
    );
    std::fs::create_dir_all(tmp.join("ho")).unwrap();
    write(
        &home.join("config.toml"),
        &format!(
            "[automemory]\nroot = \"{}\"\n[upkeep]\ntasklog_roots = [\"{}\"]\nhandoff_dirs = [\"{}\"]\n",
            am.display(),
            tmp.join("gh").display(),
            tmp.join("ho").display()
        ),
    );
    OddFiles {
        _d: d,
        home,
        day,
        odd_task: odd_task.to_str().unwrap().to_string(),
        odd_note: odd_note.to_str().unwrap().to_string(),
    }
}

/// `sm <args>` in a zone east of UTC, killed after 20 s.
fn sm_east(f: &OddFiles, args: &[&str]) -> (i32, String, String) {
    sm_within_env(&f.home, args, 20, &EAST_OF_UTC)
}

fn sources(f: &OddFiles) -> Vec<String> {
    common::column(&f.home, "SELECT source FROM items ORDER BY source")
}

fn no_traceback(out: &str, err: &str) {
    assert!(
        !out.contains("Traceback") && !err.contains("Traceback"),
        "{out}\n{err}"
    );
}

/// A task log whose as_of is in year 0 in UTC (OverflowError after the YAML load): `sm import
/// tasklog <dir>` names the file, stores the other task log and exits 1; doctor lists it as
/// unreadable; a catch-up counts it as a failed row, points at doctor and stores every other
/// file.
fn task_log_in_year_zero_utc_is_named_and_the_rest_imported(as_of: &str) {
    let f = odd_files(&format!("as_of: {as_of}\n"), "");
    let (code, out, err) = sm_east(&f, &["doctor", "--imports"]);
    assert_eq!((code, err.as_str()), (0, ""), "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines[1],
        "imports: tasklog_files=2 handoff_files=0 not_stored=1 unreadable=1 stored_tasks_without_file=0"
    );
    let row = format!("unreadable: {}: date value out of range", f.odd_task);
    assert!(lines.contains(&row.as_str()), "{out}");

    let (code, out, err) = sm_east(&f, &["import", "tasklog", f.day.to_str().unwrap()]);
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (
            1,
            "import tasklog (applied): total=1\nby_repo: r1=1\nby_tag: derived=1\n\
             total=1 inserted=1 deduped=0 superseded=0 rejected=0 supersede_missing=0 failed_rows=1\n",
            format!(
                "skipped: {}: OverflowError: date value out of range\n\
                 1 file not imported; fix it and run the import again\n",
                f.odd_task
            )
            .as_str()
        )
    );
    assert_eq!(sources(&f), ["task:20261101-090600-good"]);

    let (code, out, err) = sm_east(&f, &["import", "catch-up"]);
    assert_eq!(
        (code, err.as_str()),
        (
            0,
            "catch-up: 1 task log or handoff not imported; sm doctor --imports lists it\n"
        ),
        "{out}"
    );
    no_traceback(&out, &err);
    assert!(
        out.lines()
            .any(|l| l == "imports: ok not_stored=0 inserted=0 failed_rows=1"),
        "{out}"
    );
    assert_eq!(
        sources(&f),
        [
            "automemory:p-alpha/good-note.md",
            "automemory:p-alpha/odd-note.md",
            "task:20261101-090600-good"
        ]
    );
}

#[test]
fn a_task_log_dated_year_one_in_local_time_is_named_and_the_rest_imported() {
    task_log_in_year_zero_utc_is_named_and_the_rest_imported("0001-01-01");
}

#[test]
fn a_task_log_dated_year_one_east_of_utc_is_named_and_the_rest_imported() {
    task_log_in_year_zero_utc_is_named_and_the_rest_imported("0001-01-01T00:00:00+10:00");
}

/// A note whose front matter loads but whose field conversion raises (a date in year 0 in UTC,
/// an int too long for str()): doctor and `sm import automemory` count it as unreadable and
/// name it; the normal note is stored; a catch-up stores every other file.
fn note_with_a_field_that_cannot_convert_is_unreadable(note_extra: &str) {
    let f = odd_files("", note_extra);
    let row = format!("unreadable: {}", f.odd_note);
    let (code, out, err) = sm_east(&f, &["doctor", "--imports"]);
    assert_eq!((code, err.as_str()), (0, ""), "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert!(
        lines.contains(
            &"automemory: files=2 not_stored=1 stored_without_file=0 unmapped=0 unreadable=1"
        ),
        "{out}"
    );
    assert!(lines.contains(&row.as_str()), "{out}");

    let (code, out, err) = sm_east(&f, &["import", "automemory"]);
    assert_eq!((code, err.as_str()), (0, ""), "{out}");
    assert_eq!(
        out.lines().collect::<Vec<_>>(),
        [
            "import automemory (applied): files=2 added=1 updated=0 retired=0 unchanged=0 unreadable=1 unmapped=0 yaml_errors=0",
            row.as_str()
        ]
    );
    assert_eq!(sources(&f), ["automemory:p-alpha/good-note.md"]);

    let (code, out, err) = sm_east(&f, &["import", "catch-up"]);
    assert_eq!((code, err.as_str()), (0, ""), "{out}");
    no_traceback(&out, &err);
    assert!(
        out.lines().any(|l| l
            == "automemory: ok added=0 updated=0 retired=0 unchanged=1 unreadable=1 unmapped=0"),
        "{out}"
    );
    assert_eq!(
        sources(&f),
        [
            "automemory:p-alpha/good-note.md",
            "task:20261101-090500-odd",
            "task:20261101-090600-good"
        ]
    );
}

/// Catch-up's closing count covers the task logs and handoffs it could not import and says so;
/// an unreadable memory note is counted on the `automemory:` line, and doctor lists both.
#[test]
fn catch_up_names_the_kinds_its_not_imported_count_covers() {
    let f = odd_files("results:\n\t- tab\n", "modified: 0001-01-01\n");
    let (_, out, _) = sm_east(&f, &["doctor", "--imports"]);
    assert_eq!(
        out.lines()
            .filter(|l| l.starts_with("unreadable: "))
            .count(),
        2,
        "{out}"
    );
    let (code, out, err) = sm_east(&f, &["import", "catch-up"]);
    assert_eq!(
        (code, err.as_str()),
        (
            0,
            "catch-up: 1 task log or handoff not imported; sm doctor --imports lists it\n"
        ),
        "{out}"
    );
    assert!(out.contains(" unreadable=1 "), "{out}");
}

#[test]
fn a_note_modified_in_year_one_local_time_is_unreadable() {
    note_with_a_field_that_cannot_convert_is_unreadable("modified: 0001-01-01\n");
}

#[test]
fn a_note_name_past_4300_digits_is_unreadable() {
    note_with_a_field_that_cannot_convert_is_unreadable(&format!("name: 0x{}\n", "f".repeat(3572)));
}

/// `sm import tasklog <dir>` with two files the parser refuses: one `skipped:` line for each, the
/// good file stored, the counts printed once, one closing line and exit 1. Once the files are
/// fixed the same command stores them, exits 0 and prints nothing on stderr.
#[test]
fn a_named_import_of_a_folder_skips_each_bad_file_and_stores_the_rest() {
    let f = odd_files("results:\n\t- tab\n", "");
    let list = f.day.join("090700-list.yaml");
    write(&list, "- a list\n");
    let dir = f.day.to_str().unwrap();
    let (code, out, err) = sm_east(&f, &["import", "tasklog", dir]);
    assert_eq!(
        (code, out.as_str(), err),
        (
            1,
            "import tasklog (applied): total=1\nby_repo: r1=1\nby_tag: derived=1\n\
             total=1 inserted=1 deduped=0 superseded=0 rejected=0 supersede_missing=0 failed_rows=2\n",
            format!(
                "skipped: {}: Malformed YAML: while scanning for the next token found character '\\t' \
                 that cannot start any token at line 4, column 1: - tab ^\n\
                 skipped: {}: the file is not a YAML mapping of task log fields (id, title, results)\n\
                 2 files not imported; fix them and run the import again\n",
                f.odd_task,
                list.display()
            )
        )
    );
    assert_eq!(sources(&f), ["task:20261101-090600-good"]);
    assert_eq!(
        sm_east(&f, &["import", "catch-up", "--quiet"]),
        (0, String::new(), String::new())
    );
    let (code, out, err) = sm_east(&f, &["import", "catch-up"]);
    assert_eq!(
        (code, err.as_str()),
        (
            0,
            "catch-up: 2 task logs or handoffs not imported; sm doctor --imports lists them\n"
        ),
        "{out}"
    );
    assert!(
        out.lines()
            .any(|l| l == "imports: ok not_stored=0 inserted=0 failed_rows=2"),
        "{out}"
    );

    write(
        Path::new(&f.odd_task),
        "id: 20261101-090500-odd\ntitle: odd\nresults:\n  - x\n",
    );
    write(&list, "id: 20261101-090700-list\nresults:\n  - y\n");
    let (code, out, err) = sm_east(&f, &["import", "tasklog", dir]);
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (
            0,
            "import tasklog (applied): total=3\nby_repo: r1=3\nby_tag: derived=3\n\
             total=3 inserted=2 deduped=1 superseded=0 rejected=0 supersede_missing=0 failed_rows=0\n",
            ""
        )
    );
    let (code, out, err) = sm_east(&f, &["import", "catch-up"]);
    assert_eq!((code, err.as_str()), (0, ""), "{out}");
    assert!(
        out.lines()
            .any(|l| l == "imports: ok not_stored=0 inserted=0 failed_rows=0"),
        "no hint when no file failed: {out}"
    );
}

/// A named import of a folder with no task log stores nothing, exits 0 and says so once.
#[test]
fn a_named_import_of_an_empty_folder_prints_its_counts_once() {
    let (_d, tmp) = tmpdir();
    let home = tmp.join("home");
    std::fs::create_dir_all(tmp.join("empty")).unwrap();
    let (code, out, err) = sm(
        &home,
        &["import", "tasklog", tmp.join("empty").to_str().unwrap()],
    );
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (
            0,
            "import tasklog (applied): total=0\nby_repo: none\nby_tag: none\n\
             total=0 inserted=0 deduped=0 superseded=0 rejected=0 supersede_missing=0 failed_rows=0\n",
            ""
        )
    );
}

/// A note with no name, no description and no text after its front matter gives nothing to
/// store but its file name: doctor
/// and `sm import automemory` count it as unreadable and name it, as a note that cannot be read.
/// An item an earlier import stored from it stays as it was.
#[test]
fn a_note_with_no_name_description_or_body_is_unreadable() {
    let f = odd_files("", "");
    let (code, _, err) = sm_east(&f, &["import", "automemory"]);
    assert_eq!((code, err.as_str()), (0, ""));
    let before = common::column(
        &f.home,
        "SELECT body || ' ' || status FROM items ORDER BY source",
    );
    assert_eq!(before, ["normal note body active", "odd body active"]);

    write(Path::new(&f.odd_note), "---\ntype: project\n---\n\n  \n");
    let row = format!("unreadable: {}", f.odd_note);
    let (code, out, err) = sm_east(&f, &["doctor", "--imports"]);
    assert_eq!((code, err.as_str()), (0, ""), "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert!(
        lines.contains(
            &"automemory: files=2 not_stored=0 stored_without_file=0 unmapped=0 unreadable=1"
        ),
        "{out}"
    );
    assert!(lines.contains(&row.as_str()), "{out}");
    let (code, out, err) = sm_east(&f, &["import", "automemory"]);
    assert_eq!((code, err.as_str()), (0, ""), "{out}");
    assert_eq!(
        out.lines().collect::<Vec<_>>(),
        [
            "import automemory (applied): files=2 added=0 updated=0 retired=0 unchanged=1 unreadable=1 unmapped=0 yaml_errors=0",
            row.as_str()
        ]
    );
    assert_eq!(
        common::column(
            &f.home,
            "SELECT body || ' ' || status FROM items ORDER BY source"
        ),
        before
    );
}

/// `commits: 1234567` (a short commit id of digits, which YAML reads as an int) is one commit
/// id, stored as the one-item list `commits: ["1234567"]` is.
#[test]
fn a_commits_value_of_one_number_is_one_commit_id() {
    let f = odd_files("commits: 1234567\n", "");
    let (code, out, err) = sm_east(&f, &["import", "tasklog", f.day.to_str().unwrap()]);
    assert_eq!((code, err.as_str()), (0, ""), "{out}");
    assert_eq!(
        out.lines().last(),
        Some(
            "total=2 inserted=2 deduped=0 superseded=0 rejected=0 supersede_missing=0 failed_rows=0"
        )
    );
    let scalar = refs_tag_evidence(&f.home, "task:20261101-090500-odd");
    assert_eq!(
        (scalar.1.as_str(), scalar.2.as_str()),
        ("executed", "1234567")
    );
    let g = odd_files("commits:\n  - \"1234567\"\n", "");
    let (code, _, err) = sm_east(&g, &["import", "tasklog", g.odd_task.as_str()]);
    assert_eq!((code, err.as_str()), (0, ""));
    assert_eq!(
        refs_tag_evidence(&g.home, "task:20261101-090500-odd"),
        scalar
    );
}

fn refs_tag_evidence(home: &Path, source: &str) -> (String, String, String) {
    common::open(home)
        .query_row(
            "SELECT refs, tag, evidence FROM items WHERE source = ?1",
            [source],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
}

/// A `commits` value that is one string, plain or quoted, is one commit id (never one id per
/// character), stored as the one-item list of that value is: `1e10` is a string in YAML 1.1.
#[test]
fn a_commits_value_of_one_string_is_one_commit_id() {
    let stored = |commits: &str| {
        let f = odd_files(&format!("commits:{commits}\n"), "");
        let (code, out, err) = sm_east(&f, &["import", "tasklog", f.odd_task.as_str()]);
        assert_eq!((code, err.as_str()), (0, ""), "{out}");
        refs_tag_evidence(&f.home, "task:20261101-090500-odd")
    };
    let scalars = ["abc1234", "\"1234567\"", "1e10"];
    let got: Vec<_> = scalars.iter().map(|v| stored(&format!(" {v}"))).collect();
    assert_eq!(
        got.iter()
            .map(|(_, tag, evidence)| (tag.as_str(), evidence.as_str()))
            .collect::<Vec<_>>(),
        [
            ("executed", "abc1234"),
            ("executed", "1234567"),
            ("executed", "1e10")
        ]
    );
    let listed: Vec<_> = scalars
        .iter()
        .map(|v| stored(&format!("\n  - {v}")))
        .collect();
    assert_eq!(got, listed);
}

/// A handoff written in year 0 in UTC is that file's problem, as for a task log: doctor lists
/// it as unreadable, `sm import handoff <dir>` names it, stores the other handoff and exits 1,
/// and a catch-up counts it as a failed row and stores every other file.
#[test]
fn a_handoff_written_in_year_one_local_time_is_named_and_the_rest_imported() {
    let f = odd_files("", "");
    let ho = f.home.parent().unwrap().join("ho");
    let odd = ho.join("odd.md");
    write(&odd, "# Odd handoff\nWritten: 0001-01-01\n## Done\n- x\n");
    write(
        &ho.join("good.md"),
        "# Good handoff\nWritten: 2026-11-01T09:00:00+00:00\n## Done\n- y\n",
    );
    let odd = odd.to_str().unwrap();
    let (code, out, err) = sm_east(&f, &["doctor", "--imports"]);
    assert_eq!((code, err.as_str()), (0, ""), "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines[1],
        "imports: tasklog_files=2 handoff_files=2 not_stored=3 unreadable=1 stored_tasks_without_file=0"
    );
    let row = format!("unreadable: {odd}: date value out of range");
    assert!(lines.contains(&row.as_str()), "{out}");

    let (code, out, err) = sm_east(&f, &["import", "handoff", ho.to_str().unwrap()]);
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (
            1,
            "import handoff (applied): total=1\nby_repo: unassigned=1\nby_tag: derived=1\n\
             total=1 inserted=1 deduped=0 superseded=0 rejected=0 supersede_missing=0 failed_rows=1\n",
            format!(
                "skipped: {odd}: OverflowError: date value out of range\n\
                 1 file not imported; fix it and run the import again\n"
            )
            .as_str()
        )
    );
    assert_eq!(sources(&f), ["handoff:good.md"]);

    let (code, out, err) = sm_east(&f, &["import", "catch-up"]);
    assert_eq!(
        (code, err.as_str()),
        (
            0,
            "catch-up: 1 task log or handoff not imported; sm doctor --imports lists it\n"
        ),
        "{out}"
    );
    no_traceback(&out, &err);
    assert!(
        out.lines()
            .any(|l| l == "imports: ok not_stored=2 inserted=2 failed_rows=1"),
        "{out}"
    );
    assert_eq!(
        sources(&f),
        [
            "automemory:p-alpha/good-note.md",
            "automemory:p-alpha/odd-note.md",
            "handoff:good.md",
            "task:20261101-090500-odd",
            "task:20261101-090600-good"
        ]
    );
}

/// A proposal line is one proposal: a line whose as_of is in year 0 in UTC is one rejected
/// line, and the next line is stored. A proposal file that is not UTF-8 stops the import with
/// an error that names it.
#[test]
fn a_proposal_line_dated_year_one_is_rejected_and_a_file_not_utf8_is_named() {
    let f = odd_files("", "");
    let dir = f.home.parent().unwrap().join("prop");
    let line = |title: &str, as_of: &str| {
        json!({"repo": "r1", "kind": "fact", "title": title, "body": "B", "tag": "derived",
               "evidence": "e", "as_of": as_of})
        .to_string()
    };
    write(
        &dir.join("a.jsonl"),
        &format!(
            "{}\n{}\n",
            line("Odd", "0001-01-01"),
            line("Fine", "2026-11-01T09:00:00+00:00")
        ),
    );
    let args = ["import", "proposal", dir.to_str().unwrap(), "--apply"];
    let (code, out, err) = sm_east(&f, &args);
    assert_eq!((code, err.as_str()), (0, ""), "{out}");
    assert_eq!(
        out.lines().last(),
        Some(
            "total=1 inserted=1 deduped=0 superseded=0 rejected=1 supersede_missing=0 failed_rows=0"
        )
    );
    assert_eq!(common::column(&f.home, "SELECT title FROM items"), ["Fine"]);

    let b = dir.join("b.jsonl");
    std::fs::write(&b, b"\xff\n").unwrap();
    let (code, out, err) = sm_east(&f, &args);
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (
            1,
            "",
            format!(
                "error: {}: 'utf-8' codec can't decode byte 0xff in position 0: invalid start byte\n",
                b.display()
            )
            .as_str()
        )
    );
}

// ---------------------------------------------------------------- URL credentials

/// A scheme right after a letter, a digit or `_` still has its password redacted, through
/// `sm put` and through a handoff import.
#[test]
fn url_credentials_after_a_word_char_are_redacted_by_put_and_handoff_import() {
    let (_d, tmp) = tmpdir();
    let home = tmp.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let raw = "DB_postgres://bob:Hunter2-example@db.example.com/app x_redis://:Hunter2-example@cache.example.com 2postgres://bob:Hunter2-example@db.example.com";
    let want = "DB_[REDACTED:url_credentials]db.example.com/app x_[REDACTED:url_credentials]cache.example.com 2[REDACTED:url_credentials]db.example.com";
    let id = common::put(&home, &["--repo", "anvil", "--tag", "derived", raw]);
    let h = tmp.join("handoffs/creds.md");
    write(&h, &format!("# Creds\n## Done\n- {raw}\n"));
    let (code, _, err) = sm(&home, &["import", "handoff", h.to_str().unwrap()]);
    assert_eq!((code, err.as_str()), (0, ""));
    let db = open(&home);
    let put_body: String = db
        .query_row("SELECT body FROM items WHERE id = ?1", [&id], |r| r.get(0))
        .unwrap();
    assert_eq!(put_body, want);
    let handoff_body: String = db
        .query_row(
            "SELECT body FROM items WHERE source LIKE 'handoff:%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        handoff_body.contains(&format!("- {want}")) && !handoff_body.contains("Hunter2"),
        "{handoff_body}"
    );
}

/// A token in a URL's user or password part, or at the start of a key=value value, leaves no
/// part of the secret next to it in clear, through `sm put` and through a handoff import.
#[test]
fn a_token_next_to_a_second_secret_is_redacted_by_put_and_handoff_import() {
    let (_d, tmp) = tmpdir();
    let home = tmp.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let raw = "s3a://AKIAIOSFODNN7EXAMPLE:wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY@my-bucket/data \
               https://AKIAIOSFODNN7EXAMPLE:wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY@s3.amazonaws.com/b \
               password=AKIAIOSFODNN7EXAMPLE/Hunter2-example \
               aws_token: AKIAIOSFODNN7EXAMPLE:wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    let want = "[REDACTED:url_credentials]my-bucket/data [REDACTED:url_credentials]s3.amazonaws.com/b \
                [REDACTED:key_value] [REDACTED:key_value]";
    let id = common::put(&home, &["--repo", "anvil", "--tag", "derived", raw]);
    let h = tmp.join("handoffs/tokens.md");
    write(&h, &format!("# Tokens\n## Done\n- {raw}\n"));
    let (code, _, err) = sm(&home, &["import", "handoff", h.to_str().unwrap()]);
    assert_eq!((code, err.as_str()), (0, ""));
    let db = open(&home);
    let put_body: String = db
        .query_row("SELECT body FROM items WHERE id = ?1", [&id], |r| r.get(0))
        .unwrap();
    assert_eq!(put_body, want);
    let handoff_body: String = db
        .query_row(
            "SELECT body FROM items WHERE source LIKE 'handoff:%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        handoff_body.contains(&format!("- {want}"))
            && !handoff_body.contains("Hunter2")
            && !handoff_body.contains("wJalr"),
        "{handoff_body}"
    );
}

/// A secret written with spaces in a refs value goes with every ref it spans, through
/// `put --refs` and `amend --add-refs`; the plain ref before it is kept.
#[test]
fn a_secret_with_spaces_in_refs_is_dropped_by_put_and_amend() {
    for (refs, secret) in common::SPACED_REF_SECRETS {
        let (_d, tmp) = tmpdir();
        let home = tmp.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let put = common::put(
            &home,
            &[
                "--repo",
                "anvil",
                "--tag",
                "derived",
                "--refs",
                refs,
                "rotation note one",
            ],
        );
        let old = common::put(
            &home,
            &["--repo", "anvil", "--tag", "derived", "rotation note two"],
        );
        // amend stores a new item that replaces the old one, and prints its id
        let (code, out, err) = sm(&home, &["amend", &old, "--add-refs", refs]);
        assert_eq!((code, err.as_str()), (0, ""), "{refs}");
        let amended = out.trim().to_string();
        let db = open(&home);
        for id in [&put, &amended] {
            let stored: String = db
                .query_row("SELECT refs FROM items WHERE id = ?1", [id], |r| r.get(0))
                .unwrap();
            assert_eq!(stored, "TKT-9", "{refs}");
        }
        drop(db);
        assert!(!common::db_files_hold(&home, secret), "{refs}");
    }
}

/// A secret pair written twice in refs, with the key word or the value repeated, is not stored:
/// the dedupe of the refs list must not take a pair apart before redaction reads it. Through
/// `put --refs` and `amend --add-refs`; commas alone may part the pair from its key word.
#[test]
fn a_secret_pair_written_twice_in_refs_is_dropped_by_put_and_amend() {
    let commas = (
        "TKT-9,db,password:,Hunter2-example-Zz9,redis,password:,Hunter2-example-Yy8",
        "TKT-9 db redis",
    );
    for (refs, want) in common::TWICE_REF_SECRETS.into_iter().chain([commas]) {
        let (_d, tmp) = tmpdir();
        let home = tmp.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let put = common::put(
            &home,
            &[
                "--repo",
                "anvil",
                "--tag",
                "derived",
                "--refs",
                refs,
                "rotation note one",
            ],
        );
        let old = common::put(
            &home,
            &[
                "--repo",
                "anvil",
                "--tag",
                "derived",
                "--refs",
                "TKT-1",
                "rotation note two",
            ],
        );
        let (code, out, err) = sm(&home, &["amend", &old, "--add-refs", refs]);
        assert_eq!((code, err.as_str()), (0, ""), "{refs}");
        let added = out.trim().to_string();
        let db = open(&home);
        let refs_of = |id: &str| -> String {
            db.query_row("SELECT refs FROM items WHERE id = ?1", [id], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(
            (refs_of(&put), refs_of(&added)),
            (want.to_string(), format!("TKT-1 {want}")),
            "{refs}"
        );
        drop(db);
        assert!(!common::db_files_hold_twice_secret(&home), "{refs}");
    }
}

/// `--add-refs` drops each ref it brings in that a match touches, and keeps every ref the item
/// held: a value whose key word the stored refs hold goes, and an old ref the added text pairs
/// with a key word stays. The pair alone then brings in nothing, and the amend says so.
#[test]
fn amend_drops_the_refs_a_match_touches_in_what_it_brings_in() {
    let cases = [
        ("password: TKT-2", "password: Hunter2-example-Zz9"),
        ("TKT-9 docs/x.md", "Bearer docs/x.md"),
        ("TKT-9 docs/keep.md", "aws_secret_access_key = docs/keep.md"),
    ];
    let mut got = Vec::new();
    for (old_refs, pair) in cases {
        let mut run = Vec::new();
        for add in [pair.to_string(), format!("{pair} TKT-3")] {
            let (_d, tmp) = tmpdir();
            let home = tmp.join("home");
            std::fs::create_dir_all(&home).unwrap();
            let old = common::put(
                &home,
                &[
                    "--repo",
                    "anvil",
                    "--tag",
                    "derived",
                    "--refs",
                    old_refs,
                    "rotation note",
                ],
            );
            let (code, _, err) = sm(&home, &["amend", &old, "--add-refs", &add]);
            let refs = common::column(&home, "SELECT refs FROM items WHERE status = 'active'");
            run.push((code, err, refs));
            assert!(
                !common::db_files_hold(&home, "Hunter2-example-Zz9"),
                "{add}"
            );
        }
        got.push(run);
    }
    // the pair alone brings in nothing: the amend stores nothing and the item keeps its refs
    let kept = |refs: &str| (1, "error: No changes to apply\n".to_string(), vec![s(refs)]);
    let added = |refs: &str| (0, String::new(), vec![s(refs)]);
    assert_eq!(
        got,
        [
            [kept("password: TKT-2"), added("password: TKT-2 TKT-3")],
            [kept("TKT-9 docs/x.md"), added("TKT-9 docs/x.md TKT-3")],
            [
                kept("TKT-9 docs/keep.md"),
                added("TKT-9 docs/keep.md TKT-3")
            ],
        ]
    );
}

#[test]
fn handoff_missing_repo_never_inserts_unassigned_over_known_repo() {
    let (_d, tmp) = tmpdir();
    let repos = tmp.join("repos");
    let alpha = repos.join("alpha");
    std::fs::create_dir_all(&alpha).unwrap();
    let hdir = tmp.join("handoffs");
    let key: String = alpha
        .to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let session_dir = hdir.join(key);
    std::fs::create_dir_all(&session_dir).unwrap();
    let hf = session_dir.join("20261101T0900Z-s1.md");
    std::fs::write(
        &hf,
        "# Alpha handoff\n\n## Done\n- Kept concrete repo scope.\n",
    )
    .unwrap();
    let home = tmp.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let cfg_text = format!(
        "[repos]\nroots = [\"{}\"]\n[automemory]\nenabled = false\n[upkeep]\ntasklog_roots = []\nhandoff_dirs = [\"{}\"]\n",
        repos.display(),
        hdir.display()
    );
    std::fs::write(home.join("config.toml"), &cfg_text).unwrap();

    // 1) Normal import resolves concrete repo = alpha.
    let res1 = import(&home, "handoff", &hf, false, false);
    assert_eq!((res1.inserted, res1.by_repo.get("alpha").copied()), (1, Some(1)));

    // 2) Hide alpha so RepoRoots::scan can no longer see it (simulating netjail deny_read_paths).
    std::fs::remove_dir_all(&alpha).unwrap();

    // Verify the probe itself proves the degraded scan parses this handoff as unassigned.
    let probe_home = tmp.join("probe_home");
    std::fs::create_dir_all(&probe_home).unwrap();
    std::fs::write(probe_home.join("config.toml"), &cfg_text).unwrap();
    let probe = import(&probe_home, "handoff", &hf, true, false);
    assert_eq!(probe.by_repo.get("unassigned").copied(), Some(1));

    // 3) Neither doctor import_lag nor run_import inserts or flags an unassigned duplicate.
    let doc = run_doctor(false, false, false, true, home.to_str()).unwrap();
    let lag = doc.lag.unwrap().0;
    assert!(lag.not_stored.is_empty(), "{:?}", lag.not_stored);
    let res2 = import(&home, "handoff", &hf, false, false);
    assert_eq!((res2.inserted, res2.deduped), (0, 1));
    assert_eq!(
        common::column(&home, "SELECT repo || ':' || status FROM items ORDER BY rowid"),
        vec!["alpha:active"]
    );
}
