//! `[scopes]` (group/project paths in config.toml) through the `sm` binary, as its user
//! runs it: `sm query --in`, the `sm stats` scopes block and `sm config`. Every name is made up.

mod common;

use common::{scratch, sm, write};
use serde_json::{Value, json};
use simple_memory::core::item::Item;
use simple_memory::store::connect::connect;
use simple_memory::store::items::put_item;
use std::path::Path;

const TS: &str = "2026-10-25T10:00:00+00:00";

const SCOPES: &str = "[scopes]\n\
acme = [\"repo:acme-ops\"]\n\
\"acme/globex\" = [\"GLX\"]\n\
\"acme/globex/web\" = [\"repo:globex-web\"]\n\
initech = [\"INI\"]\n";

/// (id, repo, title, body, refs, tags, status)
type Row<'a> = (
    &'a str,
    &'a str,
    &'a str,
    &'a str,
    &'a str,
    &'a str,
    &'a str,
);

fn seed(home: &Path, rows: &[Row]) {
    let conn = connect(home.join("memory.db").to_str().unwrap(), None).unwrap();
    for (id, repo, title, body, refs, tags, status) in rows {
        let mut it = Item::bare(id, repo, "fact", title, body, "derived");
        it.refs = (*refs).to_string();
        it.tags = (*tags).to_string();
        it.as_of = TS.to_string();
        it.status = (*status).to_string();
        put_item(&conn, &it.post_init(), None).unwrap();
    }
}

/// Every item holds "kiwi". Which path holds which item, by hand:
/// m_ops (repo acme-ops): acme. m_glx (GLX in the body): acme/globex, acme.
/// m_web (repo globex-web): acme/globex/web, acme/globex, acme. m_ref (GLX-1 in refs: `-` is
/// no word letter): acme/globex, acme. m_tags (GLX in tags): acme/globex, acme.
/// m_ini (INI in the title): initech. Outside: m_long (GLXY), m_low (glx), m_acc (GLXé: é is a
/// word letter). m_old (GLX, superseded) is in acme/globex but not active.
fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let (d, home) = scratch();
    write(&home.join("config.toml"), SCOPES);
    seed(
        &home,
        &[
            (
                "m_ops",
                "acme-ops",
                "kiwi ops",
                "kiwi ops notes",
                "",
                "",
                "active",
            ),
            (
                "m_glx",
                "misc",
                "kiwi rollout",
                "GLX kiwi rollout",
                "",
                "",
                "active",
            ),
            (
                "m_web",
                "globex-web",
                "kiwi web",
                "kiwi web build",
                "",
                "",
                "active",
            ),
            (
                "m_ref",
                "misc",
                "kiwi ref",
                "kiwi ref note",
                "GLX-1",
                "",
                "active",
            ),
            (
                "m_tags",
                "misc",
                "kiwi tagged",
                "kiwi tagged note",
                "",
                "GLX",
                "active",
            ),
            ("m_ini", "misc", "INI kiwi", "kiwi plan", "", "", "active"),
            ("m_long", "misc", "kiwi long", "GLXY kiwi", "", "", "active"),
            ("m_low", "misc", "kiwi low", "glx kiwi", "", "", "active"),
            ("m_acc", "misc", "kiwi acc", "GLXé kiwi", "", "", "active"),
            (
                "m_old",
                "misc",
                "kiwi old",
                "GLX kiwi old",
                "",
                "",
                "superseded",
            ),
        ],
    );
    (d, home)
}

fn query_json(home: &Path, extra: &[&str]) -> Value {
    let mut args = vec!["query", "kiwi", "--format", "json", "--limit", "20"];
    args.extend_from_slice(extra);
    let (code, out, err) = sm(home, &args);
    assert_eq!(code, 0, "{err}");
    serde_json::from_str(&out).unwrap()
}

