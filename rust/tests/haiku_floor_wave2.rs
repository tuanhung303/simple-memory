//! Integration and boundary tests for Wave 2 Lane B:
//! Haiku Recall Abstention Floor (`[prompt_hook] min_idf`).
//!
//! Every fixture uses made-up names (`anvil`, `acme`, `globex`, `globex-web`, `GLX`, `TICKET-42`)
//! and runs against an isolated scratch `SIMPLE_MEMORY_HOME`.

mod common;

use std::cell::Cell;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use common::{git, open, scratch, write};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use simple_memory::clock;
use simple_memory::config::{config_toml, defaults, load_config};
use simple_memory::core::item::Item;
use simple_memory::error::SmError;
use simple_memory::py::path::PyPath;
use simple_memory::py::value::PyValue;
use simple_memory::service::capture::Completed;
use simple_memory::service::recall::{
    Backoff, DEFAULT_HAIKU_MIN_IDF, HAIKU_FLOOR_MIN_SCOPE, Recall, SessionState, haiku_min_idf,
    run_prompt_hook,
};
use simple_memory::store::items::put_item;

const TS: &str = "2026-10-25T10:00:00+00:00";

fn s(v: &str) -> PyValue {
    PyValue::Str(v.to_string())
}

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

fn fake_claude(dir: &Path, reply: &str, marker: Option<&Path>) -> String {
    let path = dir.join("fake-claude.sh");
    let touch_line = marker.map_or_else(String::new, |m| format!("touch '{}'\n", m.display()));
    write(
        &path,
        &format!("#!/bin/sh\ncat >/dev/null\n{touch_line}printf '%s\\n' '{reply}'\n"),
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

fn seed_item(home: &Path, id: &str, repo: &str, title: &str, body: &str, refs: &str) {
    let conn = open(home);
    let mut it = Item::bare(id, repo, "fact", title, body, "decided");
    it.refs = refs.to_string();
    it.evidence = "made-up check".to_string();
    it.as_of = TS.to_string();
    put_item(&conn, &it.post_init(), None).unwrap();
}

fn seed_fillers(home: &Path, repo: &str, prefix: &str, count: usize) {
    for i in 0..count {
        seed_item(
            home,
            &format!("{prefix}_{i:02}"),
            repo,
            &format!("neutral entry {prefix} {i}"),
            &format!("background text {prefix} {i}"),
            "",
        );
    }
}

fn write_cfg(home: &Path, extra: &str) {
    write(
        &home.join("config.toml"),
        &format!("[catch_up]\nenabled = false\n{extra}"),
    );
}

fn payload(prompt: &str, cwd: &Path, session: &str) -> PyValue {
    let mut pairs = vec![
        (s("prompt"), s(prompt)),
        (s("cwd"), s(cwd.to_str().unwrap())),
    ];
    if !session.is_empty() {
        pairs.push((s("session_id"), s(session)));
        pairs.push((s("transcript_path"), s("")));
    }
    PyValue::Dict(pairs)
}

fn hook_log_rows(home: &Path) -> Vec<Value> {
    std::fs::read_to_string(home.join("hook.log"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn session_state_path(home: &Path, session_id: &str) -> PyPath {
    let digest = Sha256::digest(session_id.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    PyPath::new(home.to_str().unwrap())
        .join("hook")
        .join(&format!("claude-{}.json", &hex[..16]))
}

fn run_ok(
    home: &Path,
    prompt: &str,
    cwd: &Path,
    session: &str,
    reply: &str,
) -> Result<Option<Recall>, SmError> {
    let run = |_cmd: &[String],
               _input: &str,
               _timeout: f64,
               _env: &[(&str, &str)],
               _cwd: Option<&str>| {
        Ok(Completed {
            returncode: 0,
            stdout: format!("{reply}\n"),
            stderr: String::new(),
        })
    };
    run_prompt_hook(
        &payload(prompt, cwd, session),
        false,
        Some(home.to_str().unwrap()),
        &run,
    )
}

// ---------------------------------------------------------------- 1. Config defaults & validation

#[test]
fn config_defaults_include_min_idf_and_commented_note_round_trips() {
    assert_eq!(DEFAULT_HAIKU_MIN_IDF, 3.0);
    assert_eq!(HAIKU_FLOOR_MIN_SCOPE, 20);

    let (_d, home) = scratch();
    let cfg = load_config(Some(home.to_str().unwrap())).unwrap();
    let PyValue::Dict(sections) = &cfg else {
        panic!("expected config dict");
    };
    let hook = sections
        .iter()
        .find(|(k, _)| matches!(k, PyValue::Str(name) if name == "prompt_hook"))
        .map(|(_, v)| v)
        .expect("prompt_hook section");
    assert_eq!(haiku_min_idf(hook).unwrap(), 3.0);
    assert_eq!(haiku_min_idf(&PyValue::Dict(Vec::new())).unwrap(), 3.0);
    assert_eq!(
        haiku_min_idf(&PyValue::Dict(vec![(s("min_idf"), PyValue::None)])).unwrap(),
        3.0
    );

    let commented = config_toml(&defaults(), true).unwrap();
    assert!(
        commented.contains(
            "# Minimum matched-term IDF sum for Haiku hook recall when scope has >= 20 items (0.0 disables; exact ref hits bypass).\n# min_idf = 3.0\n"
        ),
        "{commented}"
    );
}

#[test]
fn invalid_min_idf_values_fail_before_claude_and_keep_local_fallback_working() {
    let (d, home) = scratch();
    let repo = init_git_repo(d.path(), "anvil");
    seed_fillers(&home, "anvil", "f", 20);
    seed_item(
        &home,
        "m_local",
        "anvil",
        "cobalt nickel tungsten",
        "cobalt nickel tungsten body",
        "",
    );

    let bad_values = [
        ("true", "[prompt_hook] min_idf must be a finite number >= 0"),
        (
            "false",
            "[prompt_hook] min_idf must be a finite number >= 0",
        ),
        ("-0.1", "[prompt_hook] min_idf must be a finite number >= 0"),
        ("-1", "[prompt_hook] min_idf must be a finite number >= 0"),
        ("nan", "[prompt_hook] min_idf must be a finite number >= 0"),
        ("inf", "[prompt_hook] min_idf must be a finite number >= 0"),
        ("-inf", "[prompt_hook] min_idf must be a finite number >= 0"),
        ("\"bad\"", "could not convert string to float"),
        ("[]", "float() argument must be a string or a real number"),
    ];

    for (raw, expected_msg) in bad_values {
        write_cfg(&home, &format!("[prompt_hook]\nmin_idf = {raw}\n"));
        let calls = Cell::new(0usize);
        let run = |_c: &[String], _i: &str, _t: f64, _e: &[(&str, &str)], _w: Option<&str>| {
            calls.set(calls.get() + 1);
            Ok(Completed {
                returncode: 0,
                stdout: "cobalt\n".into(),
                stderr: String::new(),
            })
        };
        let err = run_prompt_hook(
            &payload("check cobalt nickel tungsten now", &repo, ""),
            false,
            Some(home.to_str().unwrap()),
            &run,
        )
        .unwrap_err();
        assert_eq!(
            calls.get(),
            0,
            "claude runner must not be called for min_idf = {raw}"
        );
        assert!(
            err.msg.contains(expected_msg),
            "min_idf = {raw}: got {:?}",
            err.msg
        );
    }

    // CLI path: fake claude binary writes a marker if ever spawned; invalid min_idf fails first.
    let marker = d.path().join("claude-was-called");
    let claude = fake_claude(d.path(), "cobalt", Some(&marker));
    for raw in ["true", "-0.1"] {
        write_cfg(
            &home,
            &format!("[prompt_hook]\nclaude_bin = \"{claude}\"\nmin_idf = {raw}\n"),
        );
        let stdin = json!({
            "prompt": "check cobalt nickel tungsten now",
            "cwd": repo.to_str().unwrap(),
        })
        .to_string();
        let (code, stdout, stderr) = hook_prompt(&home, &["hook", "prompt"], &stdin);
        assert_eq!(code, 0);
        assert_eq!(stdout, "");
        assert!(
            stderr.contains("sm hook: [prompt_hook] min_idf must be a finite number >= 0"),
            "{raw}: {stderr}"
        );
        assert!(!marker.exists(), "claude binary was spawned for {raw}");
    }

    // model = "none" must NOT be broken by invalid min_idf (local fallback path).
    write_cfg(&home, "[prompt_hook]\nmodel = \"none\"\nmin_idf = -1.0\n");
    let calls = Cell::new(0usize);
    let run = |_c: &[String], _i: &str, _t: f64, _e: &[(&str, &str)], _w: Option<&str>| {
        calls.set(calls.get() + 1);
        unreachable!("model=none never calls runner");
    };
    let local_res = run_prompt_hook(
        &payload("check cobalt nickel tungsten now", &repo, ""),
        false,
        Some(home.to_str().unwrap()),
        &run,
    )
    .unwrap()
    .expect("local fallback recall");
    assert_eq!(calls.get(), 0);
    assert_eq!(local_res.fallback, "off");
    assert_eq!(
        local_res
            .items
            .iter()
            .map(|it| it.id.as_str())
            .collect::<Vec<_>>(),
        vec!["m_local"]
    );

    // Active backoff must NOT be broken by invalid min_idf either.
    write_cfg(&home, "[prompt_hook]\nmin_idf = true\n");
    let bo_path = PyPath::new(home.to_str().unwrap())
        .join("hook")
        .join("backoff.json");
    Backoff {
        failures: 2,
        until: clock::now_epoch() + 600.0,
        error: "simulated".into(),
        cause: "error".into(),
        notified: Vec::new(),
        ..Default::default()
    }
    .write(&bo_path)
    .unwrap();
    let backoff_res = run_prompt_hook(
        &payload("check cobalt nickel tungsten now", &repo, "s-bo"),
        false,
        Some(home.to_str().unwrap()),
        &run,
    )
    .unwrap()
    .expect("backoff fallback recall");
    assert_eq!(calls.get(), 0);
    assert_eq!(backoff_res.fallback, "backoff");
    assert_eq!(
        backoff_res
            .items
            .iter()
            .map(|it| it.id.as_str())
            .collect::<Vec<_>>(),
        vec!["m_local"]
    );
}

// ---------------------------------------------------------------- 2. Default floor, logging, session state & exact >= boundary

#[test]
fn default_floor_drops_low_idf_keeps_high_idf_logs_and_excludes_from_session() {
    let (d, home) = scratch();
    let repo = init_git_repo(d.path(), "anvil");
    // 25 active items in "anvil":
    // - 10 generic items: df("generic") = 10 -> ln(25/10) = 0.9163 < 3.0
    // - 1 rare item: df("quartz") = 1 -> ln(25/1) = 3.2189 >= 3.0
    // - 14 neutral fillers
    for i in 0..10 {
        seed_item(
            &home,
            &format!("m_gen_{i:02}"),
            "anvil",
            &format!("generic topic {i}"),
            &format!("generic body unique{i}"),
            "",
        );
    }
    seed_item(
        &home,
        "m_rare",
        "anvil",
        "quartz resonator note",
        "quartz resonator spec",
        "",
    );
    seed_fillers(&home, "anvil", "f", 14);
    write_cfg(&home, "[prompt_hook]\nlimit = 4\n");

    // Case A: all 4 generic hits fall below default min_idf = 3.0 -> nothing injected,
    // floor_dropped logged, and session.injected stays empty.
    let res_all_dropped = run_ok(
        &home,
        "tell me about generic stuff",
        &repo,
        "sess-floor",
        "generic",
    )
    .unwrap()
    .expect("haiku path returns Recall with empty items");
    assert!(res_all_dropped.items.is_empty());
    let log = hook_log_rows(&home);
    assert_eq!(log.len(), 1);
    assert_eq!(log[0]["path"], "haiku");
    assert_eq!(log[0]["ids"], json!([]));
    assert_eq!(log[0]["dropped"], json!([]));
    let fd_a: Vec<String> = log[0]["floor_dropped"]
        .as_array()
        .expect("floor_dropped array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(fd_a.len(), 4);
    assert!(fd_a.iter().all(|id| id.starts_with("m_gen_")));
    let st = SessionState::read(&session_state_path(&home, "sess-floor"))
        .unwrap()
        .expect("session state written");
    assert!(
        st.injected.is_empty(),
        "floor_dropped IDs must never enter session.injected: {:?}",
        st.injected
    );

    // Follow-up in the SAME session targeting one of the previously floor-dropped items with
    // a rare term (`unique0`, df=1 -> ln(25)=3.2189 >= 3.0) MUST still inject it.
    let first_dropped_idx = fd_a[0]
        .strip_prefix("m_gen_")
        .unwrap()
        .parse::<usize>()
        .unwrap();
    let res_later = run_ok(
        &home,
        "now check that exact unique entry",
        &repo,
        "sess-floor",
        &format!("unique{first_dropped_idx}"),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        res_later
            .items
            .iter()
            .map(|it| it.id.as_str())
            .collect::<Vec<_>>(),
        vec![fd_a[0].as_str()]
    );
    let log = hook_log_rows(&home);
    assert_eq!(log.len(), 2);
    assert_eq!(log[1]["ids"], json!([fd_a[0]]));
    assert!(
        log[1].get("floor_dropped").is_none(),
        "floor_dropped must be omitted when empty: {}",
        log[1]
    );

    // Case B: mixed query ("quartz generic") keeps m_rare and floor-drops the 3 generic hits.
    let res_mixed = run_ok(
        &home,
        "check quartz and generic items",
        &repo,
        "sess-mixed",
        "quartz generic",
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        res_mixed
            .items
            .iter()
            .map(|it| it.id.as_str())
            .collect::<Vec<_>>(),
        vec!["m_rare"]
    );
    let log = hook_log_rows(&home);
    let mixed_row = log.last().unwrap();
    assert_eq!(mixed_row["ids"], json!(["m_rare"]));
    let mixed_fd = mixed_row["floor_dropped"].as_array().unwrap();
    assert_eq!(mixed_fd.len(), 3);
    assert!(
        mixed_fd
            .iter()
            .all(|v| v.as_str().unwrap().starts_with("m_gen_"))
    );
    let st_mixed = SessionState::read(&session_state_path(&home, "sess-mixed"))
        .unwrap()
        .unwrap();
    assert_eq!(st_mixed.injected, vec!["m_rare"]);

    // Exact >= boundary: df("generic") = 10 in n_scope = 25 -> ln(25 / 10) = ln(2.5) = 0.9162907318741551.
    write_cfg(
        &home,
        "[prompt_hook]\nlimit = 4\nmin_idf = 0.9162907318741551\n",
    );
    let at_eq = run_ok(&home, "exact boundary check generic", &repo, "", "generic")
        .unwrap()
        .unwrap();
    assert_eq!(at_eq.items.len(), 4);

    write_cfg(
        &home,
        "[prompt_hook]\nlimit = 4\nmin_idf = 0.9162907318741552\n",
    );
    let above_eq = run_ok(
        &home,
        "just above boundary check generic",
        &repo,
        "",
        "generic",
    )
    .unwrap()
    .unwrap();
    assert!(above_eq.items.is_empty());

    // CLI stdout stays empty when every hit is floor-dropped.
    let claude = fake_claude(d.path(), "generic", None);
    write_cfg(
        &home,
        &format!("[prompt_hook]\nclaude_bin = \"{claude}\"\nlimit = 4\n"),
    );
    let stdin = json!({
        "prompt": "tell me about generic stuff",
        "cwd": repo.to_str().unwrap(),
    })
    .to_string();
    let (code, stdout, stderr) = hook_prompt(&home, &["hook", "prompt"], &stdin);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout, "");
    assert_eq!(stderr, "");
}

// ---------------------------------------------------------------- 3. n_scope 19 vs 20 boundary

#[test]
fn n_scope_19_vs_20_boundary_skips_then_enforces_floor() {
    let (d, home) = scratch();
    let repo = init_git_repo(d.path(), "anvil");
    // 19 active items: 5 common + 1 single-term rare + 13 fillers.
    for i in 0..5 {
        seed_item(
            &home,
            &format!("m_com_{i}"),
            "anvil",
            &format!("common topic {i}"),
            &format!("common body {i}"),
            "",
        );
    }
    seed_item(
        &home,
        "m_solo",
        "anvil",
        "obsidian note",
        "obsidian detail",
        "",
    );
    seed_fillers(&home, "anvil", "f", 13);

    // Add 1 superseded item (20 total DB rows, but only 19 active in scope).
    let conn = open(&home);
    let mut old = Item::bare(
        "m_old",
        "anvil",
        "fact",
        "old filler",
        "old filler body",
        "decided",
    );
    old.status = "superseded".into();
    old.as_of = TS.into();
    put_item(&conn, &old.post_init(), None).unwrap();
    drop(conn);

    write_cfg(&home, "[prompt_hook]\nlimit = 4\n");

    // n_scope = 19 < 20: floor skipped even though ln(19/5) = 1.335 < 3.0.
    let res19 = run_ok(&home, "ask about common notes", &repo, "", "common")
        .unwrap()
        .unwrap();
    assert_eq!(res19.items.len(), 4);
    assert!(
        hook_log_rows(&home)
            .last()
            .unwrap()
            .get("floor_dropped")
            .is_none()
    );

    // Add 1 active filler -> n_scope = 20 == HAIKU_FLOOR_MIN_SCOPE: floor activates!
    seed_item(
        &home,
        "f_20",
        "anvil",
        "extra filler 20",
        "extra body 20",
        "",
    );

    // common: ln(20/5) = 1.386 < 3.0 -> dropped.
    let res20_common = run_ok(&home, "ask about common notes", &repo, "", "common")
        .unwrap()
        .unwrap();
    assert!(res20_common.items.is_empty());
    assert_eq!(
        hook_log_rows(&home).last().unwrap()["floor_dropped"]
            .as_array()
            .unwrap()
            .len(),
        4
    );

    // Single unique term at n_scope = 20: ln(20/1) = 2.9957 < 3.0 -> dropped!
    let res20_solo = run_ok(&home, "ask about obsidian note", &repo, "", "obsidian")
        .unwrap()
        .unwrap();
    assert!(res20_solo.items.is_empty());
    assert_eq!(
        hook_log_rows(&home).last().unwrap()["floor_dropped"],
        json!(["m_solo"])
    );

    // Add 1 more active filler -> n_scope = 21: ln(21/1) = 3.0445 >= 3.0 -> kept!
    seed_item(
        &home,
        "f_21",
        "anvil",
        "extra filler 21",
        "extra body 21",
        "",
    );
    let res21_solo = run_ok(&home, "ask about obsidian note", &repo, "", "obsidian")
        .unwrap()
        .unwrap();
    assert_eq!(
        res21_solo
            .items
            .iter()
            .map(|it| it.id.as_str())
            .collect::<Vec<_>>(),
        vec!["m_solo"]
    );
    assert!(
        hook_log_rows(&home)
            .last()
            .unwrap()
            .get("floor_dropped")
            .is_none()
    );
}

// ---------------------------------------------------------------- 4. min_idf = 0.0 fast path

#[test]
fn min_idf_zero_disables_floor_via_fast_path() {
    let (d, home) = scratch();
    let repo = init_git_repo(d.path(), "anvil");
    for i in 0..10 {
        seed_item(
            &home,
            &format!("m_gen_{i:02}"),
            "anvil",
            &format!("generic entry {i}"),
            &format!("generic body {i}"),
            "",
        );
    }
    seed_fillers(&home, "anvil", "f", 15);

    // Both float 0.0 and integer 0 disable the floor cleanly.
    for cfg_val in ["0.0", "0"] {
        write_cfg(
            &home,
            &format!("[prompt_hook]\nlimit = 4\nmin_idf = {cfg_val}\n"),
        );
        let res = run_ok(&home, "tell me about generic entries", &repo, "", "generic")
            .unwrap()
            .unwrap();
        assert_eq!(res.items.len(), 4, "min_idf = {cfg_val}");
        let row = hook_log_rows(&home).pop().unwrap();
        assert_eq!(row["path"], "haiku");
        assert!(row.get("floor_dropped").is_none(), "{row}");
    }

    // CLI output delivers all 4 recall lines when min_idf = 0.0.
    let claude = fake_claude(d.path(), "generic", None);
    write_cfg(
        &home,
        &format!("[prompt_hook]\nclaude_bin = \"{claude}\"\nlimit = 4\nmin_idf = 0.0\n"),
    );
    let stdin = json!({
        "prompt": "tell me about generic entries",
        "cwd": repo.to_str().unwrap(),
    })
    .to_string();
    let (code, stdout, stderr) = hook_prompt(&home, &["hook", "prompt"], &stdin);
    assert_eq!(code, 0, "{stderr}");
    let v: Value = serde_json::from_str(&stdout).unwrap();
    let ctx = v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert_eq!(ctx.lines().skip(1).count(), 4, "{ctx}");
}

// ---------------------------------------------------------------- 5. Exact ref_hits bypass

#[test]
fn exact_ref_hits_bypass_floor_while_non_ref_low_idf_items_drop() {
    let (d, home) = scratch();
    let repo = init_git_repo(d.path(), "anvil");
    // 25 active items:
    // - 15 items mention "ticket", "42", and "common" in body (no refs) ->
    //   df("ticket") >= 15, df("42") >= 15, df("common") >= 15; each idf <= ln(25/15) = 0.5108,
    //   sum <= 1.532 < 3.0.
    // - m_ref_1 has refs="TICKET-42" and low-IDF text -> bypasses via exact ref_hit!
    // - m_ref_2 has refs="TICKET-42" and unrelated text -> bypasses via exact ref_hit!
    for i in 0..15 {
        seed_item(
            &home,
            &format!("m_body_{i:02}"),
            "anvil",
            &format!("common ticket 42 note {i}"),
            &format!("discussing ticket 42 and common issue {i}"),
            "",
        );
    }
    seed_item(
        &home,
        "m_ref_1",
        "anvil",
        "common note with ref",
        "common details",
        "TICKET-42",
    );
    seed_item(
        &home,
        "m_ref_2",
        "anvil",
        "unrelated archive note",
        "plain archive text",
        "TICKET-42",
    );
    seed_fillers(&home, "anvil", "f", 8);
    write_cfg(&home, "[prompt_hook]\nlimit = 4\n");

    let res = run_ok(
        &home,
        "status of TICKET-42 common issue",
        &repo,
        "",
        "TICKET-42 common",
    )
    .unwrap()
    .unwrap();
    let kept_ids: Vec<&str> = res.items.iter().map(|it| it.id.as_str()).collect();
    assert_eq!(kept_ids.len(), 2);
    assert!(kept_ids.contains(&"m_ref_1"), "{kept_ids:?}");
    assert!(kept_ids.contains(&"m_ref_2"), "{kept_ids:?}");

    let row = hook_log_rows(&home).pop().unwrap();
    let dropped_ids: Vec<&str> = row["floor_dropped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(dropped_ids.len(), 2);
    assert!(dropped_ids.iter().all(|id| id.starts_with("m_body_")));

    // Single-word query matching an exact ref bypasses floor even when the word is generic in body.
    seed_item(
        &home,
        "m_single_ref",
        "anvil",
        "single ref item",
        "plain body",
        "common",
    );
    let res_single = run_ok(&home, "check single ref common", &repo, "", "common")
        .unwrap()
        .unwrap();
    assert_eq!(
        res_single
            .items
            .iter()
            .map(|it| it.id.as_str())
            .collect::<Vec<_>>(),
        vec!["m_single_ref"]
    );
}

// ---------------------------------------------------------------- 6. keyword_aliases expansion

#[test]
fn keyword_aliases_expansion_computes_idf_for_alias_matched_items() {
    let (d, home) = scratch();
    let repo = init_git_repo(d.path(), "anvil");
    // 25 active items:
    // - m_k8s mentions only "kubernetes" (df=1 -> ln(25/1) = 3.2189 >= 3.0), never "k8s" (df=0).
    // - 4 items mention only "sqlite" (df=5 with m_both -> ln(25/5) = 1.6094 < 3.0).
    // - 4 items mention only "walmode" (df=5 with m_both -> ln(25/5) = 1.6094 < 3.0).
    // - m_both mentions both "sqlite" and "walmode" -> sum = 3.2189 >= 3.0.
    // - 15 neutral fillers.
    seed_item(
        &home,
        "m_k8s",
        "anvil",
        "kubernetes pod eviction",
        "kubernetes cluster sizing",
        "",
    );
    for i in 0..4 {
        seed_item(
            &home,
            &format!("m_sql_{i}"),
            "anvil",
            &format!("sqlite note {i}"),
            &format!("sqlite storage {i}"),
            "",
        );
        seed_item(
            &home,
            &format!("m_wal_{i}"),
            "anvil",
            &format!("walmode note {i}"),
            &format!("walmode tuning {i}"),
            "",
        );
    }
    seed_item(
        &home,
        "m_both",
        "anvil",
        "sqlite walmode journal",
        "sqlite uses walmode for concurrency",
        "",
    );
    seed_fillers(&home, "anvil", "f", 15);
    write_cfg(
        &home,
        "[prompt_hook]\nlimit = 4\n[keyword_aliases]\n\"k8s\" = \"kubernetes\"\n\"db\" = [\"sqlite\", \"walmode\"]\n",
    );

    // String alias: query "k8s" expands to "kubernetes" and clears min_idf = 3.0.
    let res_k8s = run_ok(&home, "check k8s pod eviction", &repo, "", "k8s")
        .unwrap()
        .unwrap();
    assert_eq!(
        res_k8s
            .items
            .iter()
            .map(|it| it.id.as_str())
            .collect::<Vec<_>>(),
        vec!["m_k8s"]
    );
    assert!(
        hook_log_rows(&home)
            .last()
            .unwrap()
            .get("floor_dropped")
            .is_none()
    );

    // List alias: query "db" expands to ["sqlite", "walmode"]; m_both clears (3.2189), singles drop (1.6094).
    let res_db = run_ok(&home, "check db locking behavior", &repo, "", "db")
        .unwrap()
        .unwrap();
    assert_eq!(
        res_db
            .items
            .iter()
            .map(|it| it.id.as_str())
            .collect::<Vec<_>>(),
        vec!["m_both"]
    );
    let db_row = hook_log_rows(&home).pop().unwrap();
    assert_eq!(db_row["floor_dropped"].as_array().unwrap().len(), 3);
}

// ---------------------------------------------------------------- 7. prefix_overlaps dedup

#[test]
fn prefix_overlaps_dedup_rarest_first_prevents_double_counting_and_keeps_specific_stem() {
    // Subcase A: prevent double-counting ("deploy deployment"):
    // n_scope = 25: 3 items hold "deployment", 4 more hold "deploy" ->
    // df("deployment") = 3 -> ln(25/3) = 2.1203 (< 3.0)
    // df("deploy")     = 7 -> ln(25/7) = 1.2730 (< 3.0)
    // Without dedup: 2.1203 + 1.2730 = 3.3933 >= 3.0 (falsely passes).
    // With dedup: only "deployment" (2.1203) counts -> dropped!
    let (d1, home1) = scratch();
    let repo1 = init_git_repo(d1.path(), "anvil");
    for i in 0..3 {
        seed_item(
            &home1,
            &format!("m_dep_long_{i}"),
            "anvil",
            &format!("deployment checklist {i}"),
            &format!("deployment rollout {i}"),
            "",
        );
    }
    for i in 0..4 {
        seed_item(
            &home1,
            &format!("m_dep_short_{i}"),
            "anvil",
            &format!("deploy script {i}"),
            &format!("deploy step {i}"),
            "",
        );
    }
    seed_fillers(&home1, "anvil", "f", 18);
    write_cfg(&home1, "[prompt_hook]\nlimit = 4\n");

    let res_dup = run_ok(
        &home1,
        "check deploy deployment status",
        &repo1,
        "",
        "deploy deployment",
    )
    .unwrap()
    .unwrap();
    assert!(
        res_dup.items.is_empty(),
        "prefix-overlapping terms must not double-count IDF: {:?}",
        res_dup
            .items
            .iter()
            .map(|it| it.id.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        hook_log_rows(&home1).last().unwrap()["floor_dropped"]
            .as_array()
            .unwrap()
            .len(),
        4
    );

    // Subcase B: rarest-df-first keeps the specific stem even when the generic prefix appears first.
    // n_scope = 25: 1 item holds "deployment" (df=1 -> ln(25)=3.2189 >= 3.0),
    // 9 others hold "deploy" (df("deploy")=10 -> ln(2.5)=0.9163 < 3.0).
    // Also verify:
    // - df=0 longer term ("deployzz") never shadows df=1 "deploy".
    // - non-prefix-overlapping digit tokens ("port80 port8080") both count.
    let (d2, home2) = scratch();
    let repo2 = init_git_repo(d2.path(), "anvil");
    seed_item(
        &home2,
        "m_spec",
        "anvil",
        "deployment canary gate",
        "deployment canary rollout",
        "",
    );
    for i in 0..9 {
        seed_item(
            &home2,
            &format!("m_gen_dep_{i}"),
            "anvil",
            &format!("deploy note {i}"),
            &format!("deploy detail {i}"),
            "",
        );
    }
    // 4 items with port80 (including m_ports), 4 items with port8080 (including m_ports):
    // ln(25/4) = 1.8326 each; together 3.665 >= 3.0.
    seed_item(
        &home2,
        "m_ports",
        "anvil",
        "port80 to port8080 proxy",
        "forward port80 into port8080",
        "",
    );
    for i in 0..3 {
        seed_item(
            &home2,
            &format!("m_p80_{i}"),
            "anvil",
            &format!("port80 listener {i}"),
            &format!("port80 detail {i}"),
            "",
        );
        seed_item(
            &home2,
            &format!("m_p8080_{i}"),
            "anvil",
            &format!("port8080 listener {i}"),
            &format!("port8080 detail {i}"),
            "",
        );
    }
    seed_item(
        &home2,
        "m_cobalt",
        "anvil",
        "cobalt furnace check",
        "cobalt furnace spec",
        "",
    );
    seed_fillers(&home2, "anvil", "f", 7);
    write_cfg(&home2, "[prompt_hook]\nlimit = 4\n");

    let res_spec = run_ok(
        &home2,
        "check deploy deployment canary",
        &repo2,
        "",
        "deploy deployment",
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        res_spec
            .items
            .iter()
            .map(|it| it.id.as_str())
            .collect::<Vec<_>>(),
        vec!["m_spec"]
    );

    // Non-overlapping digit tokens "port80" and "port8080" both contribute IDF.
    let res_ports = run_ok(
        &home2,
        "check port80 port8080 proxy",
        &repo2,
        "",
        "port80 port8080",
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        res_ports
            .items
            .iter()
            .map(|it| it.id.as_str())
            .collect::<Vec<_>>(),
        vec!["m_ports"]
    );

    // Zero-df longer term "cobaltzz" must NOT shadow df=1 "cobalt" (ln(25)=3.2189 >= 3.0).
    let res_zero_df = run_ok(
        &home2,
        "check cobaltzz and cobalt",
        &repo2,
        "",
        "cobaltzz cobalt",
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        res_zero_df
            .items
            .iter()
            .map(|it| it.id.as_str())
            .collect::<Vec<_>>(),
        vec!["m_cobalt"]
    );

    // Subcase C: per-item prefix_overlaps dedup keeps rare shorter-stem hits that do NOT contain
    // the longer word ("deploy deployment": n_scope=42, df("deployment")=1 -> ln(42)=3.738,
    // df("deploy")=2 -> ln(21)=3.045 >= 3.0; both items must survive).
    let (d3, home3) = scratch();
    let repo3 = init_git_repo(d3.path(), "anvil");
    seed_item(
        &home3,
        "m_long_only",
        "anvil",
        "deployment canary rollout alpha",
        "deployment canary rollout alpha",
        "",
    );
    seed_item(
        &home3,
        "m_short_only",
        "anvil",
        "deploy checklist beta",
        "deploy checklist beta",
        "",
    );
    seed_fillers(&home3, "anvil", "f", 40);
    write_cfg(&home3, "[prompt_hook]\nlimit = 4\n");

    let res_both = run_ok(
        &home3,
        "check deploy deployment steps",
        &repo3,
        "",
        "deploy deployment",
    )
    .unwrap()
    .unwrap();
    let mut got_ids: Vec<&str> = res_both.items.iter().map(|it| it.id.as_str()).collect();
    got_ids.sort_unstable();
    assert_eq!(got_ids, vec!["m_long_only", "m_short_only"]);
    assert!(
        hook_log_rows(&home3)
            .last()
            .unwrap()
            .get("floor_dropped")
            .is_none()
    );
}

// ---------------------------------------------------------------- 8. limit = -1 guard before DB/git

#[test]
fn negative_limit_fails_before_db_or_git_work() {
    let (d, home) = scratch();
    let repo = init_git_repo(d.path(), "anvil");
    // Replace memory.db with a directory and set bogus bias/roots so any work past the
    // fetch_limit <= 0 guard would fail with a different error first.
    std::fs::create_dir_all(home.join("memory.db")).unwrap();

    for min_idf_cfg in ["", "min_idf = 0.0\n"] {
        write_cfg(
            &home,
            &format!(
                "[prompt_hook]\nlimit = -1\n{min_idf_cfg}\
                 [query]\ndefault_bias = [\"bogus\"]\n\
                 [repos]\nroots = 5\n"
            ),
        );
        let err = run_ok(&home, "check anvil negative limit", &repo, "", "anvil").unwrap_err();
        assert_eq!(err.class, "ValueError");
        assert_eq!(err.msg, "--limit must be at least 1");
    }
    // No hook.log row written on unhandled ValueError, and take_git_ms stayed 0.
    assert!(hook_log_rows(&home).is_empty());
    assert_eq!(simple_memory::config::take_git_ms(), 0);
}

// ---------------------------------------------------------------- 9. Dedupe runs AFTER floor

#[test]
fn dedupe_runs_after_floor_so_low_idf_items_never_fill_deduped_slots() {
    let (d, home) = scratch();
    let repo = init_git_repo(d.path(), "anvil");
    // 25 active items:
    // - m_hi_1 and m_hi_2 hold "cobalt" (df=2 -> ln(12.5)=2.5257) + "nickel" (df=2 -> 2.5257) = 5.05 >= 3.0
    //   and also "common".
    // - m_lo_0..m_lo_5 hold only "common" (df=8 -> ln(25/8)=1.1394 < 3.0).
    // - 17 fillers.
    seed_item(
        &home,
        "m_hi_1",
        "anvil",
        "cobalt nickel common alpha",
        "cobalt nickel common details alpha",
        "",
    );
    seed_item(
        &home,
        "m_hi_2",
        "anvil",
        "cobalt nickel common beta",
        "cobalt nickel common details beta",
        "",
    );
    for i in 0..6 {
        seed_item(
            &home,
            &format!("m_lo_{i}"),
            "anvil",
            &format!("common note {i}"),
            &format!("common text {i}"),
            "",
        );
    }
    seed_fillers(&home, "anvil", "f", 17);
    write_cfg(&home, "[prompt_hook]\nlimit = 2\n");

    // Pre-seed session "s-order" with m_hi_1 already injected so fetch_limit = 2 + 1 = 3.
    // Top 3 ranked candidates are [m_hi_1, m_hi_2, m_lo_*].
    // Floor runs first (drops m_lo_*), then dedupe drops m_hi_1, returning ONLY [m_hi_2].
    let st_path = session_state_path(&home, "s-order");
    SessionState {
        transcript_size: 0,
        injected: vec!["m_hi_1".into()],
    }
    .write(&st_path)
    .unwrap();

    let res = run_ok(
        &home,
        "check cobalt nickel common notes",
        &repo,
        "s-order",
        "cobalt nickel common",
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        res.items
            .iter()
            .map(|it| it.id.as_str())
            .collect::<Vec<_>>(),
        vec!["m_hi_2"]
    );

    let row = hook_log_rows(&home).pop().unwrap();
    assert_eq!(row["ids"], json!(["m_hi_2"]));
    assert_eq!(row["dropped"], json!(["m_hi_1"]));
    let fd: Vec<&str> = row["floor_dropped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(fd.len(), 1);
    assert!(fd[0].starts_with("m_lo_"), "{fd:?}");

    let st_after = SessionState::read(&st_path).unwrap().unwrap();
    assert_eq!(st_after.injected, vec!["m_hi_1", "m_hi_2"]);
}

// ---------------------------------------------------------------- 10. Additive [scopes] & git_ms <= db_ms

#[test]
fn additive_scopes_widen_search_and_floor_n_scope_and_keep_git_ms_within_db_ms() {
    let (d, home) = scratch();
    let repo = init_git_repo(d.path(), "anvil");
    // 10 items in "anvil" (alone < 20) + 12 items in "globex-web" -> combined n_scope = 22 >= 20!
    seed_fillers(&home, "anvil", "a", 10);
    for i in 0..11 {
        seed_item(
            &home,
            &format!("m_glx_gen_{i:02}"),
            "globex-web",
            &format!("GLX common workflow {i}"),
            &format!("common workflow detail {i}"),
            "",
        );
    }
    seed_item(
        &home,
        "m_glx_rare",
        "globex-web",
        "GLX zircon failover",
        "zircon failover runbook",
        "",
    );
    write_cfg(
        &home,
        "[prompt_hook]\nlimit = 4\n\
         [prompt_hook.projects]\nGLX = \"Globex web\"\n\
         [scopes]\n\"acme/globex\" = [\"repo:globex-web\", \"GLX\"]\n",
    );

    // Generic term in widened scope (n_scope = 22 >= 20, df("common") = 11 -> ln(2) = 0.693 < 3.0)
    // is dropped by the floor even though repo "anvil" alone only has 10 items.
    let res_gen = run_ok(&home, "check GLX common workflow", &repo, "", "common")
        .unwrap()
        .unwrap();
    assert!(res_gen.items.is_empty());
    let row_gen = hook_log_rows(&home).pop().unwrap();
    assert_eq!(row_gen["scopes"], json!(["acme/globex"]));
    assert_eq!(row_gen["floor_dropped"].as_array().unwrap().len(), 4);
    assert!(
        row_gen["git_ms"].as_i64().unwrap() <= row_gen["db_ms"].as_i64().unwrap(),
        "git_ms must be <= db_ms: {row_gen}"
    );

    // Rare term in widened scope (df("zircon") = 1 in n_scope = 22 -> ln(22) = 3.091 >= 3.0)
    // passes floor and returns the item from globex-web.
    let res_rare = run_ok(&home, "check GLX zircon failover", &repo, "", "zircon")
        .unwrap()
        .unwrap();
    assert_eq!(
        res_rare
            .items
            .iter()
            .map(|it| it.id.as_str())
            .collect::<Vec<_>>(),
        vec!["m_glx_rare"]
    );
    let row_rare = hook_log_rows(&home).pop().unwrap();
    assert_eq!(row_rare["scopes"], json!(["acme/globex"]));
    assert!(row_rare.get("floor_dropped").is_none());
    let (ms, rewrite_ms, db_ms, git_ms, fin_ms) = (
        row_rare["ms"].as_i64().unwrap(),
        row_rare["rewrite_ms"].as_i64().unwrap(),
        row_rare["db_ms"].as_i64().unwrap(),
        row_rare["git_ms"].as_i64().unwrap(),
        row_rare["fin_ms"].as_i64().unwrap(),
    );
    assert!(git_ms <= db_ms, "{row_rare}");
    assert!(rewrite_ms + db_ms + fin_ms <= ms, "{row_rare}");
}
