//! Wave 3 Lane A tests: `scope:<path>` soft bias and automatic prompt-hook scope bias.
//!
//! Every fixture uses made-up names (`acme`, `acme/GLX`, `acme/globex`, `acme/GlobexWeb`,
//! `acme/other`, `initech`, `anvil`, `globex-web`, `GLX`, `INI`) and an isolated scratch home.

mod common;

use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use common::{git, open, scratch, sm, write};
use serde_json::{Value, json};
use simple_memory::config::{load_config, toml_loads};
use simple_memory::core::item::Item;
use simple_memory::core::rank::{
    Candidate, ScoreOpts, is_pure_relevance, item_matches_scope_matchers, parse_bias,
    parse_bias_list, parse_scope_bias_path, score_candidates,
};
use simple_memory::service::query::resolve_scope_biases;
use simple_memory::store::items::put_item;
use simple_memory::store::search::{Filters, SearchOpts, search_scored};

const TS: &str = "2026-10-25T10:00:00+00:00";

const SCOPES_TOML: &str = "[scopes]\n\
\"acme/GLX\" = [\"repo:globex-web\", \"GLX\"]\n\
\"acme/other\" = [\"repo:acme-other\"]\n\
initech = [\"INI\"]\n";

fn init_git_repo(parent: &Path, name: &str) -> PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    git(&["init", "-q", dir.to_str().unwrap()]);
    dir
}

#[allow(clippy::too_many_arguments)]
fn seed_item(
    home: &Path,
    id: &str,
    repo: &str,
    kind: &str,
    title: &str,
    body: &str,
    tag: &str,
    refs: &str,
    tags: &str,
    as_of: &str,
) {
    let conn = open(home);
    let mut it = Item::bare(id, repo, kind, title, body, tag);
    it.refs = refs.to_string();
    it.tags = tags.to_string();
    it.evidence = "made-up check".to_string();
    it.as_of = as_of.to_string();
    put_item(&conn, &it.post_init(), None).unwrap();
}

fn fake_claude(dir: &Path, reply: &str) -> String {
    let path = dir.join("fake-claude.sh");
    write(
        &path,
        &format!("#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{reply}'\n"),
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_str().unwrap().to_string()
}

fn sm_in_cwd(home: &Path, cwd: &Path, args: &[&str]) -> (i32, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sm"));
    cmd.args(args)
        .current_dir(cwd)
        .env("HOME", home.parent().unwrap_or(home))
        .env("SIMPLE_MEMORY_HOME", home)
        .env_remove("SM_PARITY_NOW")
        .env_remove("SM_PARITY_SEED")
        .env_remove("SM_PARITY_TRACE");
    let out = cmd.output().expect("spawn sm");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8(out.stdout).expect("utf-8 stdout"),
        String::from_utf8(out.stderr).expect("utf-8 stderr"),
    )
}