fn ids(v: &Value) -> Vec<String> {
    let mut ids: Vec<String> = v["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    ids
}

#[test]
fn in_keeps_the_items_of_each_path_and_the_paths_below_it() {
    let (_d, home) = fixture();
    assert_eq!(
        ids(&query_json(&home, &["--in", "acme/globex"])),
        ["m_glx", "m_ref", "m_tags", "m_web"]
    );
    assert_eq!(
        ids(&query_json(&home, &["--in", "acme"])),
        ["m_glx", "m_ops", "m_ref", "m_tags", "m_web"]
    );
    assert_eq!(
        ids(&query_json(&home, &["--in", "acme/globex/web"])),
        ["m_web"]
    );
    // a comma list and a repeated flag are the same union
    let listed = query_json(&home, &["--in", "initech,acme/globex/web"]);
    assert_eq!(ids(&listed), ["m_ini", "m_web"]);
    assert_eq!(
        ids(&query_json(
            &home,
            &["--in", "initech", "--in", "acme/globex/web"]
        )),
        ["m_ini", "m_web"]
    );
    // every repo is searched, as with --all, and the JSON names the paths under scope
    assert_eq!(
        listed["scope"],
        json!({"repo": null, "all_repos": true, "in": ["initech", "acme/globex/web"]})
    );
    // with --include-history the superseded GLX item is in the path too
    assert_eq!(
        ids(&query_json(
            &home,
            &["--in", "acme/globex", "--include-history"]
        )),
        ["m_glx", "m_old", "m_ref", "m_tags", "m_web"]
    );
}

#[test]
fn in_works_with_kind_tag_bias_recent_and_brief() {
    let (_d, home) = fixture();
    assert_eq!(
        ids(&query_json(&home, &["--in", "acme", "--kind", "decision"])),
        Vec::<String>::new()
    );
    assert_eq!(
        ids(&query_json(&home, &["--in", "initech", "--tag", "derived"])),
        ["m_ini"]
    );
    assert_eq!(
        ids(&query_json(
            &home,
            &["--in", "initech", "--bias", "relevance"]
        )),
        ["m_ini"]
    );
    assert_eq!(
        ids(&query_json(&home, &["--in", "initech", "--recent"])),
        ["m_ini"]
    );
    let (code, out, _) = sm(
        &home,
        &["query", "kiwi", "--in", "initech", "--format", "brief"],
    );
    assert_eq!(code, 0);
    assert!(
        out.starts_with("[m_ini] #1 fact derived 2026-10-25"),
        "{out}"
    );
}

#[test]
fn the_scope_filter_runs_before_the_limit_and_the_pool_cap() {
    let (d, home) = scratch();
    write(&home.join("config.toml"), SCOPES);
    // 110 outside items that rank above the one initech item fill even the widened pool of 100
    let rows: Vec<(String, String)> = (0..110)
        .map(|i| (format!("m_out{i:02}"), "kiwi kiwi kiwi".to_string()))
        .collect();
    let mut seeded: Vec<Row> = rows
        .iter()
        .map(|(id, body)| (id.as_str(), "misc", "kiwi", body.as_str(), "", "", "active"))
        .collect();
    seeded.push((
        "m_ini",
        "misc",
        "INI",
        "kiwi and a long tail of other words",
        "",
        "",
        "active",
    ));
    seed(&home, &seeded);
    let (code, out, err) = sm(
        &home,
        &[
            "query", "kiwi", "--in", "initech", "--limit", "1", "--format", "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(ids(&v), ["m_ini"]);
    assert_eq!(v["more"]["beyond_limit"], 0);
    drop(d);
}

#[test]
fn an_unknown_path_exits_2_and_lists_the_known_ones() {
    let (_d, home) = fixture();
    let (code, out, err) = sm(&home, &["query", "kiwi", "--in", "acme,acme/nope"]);
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (
            2,
            "",
            "unknown scope: acme/nope; known: acme, acme/globex, acme/globex/web, initech\n"
        )
    );
    // no [scopes] at all: nothing is known
    let (_e, bare) = scratch();
    let (code, _, err) = sm(&bare, &["query", "kiwi", "--in", "acme"]);
    assert_eq!((code, err.as_str()), (2, "unknown scope: acme; known: \n"));
}

#[test]
fn a_bad_scopes_table_fails_loud() {
    for (toml, want) in [
        (
            "[scopes]\n\"acme/glo bex\" = [\"GLX\"]\n",
            "error: [scopes] path 'acme/glo bex': each level must match [A-Za-z0-9_.-]+\n",
        ),
        (
            "[scopes]\n\"acme//x\" = [\"GLX\"]\n",
            "error: [scopes] path 'acme//x': each level must match [A-Za-z0-9_.-]+\n",
        ),
        (
            "[scopes]\nacme = \"GLX\"\n",
            "error: [scopes] path 'acme': the value must be a list of non-empty strings\n",
        ),
        (
            "[scopes]\nacme = [\"\"]\n",
            "error: [scopes] path 'acme': the value must be a list of non-empty strings\n",
        ),
    ] {
        let (_d, home) = scratch();
        write(&home.join("config.toml"), toml);
        let (code, _, err) = sm(&home, &["query", "kiwi", "--in", "acme"]);
        assert_eq!((code, err.as_str()), (1, want), "{toml}");
        let (code, _, err) = sm(&home, &["stats"]);
        assert_eq!((code, err.as_str()), (1, want), "stats: {toml}");
    }
}

#[test]
fn footer_commands_carry_in() {
    let (_d, home) = fixture();
    // 5 hits in acme past --limit 1, and "rollout" is missing from the shown hit
    let (code, out, err) = sm(
        &home,
        &[
            "query",
            "kiwi rollout",
            "--in",
            "acme",
            "--limit",
            "1",
            "--bias",
            "relevance",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let footer: Vec<&str> = out
        .lines()
        .filter(|l| l.starts_with("more:") || l.starts_with("missing:"))
        .collect();
    assert_eq!(footer.len(), 1, "{out}");
    assert_eq!(
        footer[0],
        "more: 4 beyond --limit 1 -> sm query \"kiwi rollout\" --in acme --bias relevance --limit 5"
    );
    let (code, out, _) = sm(
        &home,
        &[
            "query",
            "kiwi ops",
            "--in",
            "acme/globex",
            "--in",
            "initech",
            "--limit",
            "1",
        ],
    );
    assert_eq!(code, 0);
    let missing = out.lines().find(|l| l.starts_with("missing:")).unwrap();
    assert_eq!(missing, "missing: ops=0");
    let more = out.lines().find(|l| l.starts_with("more:")).unwrap();
    assert!(
        more.ends_with("-> sm query \"kiwi ops\" --in acme/globex,initech --limit 5"),
        "{more}"
    );
}

/// `--in` searches every repo, so a `--repo` beside it adds no "other repos" count to the
/// `more:` footer (review U1 row 4: the case where `--all` and `--in` differ).
#[test]
fn in_with_repo_has_no_other_repos_in_the_footer() {
    let (_d, home) = fixture();
    let (code, out, err) = sm(
        &home,
        &[
            "query", "kiwi", "--in", "acme", "--repo", "misc", "--limit", "1",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let more: Vec<&str> = out.lines().filter(|l| l.starts_with("more:")).collect();
    // the 5 active acme items (fixture doc), 1 shown
    assert_eq!(
        more,
        ["more: 4 beyond --limit 1 -> sm query \"kiwi\" --in acme --limit 5"],
        "{out}"
    );
}

#[test]
fn stats_prints_active_items_per_path_rolled_up_and_outside() {
    let (_d, home) = fixture();
    let (code, out, err) = sm(&home, &["stats"]);
    assert_eq!(code, 0, "{err}");
    let lines: Vec<&str> = out.lines().collect();
    let at = lines.iter().position(|l| *l == "scopes:").expect(&out);
    // 9 active: acme 5, acme/globex 4, acme/globex/web 1, initech 1; outside 3 (m_long,
    // m_low, m_acc)
    assert_eq!(
        lines[at..at + 6],
        [
            "scopes:",
            "  acme active=5",
            "    acme/globex active=4",
            "      acme/globex/web active=1",
            "  initech active=1",
            "  outside: active=3",
        ]
    );
    let (code, out, _) = sm(&home, &["stats", "--format", "json"]);
    assert_eq!(code, 0);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["scopes"],
        json!({"paths": [
            {"path": "acme", "active": 5},
            {"path": "acme/globex", "active": 4},
            {"path": "acme/globex/web", "active": 1},
            {"path": "initech", "active": 1},
        ], "outside": 3})
    );
}

#[test]
fn stats_sorts_paths_by_level_and_skips_the_block_without_scopes() {
    let (_d, home) = scratch();
    // "acme-x" sorts before "acme/globex" as text; by level the child stays under its parent
    write(
        &home.join("config.toml"),
        "[scopes]\n\"acme-x\" = [\"AX\"]\n\"acme/globex\" = [\"GLX\"]\nacme = []\n",
    );
    seed(&home, &[("m_1", "misc", "t", "AX GLX", "", "", "active")]);
    let (code, out, _) = sm(&home, &["stats"]);
    assert_eq!(code, 0);
    let lines: Vec<&str> = out.lines().collect();
    let at = lines.iter().position(|l| *l == "scopes:").expect(&out);
    assert_eq!(
        lines[at..at + 5],
        [
            "scopes:",
            "  acme active=1",
            "    acme/globex active=1",
            "  acme-x active=1",
            "  outside: active=0",
        ]
    );
    let (_e, bare) = scratch();
    seed(&bare, &[("m_1", "misc", "t", "GLX", "", "", "active")]);
    let (_, out, _) = sm(&bare, &["stats"]);
    assert!(!out.contains("scopes:"), "{out}");
    let (_, out, _) = sm(&bare, &["stats", "--format", "json"]);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v.get("scopes"), None);
}

#[test]
fn config_shows_scopes_and_init_writes_the_note() {
    let (_d, home) = fixture();
    let (code, out, _) = sm(&home, &["config"]);
    assert_eq!(code, 0);
    assert!(
        out.contains(
            "\n[scopes]\nacme = [\"repo:acme-ops\"]\n\"acme/globex\" = [\"GLX\"]\n\"acme/globex/web\" = [\"repo:globex-web\"]\ninitech = [\"INI\"]\n\n[backup]\n"
        ),
        "{out}"
    );
    let (_e, fresh) = scratch();
    std::fs::remove_dir(&fresh).unwrap();
    let (code, _, err) = sm(&fresh, &["init"]);
    assert_eq!(code, 0, "{err}");
    let text = std::fs::read_to_string(fresh.join("config.toml")).unwrap();
    assert!(
        text.contains(
            "\n# Paths company, company/client or company/client/project -> matchers for `sm query --in` and `sm stats`: \"repo:<name>\", or a code that stands as a whole word in the same case, for example \"acme/globex\" = [\"repo:globex-web\", \"GLX\"].\n[scopes]\n\n# Catch-up copies"
        ),
        "{text}"
    );
}

#[test]
fn every_ancestor_of_a_configured_path_is_a_known_path() {
    let (_d, home) = scratch();
    write(
        &home.join("config.toml"),
        "[scopes]\n\"acme/globex\" = [\"GLX\"]\n\"acme/initech/api\" = [\"repo:api\"]\n",
    );
    seed(
        &home,
        &[
            ("m_glx", "misc", "kiwi glx", "GLX kiwi", "", "", "active"),
            ("m_api", "api", "kiwi api", "kiwi api", "", "", "active"),
            ("m_out", "misc", "kiwi out", "kiwi out", "", "", "active"),
        ],
    );
    // acme and acme/initech are not keys; each holds what the paths below it hold
    assert_eq!(
        ids(&query_json(&home, &["--in", "acme"])),
        ["m_api", "m_glx"]
    );
    assert_eq!(
        ids(&query_json(&home, &["--in", "acme/initech"])),
        ["m_api"]
    );
    let (code, _, err) = sm(&home, &["query", "kiwi", "--in", "acme/nope"]);
    assert_eq!(
        (code, err.as_str()),
        (
            2,
            "unknown scope: acme/nope; known: acme, acme/globex, acme/initech, acme/initech/api\n"
        )
    );
    // stats lists the implied lines too, so its rollup reads as --in does
    let (code, out, _) = sm(&home, &["stats"]);
    assert_eq!(code, 0);
    let lines: Vec<&str> = out.lines().collect();
    let at = lines.iter().position(|l| *l == "scopes:").expect(&out);
    assert_eq!(
        lines[at..at + 6],
        [
            "scopes:",
            "  acme active=2",
            "    acme/globex active=1",
            "    acme/initech active=1",
            "      acme/initech/api active=1",
            "  outside: active=1",
        ]
    );
    // sm config prints the file's keys only, not the implied ones
    let (_, out, _) = sm(&home, &["config"]);
    assert!(
        out.contains("\n[scopes]\n\"acme/globex\" = [\"GLX\"]\n\"acme/initech/api\" = [\"repo:api\"]\n\n[backup]\n"),
        "{out}"
    );
}

#[test]
fn a_bad_scopes_table_fails_only_the_commands_that_read_scopes() {
    let (d, home) = scratch();
    let claude = d.path().join("fake-claude.sh");
    write(&claude, "#!/bin/sh\ncat >/dev/null\necho 'kiwi'\n");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    write(
        &home.join("config.toml"),
        &format!(
            "[prompt_hook]\nclaude_bin = \"{}\"\n[automemory]\nenabled = false\n[capture]\nenabled = false\n[scopes]\n\"acme/glo bex\" = [\"GLX\"]\nacme = \"GLX\"\n",
            claude.display()
        ),
    );
    seed(
        &home,
        &[("m_k1", "global", "kiwi one", "kiwi one", "", "", "active")],
    );
    let (code, _, err) = sm(
        &home,
        &["put", "--tag", "derived", "--repo", "global", "kiwi two"],
    );
    assert_eq!(code, 0, "put: {err}");
    let v = query_json(&home, &["--all"]);
    assert_eq!(v["results"].as_array().unwrap().len(), 2);
    let (code, _, err) = sm(&home, &["config"]);
    assert_eq!(code, 0, "config: {err}");
    let mut hook = std::process::Command::new(env!("CARGO_BIN_EXE_sm"));
    hook.args(["hook", "prompt"])
        .env("HOME", home.parent().unwrap())
        .env("SIMPLE_MEMORY_HOME", &home)
        .env_remove("SM_PROMPT_HOOK")
        .env_remove("SM_PARITY_NOW")
        .env_remove("SM_PARITY_SEED")
        .env_remove("SM_PARITY_TRACE")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = hook.spawn().unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"prompt": "what about kiwi", "cwd": "/nonexistent-sm-scopes"}"#)
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout.contains("[m_k1]"), "hook: {stdout}");
    let (code, _, err) = sm(&home, &["import", "catch-up", "--quiet"]);
    assert_eq!(code, 0, "catch-up: {err}");
    // the commands that read [scopes] name the first bad path
    let want = "error: [scopes] path 'acme/glo bex': each level must match [A-Za-z0-9_.-]+\n";
    let (code, _, err) = sm(&home, &["query", "kiwi", "--in", "acme"]);
    assert_eq!((code, err.as_str()), (1, want));
    let (code, _, err) = sm(&home, &["stats"]);
    assert_eq!((code, err.as_str()), (1, want));
}
