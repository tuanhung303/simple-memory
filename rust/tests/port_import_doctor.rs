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
        .ok_or_else(|| SmError::key(&format!("Item not found: {item_id}")))
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
        err.is(Base::Value) && err.msg.contains("requires non-empty --evidence"),
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
    // same kept body as h1 in the same repo: the import dedupes it onto h1's item
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
    // another id, same body in the same repo: the import dedupes it onto a.yaml's item
    let same_body = root.join("r1/docs/tasklog/2026-11-01/a2.yaml");
    let text = std::fs::read_to_string(&stored).unwrap();
    write(
        &same_body,
        &text
            .replacen("0000000a-a-aaaa", "00000002-a-2222", 1)
            .replacen("title: T", "title: Retitled", 1),
    );
    assert_eq!(
        import(&home, "tasklog", &same_body, false, false).deduped,
        1
    );
    import(&home, "handoff", &handoffs.join("h1.md"), false, false);
    assert_eq!(
        import(&home, "handoff", &handoffs.join("h3.md"), false, false).deduped,
        1
    );
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
            && err.msg.ends_with("nope"),
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
        err.msg.starts_with("upkeep.handoff_dirs entry not found: ") && err.msg.ends_with("gone"),
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
    assert!(err.contains("requires non-empty --evidence"), "{err}");

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
    // Item 19: malformed YAML and bad SQLite path print a one-line message to stderr and exit 1
    let bad_yaml = tmp.join("bad.yaml");
    write(&bad_yaml, "id: [unclosed\n  broken: :\n");
    let (code, _, err) = sm(&home, &["import", "tasklog", bad_yaml.to_str().unwrap()]);
    assert_eq!(code, 1);
    assert!(
        !err.trim().contains('\n') && err.starts_with("error:"),
        "{err}"
    );
    let bad_db = tmp.join("not_a_sqlite_dir");
    std::fs::create_dir(&bad_db).unwrap();
    let (code, _, err) = sm(
        &home,
        &["import", "legacy", bad_db.to_str().unwrap(), "--dry-run"],
    );
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
        // same body: the second dedupes
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
        // same body: the second dedupes
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
        (1, 1, Some(2), 1)
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
        (1, 1, Some(2), 1)
    );
    // doctor parses each file with the same roots and aliases, so a deduped file counts as stored
    assert_eq!(
        doctor_imports(&home).lag.unwrap().0.not_stored,
        Vec::<String>::new()
    );
    let aliases: Aliases = [(s("proj-wt"), s("proj"))].into_iter().collect();
    let dirs = tasklog_repo_dirs(
        &PyValue::List(vec![PyValue::Str(s(code.to_str().unwrap()))]),
        &aliases,
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
