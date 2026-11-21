//! Integration and boundary tests for v0.6.0 search, ranking, date bounds, and hook features:
//! 1. Query exclusions (`-term`, `-"phrase"`, pure `-R6` literal preservation, uppercase `OR`/`AND` stripping)
//! 2. Active-first history ranking (`--include-history`)
//! 3. Snake_case & alias-aware `why=` / `missing:` + circular self-hint suppression
//! 4. Additive `--bias scope:<path>`
//! 5. Relative `--since` / `--until` shorthands (`m`/`h`/`d`/`w`, bounds `1..=36500`)
//! 6. `[prompt_hook] local_min_idf` config knob (default `8.0`, `0.0` disables, invalid fails loud)
//! 7. Hook `--in <scopes>` follow-up hint (Haiku + local paths)
//! 8. Config-aware backoff auto-invalidation (`rewrite_cmd`, `rewrite_model`, `rewrite_timeout` + legacy compat)
//! 9. Clearer tag/bias error messages

mod common;

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use common::{git, open, scratch, sm, write};
use serde_json::{Value, json};
use simple_memory::clock;
use simple_memory::config::{config_toml, defaults, load_config};
use simple_memory::core::item::{Item, epoch_to_as_of};
use simple_memory::py::path::PyPath;
use simple_memory::py::value::PyValue;
use simple_memory::service::capture::Completed;
use simple_memory::service::query::local_min_idf_from_cfg;
use simple_memory::service::recall::{Backoff, local_query, run_prompt_hook};
use simple_memory::store::items::put_item;

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

fn seed_superseded(
    home: &Path,
    id: &str,
    title: &str,
    body: &str,
    refs: &str,
    superseded_by: Option<&str>,
    as_of: &str,
) {
    let conn = open(home);
    let mut it = Item::bare(id, "anvil", "fact", title, body, "executed");
    it.refs = refs.to_string();
    it.evidence = "made-up check".to_string();
    it.as_of = as_of.to_string();
    it.status = "superseded".to_string();
    it.superseded_by = superseded_by.map(String::from);
    put_item(&conn, &it.post_init(), None).unwrap();
}

