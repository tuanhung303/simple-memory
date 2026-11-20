//! Integration and boundary tests for Wave 1 Lane B:
//! 1. Match-centered `sm query` snippets (text and JSON)
//! 2. Evidence `tag` in `sm hook prompt` recall lines
//! 3. Additive `[scopes]` hook routing (Haiku and local fallback paths)
//!
//! Every fixture uses made-up names (`anvil`, `acme`, `globex`, `initech`, `GLX`, `INI`) and runs
//! against a scratch `SIMPLE_MEMORY_HOME`.

mod common;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use common::{git, open, put, scratch, sm, write};
use rusqlite::types::Value as Sql;
use serde_json::{Value, json};
use simple_memory::config::{load_config, scope_matchers, scopes};
use simple_memory::core::item::Item;
use simple_memory::core::rank::match_snippet;
use simple_memory::py::value::PyValue;
use simple_memory::service::query::{QueryArgs, StrList, run_query};
use simple_memory::service::recall::matching_scope_paths;
use simple_memory::store::items::put_item;
use simple_memory::store::search::{Filters, build_item_filters};

const TS: &str = "2026-10-25T10:00:00+00:00";

fn hook_prompt(home: &Path, args: &[&str], stdin: &str) -> (i32, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sm"));
    cmd.args(args)
        .env("HOME", home.parent().unwrap_or(home))
        .env("SIMPLE_MEMORY_HOME", home)
        .env_remove("SM_PROMPT_HOOK")
        .env_remove("SM_PARITY_NOW")
        .env_remove("SM_PARITY_SEED")
        .env_remove("SM_PARITY_TRACE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn sm hook");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("wait sm hook");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8(out.stdout).expect("utf-8 stdout"),
        String::from_utf8(out.stderr).expect("utf-8 stderr"),
    )
}

fn fake_claude(dir: &Path, reply: &str) -> String {
    let path = dir.join("fake-claude.sh");
    write(
        &path,
        &format!("#!/bin/sh\ncat >/dev/null\necho '{reply}'\n"),
    );
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_str().unwrap().to_string()
}

fn init_git_repo(parent: &Path, name: &str) -> std::path::PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    git(&["init", "-q", dir.to_str().unwrap()]);
    dir
}

fn seed_item(home: &Path, id: &str, repo: &str, title: &str, body: &str, tag: &str, refs: &str) {
    let conn = open(home);
    let mut it = Item::bare(id, repo, "fact", title, body, tag);
    it.refs = refs.to_string();
    it.evidence = "made-up check".to_string();
    it.as_of = TS.to_string();
    put_item(&conn, &it.post_init(), None).unwrap();
}