fn hook_prompt(home: &Path, stdin: &str) -> (i32, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sm"));
    cmd.args(["hook", "prompt"])
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

fn hook_log_rows(home: &Path) -> Vec<Value> {
    std::fs::read_to_string(home.join("hook.log"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn mcp_converse(home: &Path, cwd: &Path, msgs: &[Value]) -> (Vec<Value>, String) {
    let mut data = Vec::new();
    for m in msgs {
        data.extend(serde_json::to_vec(m).unwrap());
        data.push(b'\n');
    }
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sm"));
    cmd.args(["mcp"])
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .env("HOME", home.parent().unwrap_or(home))
        .env("SIMPLE_MEMORY_HOME", home)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in ["TZ", "SM_UTC_OFFSET"] {
        if let Some(val) = std::env::var_os(key) {
            cmd.env(key, val);
        }
    }
    let mut child = cmd.spawn().expect("spawn sm mcp");
    child.stdin.take().unwrap().write_all(&data).unwrap();
    let out = child.wait_with_output().expect("wait sm mcp");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "{stderr}");
    let responses: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    (responses, stderr)
}

fn mcp_payload(resp: &Value) -> Value {
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    serde_json::from_str(text).unwrap()
}

// ---------------------------------------------------------------- 1. Parsing, UTF-8 & surrogate boundaries

#[test]
fn parse_bias_handles_scope_case_validation_utf8_and_surrogate_boundaries() {
    // Prefix lowercases; path preserves case; deduplicates in order.
    assert_eq!(
        parse_bias("scope:acme/globex").unwrap(),
        vec!["scope:acme/globex"]
    );
    assert_eq!(
        parse_bias("SCOPE:acme/GLX").unwrap(),
        vec!["scope:acme/GLX"]
    );
    assert_eq!(
        parse_bias("  ScOpE: acme/GLX , scope:acme/GLX , recency ").unwrap(),
        vec!["scope:acme/GLX", "recency"]
    );
    // Distinct path casing stays distinct (paths in [scopes] are case-sensitive).
    assert_eq!(
        parse_bias("scope:acme/GLX,scope:acme/glx").unwrap(),
        vec!["scope:acme/GLX", "scope:acme/glx"]
    );
    assert_eq!(
        parse_bias("scope:acme_1/GLX-2.0/web").unwrap(),
        vec!["scope:acme_1/GLX-2.0/web"]
    );
    assert_eq!(
        parse_bias_list(Some(&["SCOPE:acme/GlobexWeb".to_string()])).unwrap(),
        vec!["scope:acme/GlobexWeb"]
    );

    // Invalid scope:<path> tokens produce the dedicated scope-level error.
    for (bad, repr) in [
        ("scope:", "'scope:'"),
        ("SCOPE:", "'SCOPE:'"),
        ("scope:   ", "'scope:   '"),
        ("scope:a//b", "'scope:a//b'"),
        ("scope:/acme", "'scope:/acme'"),
        ("scope:acme/", "'scope:acme/'"),
        ("scope:bad!", "'scope:bad!'"),
        ("scope:acme/glo bex", "'scope:acme/glo bex'"),
        ("scope:acme/café", "'scope:acme/café'"),
    ] {
        let err = parse_bias(bad).unwrap_err();
        assert_eq!(err.class, "ValueError", "{bad}");
        assert_eq!(
            err.msg,
            format!("Invalid bias {repr}; scope path levels must match [A-Za-z0-9_.-]+"),
            "{bad}"
        );
    }

    // Non-ASCII strings whose byte index 6 lands inside a multi-byte UTF-8 char must NEVER panic
    // and must keep the exact unchanged non-scope error message.
    const EXPECTED_TAIL: &str = "expected one of recency, relevance, executed, verified, decided, \
        client-stated, derived, assumed, prior, planned, fact, decision, learning, episode, task, handoff, or scope:<path>";
    for bad in ["12345é", "caféé", "scópé:acme", "newest"] {
        let err = parse_bias(bad).unwrap_err();
        assert_eq!(err.class, "ValueError", "{bad}");
        assert_eq!(
            err.msg,
            format!("Invalid bias '{bad}'; {EXPECTED_TAIL}"),
            "{bad}"
        );
    }

    // Helper predicates.
    assert_eq!(parse_scope_bias_path("scope:acme/GLX"), Some("acme/GLX"));
    assert_eq!(parse_scope_bias_path("scope:"), None);
    assert_eq!(parse_scope_bias_path("SCOPE:acme/GLX"), None);
    assert_eq!(parse_scope_bias_path("recency"), None);

    assert!(is_pure_relevance(&["relevance"]));
    assert!(!is_pure_relevance(&["relevance", "scope:acme/GLX"]));
    assert!(!is_pure_relevance(&["recency"]));
    assert!(!is_pure_relevance::<&str>(&[]));

    // CLI raw non-UTF-8 argv bytes around the 6-byte prefix boundary: zero panic!
    let (_d, home) = scratch();
    for (raw_bias, want_fragment) in [
        (
            b"scope\xff".as_slice(),
            format!("argument --bias: Invalid bias 'scope\\udcff'; {EXPECTED_TAIL}"),
        ),
        (
            b"scope:\xff".as_slice(),
            "argument --bias: Invalid bias 'scope:\\udcff'; scope path levels must match [A-Za-z0-9_.-]+"
                .to_string(),
        ),
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_sm"))
            .args([
                OsStr::new("query"),
                OsStr::new("kiwi"),
                OsStr::new("--bias"),
                OsStr::from_bytes(raw_bias),
            ])
            .env("HOME", home.parent().unwrap())
            .env("SIMPLE_MEMORY_HOME", &home)
            .env_remove("SM_PARITY_NOW")
            .env_remove("SM_PARITY_SEED")
            .env_remove("SM_PARITY_TRACE")
            .output()
            .expect("spawn sm with raw bytes");
        assert_eq!(out.status.code(), Some(2), "{raw_bias:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(&want_fragment),
            "raw={raw_bias:?}\nwant={want_fragment}\ngot={stderr}"
        );
    }
}

// ---------------------------------------------------------------- 2. Pure-core matcher parity, dedup & pool widening

#[test]
fn scope_matcher_parity_score_dedup_and_pool_widening() {
    let mk = |id: &str, repo: &str, title: &str, body: &str, refs: &str, tags: &str| {
        let mut it = Item::bare(id, repo, "fact", title, body, "derived");
        it.refs = refs.to_string();
        it.tags = tags.to_string();
        it.as_of = TS.to_string();
        it.post_init()
    };

    let m_repo = mk("m_repo", "globex-web", "kiwi", "kiwi", "", "");
    let m_title = mk("m_title", "misc", "GLX kiwi", "kiwi", "", "");
    let m_body = mk("m_body", "misc", "kiwi", "see GLX here", "", "");
    let m_refs = mk("m_refs", "misc", "kiwi", "kiwi", "GLX-1", "");
    let m_tags = mk("m_tags", "misc", "kiwi", "kiwi", "", "GLX");
    let m_repo_only_code = mk("m_rc", "GLX", "kiwi", "kiwi", "", "");
    let m_glxy = mk("m_glxy", "misc", "kiwi", "GLXY kiwi", "", "");
    let m_low = mk("m_low", "misc", "kiwi", "glx kiwi", "", "");
    let m_acc = mk("m_acc", "misc", "kiwi", "GLXé kiwi", "", "");
    let m_other_repo = mk("m_or", "globex-web-2", "kiwi", "globex-web", "", "");

    let repo_m = vec!["repo:globex-web".to_string()];
    let word_m = vec!["GLX".to_string()];
    assert!(item_matches_scope_matchers(&m_repo, &repo_m));
    assert!(!item_matches_scope_matchers(&m_other_repo, &repo_m));
    assert!(!item_matches_scope_matchers(&m_body, &repo_m));

    for hit in [&m_title, &m_body, &m_refs, &m_tags] {
        assert!(item_matches_scope_matchers(hit, &word_m), "{}", hit.id);
    }
    for miss in [&m_repo_only_code, &m_glxy, &m_low, &m_acc] {
        assert!(!item_matches_scope_matchers(miss, &word_m), "{}", miss.id);
    }
    assert!(!item_matches_scope_matchers(&m_repo, &[]));

    // Duplicate scope_biases labels must not double-boost a single item.
    let cands = vec![
        Candidate {
            item: mk("m_cwd", "anvil", "kiwi note", "kiwi note", "", ""),
            fold_bm25: Some(-5.0),
            raw_bm25: None,
        },
        Candidate {
            item: m_repo.clone(),
            fold_bm25: Some(-5.0),
            raw_bm25: None,
        },
    ];
    let dup_scopes = vec![
        ("scope:acme/GLX".to_string(), repo_m.clone()),
        ("scope:acme/GLX".to_string(), repo_m.clone()),
    ];
    let scored = score_candidates(
        &cands,
        "kiwi",
        &ScoreOpts {
            query_refs: None,
            current_repo: Some("anvil"),
            recent: false,
            now_epoch: Some(1_800_000_000.0),
            bias: &["scope:acme/GLX".to_string()],
            scope_biases: &dup_scopes,
            explain: None,
            aliases: None,
        },
    )
    .unwrap();
    assert_eq!(scored[0].item.id, "m_repo");
    assert_eq!(scored[0].why.bias, vec!["scope:acme/GLX"]);
    assert_eq!(scored[0].why.score, 1.3);
    assert_eq!(scored[1].item.id, "m_cwd");
    assert!(scored[1].why.bias.is_empty());
    assert_eq!(scored[1].why.score, 1.05);

    // Scope bias must not override ref_hits tier.
    let ref_cands = vec![
        Candidate {
            item: mk("m_ref", "anvil", "note", "body", "R6", ""),
            fold_bm25: Some(-1.0),
            raw_bm25: None,
        },
        Candidate {
            item: mk("m_scoped", "globex-web", "R6", "R6", "", ""),
            fold_bm25: Some(-10.0),
            raw_bm25: None,
        },
    ];
    let ref_scored = score_candidates(
        &ref_cands,
        "R6",
        &ScoreOpts {
            query_refs: None,
            current_repo: Some("anvil"),
            recent: false,
            now_epoch: Some(1_800_000_000.0),
            bias: &["scope:acme/GLX".to_string()],
            scope_biases: &[("scope:acme/GLX".to_string(), repo_m.clone())],
            explain: None,
            aliases: None,
        },
    )
    .unwrap();
    assert_eq!(ref_scored[0].item.id, "m_ref");
    assert_eq!(ref_scored[0].why.ref_hits, vec!["R6"]);

    // Pool widening: when bias is ["recency"] (bias_widens_pool == false), a non-empty
    // scope_biases still widens the FTS pool from 50 to 100 (and --recent keeps 50).
    let (_d, home) = scratch();
    for i in 0..70 {
        seed_item(
            &home,
            &format!("m_f{i:02}"),
            "anvil",
            "fact",
            "nebula nebula nebula",
            &format!("nebula nebula nebula {i}"),
            "derived",
            "",
            "",
            TS,
        );
    }
    let long_body = format!(
        "nebula {}",
        (0..200)
            .map(|n| format!("filler{n}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    seed_item(
        &home,
        "m_glx_deep",
        "globex-web",
        "fact",
        "short title",
        &long_body,
        "derived",
        "",
        "",
        TS,
    );
    let conn = open(&home);
    let f = Filters {
        include_all_repos: true,
        ..Default::default()
    };
    let rec_bias = vec!["recency".to_string()];
    let (plain, plain_cap) = search_scored(
        &conn,
        "nebula",
        &f,
        &SearchOpts {
            recent: false,
            aliases: None,
            bias: &rec_bias,
            scope_biases: &[],
        },
        Some(0),
    )
    .unwrap();
    assert_eq!(
        (
            plain.len(),
            plain.iter().any(|s| s.item.id == "m_glx_deep"),
            plain_cap
        ),
        (50, false, Some(50))
    );

    let scope_b = vec![("scope:acme/GLX".to_string(), repo_m)];
    let (widened, widened_cap) = search_scored(
        &conn,
        "nebula",
        &f,
        &SearchOpts {
            recent: false,
            aliases: None,
            bias: &rec_bias,
            scope_biases: &scope_b,
        },
        Some(0),
    )
    .unwrap();
    assert_eq!(
        (
            widened.len(),
            widened.iter().any(|s| s.item.id == "m_glx_deep"),
            widened_cap
        ),
        (71, true, None)
    );

    let (recent_only, recent_cap) = search_scored(
        &conn,
        "nebula zzz",
        &f,
        &SearchOpts {
            recent: true,
            aliases: None,
            bias: &rec_bias,
            scope_biases: &scope_b,
        },
        Some(0),
    )
    .unwrap();
    assert_eq!((recent_only.len(), recent_cap), (50, Some(50)));
}

// ---------------------------------------------------------------- 3. CLI soft-reordering, why=, JSON, footer & unknown scope

#[test]
fn cli_scope_bias_soft_reorders_preserves_footer_and_errors_on_unknown() {
    let (d, home) = scratch();
    let cwd_repo = init_git_repo(d.path(), "anvil");
    write(&home.join("config.toml"), SCOPES_TOML);

    seed_item(
        &home,
        "m_z_cwd",
        "anvil",
        "fact",
        "kiwi rollout plan",
        "kiwi rollout plan details",
        "derived",
        "",
        "",
        TS,
    );
    seed_item(
        &home,
        "m_a_glx",
        "globex-web",
        "fact",
        "kiwi rollout plan",
        "kiwi rollout plan details",
        "derived",
        "",
        "",
        TS,
    );
    seed_item(
        &home,
        "m_m_other",
        "unrelated",
        "fact",
        "kiwi rollout plan",
        "kiwi rollout plan details",
        "derived",
        "",
        "",
        TS,
    );
    // Global item matching word matcher GLX inside default repo scope (no --all).
    seed_item(
        &home,
        "m_g_glx",
        "global",
        "fact",
        "mango deploy note",
        "mango deploy note GLX",
        "derived",
        "",
        "",
        TS,
    );
    seed_item(
        &home,
        "m_z_mango_cwd",
        "anvil",
        "fact",
        "mango deploy note",
        "mango deploy note local",
        "derived",
        "",
        "",
        TS,
    );

    // Soft-reorder across --all with --repo anvil: m_a_glx (+0.3) > m_z_cwd (+0.05) > m_m_other (0.0);
    // non-matching items stay in results (never filtered out).
    let (code, text_out, err) = sm_in_cwd(
        &home,
        &cwd_repo,
        &[
            "query",
            "kiwi rollout",
            "--all",
            "--repo",
            "anvil",
            "--bias",
            "scope:acme/GLX",
            "--limit",
            "10",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let header_lines: Vec<&str> = text_out.lines().filter(|l| l.starts_with("[m_")).collect();
    assert_eq!(header_lines.len(), 3, "{text_out}");
    assert!(
        header_lines[0].contains("[m_a_glx] #1")
            && header_lines[0].contains("why=phrase,terms:2/2,bias:fact,bias:scope:acme/GLX"),
        "{}",
        header_lines[0]
    );
    assert!(
        header_lines[1].contains("[m_z_cwd] #2") && !header_lines[1].contains("bias:scope:"),
        "{}",
        header_lines[1]
    );
    assert!(
        header_lines[2].contains("[m_m_other] #3"),
        "{}",
        header_lines[2]
    );

    // JSON carries additive default bias + case-preserved `"scope:acme/GLX"`.
    let (code, json_out, err) = sm_in_cwd(
        &home,
        &cwd_repo,
        &[
            "query",
            "kiwi rollout",
            "--all",
            "--repo",
            "anvil",
            "--bias",
            "SCOPE:acme/GLX",
            "--limit",
            "10",
            "--format",
            "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let v: Value = serde_json::from_str(&json_out).unwrap();
    assert_eq!(
        v["bias"],
        json!([
            "recency",
            "fact",
            "decision",
            "learning",
            "executed",
            "verified",
            "decided",
            "client-stated",
            "scope:acme/GLX"
        ])
    );
    let hits = v["results"].as_array().unwrap();
    assert_eq!(
        hits.iter()
            .map(|r| (r["id"].as_str().unwrap(), r["why"]["bias"].clone()))
            .collect::<Vec<_>>(),
        vec![
            ("m_a_glx", json!(["fact", "scope:acme/GLX"])),
            ("m_z_cwd", json!(["fact"])),
            ("m_m_other", json!(["fact"])),
        ]
    );

    // Without --all (cwd repo = anvil + global): global GLX item outranks equal-BM25 cwd item,
    // and cwd item stays in results.
    let (code, default_scope_json, err) = sm_in_cwd(
        &home,
        &cwd_repo,
        &[
            "query",
            "mango deploy",
            "--bias",
            "scope:acme/GLX",
            "--format",
            "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let dv: Value = serde_json::from_str(&default_scope_json).unwrap();
    let d_ids: Vec<&str> = dv["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(d_ids, vec!["m_g_glx", "m_z_mango_cwd"]);
    assert_eq!(
        dv["results"][0]["why"]["bias"],
        json!(["fact", "scope:acme/GLX"])
    );

    // Footer preserves normalized `--bias scope:acme/GLX`.
    let (code, lim_out, err) = sm_in_cwd(
        &home,
        &cwd_repo,
        &[
            "query",
            "kiwi rollout",
            "--all",
            "--bias",
            "SCOPE:acme/GLX",
            "--limit",
            "1",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let more_line = lim_out.lines().find(|l| l.starts_with("more:")).unwrap();
    assert_eq!(
        more_line,
        "more: 2 beyond --limit 1 -> sm query \"kiwi rollout\" --all --bias scope:acme/GLX --limit 3"
    );

    // Unknown scope exits 2 and lists known scopes.
    let (code, out, err) = sm(&home, &["query", "kiwi", "--bias", "scope:unknown"]);
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (
            2,
            "",
            "unknown scope: unknown; known: acme, acme/GLX, acme/other, initech\n"
        )
    );

    // CLI help and `sm init` config note mention `scope:<path>` without altering `[scopes]` note.
    let (code, help_out, err) = sm(&home, &["query", "--help"]);
    assert_eq!(code, 0, "{err}");
    assert!(help_out.contains("scope:<path>"), "{help_out}");

    let (_e, fresh) = scratch();
    std::fs::remove_dir(&fresh).unwrap();
    let (code, _, err) = sm(&fresh, &["init"]);
    assert_eq!(code, 0, "{err}");
    let init_cfg = std::fs::read_to_string(fresh.join("config.toml")).unwrap();
    assert!(
        init_cfg.contains(
            "# Same values as `--bias`: recency, relevance, any evidence tag, any kind, or scope:<path>.\n"
        ),
        "{init_cfg}"
    );
}

// ---------------------------------------------------------------- 4. Hierarchical specificity & relevance+scope

#[test]
fn hierarchical_specificity_and_relevance_plus_scope_bias() {
    let (_d, home) = scratch();
    write(&home.join("config.toml"), SCOPES_TOML);

    seed_item(
        &home,
        "m_a_glx",
        "globex-web",
        "fact",
        "kiwi rollout plan",
        "kiwi rollout plan details",
        "derived",
        "",
        "",
        TS,
    );
    seed_item(
        &home,
        "m_m_sibling",
        "acme-other",
        "fact",
        "kiwi rollout plan",
        "kiwi rollout plan details",
        "derived",
        "",
        "",
        TS,
    );
    seed_item(
        &home,
        "m_z_outside",
        "unrelated",
        "fact",
        "kiwi rollout plan",
        "kiwi rollout plan details",
        "derived",
        "",
        "",
        TS,
    );

    // `--bias scope:acme,scope:acme/GLX` merges additively over defaults: child matches fact + both
    // scopes (+0.6 over outside), sibling matches fact + parent (+0.3 over outside), outside matches fact.
    let (code, out, err) = sm(
        &home,
        &[
            "query",
            "kiwi rollout",
            "--all",
            "--bias",
            "scope:acme,scope:acme/GLX",
            "--format",
            "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let v: Value = serde_json::from_str(&out).unwrap();
    let rows: Vec<(&str, Value, f64)> = v["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["id"].as_str().unwrap(),
                r["why"]["bias"].clone(),
                r["why"]["score"].as_f64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        rows.iter()
            .map(|(id, b, _)| (*id, b.clone()))
            .collect::<Vec<_>>(),
        vec![
            ("m_a_glx", json!(["fact", "scope:acme", "scope:acme/GLX"])),
            ("m_m_sibling", json!(["fact", "scope:acme"])),
            ("m_z_outside", json!(["fact"])),
        ]
    );
    // Exact +0.3 per scope boost on equal-BM25 items.
    assert!(((rows[0].2 - rows[1].2) - 0.3).abs() < 1e-4, "{rows:?}");
    assert!(((rows[1].2 - rows[2].2) - 0.3).abs() < 1e-4, "{rows:?}");

    // With explicit `relevance`, defaults are replaced and exact scores are 1.6 / 1.3 / 1.0.
    let (code, out_rel_hier, err) = sm(
        &home,
        &[
            "query",
            "kiwi rollout",
            "--all",
            "--bias",
            "relevance,scope:acme,scope:acme/GLX",
            "--format",
            "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let v_rh: Value = serde_json::from_str(&out_rel_hier).unwrap();
    let rh_rows: Vec<(&str, Value, f64)> = v_rh["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["id"].as_str().unwrap(),
                r["why"]["bias"].clone(),
                r["why"]["score"].as_f64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        rh_rows,
        vec![
            ("m_a_glx", json!(["scope:acme", "scope:acme/GLX"]), 1.6),
            ("m_m_sibling", json!(["scope:acme"]), 1.3),
            ("m_z_outside", json!([]), 1.0),
        ]
    );

    // Repeated --bias flags match comma-separated form; combining --in acme with --bias scope:acme/GLX
    // hard-filters to acme while soft-boosting acme/GLX above acme/other.
    let (code, out_rep, err) = sm(
        &home,
        &[
            "query",
            "kiwi rollout",
            "--all",
            "--bias",
            "scope:acme",
            "--bias",
            "scope:acme/GLX",
            "--format",
            "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(serde_json::from_str::<Value>(&out_rep).unwrap(), v);

    let (code, out_in_bias, err) = sm(
        &home,
        &[
            "query",
            "kiwi rollout",
            "--in",
            "acme",
            "--bias",
            "scope:acme/GLX",
            "--format",
            "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let v_ib: Value = serde_json::from_str(&out_in_bias).unwrap();
    let ib_ids: Vec<&str> = v_ib["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ib_ids, vec!["m_a_glx", "m_m_sibling"]);
    assert_eq!(
        v_ib["results"][0]["why"]["bias"],
        json!(["fact", "scope:acme/GLX"])
    );
    assert_eq!(v_ib["results"][1]["why"]["bias"], json!(["fact"]));

    // `--bias relevance,scope:acme/GLX`: disables recency + kind/tag boosts while keeping scope:acme/GLX.
    let (_d2, home2) = scratch();
    write(&home2.join("config.toml"), SCOPES_TOML);
    seed_item(
        &home2,
        "m_fresh_dec",
        "anvil",
        "decision",
        "kiwi plan",
        "kiwi plan details",
        "executed",
        "",
        "",
        "2026-11-20T00:00:00+00:00",
    );
    seed_item(
        &home2,
        "m_old_glx",
        "globex-web",
        "episode",
        "kiwi plan",
        "kiwi plan details",
        "derived",
        "",
        "",
        "2025-01-01T00:00:00+00:00",
    );

    let (code, out_rel_scope, err) = sm(
        &home2,
        &[
            "query",
            "kiwi plan",
            "--all",
            "--repo",
            "anvil",
            "--bias",
            "relevance,scope:acme/GLX",
            "--format",
            "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let v_rs: Value = serde_json::from_str(&out_rel_scope).unwrap();
    assert_eq!(v_rs["bias"], json!(["relevance", "scope:acme/GLX"]));
    let r_rs = v_rs["results"].as_array().unwrap();
    assert_eq!(
        (
            r_rs[0]["id"].as_str().unwrap(),
            r_rs[0]["why"]["bias"].clone(),
            r_rs[0]["why"]["score"].as_f64().unwrap(),
            r_rs[1]["id"].as_str().unwrap(),
            r_rs[1]["why"]["bias"].clone(),
            r_rs[1]["why"]["score"].as_f64().unwrap(),
        ),
        (
            "m_old_glx",
            json!(["scope:acme/GLX"]),
            1.3,
            "m_fresh_dec",
            json!([]),
            1.05,
        )
    );
}

// ---------------------------------------------------------------- 5. MCP memory_query with scope:<path>

#[test]
fn mcp_memory_query_supports_scope_bias_and_reports_unknown_or_invalid() {
    let (d, home) = scratch();
    let cwd_repo = init_git_repo(d.path(), "anvil");
    write(&home.join("config.toml"), SCOPES_TOML);
    seed_item(
        &home,
        "m_z_cwd",
        "anvil",
        "fact",
        "kiwi rollout plan",
        "kiwi rollout plan details",
        "derived",
        "",
        "",
        TS,
    );
    seed_item(
        &home,
        "m_a_glx",
        "globex-web",
        "fact",
        "kiwi rollout plan",
        "kiwi rollout plan details",
        "derived",
        "",
        "",
        TS,
    );

    let init = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "t", "version": "1"}
        }
    });
    let (resps, _stderr) = mcp_converse(
        &home,
        &cwd_repo,
        &[
            init,
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "memory_query",
                    "arguments": {
                        "query": "kiwi rollout",
                        "all_repos": true,
                        "repo": "anvil",
                        "bias": "scope:acme/GLX"
                    }
                }
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "memory_query",
                    "arguments": {
                        "query": "kiwi rollout",
                        "bias": "scope:unknown"
                    }
                }
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": "memory_query",
                    "arguments": {
                        "query": "kiwi rollout",
                        "bias": "scope:bad!"
                    }
                }
            }),
        ],
    );

    // tools/list describes scope:<path> on memory_query.bias.
    let tools = resps[1]["result"]["tools"].as_array().unwrap();
    let mq = tools.iter().find(|t| t["name"] == "memory_query").unwrap();
    let bias_desc = mq["inputSchema"]["properties"]["bias"]["description"]
        .as_str()
        .unwrap();
    assert!(bias_desc.contains("scope:<path>"), "{bias_desc}");

    // Valid scope:acme/GLX merges onto default bias and boosts m_a_glx above m_z_cwd without filtering m_z_cwd out.
    assert_eq!(resps[2]["result"]["isError"], json!(false));
    let q_ok = mcp_payload(&resps[2]);
    assert_eq!(
        q_ok["bias"],
        json!([
            "recency",
            "fact",
            "decision",
            "learning",
            "executed",
            "verified",
            "decided",
            "client-stated",
            "scope:acme/GLX"
        ])
    );
    let hits = q_ok["results"].as_array().unwrap();
    assert_eq!(
        hits.iter()
            .map(|r| (r["id"].as_str().unwrap(), r["why"]["bias"].clone()))
            .collect::<Vec<_>>(),
        vec![
            ("m_a_glx", json!(["fact", "scope:acme/GLX"])),
            ("m_z_cwd", json!(["fact"])),
        ]
    );

    // Unknown scope returns isError: true with unknown scope message.
    assert_eq!(resps[3]["result"]["isError"], json!(true));
    assert_eq!(
        resps[3]["result"]["content"][0]["text"],
        json!("error: unknown scope: unknown; known: acme, acme/GLX, acme/other, initech")
    );

    // Malformed scope bias returns isError: true with level validation error.
    assert_eq!(resps[4]["result"]["isError"], json!(true));
    assert_eq!(
        resps[4]["result"]["content"][0]["text"],
        json!("error: Invalid bias 'scope:bad!'; scope path levels must match [A-Za-z0-9_.-]+")
    );
}

// ---------------------------------------------------------------- 6. [query] default_bias & resolve_scope_biases unit checks

#[test]
fn default_bias_config_with_scope_and_resolve_scope_biases_rules() {
    let (d, home) = scratch();
    let cwd_repo = init_git_repo(d.path(), "anvil");
    write(
        &home.join("config.toml"),
        &format!("[query]\ndefault_bias = [\"recency\", \"scope:acme/GLX\"]\n{SCOPES_TOML}"),
    );
    seed_item(
        &home,
        "m_z_cwd",
        "anvil",
        "fact",
        "kiwi rollout plan",
        "kiwi rollout plan details",
        "derived",
        "",
        "",
        TS,
    );
    seed_item(
        &home,
        "m_a_glx",
        "globex-web",
        "fact",
        "kiwi rollout plan",
        "kiwi rollout plan details",
        "derived",
        "",
        "",
        TS,
    );

    // Omitting --bias applies [query] default_bias = ["recency", "scope:acme/GLX"].
    let (code, out, err) = sm_in_cwd(
        &home,
        &cwd_repo,
        &[
            "query",
            "kiwi rollout",
            "--all",
            "--repo",
            "anvil",
            "--format",
            "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["bias"], json!(["recency", "scope:acme/GLX"]));
    assert_eq!(v["results"][0]["id"], json!("m_a_glx"));
    assert_eq!(v["results"][0]["why"]["bias"], json!(["scope:acme/GLX"]));
    assert_eq!(v["results"][1]["id"], json!("m_z_cwd"));

    // Direct resolve_scope_biases checks:
    // 1) Lazy skip: bad [scopes] is NOT parsed when neither bias_list nor extra_scope_paths activates a scope.
    let bad_cfg = toml_loads("[scopes]\n\"acme/glo bex\" = [\"GLX\"]\n").unwrap();
    assert_eq!(
        resolve_scope_biases(&bad_cfg, &["recency".to_string(), "fact".to_string()], &[]).unwrap(),
        vec![]
    );
    // 2) Pure relevance suppresses extra_scope_paths (and also skips parsing [scopes]).
    assert_eq!(
        resolve_scope_biases(
            &bad_cfg,
            &["relevance".to_string()],
            &["acme/GLX".to_string()]
        )
        .unwrap(),
        vec![]
    );
    // 3) Deduplication within bias_list and across extra_scope_paths preserves order.
    let good_cfg = load_config(home.to_str()).unwrap();
    let resolved = resolve_scope_biases(
        &good_cfg,
        &[
            "recency".to_string(),
            "scope:acme/GLX".to_string(),
            "scope:acme/GLX".to_string(),
        ],
        &["acme/GLX".to_string(), "initech".to_string()],
    )
    .unwrap();
    assert_eq!(
        resolved,
        vec![
            (
                "scope:acme/GLX".to_string(),
                vec!["repo:globex-web".to_string(), "GLX".to_string()]
            ),
            ("scope:initech".to_string(), vec!["INI".to_string()]),
        ]
    );
    // 4) Non-pure relevance (["relevance", "scope:acme/GLX"]) keeps both explicit and extra scopes.
    let mixed = resolve_scope_biases(
        &good_cfg,
        &["relevance".to_string(), "scope:acme/GLX".to_string()],
        &["initech".to_string()],
    )
    .unwrap();
    assert_eq!(
        mixed
            .iter()
            .map(|(label, _)| label.as_str())
            .collect::<Vec<_>>(),
        vec!["scope:acme/GLX", "scope:initech"]
    );
}

// ---------------------------------------------------------------- 7. Automatic prompt-hook scope bias (all 3 paths + relevance suppression)

#[test]
fn prompt_hook_auto_scope_bias_ranks_scope_above_cwd_on_haiku_floor_fast_and_local_paths() {
    let (d, home) = scratch();
    let cwd_repo = init_git_repo(d.path(), "anvil");
    let claude = fake_claude(d.path(), "walrus okapi quagga");

    // Seed 35 neutral fillers in anvil + 2 equal-BM25 target items:
    // n_scope = 37 (>= HAIKU_FLOOR_MIN_SCOPE = 20):
    // - Haiku floor: 3 rare terms, df = 2 -> 3 * ln(37/2) = 8.75 >= 3.0.
    // - Local floor: 3 rare terms, df = 2 -> 3 * ln(37/2) = 8.75 >= LOCAL_TAU (8.0).
    // IDs chosen so tie-break (id DESC) AND CURRENT_REPO_BOOST (+0.05) BOTH favor m_z_cwd;
    // only automatic scope:acme/GLX (+0.3) can put m_a_glx ahead of m_z_cwd!
    for i in 0..35 {
        seed_item(
            &home,
            &format!("f_{i:02}"),
            "anvil",
            "fact",
            &format!("neutral entry {i}"),
            &format!("ordinary filler body {i}"),
            "derived",
            "",
            "",
            TS,
        );
    }
    seed_item(
        &home,
        "m_z_cwd",
        "anvil",
        "fact",
        "walrus okapi quagga rollout",
        "walrus okapi quagga rollout plan",
        "decided",
        "",
        "",
        TS,
    );
    seed_item(
        &home,
        "m_a_glx",
        "globex-web",
        "fact",
        "walrus okapi quagga rollout",
        "walrus okapi quagga rollout plan",
        "decided",
        "",
        "",
        TS,
    );

    let payload = json!({
        "prompt": "check GLX walrus okapi quagga rollout",
        "cwd": cwd_repo.to_str().unwrap(),
    })
    .to_string();

    let run_case = |extra_cfg: &str, want_path: &str, want_order: &[&str]| {
        let log_path = home.join("hook.log");
        let _ = std::fs::remove_file(&log_path);
        write(
            &home.join("config.toml"),
            &format!(
                "[catch_up]\nenabled = false\n\
                 [prompt_hook]\nclaude_bin = \"{claude}\"\nlimit = 4\n\
                 {extra_cfg}\n\
                 [prompt_hook.projects]\nGLX = \"Globex client\"\n\
                 {SCOPES_TOML}"
            ),
        );
        let (code, stdout, stderr) = hook_prompt(&home, &payload);
        assert_eq!((code, stderr.as_str()), (0, ""), "{extra_cfg}");
        let v: Value = serde_json::from_str(&stdout).unwrap();
        let ctx = v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        let lines: Vec<&str> = ctx.lines().skip(1).collect();
        let got_ids: Vec<&str> = lines
            .iter()
            .map(|l| {
                let start = l.find('[').unwrap() + 1;
                let end = l.find(']').unwrap();
                &l[start..end]
            })
            .collect();
        assert_eq!(got_ids, want_order, "cfg={extra_cfg}\nctx={ctx}");

        let rows = hook_log_rows(&home);
        assert_eq!(rows.len(), 1, "{extra_cfg}");
        assert_eq!(rows[0]["path"], json!(want_path), "{extra_cfg}");
        assert_eq!(rows[0]["scopes"], json!(["acme/GLX"]), "{extra_cfg}");
        assert_eq!(rows[0]["ids"], json!(want_order), "{extra_cfg}");
    };

    // 1) Haiku path with default min_idf = 3.0 (`haiku_search_and_floor`): scope beats cwd!
    run_case("min_idf = 3.0", "haiku", &["m_a_glx", "m_z_cwd"]);
    // 2) Haiku path with min_idf = 0.0 (`run_query` fast path): scope beats cwd!
    run_case("min_idf = 0.0", "haiku", &["m_a_glx", "m_z_cwd"]);
    // 3) Local fallback (`model = "none"` -> `local_query_at`): scope beats cwd!
    run_case("model = \"none\"", "local", &["m_a_glx", "m_z_cwd"]);

    // 4) Pure `[query] default_bias = ["relevance"]` suppresses automatic scope bias on ALL 3
    //    paths while keeping additive SQL (both items return, m_z_cwd wins via +0.05 repo boost).
    run_case(
        "min_idf = 3.0\n[query]\ndefault_bias = [\"relevance\"]",
        "haiku",
        &["m_z_cwd", "m_a_glx"],
    );
    run_case(
        "min_idf = 0.0\n[query]\ndefault_bias = [\"relevance\"]",
        "haiku",
        &["m_z_cwd", "m_a_glx"],
    );
    run_case(
        "model = \"none\"\n[query]\ndefault_bias = [\"relevance\"]",
        "local",
        &["m_z_cwd", "m_a_glx"],
    );
}

// ---------------------------------------------------------------- 8. Bad [scopes] isolation

#[test]
fn bad_scopes_table_still_allows_plain_query_and_fails_loud_on_scope_bias() {
    let (_d, home) = scratch();
    write(
        &home.join("config.toml"),
        "[scopes]\n\"acme/glo bex\" = [\"GLX\"]\n",
    );
    seed_item(
        &home,
        "m_1",
        "global",
        "fact",
        "kiwi plain note",
        "kiwi plain note body",
        "derived",
        "",
        "",
        TS,
    );

    // Plain `sm query` with no `scope:` bias does not parse `[scopes]` and succeeds.
    let (code, out, err) = sm(&home, &["query", "kiwi", "--all", "--format", "json"]);
    assert_eq!(code, 0, "{err}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["results"][0]["id"], json!("m_1"));

    // Explicit `--bias scope:acme` reads `[scopes]` and fails loud with the bad-path error.
    let (code, out, err) = sm(&home, &["query", "kiwi", "--all", "--bias", "scope:acme"]);
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (
            1,
            "",
            "error: [scopes] path 'acme/glo bex': each level must match [A-Za-z0-9_.-]+\n"
        )
    );
}