fn fake_claude(dir: &Path, name: &str, reply: &str) -> String {
    let path = dir.join(name);
    write(
        &path,
        &format!("#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{reply}'\n"),
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_str().unwrap().to_string()
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

fn s(v: &str) -> PyValue {
    PyValue::Str(v.to_string())
}

// ---------------------------------------------------------------- 1. Query exclusions & operators

#[test]
fn query_exclusions_filter_terms_phrases_other_repos_and_preserve_literal_leading_dash() {
    let (_d, home) = scratch();
    seed_item(
        &home,
        "m_prod",
        "anvil",
        "fact",
        "postgres tuning",
        "postgres production settings",
        "decided",
        "",
        "",
        TS,
    );
    seed_item(
        &home,
        "m_stg",
        "anvil",
        "fact",
        "postgres staging",
        "postgres staging environment",
        "decided",
        "",
        "",
        TS,
    );
    seed_item(
        &home,
        "m_pool",
        "anvil",
        "fact",
        "postgres pool",
        "postgres connection pool limits",
        "decided",
        "",
        "",
        TS,
    );
    seed_item(
        &home,
        "m_other_stg",
        "other",
        "fact",
        "postgres staging other",
        "postgres staging in other repo",
        "decided",
        "",
        "",
        TS,
    );
    seed_item(
        &home,
        "m_other_prod",
        "other",
        "fact",
        "postgres prod other",
        "postgres production in other repo",
        "decided",
        "",
        "",
        TS,
    );
    seed_item(
        &home,
        "m_r6",
        "anvil",
        "fact",
        "Flag -R6 note",
        "Flag -R6 detail body",
        "decided",
        "R6",
        "",
        TS,
    );

    // Single term exclusion + phrase exclusion: only m_prod survives in repo anvil,
    // and other_repos excludes m_other_stg while keeping m_other_prod.
    let (code, out, err) = sm(
        &home,
        &[
            "query",
            "postgres -staging -\"connection pool\"",
            "--repo",
            "anvil",
            "--format",
            "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let v: Value = serde_json::from_str(&out).unwrap();
    let ids: Vec<&str> = v["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["m_prod"]);
    // Excluded terms must never pollute why.terms_missing or footer missing.
    assert_eq!(v["results"][0]["why"]["terms_matched"], json!(["postgres"]));
    assert_eq!(v["results"][0]["why"]["terms_missing"], json!([]));
    assert_eq!(v["missing"], json!([]));
    assert_eq!(v["more"]["other_repos"], json!({"other": 1}));

    // Leading exclusion before positive term works identically (`-staging postgres`).
    let (code, out_lead, err) = sm(
        &home,
        &[
            "query",
            "--repo",
            "anvil",
            "--format",
            "json",
            "--",
            "-staging postgres",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let v_lead: Value = serde_json::from_str(&out_lead).unwrap();
    let lead_ids: Vec<&str> = v_lead["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(lead_ids, vec!["m_prod", "m_pool"]);

    // Quoted `"-staging" postgres` treats staging as positive (not excluded).
    let (code, out_quoted, err) = sm(
        &home,
        &[
            "query",
            "\"-staging\" postgres",
            "--repo",
            "anvil",
            "--format",
            "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let v_q: Value = serde_json::from_str(&out_quoted).unwrap();
    let q_ids: Vec<&str> = v_q["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert!(q_ids.contains(&"m_stg"), "{q_ids:?}");

    // Pure leading-dash query (`sm query -- -R6`) stays literal (zero positive tokens -> no exclusion).
    let (code, out_r6, err) = sm(
        &home,
        &["query", "--repo", "anvil", "--format", "json", "--", "-R6"],
    );
    assert_eq!(code, 0, "{err}");
    let v_r6: Value = serde_json::from_str(&out_r6).unwrap();
    assert_eq!(v_r6["results"][0]["id"], json!("m_r6"));

    // Standalone uppercase OR / AND stripped as boolean noise; does not distort phrase/why.
    let (code, out_op, err) = sm(
        &home,
        &[
            "query",
            "postgres OR production AND settings",
            "--repo",
            "anvil",
            "--format",
            "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let v_op: Value = serde_json::from_str(&out_op).unwrap();
    assert_eq!(v_op["results"][0]["id"], json!("m_prod"));
    assert_eq!(v_op["results"][0]["why"]["phrase"], json!(true));
    assert_eq!(
        v_op["results"][0]["why"]["terms_matched"],
        json!(["postgres", "production", "settings"])
    );

    // Excluding every matching item suppresses circular `missing: postgres=N -> sm query "postgres"`.
    let (code, out_all_excl, err) = sm(
        &home,
        &[
            "query",
            "postgres -production -staging -pool",
            "--repo",
            "anvil",
            "--format",
            "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let v_ae: Value = serde_json::from_str(&out_all_excl).unwrap();
    assert_eq!(v_ae["results"], json!([]));
    assert_eq!(v_ae["missing"], json!([]));
}

// ---------------------------------------------------------------- 2. Active-first history ranking

#[test]
fn include_history_ranks_active_items_ahead_of_superseded_and_retired() {
    let (_d, home) = scratch();
    seed_superseded(
        &home,
        "m_sup",
        "R6 deploy workflow",
        "R6 deploy workflow exact phrase",
        "R6",
        Some("m_act"),
        "2026-11-10T10:00:00+00:00",
    );
    seed_superseded(
        &home,
        "m_ret",
        "R6 deploy workflow",
        "R6 deploy workflow exact phrase",
        "R6",
        None,
        "2026-11-10T10:00:00+00:00",
    );
    seed_item(
        &home,
        "m_act",
        "anvil",
        "fact",
        "deploy note",
        "updated deploy guidance without ref",
        "derived",
        "",
        "",
        "2026-01-01T10:00:00+00:00",
    );

    let (code, out, err) = sm(
        &home,
        &[
            "query",
            "R6 deploy workflow",
            "--repo",
            "anvil",
            "--include-history",
            "--format",
            "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let v: Value = serde_json::from_str(&out).unwrap();
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0]["id"], json!("m_act"));
    assert_eq!(results[0]["status"], json!("active"));
    assert_eq!(results[1]["status"], json!("superseded"));
    assert_eq!(results[2]["status"], json!("superseded"));

    // Without --include-history, only the active item is returned.
    let (code, out_no_hist, err) = sm(
        &home,
        &[
            "query",
            "R6 deploy workflow",
            "--repo",
            "anvil",
            "--format",
            "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let v_nh: Value = serde_json::from_str(&out_no_hist).unwrap();
    let nh_ids: Vec<&str> = v_nh["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(nh_ids, vec!["m_act"]);
}

// ---------------------------------------------------------------- 3. Honest why= and missing: (snake_case, aliases, self-hint)

#[test]
fn snake_case_aliases_and_circular_self_hint_suppression() {
    let (_d, home) = scratch();
    write(
        &home.join("config.toml"),
        "[keyword_aliases]\nk8s = \"kubernetes\"\n\"queue lag\" = \"queue-worker\"\n",
    );
    seed_item(
        &home,
        "m_snake",
        "anvil",
        "fact",
        "metric config",
        "tracks foo_bar_baz values",
        "decided",
        "",
        "",
        TS,
    );
    seed_item(
        &home,
        "m_embed",
        "anvil",
        "fact",
        "wrapper token",
        "contains pre_zorx_post identifier",
        "decided",
        "",
        "",
        TS,
    );
    seed_item(
        &home,
        "m_k8s",
        "anvil",
        "fact",
        "kubernetes rollout",
        "kubernetes deploy steps",
        "decided",
        "",
        "",
        TS,
    );
    seed_item(
        &home,
        "m_deploy_only",
        "anvil",
        "decision",
        "R9 deploy note",
        "deploy note without cluster",
        "executed",
        "R9",
        "",
        "2026-11-15T10:00:00+00:00",
    );

    // 1) snake_case query `foo_bar` matches `foo_bar_baz` in why= with no bogus `missing: foo_bar=1`.
    let (code, out_snake, err) = sm(
        &home,
        &["query", "foo_bar", "--repo", "anvil", "--format", "json"],
    );
    assert_eq!(code, 0, "{err}");
    let vs: Value = serde_json::from_str(&out_snake).unwrap();
    assert_eq!(vs["results"][0]["id"], json!("m_snake"));
    assert_eq!(vs["results"][0]["why"]["terms_matched"], json!(["foo_bar"]));
    assert_eq!(vs["results"][0]["why"]["terms_missing"], json!([]));
    assert_eq!(vs["missing"], json!([]));

    // 2) Single-term query `zorx` matched by FTS5 inside `pre_zorx_post`: never emit circular
    //    `missing: zorx=1 -> sm query "zorx"` when term equals the entire folded query and n > 0!
    let (code, out_emb, err) = sm(&home, &["query", "zorx", "--repo", "anvil"]);
    assert_eq!(code, 0, "{err}");
    assert!(out_emb.contains("[m_embed]"), "{out_emb}");
    assert!(!out_emb.contains("missing: zorx="), "{out_emb}");

    // 3) Alias `k8s` -> `kubernetes`: when hit holds `kubernetes`, `why=` counts `k8s` as matched.
    let (code, out_k8s, err) = sm(
        &home,
        &["query", "k8s deploy", "--repo", "anvil", "--format", "json"],
    );
    assert_eq!(code, 0, "{err}");
    let vk: Value = serde_json::from_str(&out_k8s).unwrap();
    let hit_k8s = vk["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "m_k8s")
        .unwrap();
    assert_eq!(hit_k8s["why"]["terms_matched"], json!(["k8s", "deploy"]));
    assert_eq!(hit_k8s["why"]["terms_missing"], json!([]));
    assert_eq!(vk["missing"], json!([]));

    // 4) Alias-expanded `missing:` count: with `--limit 1`, R9 ranks `m_deploy_only` first, so
    //    `k8s` is missing from shown hits; its `in_scope` count MUST include `m_k8s` via alias!
    let (code, out_miss, err) = sm(
        &home,
        &[
            "query", "R9 k8s", "--repo", "anvil", "--limit", "1", "--format", "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let vm: Value = serde_json::from_str(&out_miss).unwrap();
    assert_eq!(vm["results"][0]["id"], json!("m_deploy_only"));
    assert_eq!(vm["missing"], json!([{"term": "k8s", "in_scope": 1}]));

    // 5) Zero-hit single-term query STILL emits `missing: nonexistent=0` (no `-> sm query` hint).
    let (code, out_zero, err) = sm(&home, &["query", "nonexistent", "--repo", "anvil"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(out_zero, "missing: nonexistent=0\n");
}

// ---------------------------------------------------------------- 4. Additive --bias scope:<path>

#[test]
fn scope_only_bias_is_additive_over_default_bias_and_respects_config_relevance() {
    let (_d, home) = scratch();
    write(&home.join("config.toml"), SCOPES_TOML);
    seed_item(
        &home,
        "m_glx_dec",
        "globex-web",
        "decision",
        "cobalt rollout plan",
        "cobalt rollout plan details alpha",
        "executed",
        "",
        "",
        "2026-11-15T10:00:00+00:00",
    );
    seed_item(
        &home,
        "m_glx_ep",
        "globex-web",
        "episode",
        "cobalt rollout plan",
        "cobalt rollout plan details bravo",
        "assumed",
        "",
        "",
        "2026-01-01T10:00:00+00:00",
    );

    // Default config + `--bias scope:acme/GLX`: keeps all 8 defaults AND appends scope:acme/GLX,
    // so decision+executed outranks episode+assumed within the same scope!
    let (code, out, err) = sm(
        &home,
        &[
            "query",
            "cobalt rollout",
            "--all",
            "--bias",
            "scope:acme/GLX",
            "--format",
            "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let v: Value = serde_json::from_str(&out).unwrap();
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
    assert_eq!(v["results"][0]["id"], json!("m_glx_dec"));
    assert_eq!(
        v["results"][0]["why"]["bias"],
        json!(["decision", "executed", "scope:acme/GLX"])
    );
    assert_eq!(v["results"][1]["id"], json!("m_glx_ep"));
    assert_eq!(v["results"][1]["why"]["bias"], json!(["scope:acme/GLX"]));

    // Config `[query] default_bias = ["relevance"]` + `--bias scope:acme/GLX` yields `["relevance", "scope:acme/GLX"]`.
    write(
        &home.join("config.toml"),
        &format!("[query]\ndefault_bias = [\"relevance\"]\n{SCOPES_TOML}"),
    );
    let (code, out_cfg_rel, err) = sm(
        &home,
        &[
            "query",
            "cobalt rollout",
            "--all",
            "--bias",
            "scope:acme/GLX",
            "--format",
            "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let vcr: Value = serde_json::from_str(&out_cfg_rel).unwrap();
    assert_eq!(vcr["bias"], json!(["relevance", "scope:acme/GLX"]));
    assert_eq!(vcr["results"][0]["why"]["bias"], json!(["scope:acme/GLX"]));

    // Deduplication: if config already includes `scope:acme/GLX`, passing `--bias scope:acme/GLX` doesn't duplicate it.
    write(
        &home.join("config.toml"),
        &format!("[query]\ndefault_bias = [\"recency\", \"scope:acme/GLX\"]\n{SCOPES_TOML}"),
    );
    let (code, out_dedup, err) = sm(
        &home,
        &[
            "query",
            "cobalt rollout",
            "--all",
            "--bias",
            "scope:acme/GLX",
            "--format",
            "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let vd: Value = serde_json::from_str(&out_dedup).unwrap();
    assert_eq!(vd["bias"], json!(["recency", "scope:acme/GLX"]));
}

// ---------------------------------------------------------------- 5. Relative --since / --until

#[test]
fn relative_date_bounds_work_in_query_and_list_and_reject_invalid_forms() {
    let (_d, home) = scratch();
    let now = clock::now_epoch();
    let ts_30m = epoch_to_as_of(now - 30.0 * 60.0).unwrap();
    let ts_3d = epoch_to_as_of(now - 3.0 * 86_400.0).unwrap();
    let ts_10d = epoch_to_as_of(now - 10.0 * 86_400.0).unwrap();

    seed_item(
        &home,
        "m_30m",
        "anvil",
        "fact",
        "quasar recent",
        "quasar 30m",
        "decided",
        "",
        "",
        &ts_30m,
    );
    seed_item(
        &home,
        "m_3d",
        "anvil",
        "fact",
        "quasar mid",
        "quasar 3d",
        "decided",
        "",
        "",
        &ts_3d,
    );
    seed_item(
        &home,
        "m_10d",
        "anvil",
        "fact",
        "quasar old",
        "quasar 10d",
        "decided",
        "",
        "",
        &ts_10d,
    );

    // `--since 7d --until 1d` keeps only m_3d.
    let (code, out, err) = sm(
        &home,
        &[
            "query", "quasar", "--repo", "anvil", "--since", "7d", "--until", "1d", "--format",
            "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let v: Value = serde_json::from_str(&out).unwrap();
    let ids: Vec<&str> = v["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["m_3d"]);

    // Case-insensitive `2H` keeps only m_30m in `sm list`.
    let (code, list_out, err) = sm(
        &home,
        &[
            "list", "--repo", "anvil", "--since", "2H", "--format", "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let lv: Value = serde_json::from_str(&list_out).unwrap();
    let list_ids: Vec<&str> = lv["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(list_ids, vec!["m_30m"]);

    // `2w` covers all three items.
    let (code, out_2w, err) = sm(
        &home,
        &[
            "query", "quasar", "--repo", "anvil", "--since", "2w", "--format", "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        serde_json::from_str::<Value>(&out_2w).unwrap()["results"]
            .as_array()
            .unwrap()
            .len(),
        3
    );

    // Failure paths: reversed range and out-of-range relative shorthands.
    let (code, _, err) = sm(
        &home,
        &[
            "query", "quasar", "--repo", "anvil", "--since", "1d", "--until", "7d",
        ],
    );
    assert_eq!(code, 1);
    assert_eq!(err.trim(), "error: --since cannot be after --until");

    for bad in ["0d", "36501d", "-7d", "7x"] {
        let (code, _, err) = sm(
            &home,
            &["query", "quasar", "--repo", "anvil", "--since", bad],
        );
        assert_eq!(code, 1, "{bad}");
        assert!(err.starts_with("error:"), "{bad}: {err}");
    }
}

// ---------------------------------------------------------------- 6. [prompt_hook] local_min_idf

#[test]
fn local_min_idf_config_defaults_override_zero_disable_and_validation() {
    let commented = config_toml(&defaults(), true).unwrap();
    assert!(
        commented.contains(
            "# Minimum matched-term IDF sum for local prompt-hook fallback recall (0.0 disables).\n# local_min_idf = 8.0\n"
        ),
        "{commented}"
    );

    let (d, home) = scratch();
    let repo = init_git_repo(d.path(), "anvil");
    // 10 items, under the 20 active items the floor needs: the default admits the rare item.
    let seed_fillers = |from: usize, to: usize| {
        for i in from..to {
            seed_item(
                &home,
                &format!("f_{i:02}"),
                "anvil",
                "fact",
                &format!("ordinary filler {i}"),
                &format!("neutral body {i}"),
                "decided",
                "",
                "",
                TS,
            );
        }
    };
    seed_fillers(0, 9);
    seed_item(
        &home,
        "m_rare",
        "anvil",
        "fact",
        "obsidian protocol",
        "obsidian protocol details",
        "decided",
        "",
        "",
        TS,
    );
    write(
        &home.join("config.toml"),
        "[catch_up]\nenabled = false\n[prompt_hook]\nmodel = \"none\"\n",
    );
    let (_, items_small, _) = local_query(
        "tell me about obsidian today",
        4,
        0,
        None,
        "anvil",
        home.to_str(),
        None,
        None,
    )
    .unwrap();
    assert_eq!(items_small.len(), 1);
    assert_eq!(items_small[0].id, "m_rare");

    // 20 items total: one rare term has IDF ln(20/1) = 2.9957 (< 8.0 default, >= 2.0 custom).
    seed_fillers(9, 19);

    // Default local_min_idf = 8.0 drops single rare word (3.00 < 8.0).
    write(
        &home.join("config.toml"),
        "[catch_up]\nenabled = false\n[prompt_hook]\nmodel = \"none\"\n",
    );
    let (_, items_def, _) = local_query(
        "tell me about obsidian today",
        4,
        0,
        None,
        "anvil",
        home.to_str(),
        None,
        None,
    )
    .unwrap();
    assert!(items_def.is_empty());

    // Lowering local_min_idf = 2.0 admits m_rare (3.00 >= 2.0), while 3.5 still rejects it.
    write(
        &home.join("config.toml"),
        "[catch_up]\nenabled = false\n[prompt_hook]\nmodel = \"none\"\nlocal_min_idf = 2.0\n",
    );
    let (_, items_low, _) = local_query(
        "tell me about obsidian today",
        4,
        0,
        None,
        "anvil",
        home.to_str(),
        None,
        None,
    )
    .unwrap();
    assert_eq!(items_low.len(), 1);
    assert_eq!(items_low[0].id, "m_rare");

    write(
        &home.join("config.toml"),
        "[catch_up]\nenabled = false\n[prompt_hook]\nmodel = \"none\"\nlocal_min_idf = 3.5\n",
    );
    let (_, items_hi, _) = local_query(
        "tell me about obsidian today",
        4,
        0,
        None,
        "anvil",
        home.to_str(),
        None,
        None,
    )
    .unwrap();
    assert!(items_hi.is_empty());

    // local_min_idf = 0.0 disables the local IDF floor completely.
    write(
        &home.join("config.toml"),
        "[catch_up]\nenabled = false\n[prompt_hook]\nmodel = \"none\"\nlocal_min_idf = 0.0\n",
    );
    let (_, items_zero, _) = local_query(
        "tell me about obsidian today",
        4,
        0,
        None,
        "anvil",
        home.to_str(),
        None,
        None,
    )
    .unwrap();
    assert_eq!(items_zero.len(), 1);
    assert_eq!(items_zero[0].id, "m_rare");

    // Invalid local_min_idf fails loud in local_min_idf_from_cfg & local_query, logs local_error on
    // hook local path, and does NOT break a working Haiku path.
    for bad in ["-0.1", "true", "nan", "inf", "\"bad\"", "[]"] {
        write(
            &home.join("config.toml"),
            &format!(
                "[catch_up]\nenabled = false\n[prompt_hook]\nmodel = \"none\"\nlocal_min_idf = {bad}\n"
            ),
        );
        let cfg = load_config(home.to_str()).unwrap();
        let err = local_min_idf_from_cfg(&cfg).unwrap_err();
        assert_eq!(
            err.msg, "[prompt_hook] local_min_idf must be a finite number >= 0",
            "{bad}"
        );
    }

    let _ = std::fs::remove_file(home.join("hook.log"));
    write(
        &home.join("config.toml"),
        "[catch_up]\nenabled = false\n[prompt_hook]\nmodel = \"none\"\nlocal_min_idf = -1.0\n",
    );
    let p = json!({"prompt": "tell me about obsidian today", "cwd": repo.to_str().unwrap()})
        .to_string();
    let (code, stdout, stderr) = hook_prompt(&home, &p);
    assert_eq!((code, stdout.as_str(), stderr.as_str()), (0, "", ""));
    let rows = hook_log_rows(&home);
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0]["local_error"]
            .as_str()
            .unwrap()
            .contains("[prompt_hook] local_min_idf must be a finite number >= 0"),
        "{}",
        rows[0]
    );
}

// ---------------------------------------------------------------- 7. Hook --in <scopes> follow-up hint

#[test]
fn hook_header_includes_in_scopes_when_matched_and_omits_when_unmatched() {
    let (d, home) = scratch();
    let repo = init_git_repo(d.path(), "anvil");
    let claude = fake_claude(d.path(), "fake-claude.sh", "walrus rollout");

    seed_item(
        &home,
        "m_glx",
        "globex-web",
        "fact",
        "walrus rollout plan",
        "walrus rollout details for GLX",
        "decided",
        "",
        "",
        TS,
    );
    seed_item(
        &home,
        "m_local",
        "anvil",
        "fact",
        "walrus rollout local",
        "walrus rollout local details",
        "decided",
        "",
        "",
        TS,
    );

    write(
        &home.join("config.toml"),
        &format!(
            "[catch_up]\nenabled = false\n\
             [prompt_hook]\nclaude_bin = \"{claude}\"\nmin_idf = 0.0\nlocal_min_idf = 0.0\n\
             [prompt_hook.projects]\nGLX = \"Globex\"\nINI = \"Initech\"\n\
             {SCOPES_TOML}"
        ),
    );

    // Haiku path with 1 matched scope -> `--in acme/GLX`.
    let p_one = json!({"prompt": "check GLX walrus rollout now", "cwd": repo.to_str().unwrap()})
        .to_string();
    let (code, out_one, err) = hook_prompt(&home, &p_one);
    assert_eq!((code, err.as_str()), (0, ""));
    let ctx_one =
        serde_json::from_str::<Value>(&out_one).unwrap()["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .to_string();
    assert!(
        ctx_one
            .lines()
            .next()
            .unwrap()
            .contains("`sm query \"walrus rollout\" --in acme/GLX --limit 10`"),
        "{ctx_one}"
    );

    // Haiku path with 2 matched scopes -> `--in acme/GLX,initech`.
    let p_two =
        json!({"prompt": "check GLX and INI walrus rollout", "cwd": repo.to_str().unwrap()})
            .to_string();
    let (code, out_two, err) = hook_prompt(&home, &p_two);
    assert_eq!((code, err.as_str()), (0, ""));
    let ctx_two =
        serde_json::from_str::<Value>(&out_two).unwrap()["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .to_string();
    assert!(
        ctx_two
            .lines()
            .next()
            .unwrap()
            .contains("`sm query \"walrus rollout\" --in acme/GLX,initech --limit 10`"),
        "{ctx_two}"
    );

    // Haiku path with NO matched scope -> exact existing hint without `--in`.
    let p_none =
        json!({"prompt": "check generic walrus rollout now", "cwd": repo.to_str().unwrap()})
            .to_string();
    let (code, out_none, err) = hook_prompt(&home, &p_none);
    assert_eq!((code, err.as_str()), (0, ""));
    let ctx_none = serde_json::from_str::<Value>(&out_none).unwrap()["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        ctx_none
            .lines()
            .next()
            .unwrap()
            .contains("`sm query \"walrus rollout\" --limit 10`"),
        "{ctx_none}"
    );
    assert!(
        !ctx_none.lines().next().unwrap().contains(" --in "),
        "{ctx_none}"
    );

    // Local fallback path (`model = "none"`) with matched scope -> also includes `--in acme/GLX`.
    write(
        &home.join("config.toml"),
        &format!(
            "[catch_up]\nenabled = false\n\
             [prompt_hook]\nmodel = \"none\"\nlocal_min_idf = 0.0\n\
             [prompt_hook.projects]\nGLX = \"Globex\"\n\
             {SCOPES_TOML}"
        ),
    );
    let (code, out_loc, err) = hook_prompt(&home, &p_one);
    assert_eq!((code, err.as_str()), (0, ""));
    let ctx_loc =
        serde_json::from_str::<Value>(&out_loc).unwrap()["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .to_string();
    assert!(
        ctx_loc
            .lines()
            .next()
            .unwrap()
            .contains("--in acme/GLX --limit 10`"),
        "{ctx_loc}"
    );
}

// ---------------------------------------------------------------- 8. Config-aware backoff invalidation

#[test]
fn backoff_auto_invalidates_when_rewrite_config_changes_and_stays_active_otherwise() {
    let (d, home) = scratch();
    let repo = init_git_repo(d.path(), "anvil");
    seed_item(
        &home,
        "m_1",
        "anvil",
        "fact",
        "zebra failover note",
        "zebra failover body",
        "decided",
        "",
        "",
        TS,
    );

    let set_hook_cfg = |cmd: &str, model: &str, timeout: f64| {
        write(
            &home.join("config.toml"),
            &format!(
                "[catch_up]\nenabled = false\n\
                 [prompt_hook]\nclaude_bin = \"{cmd}\"\nmodel = \"{model}\"\ntimeout = {timeout}\nmin_idf = 0.0\nlocal_min_idf = 0.0\n"
            ),
        );
    };

    let make_payload = |prompt: &str| {
        PyValue::Dict(vec![
            (s("prompt"), s(prompt)),
            (s("cwd"), s(repo.to_str().unwrap())),
        ])
    };

    let fail_runner = |_c: &[String], _i: &str, _t: f64, _e: &[(&str, &str)], _w: Option<&str>| {
        Ok(Completed {
            returncode: 1,
            stdout: String::new(),
            stderr: "boom".into(),
        })
    };
    let ok_runner = |_c: &[String], _i: &str, _t: f64, _e: &[(&str, &str)], _w: Option<&str>| {
        Ok(Completed {
            returncode: 0,
            stdout: "zebra failover\n".into(),
            stderr: String::new(),
        })
    };

    let bo_path = home.join("hook").join("backoff.json");

    // 2 failures open backoff recording cmd/model/timeout.
    set_hook_cfg("/bad/claude", "haiku", 8.0);
    run_prompt_hook(
        &make_payload("why zebra failover broke"),
        false,
        home.to_str(),
        &fail_runner,
    )
    .unwrap();
    run_prompt_hook(
        &make_payload("why zebra failover broke"),
        false,
        home.to_str(),
        &fail_runner,
    )
    .unwrap();
    let bo = Backoff::read(&PyPath::new(bo_path.to_str().unwrap())).unwrap();
    assert_eq!(bo.failures, 2);
    assert_eq!(bo.rewrite_cmd.as_deref(), Some("/bad/claude"));
    assert_eq!(bo.rewrite_model.as_deref(), Some("haiku"));
    assert_eq!(bo.rewrite_timeout, Some(8.0));

    // Unchanged config keeps backoff active (ok_runner is NOT invoked).
    let calls = std::cell::Cell::new(0usize);
    let counting_ok = |c: &[String], i: &str, t: f64, e: &[(&str, &str)], w: Option<&str>| {
        calls.set(calls.get() + 1);
        ok_runner(c, i, t, e, w)
    };
    let same = run_prompt_hook(
        &make_payload("why zebra failover broke"),
        false,
        home.to_str(),
        &counting_ok,
    )
    .unwrap()
    .unwrap();
    assert_eq!(same.fallback, "backoff");
    assert_eq!(calls.get(), 0);

    // Changing each of (claude_bin, model, timeout) invalidates backoff and lets ok_runner succeed!
    for (new_cmd, new_model, new_timeout) in [
        ("/good/claude", "haiku", 8.0),
        ("/bad/claude", "sonnet", 8.0),
        ("/bad/claude", "haiku", 15.0),
    ] {
        // Re-seed active backoff for ("/bad/claude", "haiku", 8.0).
        Backoff {
            failures: 2,
            until: clock::now_epoch() + 600.0,
            error: "boom".into(),
            cause: "error".into(),
            notified: Vec::new(),
            rewrite_cmd: Some("/bad/claude".into()),
            rewrite_model: Some("haiku".into()),
            rewrite_timeout: Some(8.0),
        }
        .write(&PyPath::new(bo_path.to_str().unwrap()))
        .unwrap();

        set_hook_cfg(new_cmd, new_model, new_timeout);
        calls.set(0);
        let res = run_prompt_hook(
            &make_payload("why zebra failover broke"),
            false,
            home.to_str(),
            &counting_ok,
        )
        .unwrap()
        .unwrap();
        assert_eq!(res.fallback, "", "({new_cmd}, {new_model}, {new_timeout})");
        assert_eq!(calls.get(), 1, "({new_cmd}, {new_model}, {new_timeout})");
        assert!(!bo_path.exists(), "({new_cmd}, {new_model}, {new_timeout})");
    }

    // Changing config after failure #1 resets failure streak back to 1 instead of opening backoff.
    set_hook_cfg("/bad/claude", "haiku", 8.0);
    run_prompt_hook(
        &make_payload("why zebra failover broke"),
        false,
        home.to_str(),
        &fail_runner,
    )
    .unwrap();
    set_hook_cfg("/other/claude", "haiku", 8.0);
    run_prompt_hook(
        &make_payload("why zebra failover broke"),
        false,
        home.to_str(),
        &fail_runner,
    )
    .unwrap();
    let bo_reset = Backoff::read(&PyPath::new(bo_path.to_str().unwrap())).unwrap();
    assert_eq!(bo_reset.failures, 1);
    assert_eq!(bo_reset.until, 0.0);
    assert_eq!(bo_reset.rewrite_cmd.as_deref(), Some("/other/claude"));
}

// ---------------------------------------------------------------- 9. Clearer tag & bias errors

#[test]
fn clearer_tag_and_bias_errors_list_valid_options() {
    let (_d, home) = scratch();
    let (code, _, err) = sm(&home, &["put", "--repo", "anvil", "--tag", "", "some body"]);
    assert_eq!(code, 1);
    assert_eq!(
        err.trim(),
        "error: Evidence tag is required; expected one of executed, verified, decided, client-stated, derived, assumed, prior, planned"
    );

    let (code, _, err) = sm(&home, &["query", "kiwi", "--bias", "bogus"]);
    assert_eq!(code, 2);
    assert!(
        err.contains(
            "Invalid bias 'bogus'; expected one of recency, relevance, executed, verified, decided, client-stated, derived, assumed, prior, planned, fact, decision, learning, episode, task, handoff, or scope:<path>"
        ),
        "{err}"
    );
}

#[test]
fn unquoted_dash_exclusions_snake_subtoken_why_and_exact_snake_exclusion() {
    let (_d, home) = scratch();
    // 1. Unquoted `-staging` and `-http` (not colliding with `-h` help) work directly in CLI.
    let (c1, _, e1) = sm(
        &home,
        &[
            "put",
            "--repo",
            "anvil",
            "--tag",
            "decided",
            "connection pool production config",
        ],
    );
    assert_eq!(c1, 0, "{e1}");
    let (c2, _, e2) = sm(
        &home,
        &[
            "put",
            "--repo",
            "anvil",
            "--tag",
            "decided",
            "connection pool staging config",
        ],
    );
    assert_eq!(c2, 0, "{e2}");
    let (c3, _, e3) = sm(
        &home,
        &[
            "put",
            "--repo",
            "anvil",
            "--tag",
            "decided",
            "connection pool http fallback",
        ],
    );
    assert_eq!(c3, 0, "{e3}");

    let (code, out, err) = sm(
        &home,
        &[
            "search",
            "connection",
            "pool",
            "-staging",
            "-http",
            "--repo",
            "anvil",
            "--since",
            "7d",
            "--format",
            "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let v: Value = serde_json::from_str(&out).unwrap();
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "{out}");
    assert!(
        results[0]["title"].as_str().unwrap().contains("production"),
        "{out}"
    );

    // Leading `-staging` with no positive query term still fails with required query error;
    // single-letter `-z` and `--bogus` still fail as unrecognized arguments.
    let (code, _, _) = sm(&home, &["query", "--repo", "anvil", "-staging"]);
    assert_eq!(code, 2);
    let (code, _, err) = sm(&home, &["query", "connection", "-z", "--repo", "anvil"]);
    assert_eq!(code, 2);
    assert!(err.contains("unrecognized arguments: -z"), "{err}");

    // 2. Bare query term matching inside a snake_case haystack token reports full terms match and no false missing.
    let (c4, _, e4) = sm(
        &home,
        &[
            "put",
            "--repo",
            "anvil",
            "--tag",
            "decided",
            "alpha wrapper contains pre_zorx_post identifier",
        ],
    );
    assert_eq!(c4, 0, "{e4}");
    let (code, out, err) = sm(&home, &["query", "alpha zorx", "--repo", "anvil"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("why=terms:2/2"), "{out}");
    assert!(!out.contains("missing:"), "{out}");

    // 3. Snake_case exclusion `-rate_limit` excludes exact `rate_limit` / adjacent `rate limit`,
    //    NOT unrelated `rates ... limited`.
    let (c5, _, e5) = sm(
        &home,
        &[
            "put",
            "--repo",
            "anvil",
            "--tag",
            "decided",
            "quasar rates are high; access is limited for guests",
        ],
    );
    assert_eq!(c5, 0, "{e5}");
    let (c6, _, e6) = sm(
        &home,
        &[
            "put",
            "--repo",
            "anvil",
            "--tag",
            "decided",
            "quasar rate_limit set to 100 rps",
        ],
    );
    assert_eq!(c6, 0, "{e6}");
    let (c7, _, e7) = sm(
        &home,
        &[
            "put",
            "--repo",
            "anvil",
            "--tag",
            "decided",
            "quasar rate limit bucket configuration",
        ],
    );
    assert_eq!(c7, 0, "{e7}");
    let (code, out, err) = sm(
        &home,
        &[
            "query",
            "quasar",
            "-rate_limit",
            "--repo",
            "anvil",
            "--format",
            "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let v: Value = serde_json::from_str(&out).unwrap();
    let titles: Vec<&str> = v["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["title"].as_str().unwrap())
        .collect();
    assert_eq!(
        titles,
        vec!["quasar rates are high; access is limited for guests"]
    );
}

fn result_ids(out: &str) -> (Vec<String>, Value) {
    let v: Value = serde_json::from_str(out).unwrap();
    let ids = v["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    (ids, v)
}

#[test]
fn exclusions_reach_matches_past_a_pool_the_excluded_word_fills() {
    let (_d, home) = scratch();
    let conn = open(&home);
    let seed = |id: &str, body: &str, as_of: &str| {
        let mut it = Item::bare(id, "anvil", "fact", "", body, "verified");
        it.evidence = "made-up check".to_string();
        it.as_of = as_of.to_string();
        put_item(&conn, &it.post_init(), None).unwrap();
    };
    // 110 short matches of both words (best bm25, newest) all hold the excluded word: they
    // fill the 100-row biased pool and the 50-row recent AND pool on their own.
    for i in 0..110 {
        seed(
            &format!("m_x{i:03}"),
            &format!("widget gadget legacyx case{i}"),
            TS,
        );
    }
    for i in 1..=3 {
        seed(
            &format!("m_keep{i}"),
            &format!("widget gadget kept item {i} with a much longer body so its bm25 ranks lower"),
            "2026-10-01T10:00:00+00:00",
        );
    }
    // 100 newer one-word matches fill a widened --recent OR pool before it reaches m_keep*,
    // so --recent finds m_keep* only through its AND pool
    for i in 0..100 {
        seed(&format!("m_one{i:03}"), &format!("widget alone {i}"), TS);
    }
    let keep = ["m_keep1", "m_keep2", "m_keep3"].map(String::from).to_vec();
    let ones: Vec<String> = (0..100).map(|i| format!("m_one{i:03}")).collect();
    for (recent, want) in [(false, [keep.clone(), ones].concat()), (true, keep)] {
        let mut args = vec![
            "query",
            "widget gadget -legacyx",
            "--repo",
            "anvil",
            "--limit",
            "200",
            "--format",
            "json",
        ];
        if recent {
            args.push("--recent");
        }
        let (code, out, err) = sm(&home, &args);
        assert_eq!(code, 0, "{err}");
        let (mut ids, v) = result_ids(&out);
        ids.sort();
        assert_eq!(ids, want, "recent={recent}");
        // every match is in the pool, so the footer must not call it full
        assert_eq!(v["more"]["pool_full"], json!(false), "recent={recent}");
    }
}

#[test]
fn an_item_with_every_query_word_is_ranked_past_a_pool_of_one_word_matches() {
    let (_d, home) = scratch();
    let conn = open(&home);
    let seed = |id: &str, body: &str| {
        let mut it = Item::bare(id, "anvil", "fact", "", body, "verified");
        it.evidence = "made-up check".to_string();
        it.as_of = TS.to_string();
        put_item(&conn, &it.post_init(), None).unwrap();
    };
    for i in 0..60 {
        seed(&format!("m_d{i:02}"), &format!("deploy step {i}"));
        seed(&format!("m_m{i:02}"), &format!("migration step {i}"));
    }
    let mut long = "The long incident note: we ran the deploy and then the migration".to_string();
    for w in 1..=40 {
        long.push_str(&format!(" and checked shard{w}"));
    }
    seed("m_both", &long);
    let (code, out, err) = sm(
        &home,
        &[
            "query",
            "deploy migration",
            "--repo",
            "anvil",
            "--limit",
            "200",
            "--format",
            "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let (ids, v) = result_ids(&out);
    assert!(ids.iter().any(|id| id == "m_both"), "{ids:?}");
    // the one-word matches still overflow the OR pool
    assert_eq!(v["more"]["pool_full"], json!(true));
}

fn more_line(home: &Path, args: &[&str]) -> String {
    let (code, out, err) = sm(home, args);
    assert_eq!(code, 0, "{err}");
    out.lines()
        .find(|l| l.starts_with("more: "))
        .unwrap_or_else(|| panic!("no more: line in {out}"))
        .to_string()
}

#[test]
fn the_footer_names_the_pool_size_after_a_widened_or_and_extended_fetch() {
    let (_d, home) = scratch();
    let conn = open(&home);
    let seed = |id: &str, body: &str| {
        let mut it = Item::bare(id, "anvil", "fact", "", body, "verified");
        it.evidence = "made-up check".to_string();
        it.as_of = TS.to_string();
        put_item(&conn, &it.post_init(), None).unwrap();
    };
    // 110 best matches hold the excluded word, so the 100-row pool is fetched again 400 wide;
    // 400 of the 410 rows fill it and 290 of them stay
    for i in 0..110 {
        seed(&format!("m_x{i:03}"), &format!("widget legacyx case{i}"));
    }
    for i in 0..300 {
        seed(
            &format!("m_p{i:03}"),
            &format!("widget plain item number {i}"),
        );
    }
    // 60 + 60 one-word rows fill the 100-row OR pool; the AND form adds m_both as row 101
    for i in 0..60 {
        seed(&format!("m_d{i:02}"), &format!("deploy step {i}"));
        seed(&format!("m_m{i:02}"), &format!("migration step {i}"));
    }
    seed(
        "m_both",
        "a longer note: we ran the deploy and then the migration on the cluster",
    );
    let widened = more_line(&home, &["query", "widget -legacyx", "--repo", "anvil"]);
    assert!(
        widened.starts_with("more: 286+ beyond --limit 4 (pool of 400 full) -> "),
        "{widened}"
    );
    let extended = more_line(&home, &["query", "deploy migration", "--repo", "anvil"]);
    assert!(
        extended.starts_with("more: 97+ beyond --limit 4 (pool of 101 full) -> "),
        "{extended}"
    );
}

#[test]
fn an_exclusion_over_more_matches_than_sqlite_binds_in_one_statement_still_answers() {
    let (_d, home) = scratch();
    let conn = open(&home);
    // 33,000 matches pass SQLite's 32,766 bound variables; the exclusion removes all but m_keep,
    // so the pool widens until one fetch holds every match
    conn.execute_batch(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 33000)
         INSERT INTO items (id, repo, kind, title, body, tag, as_of, content_hash)
         SELECT 'm_x' || i, 'anvil', 'fact', '', 'alphaq betaqq row ' || i, 'verified',
                '2026-10-25T10:00:00+00:00', 'h' || i FROM n;
         INSERT INTO items (id, repo, kind, title, body, tag, as_of, content_hash)
         VALUES ('m_keep', 'anvil', 'fact', '', 'alphaq kept row', 'verified',
                 '2026-10-01T10:00:00+00:00', 'hk');",
    )
    .unwrap();
    for recent in [false, true] {
        let mut args = vec![
            "query",
            "alphaq -betaqq",
            "--repo",
            "anvil",
            "--format",
            "json",
        ];
        if recent {
            args.push("--recent");
        }
        let (code, out, err) = sm(&home, &args);
        assert_eq!(code, 0, "recent={recent}: {err}");
        let (ids, v) = result_ids(&out);
        assert_eq!(ids, vec!["m_keep"], "recent={recent}");
        assert_eq!(v["more"]["pool_full"], json!(false), "recent={recent}");
    }
}

// ---------------------------------------------------------------- 10. Footer truth on hidden items and filler words

fn put_item_cli(home: &Path, args: &[&str]) -> String {
    let mut a = vec!["put", "--evidence", "made-up check"];
    a.extend_from_slice(args);
    let (code, out, err) = sm(home, &a);
    assert_eq!(code, 0, "put failed: {err}");
    out.trim().to_string()
}

/// etl: a retired Fivetran decision, a Snowflake decision, a DAG fact, a filler fact; billing: two
/// Stripe facts.
fn etl_store() -> (tempfile::TempDir, PathBuf, [String; 4]) {
    let (d, home) = scratch();
    write(&home.join("config.toml"), "[catch_up]\nenabled = false\n");
    let item = |kind: &str, repo: &str, as_of: &str, body: &str| {
        put_item_cli(
            &home,
            &[
                "--kind", kind, "--tag", "verified", "--repo", repo, "--as-of", as_of, "--", body,
            ],
        )
    };
    let fivetran = item(
        "decision",
        "etl",
        "2026-06-03T10:00:00Z",
        "Use Fivetran for Shopify ingestion into BigQuery",
    );
    let (code, _, err) = sm(&home, &["retire", &fivetran, "--reason", "moved"]);
    assert_eq!(code, 0, "{err}");
    let snowflake = item(
        "decision",
        "etl",
        "2026-07-16T10:00:00Z",
        "Snowflake trial ended and we stay on BigQuery",
    );
    let dag = item(
        "fact",
        "etl",
        "2026-07-20T10:00:00Z",
        "The nightly DAG runs on Cloud Composer",
    );
    let filler = item("fact", "etl", "2026-07-21T10:00:00Z", "We moved on");
    item(
        "fact",
        "billing",
        "2026-07-01T10:00:00Z",
        "Stripe retries a failed webhook for 3 days",
    );
    item(
        "fact",
        "billing",
        "2026-07-02T10:00:00Z",
        "Stripe webhooks are signed",
    );
    (d, home, [fivetran, snowflake, dag, filler])
}

fn query_ok(home: &Path, args: &[&str]) -> String {
    let mut a = vec!["query"];
    a.extend_from_slice(args);
    let (code, out, err) = sm(home, &a);
    assert_eq!((code, err.as_str()), (0, ""), "{args:?}");
    out
}

#[test]
fn missing_names_the_items_the_status_or_a_filter_hid_and_what_shows_them() {
    let (_d, home, _) = etl_store();
    let q = |args: &[&str]| query_ok(&home, args);
    assert_eq!(
        q(&["fivetran", "--repo", "etl"]),
        "missing: fivetran: 1 retired or superseded -> sm query \"fivetran\" --repo etl --include-history\n"
    );
    assert_eq!(
        q(&["snowflake", "--repo", "etl", "--since", "7d"]),
        "missing: snowflake: 1 outside --since 7d -> sm query \"snowflake\" --repo etl\n"
    );
    assert_eq!(
        q(&["snowflake", "--repo", "etl", "--kind", "fact"]),
        "missing: snowflake: 1 outside --kind fact -> sm query \"snowflake\" --repo etl\n"
    );
    assert_eq!(
        q(&["fivetran", "--repo", "etl", "--kind", "fact"]),
        "missing: fivetran: 1 retired or superseded, outside --kind fact -> sm query \"fivetran\" --repo etl --include-history\n"
    );
    // nothing in scope holds it: term=0 stays
    assert_eq!(q(&["airbyte", "--repo", "etl"]), "missing: airbyte=0\n");
    // a stopword gets no suggested query
    assert_eq!(
        q(&["the", "--repo", "etl", "--kind", "decision"]),
        "missing: the: 1 outside --kind decision\n"
    );
    let footer = q(&[
        "we", "the", "--repo", "etl", "--limit", "1", "--format", "brief",
    ]);
    let missing = footer.lines().find(|l| l.starts_with("missing: ")).unwrap();
    assert!(!missing.contains("->"), "{footer}");
    // the JSON keeps its keys and its count, and adds what the text line says
    let v: Value =
        serde_json::from_str(&q(&["fivetran", "--repo", "etl", "--format", "json"])).unwrap();
    assert_eq!(
        v["missing"],
        json!([{"term": "fivetran", "in_scope": 0, "hidden": 1, "hidden_by": "retired or superseded"}])
    );
    let missing = |args: &[&str]| -> Value {
        serde_json::from_str::<Value>(&q(args)).unwrap()["missing"].clone()
    };
    assert_eq!(
        missing(&[
            "snowflake",
            "--repo",
            "etl",
            "--kind",
            "fact",
            "--format",
            "json"
        ]),
        json!([{"term": "snowflake", "in_scope": 0, "hidden": 1, "hidden_by": "outside --kind fact"}])
    );
    assert_eq!(
        missing(&["airbyte", "--repo", "etl", "--format", "json"]),
        json!([{"term": "airbyte", "in_scope": 0}])
    );
    let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "query", "bias", "scope", "results", "missing", "more", "refs", "next"
        ]
    );
}

#[test]
fn a_hit_that_matched_only_filler_words_is_not_listed() {
    let (_d, home, [_, snowflake, dag, filler]) = etl_store();
    // no topic word in the repo: the pointer to the other repo comes first
    assert_eq!(
        query_ok(
            &home,
            &["how do we handle stripe webhooks", "--repo", "etl"]
        ),
        "more: other repos billing=2 -> sm query \"how do we handle stripe webhooks\" --all\n\
         missing: handle=0; stripe: 2 in other repos -> sm query \"stripe\" --all; webhooks: 1 in other repos -> sm query \"webhooks\" --all\n"
    );
    // the dag matches 3 of 4 words, the snowflake note 2, the filler note only "on"
    let ids = |args: &[&str]| -> Vec<String> {
        let v: Value = serde_json::from_str(&query_ok(&home, args)).unwrap();
        v["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        ids(&[
            "on composer cloud bigquery",
            "--repo",
            "etl",
            "--format",
            "json"
        ]),
        [dag.clone(), snowflake.clone()]
    );
    // a query of only stopwords keeps every hit
    let mut all = ids(&["we on", "--repo", "etl", "--format", "json"]);
    all.sort();
    let mut want = vec![snowflake, dag, filler];
    want.sort();
    assert_eq!(all, want);
}

/// alpha: 110 notes and beta: 40 notes made of filler words, plus one refund note in each repo.
fn filler_pool_store() -> (tempfile::TempDir, PathBuf) {
    let (d, home) = scratch();
    write(&home.join("config.toml"), "[catch_up]\nenabled = false\n");
    let note = |id: String, repo: &str, body: String| {
        seed_item(&home, &id, repo, "fact", "", &body, "verified", "", "", TS)
    };
    for i in 0..110 {
        note(
            format!("m_a{i}"),
            "alpha",
            format!("Note {i}: what is in the build cache for the lint step"),
        );
    }
    for i in 0..40 {
        note(
            format!("m_b{i}"),
            "beta",
            format!("Note {i}: what is in the deploy queue for the web step"),
        );
    }
    note(
        "m_refund".into(),
        "alpha",
        "Refunds over 5000 USD need a second approver".into(),
    );
    note(
        "m_brefund".into(),
        "beta",
        "Refunds in beta wait a day".into(),
    );
    (d, home)
}

#[test]
fn a_pool_full_of_filler_hits_counts_only_the_hits_the_list_shows() {
    let (_d, home) = filler_pool_store();
    let q = "what is the rule for refunds that are over the limit";
    let footer = |args: &[&str]| -> Vec<String> {
        query_ok(&home, args)
            .lines()
            .filter(|l| !l.starts_with('['))
            .map(String::from)
            .collect()
    };
    // the pool of 100 is full only of filler notes: nothing lies beyond it, and beta shows 1 hit
    assert_eq!(
        footer(&["--repo", "alpha", "--format", "brief", "--", q]),
        [
            "missing: rule=0; limit=0".to_string(),
            format!("more: other repos beta=1 -> sm query \"{q}\" --all"),
            "next: sm show m_refund; sm related m_refund".to_string(),
        ]
    );
    let more = |args: &[&str]| -> Value {
        let v: Value = serde_json::from_str(&query_ok(&home, args)).unwrap();
        v["more"].clone()
    };
    assert_eq!(
        more(&["--repo", "alpha", "--format", "json", "--", q]),
        json!({"beyond_limit": 0, "pool_full": false, "other_repos": {"beta": 1}, "other_repos_pool_full": false})
    );
    // --all lists the two refund notes and announces nothing more
    assert_eq!(
        footer(&["--all", "--format", "brief", "--", q])[1..],
        ["next: sm show m_refund m_brefund; sm related m_refund"]
    );
    assert_eq!(
        more(&["--all", "--format", "json", "--", q]),
        json!({"beyond_limit": 0, "pool_full": false, "other_repos": {}, "other_repos_pool_full": false})
    );
}

#[test]
fn a_pool_full_of_topic_hits_still_counts_past_the_limit_when_filler_hits_drop() {
    let (_d, home) = scratch();
    write(&home.join("config.toml"), "[catch_up]\nenabled = false\n");
    for i in 0..120 {
        let body = format!("Refund case {i}");
        seed_item(
            &home,
            &format!("m_r{i}"),
            "alpha",
            "fact",
            "",
            &body,
            "verified",
            "",
            "",
            TS,
        );
    }
    for i in 0..10 {
        let body = format!("what is in the cache {i}");
        seed_item(
            &home,
            &format!("m_f{i}"),
            "alpha",
            "fact",
            "",
            &body,
            "verified",
            "",
            "",
            TS,
        );
    }
    // the 10 filler notes rank inside the pool of 100 and drop out: 90 refund notes stay listed
    // or counted, and 30 more refund notes lie past the pool
    let out = query_ok(
        &home,
        &["what is the refund", "--repo", "alpha", "--format", "brief"],
    );
    let more: Vec<&str> = out.lines().filter(|l| l.starts_with("more:")).collect();
    assert_eq!(
        more,
        [
            "more: 86+ beyond --limit 4 (pool of 100 full) -> sm query \"what is the refund\" --repo alpha --limit 14"
        ]
    );
}

#[test]
fn missing_names_a_term_that_only_other_repos_hold_after_the_other_repos_line() {
    let (_d, home, _) = etl_store();
    let q = |args: &[&str]| query_ok(&home, args);
    assert_eq!(
        q(&["stripe webhook", "--repo", "etl"]),
        "more: other repos billing=2 -> sm query \"stripe webhook\" --all\n\
         missing: stripe: 2 in other repos -> sm query \"stripe\" --all; webhook: 2 in other repos -> sm query \"webhook\" --all\n"
    );
    // the filters go along to the other repos
    assert_eq!(
        q(&["stripe", "--repo", "etl", "--kind", "fact"]),
        "more: other repos billing=2 -> sm query \"stripe\" --all --kind fact\n\
         missing: stripe: 2 in other repos -> sm query \"stripe\" --all --kind fact\n"
    );
    // held nowhere: term=0 stays, and --all names no other repo
    assert_eq!(q(&["airbyte", "--all"]), "missing: airbyte=0\n");
    let v: Value =
        serde_json::from_str(&q(&["stripe webhook", "--repo", "etl", "--format", "json"])).unwrap();
    assert_eq!(
        v["missing"],
        json!([
            {"term": "stripe", "in_scope": 0, "in_other_repos": 2},
            {"term": "webhook", "in_scope": 0, "in_other_repos": 2}
        ])
    );
}

#[test]
fn query_names_an_unknown_repo_a_dropped_repo_and_a_config_with_no_scopes() {
    let (_d, home) = scratch();
    write(&home.join("config.toml"), "[catch_up]\nenabled = false\n");
    for repo in [
        "kestrel-etl",
        "kestrel-etl",
        "lumen-app",
        "orion",
        "a",
        "b",
        "c",
    ] {
        put_item_cli(
            &home,
            &[
                "--kind",
                "fact",
                "--tag",
                "verified",
                "--repo",
                repo,
                "--",
                &format!("note in {repo}"),
            ],
        );
    }
    let (code, _, err) = sm(&home, &["query", "note", "--repo", "kestrel"]);
    assert_eq!(
        (code, err.as_str()),
        (
            0,
            "no items in repo 'kestrel'; known repos: kestrel-etl, a, b, c, lumen-app (+1 more)\n"
        )
    );
    let (code, _, err) = sm(&home, &["query", "note", "--repo", "kestrel-etl"]);
    assert_eq!((code, err.as_str()), (0, ""));
    let (code, out, err) = sm(&home, &["query", "note", "--all", "--repo", "kestrel-etl"]);
    assert_eq!(
        (code, err.as_str()),
        (
            0,
            "--all searches every repo, so --repo kestrel-etl is not used\n"
        )
    );
    assert_eq!(out, sm(&home, &["query", "note", "--all"]).1);
    let (code, _, err) = sm(&home, &["query", "note", "--in", "acme"]);
    assert_eq!(
        (code, err),
        (
            2,
            format!(
                "unknown scope: acme; {}/config.toml has no [scopes] table (README: Multi-repo scopes and maintenance)\n",
                home.display()
            )
        )
    );
    write(
        &home.join("config.toml"),
        "[catch_up]\nenabled = false\n[scopes]\nacme = [\"repo:orion\"]\n",
    );
    let (code, _, err) = sm(&home, &["query", "note", "--in", "nope"]);
    assert_eq!(
        (code, err.as_str()),
        (2, "unknown scope: nope; known: acme\n")
    );
}