fn hook_log_rows(home: &Path) -> Vec<Value> {
    std::fs::read_to_string(home.join("hook.log"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

// ---------------------------------------------------------------- 1. Match-centered snippets

#[test]
fn query_json_and_text_center_long_snippets_on_deep_matches_and_keep_front_matches_prefixed() {
    let (_d, home) = scratch();
    let filler = "neutral background context sentence without the keyword. ".repeat(8);
    let deep_body = format!(
        "{filler}critical walrus failover switchover verified at step nine. {}",
        "trailing operational paragraph with additional notes. ".repeat(4)
    );
    let id_deep = put(
        &home,
        &[
            "--repo",
            "anvil",
            "--tag",
            "verified",
            "--evidence",
            "runbook check",
            "--title",
            "Long operational handbook",
            &deep_body,
        ],
    );
    let front_body = format!("walrus starts right at the beginning of this note. {filler}");
    let id_front = put(
        &home,
        &[
            "--repo",
            "anvil",
            "--tag",
            "verified",
            "--evidence",
            "runbook check",
            "--title",
            "walrus starts right at the beginning",
            &front_body,
        ],
    );

    let (code, out, err) = sm(
        &home,
        &[
            "query", "walrus", "--repo", "anvil", "--format", "json", "--limit", "4",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let v: Value = serde_json::from_str(&out).unwrap();
    let results = v["results"].as_array().unwrap();
    let deep = results.iter().find(|r| r["id"] == id_deep).unwrap();
    let deep_snip = deep["snippet"].as_str().unwrap();
    assert!(
        deep_snip.starts_with("...") && deep_snip.ends_with("..."),
        "expected centered window: {deep_snip}"
    );
    assert!(deep_snip.contains("walrus failover"), "{deep_snip}");
    assert!(
        deep_snip.chars().count() <= 300,
        "{}",
        deep_snip.chars().count()
    );

    let front = results.iter().find(|r| r["id"] == id_front).unwrap();
    let front_snip = front["snippet"].as_str().unwrap();
    assert!(
        front_snip.starts_with("walrus starts") && front_snip.ends_with("..."),
        "expected prefix cut for front match: {front_snip}"
    );
    assert!(front_snip.chars().count() <= 300);

    // Text mode with multiple long items shrinks each item's budget below full body length
    // and still centers the deep match inside the snippet line.
    for i in 0..3 {
        let pad = format!("unrelated walrus note {i} {}", "padding word ".repeat(60));
        put(&home, &["--repo", "anvil", "--tag", "derived", &pad]);
    }
    let (code, text_out, err) = sm(
        &home,
        &["query", "walrus", "--repo", "anvil", "--limit", "5"],
    );
    assert_eq!(code, 0, "{err}");
    let lines: Vec<&str> = text_out.lines().collect();
    let header_idx = lines
        .iter()
        .position(|l| l.starts_with(&format!("[{id_deep}]")))
        .expect(&text_out);
    let snippet_line = lines[header_idx + 1];
    assert!(
        snippet_line.starts_with("  ...") && snippet_line.contains("walrus failover"),
        "{snippet_line}"
    );
}

#[test]
fn query_snippets_handle_utf8_nfd_emdash_ref_punctuation_and_tail_matches() {
    let (_d, home) = scratch();
    let pad = "bình thường một đoạn văn dài ".repeat(14);
    let body =
        format!("{pad}tham chiếu (R5), cùng tệp src/a.py:42. và cafe\u{0301} Đorđe ở đoạn cuối");
    let id = put(
        &home,
        &[
            "--repo",
            "anvil",
            "--tag",
            "decided",
            "--title",
            "Ghi chú dài — cấu hình hệ thống",
            "--refs",
            "R5 src/a.py",
            &body,
        ],
    );

    // Query by ref with punctuation around it in body: "(R5)," and "src/a.py:42."
    for (q, expected_sub) in [
        ("R5", "(R5),"),
        ("src/a.py", "src/a.py:42."),
        ("cafe", "cafe\u{0301} Đorđe"),
    ] {
        let (code, out, err) = sm(&home, &["query", q, "--repo", "anvil", "--format", "json"]);
        assert_eq!(code, 0, "{q}: {err}");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["results"][0]["id"], id);
        let snip = v["results"][0]["snippet"].as_str().unwrap();
        assert!(snip.starts_with("..."), "{q}: {snip}");
        assert!(snip.contains(expected_sub), "{q}: {snip}");
        assert!(
            snip.chars().count() <= 300,
            "{q}: len={}",
            snip.chars().count()
        );
        // Tail match drops trailing "..." and uses the window all the way to the end.
        assert!(
            !snip.ends_with("...") && snip.ends_with("ở đoạn cuối"),
            "{q}: expected tail window without trailing ellipsis: {snip}"
        );
    }

    // Direct boundary checks on match_snippet: cap <= 6 with a real late match, NFD, em-dash, empty terms/refs.
    assert_eq!(match_snippet("abcde ghij", &["ghij".into()], &[], 0), "");
    assert_eq!(match_snippet("abcde ghij", &["ghij".into()], &[], 3), "abc");
    assert_eq!(
        match_snippet("abcde ghij", &["ghij".into()], &[], 5),
        "ab..."
    );
    assert_eq!(
        match_snippet("abcde ghij", &["ghij".into()], &[], 6),
        "abc..."
    );
    let em = format!("Tiêu đề — {} cuối cùng", "dữ liệu ".repeat(20));
    let em_snip = match_snippet(&em, &[], &[], 30);
    assert!(
        em_snip.starts_with("Tiêu đề — ") && em_snip.ends_with("..."),
        "{em_snip}"
    );
    assert!(em_snip.chars().count() <= 30);
}

// ---------------------------------------------------------------- 2. Hook tag formatting

#[test]
fn hook_prompt_lines_include_evidence_tag_for_both_harnesses() {
    let (d, home) = scratch();
    let claude = fake_claude(d.path(), "R5 failover");
    write(
        &home.join("config.toml"),
        &format!("[catch_up]\nenabled = false\n[prompt_hook]\nclaude_bin = \"{claude}\"\n"),
    );
    seed_item(
        &home,
        "m_exec",
        "global",
        "R5 failover passed smoke check",
        "R5 failover passed smoke check",
        "executed",
        "R5 src/a.py",
    );
    seed_item(
        &home,
        "m_plan",
        "global",
        "R5 failover rollout plan",
        "R5 failover rollout plan",
        "planned",
        "",
    );

    let payload = json!({"prompt": "check the R5 failover status", "cwd": "/nonexistent-sm-wave1"})
        .to_string();
    let (code, out, err) = hook_prompt(&home, &["hook", "prompt"], &payload);
    assert_eq!(code, 0, "{err}");
    let v: Value = serde_json::from_str(&out).unwrap();
    let ctx = v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(
        ctx.contains("[m_exec] global fact executed 2026-10-25 (refs: R5 src/a.py): R5 failover passed smoke check"),
        "{ctx}"
    );
    assert!(
        ctx.contains("[m_plan] global fact planned 2026-10-25: R5 failover rollout plan"),
        "{ctx}"
    );

    let (code, agy_out, err) = hook_prompt(&home, &["hook", "prompt", "--agy"], &payload);
    assert_eq!(code, 0, "{err}");
    let av: Value = serde_json::from_str(&agy_out).unwrap();
    let msg = av["injectSteps"][0]["ephemeralMessage"].as_str().unwrap();
    assert!(
        msg.contains("[m_exec] global fact executed 2026-10-25 (refs: R5 src/a.py): R5 failover passed smoke check"),
        "{msg}"
    );
}

// ---------------------------------------------------------------- 3. Additive [scopes] routing

#[test]
fn hook_haiku_path_routes_scopes_additively_without_dropping_cwd_repo_items() {
    let (d, home) = scratch();
    let cwd_repo = init_git_repo(d.path(), "anvil");
    // Prompt says "Globex" in plain text; fake Haiku rewrites it to "GLX deploy" so the code
    // appears in `query` (not `prompt`), proving both prompt and query are matched.
    let claude = fake_claude(d.path(), "GLX deploy");
    write(
        &home.join("config.toml"),
        &format!(
            "[catch_up]\nenabled = false\n\
             [prompt_hook]\nclaude_bin = \"{claude}\"\nlimit = 10\n\
             [prompt_hook.projects]\n\
             GLX = \"Globex client\"\n\
             [scopes]\n\
             \"acme/globex\" = [\"GLX\"]\n\
             \"acme/globex/web\" = [\"repo:globex-web\"]\n\
             initech = [\"INI\"]\n"
        ),
    );
    seed_item(
        &home,
        "m_cwd",
        "anvil",
        "anvil deploy checklist",
        "anvil deploy steps",
        "decided",
        "",
    );
    seed_item(
        &home,
        "m_web",
        "globex-web",
        "web deploy pipeline",
        "globex web deploy config",
        "verified",
        "",
    );
    seed_item(
        &home,
        "m_word",
        "misc",
        "GLX deploy note",
        "GLX deploy details",
        "executed",
        "",
    );
    seed_item(
        &home,
        "m_other",
        "unrelated",
        "unrelated deploy note",
        "unrelated deploy steps",
        "decided",
        "",
    );

    let payload = json!({
        "prompt": "how does the Globex deploy work here?",
        "cwd": cwd_repo.to_str().unwrap(),
    })
    .to_string();
    let (code, out, err) = hook_prompt(&home, &["hook", "prompt"], &payload);
    assert_eq!(code, 0, "{err}");
    let v: Value = serde_json::from_str(&out).unwrap();
    let ctx = v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    // Additive: cwd-repo item stays, cross-repo scope items (repo: + word matcher) join, unrelated stays out.
    assert!(ctx.contains("[m_cwd] anvil fact decided"), "{ctx}");
    assert!(ctx.contains("[m_web] globex-web fact verified"), "{ctx}");
    assert!(ctx.contains("[m_word] misc fact executed"), "{ctx}");
    assert!(!ctx.contains("[m_other]"), "{ctx}");

    let rows = hook_log_rows(&home);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["path"], json!("haiku"));
    assert_eq!(rows[0]["scopes"], json!(["acme/globex"]));
}

#[test]
fn hook_local_fallback_routes_scopes_additively_without_dropping_cwd_repo_items() {
    let (d, home) = scratch();
    let cwd_repo = init_git_repo(d.path(), "anvil");
    write(
        &home.join("config.toml"),
        "[catch_up]\nenabled = false\n\
         [prompt_hook]\nmodel = \"none\"\nlimit = 10\n\
         [prompt_hook.projects]\n\
         GLX = \"Globex client\"\n\
         [scopes]\n\
         \"acme/globex\" = [\"GLX\"]\n\
         \"acme/globex/web\" = [\"repo:globex-web\"]\n",
    );
    // 45 filler items + 3 in-scope matches => n_scope = 48, df = 3 for each of the 3 rare terms:
    // 3 * ln(48 / 3) = 3 * ln(16) = 8.32 >= LOCAL_TAU (8.0).
    for i in 0..45 {
        seed_item(
            &home,
            &format!("f_{i:02}"),
            "anvil",
            &format!("filler record {i}"),
            &format!("ordinary filler body {i}"),
            "derived",
            "",
        );
    }
    seed_item(
        &home,
        "m_cwd",
        "anvil",
        "walrus okapi quagga anvil note",
        "walrus okapi quagga anvil steps",
        "decided",
        "",
    );
    seed_item(
        &home,
        "m_web",
        "globex-web",
        "walrus okapi quagga web note",
        "walrus okapi quagga web steps",
        "verified",
        "",
    );
    seed_item(
        &home,
        "m_word",
        "misc",
        "GLX walrus okapi quagga note",
        "GLX walrus okapi quagga steps",
        "executed",
        "",
    );
    seed_item(
        &home,
        "m_other",
        "unrelated",
        "walrus okapi quagga unrelated",
        "walrus okapi quagga unrelated body",
        "decided",
        "",
    );

    let payload = json!({
        "prompt": "about GLX walrus okapi quagga",
        "cwd": cwd_repo.to_str().unwrap(),
    })
    .to_string();
    let (code, out, err) = hook_prompt(&home, &["hook", "prompt"], &payload);
    assert_eq!(code, 0, "{err}");
    let v: Value = serde_json::from_str(&out).unwrap();
    let ctx = v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(ctx.contains("[m_cwd] anvil fact decided"), "{ctx}");
    assert!(ctx.contains("[m_web] globex-web fact verified"), "{ctx}");
    assert!(ctx.contains("[m_word] misc fact executed"), "{ctx}");
    assert!(!ctx.contains("[m_other]"), "{ctx}");

    let rows = hook_log_rows(&home);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["path"], json!("local"));
    assert_eq!(rows[0]["scopes"], json!(["acme/globex"]));
}

#[test]
fn matching_scope_paths_skips_empty_keys_matches_leaf_case_insensitively_and_degrades_on_bad_scopes()
 {
    let (d, home) = scratch();
    let claude = fake_claude(d.path(), "GLX kiwi");
    // Valid scopes with an implied ancestor "acme/globex" and an empty-matcher scope "emptyscope".
    write(
        &home.join("config.toml"),
        "[scopes]\n\
         \"acme/globex/web\" = [\"repo:globex-web\", \"WEB\"]\n\
         initech = [\"INI\"]\n\
         emptyscope = []\n\
         whitespace = [\"   \"]\n",
    );
    let cfg = load_config(home.to_str()).unwrap();
    let s = |v: &str| PyValue::Str(v.to_string());
    let projects = PyValue::Dict(vec![
        (s(""), s("empty key must be skipped")),
        (s("   "), s("blank key must be skipped")),
        (s("GLOBEX"), s("Globex ancestor segment")),
        (s("INI"), s("Initech matcher")),
        (s("emptyscope"), s("Empty matcher scope")),
    ]);
    // Prompt with no project code matches nothing despite empty/blank keys in [prompt_hook.projects]
    // (five spaces contain a 3-space run flanked by non-word chars, so stands_as_word would match "   ").
    assert_eq!(
        matching_scope_paths(&cfg, "plain     unrelated prompt", "plain query", &projects),
        Vec::<String>::new()
    );
    // Case-insensitive leaf segment match hits implied ancestor "acme/globex"; matcher hits "initech".
    assert_eq!(
        matching_scope_paths(&cfg, "check GLOBEX status", "INI update", &projects),
        vec!["acme/globex".to_string(), "initech".to_string()]
    );

    // Invalid [scopes] degrades gracefully in matching_scope_paths AND in `sm hook prompt` even
    // when a configured project code DOES match the prompt/query.
    write(
        &home.join("config.toml"),
        &format!(
            "[catch_up]\nenabled = false\n\
             [prompt_hook]\nclaude_bin = \"{claude}\"\n\
             [prompt_hook.projects]\n\
             \"\" = \"empty\"\n\
             GLX = \"Globex\"\n\
             [scopes]\n\
             \"acme/glo bex\" = [\"GLX\"]\n"
        ),
    );
    let bad_cfg = load_config(home.to_str()).unwrap();
    let glx_projects = PyValue::Dict(vec![(s(""), s("empty")), (s("GLX"), s("Globex"))]);
    assert_eq!(
        matching_scope_paths(&bad_cfg, "check GLX kiwi", "GLX kiwi", &glx_projects),
        Vec::<String>::new()
    );

    seed_item(
        &home,
        "m_g1",
        "global",
        "GLX kiwi global",
        "GLX kiwi global",
        "decided",
        "",
    );
    let payload =
        json!({"prompt": "check GLX kiwi now", "cwd": "/nonexistent-sm-wave1"}).to_string();
    let (code, out, err) = hook_prompt(&home, &["hook", "prompt"], &payload);
    assert_eq!((code, err.as_str()), (0, ""));
    assert!(out.contains("[m_g1] global fact decided"), "{out}");
    let rows = hook_log_rows(&home);
    assert_eq!(rows[0].get("scopes"), None);

    // Local fallback with invalid [scopes] and matching project code also degrades gracefully.
    write(
        &home.join("config.toml"),
        "[catch_up]\nenabled = false\n\
         [prompt_hook]\nmodel = \"none\"\n\
         [prompt_hook.projects]\n\
         GLX = \"Globex\"\n\
         [scopes]\n\
         \"acme/glo bex\" = [\"GLX\"]\n",
    );
    let (code, _local_out, err) = hook_prompt(&home, &["hook", "prompt"], &payload);
    assert_eq!((code, err.as_str()), (0, ""));
    let rows_after = hook_log_rows(&home);
    assert_eq!(rows_after.last().unwrap()["path"], json!("local"));
    assert_eq!(rows_after.last().unwrap().get("scopes"), None);

    // Empty-matcher scope matched in hook does not produce invalid SQL (`OR ()`).
    write(
        &home.join("config.toml"),
        &format!(
            "[catch_up]\nenabled = false\n\
             [prompt_hook]\nclaude_bin = \"{claude}\"\n\
             [prompt_hook.projects]\n\
             emptyscope = \"Empty\"\n\
             [scopes]\n\
             emptyscope = []\n"
        ),
    );
    let payload_empty =
        json!({"prompt": "check emptyscope kiwi now", "cwd": "/nonexistent-sm-wave1"}).to_string();
    let (code, out, err) = hook_prompt(&home, &["hook", "prompt"], &payload_empty);
    assert_eq!((code, err.as_str()), (0, ""));
    assert!(out.contains("[m_g1] global fact decided"), "{out}");
}

#[test]
fn scope_matchers_dedupes_sql_orders_repo_before_sm_word_and_unknown_extra_scope_fails_loud() {
    let (_d, home) = scratch();
    write(
        &home.join("config.toml"),
        "[scopes]\n\
         acme = [\"GLX\", \"repo:ops\"]\n\
         \"acme/globex\" = [\"GLX\", \"repo:web\", \"GLX\"]\n",
    );
    let cfg = load_config(home.to_str()).unwrap();
    let sc = scopes(&cfg).unwrap();
    let matchers = scope_matchers(&sc, &["acme", "acme/globex"]).unwrap();
    assert_eq!(matchers, vec!["GLX", "repo:ops", "repo:web"]);

    // `repo = ?` clauses must appear BEFORE `sm_word(...)` clauses in SQL and params.
    let f = Filters {
        repo: Some("anvil".into()),
        extra_scope: Some(matchers),
        ..Default::default()
    };
    let (sql, params) = build_item_filters("i", &f);
    assert_eq!(
        sql,
        "i.status = 'active' AND (i.repo IN (?, 'global', 'unassigned') OR \
         (i.repo = ? OR i.repo = ? OR sm_word(?, i.title, i.body, i.refs, i.tags)))"
    );
    assert_eq!(
        params,
        vec![
            Sql::Text("anvil".into()),
            Sql::Text("ops".into()),
            Sql::Text("web".into()),
            Sql::Text("GLX".into()),
        ]
    );

    // Direct run_query with an unknown extra_scopes path fails loud with UnknownScope.
    let err = run_query(
        "kiwi",
        &QueryArgs {
            repo: "anvil",
            extra_scopes: StrList::Str("acme/missing"),
            home: home.to_str(),
            ..Default::default()
        },
        None,
    )
    .unwrap_err();
    assert_eq!(err.class, "UnknownScope");
    assert_eq!(
        err.msg,
        "unknown scope: acme/missing; known: acme, acme/globex"
    );

    // When `all_repos` is true or explicit `scopes` (`--in`) is set, `extra_scopes` is ignored.
    assert!(
        run_query(
            "kiwi",
            &QueryArgs {
                all_repos: true,
                extra_scopes: StrList::Str("acme/missing"),
                home: home.to_str(),
                ..Default::default()
            },
            None,
        )
        .unwrap()
        .is_empty()
    );
    assert!(
        run_query(
            "kiwi",
            &QueryArgs {
                scopes: StrList::Str("acme"),
                extra_scopes: StrList::Str("acme/missing"),
                home: home.to_str(),
                ..Default::default()
            },
            None,
        )
        .unwrap()
        .is_empty()
    );
}
