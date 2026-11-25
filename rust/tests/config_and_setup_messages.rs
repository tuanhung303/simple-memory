//! What `sm` prints when config.toml is wrong, the store cannot be opened or is damaged, and
//! what `sm init` and `sm hooks print` tell a new user: each case runs the built binary.
mod common;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use common::{scratch, sm, sm_env, write};

/// `sm <args>` with `stdin` on its standard input and only `env` added.
fn sm_stdin(home: &Path, args: &[&str], stdin: &str) -> (i32, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sm"))
        .args(args)
        .env("HOME", home.parent().unwrap())
        .env("SIMPLE_MEMORY_HOME", home)
        .env("SM_NO_UPDATE_CHECK", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

const PROMPT: &str = r#"{"prompt":"what is the pool limit","session_id":"s1","cwd":"/tmp","hook_event_name":"UserPromptSubmit"}"#;

#[test]
fn a_config_that_does_not_parse_gives_the_same_message_on_every_surface() {
    let (_d, home) = scratch();
    let cfg = home.join("config.toml");
    write(
        &cfg,
        "[prompt_hook]\nlimit = 2\n\n[prompt_hook]\nmodel = \"none\"\n",
    );
    let want = format!(
        "{}: line 4, column 13: [prompt_hook] is declared twice; keep one header and put the keys under it; fix the line, then run sm config",
        cfg.display()
    );
    for args in [&["query", "pool"][..], &["config"], &["doctor"]] {
        let (code, out, err) = sm(&home, args);
        assert_eq!((code, out.as_str()), (1, ""), "{args:?}");
        assert_eq!(err, format!("error: {want}\n"), "{args:?}");
    }
    let (code, out, err) = sm_stdin(&home, &["hook", "prompt"], PROMPT);
    let hook: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        (code, &hook["hookSpecificOutput"]["additionalContext"]),
        (0, &serde_json::json!(format!("sm recall is off: {want}")))
    );
    assert_eq!(err, format!("sm hook: {want}\n"));
    let rpc = [
        r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"memory_query","arguments":{"query":"pool"}}}"#,
    ]
    .join("\n");
    let (_, out, _) = sm_stdin(&home, &["mcp"], &format!("{rpc}\n"));
    let reply: serde_json::Value = serde_json::from_str(out.lines().nth(1).unwrap()).unwrap();
    assert_eq!(
        reply["result"]["content"][0]["text"],
        format!("error: {want}")
    );
}

#[test]
fn doctor_refuses_a_list_key_of_another_type() {
    let (_d, home) = scratch();
    let cfg = home.join("config.toml");
    write(&cfg, "[upkeep]\ntasklog_roots = 5\n");
    let (code, out, err) = sm(&home, &["doctor"]);
    assert_eq!((code, out.as_str()), (1, ""));
    assert_eq!(
        err,
        format!(
            "error: {}: upkeep.tasklog_roots must be a list, for example []\n",
            cfg.display()
        )
    );
}

#[test]
fn a_string_for_a_list_key_is_read_as_a_one_item_list_by_every_command() {
    let (d, home) = scratch();
    std::fs::create_dir_all(d.path().join(".claude/projects")).unwrap();
    let (logs, notes) = (d.path().join("logs"), d.path().join("notes"));
    std::fs::create_dir_all(&notes).unwrap();
    write(
        &logs.join("anvil/docs/tasklog/2026-11-01/090000-aaaa-t.yaml"),
        "id: 20261101-090000-aaaa-t\ntitle: Walrus rollout plan\nas_of: 2026-11-01T09:00:00+00:00\nresults:\n  - done walrus rollout\n",
    );
    write(
        &home.join("config.toml"),
        &format!(
            "[repos]\nroots = \"~/code\"\n[prompt_hook]\nmodel = \"none\"\nextra_stopwords = \"pool\"\n\
             [catch_up]\nenabled = false\n[upkeep]\ntasklog_roots = \"{}\"\nhandoff_dirs = \"{}\"\n",
            logs.display(),
            notes.display()
        ),
    );
    let (code, out, err) = sm(&home, &["config"]);
    assert_eq!((code, err.as_str()), (0, ""));
    for line in [
        "roots = [\"~/code\"]".to_string(),
        "extra_stopwords = [\"pool\"]".to_string(),
        format!("tasklog_roots = [\"{}\"]", logs.display()),
        format!("handoff_dirs = [\"{}\"]", notes.display()),
    ] {
        assert!(out.lines().any(|l| l == line), "{line}\n{out}");
    }
    let (code, out, err) = sm(&home, &["import", "catch-up"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(
        out.contains("imports: ok not_stored=1 inserted=1 "),
        "{out}"
    );
    common::put(
        &home,
        &[
            "--tag",
            "decided",
            "--repo",
            "global",
            "Pool size is 16 sockets",
        ],
    );
    let (code, out, _) = sm(&home, &["query", "walrus", "--all"]);
    assert_eq!(code, 0);
    assert!(out.contains("Walrus rollout plan"), "{out}");
    let (code, _, err) = sm_stdin(
        &home,
        &["hook", "prompt"],
        r#"{"prompt":"what pool size do the workers use","session_id":"s1","cwd":"/tmp","hook_event_name":"UserPromptSubmit"}"#,
    );
    assert_eq!((code, err.as_str()), (0, ""));
    let log = std::fs::read_to_string(home.join("hook.log")).unwrap();
    let row: serde_json::Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
    assert_eq!(row["query"], "size", "{row}");
}

#[test]
fn a_bad_default_bias_word_is_named_by_config_doctor_and_catch_up_and_put_still_works() {
    let (d, home) = scratch();
    std::fs::create_dir_all(d.path().join(".claude/projects")).unwrap();
    let cfg = home.join("config.toml");
    let want = format!(
        "warning: {}: query.default_bias: \"decisions\" is not a bias; use recency, relevance, executed, verified, \
         decided, client-stated, derived, assumed, prior, planned, fact, decision, learning, episode, task, handoff, \
         or scope:<path>\n",
        cfg.display()
    );
    for (value, body) in [
        ("[\"recency\", \"decisions\"]", "Pool size is 16 sockets"),
        ("\"recency, decisions\"", "Queue lag alert at 5000"),
    ] {
        write(
            &cfg,
            &format!("[catch_up]\nenabled = false\n[query]\ndefault_bias = {value}\n"),
        );
        for args in [&["config"][..], &["doctor"], &["import", "catch-up"]] {
            let (code, _, err) = sm(&home, args);
            assert_eq!((code, err.as_str()), (0, want.as_str()), "{value} {args:?}");
        }
        let (code, _, err) = sm(
            &home,
            &["put", "--tag", "decided", "--repo", "global", body],
        );
        assert_eq!((code, err.as_str()), (0, ""), "{value}");
    }
    write(
        &cfg,
        "[scopes]\n\"acme/web\" = [\"GLX\"]\n[query]\ndefault_bias = [\"Recency\", \"scope:acme/web\"]\n",
    );
    assert_eq!(sm(&home, &["config"]).2, "");
    write(&cfg, "[query]\ndefault_bias = [\"recency\", 5]\n");
    assert_eq!(sm(&home, &["config"]).2, want.replace("\"decisions\"", "5"));
}

/// A `scope:` word in the default bias that names no `[scopes]` path stops every query, so
/// config, doctor and catch-up warn with the query's own message; a path the table holds or
/// implies gets no warning, and the query runs.
#[test]
fn a_default_bias_scope_the_query_refuses_is_named_by_config_doctor_and_catch_up() {
    let (d, home) = scratch();
    std::fs::create_dir_all(d.path().join(".claude/projects")).unwrap();
    common::put(
        &home,
        &[
            "--tag",
            "decided",
            "--repo",
            "global",
            "Pool size is 16 sockets",
        ],
    );
    let cfg = home.join("config.toml");
    let no_table = format!(
        "unknown scope: acmee; {} has no [scopes] table (README: Multi-repo scopes and maintenance)",
        cfg.display()
    );
    for (scopes, value, why) in [
        (
            "[scopes]\nacme = [\"GLX\"]\n",
            "[\"recency\", \"scope:acmee\"]",
            "unknown scope: acmee; known: acme",
        ),
        (
            "[scopes]\nacme = [\"GLX\"]\n",
            "\"recency, scope:acmee\"",
            "unknown scope: acmee; known: acme",
        ),
        ("", "[\"recency\", \"scope:acmee\"]", no_table.as_str()),
    ] {
        write(
            &cfg,
            &format!("[catch_up]\nenabled = false\n{scopes}[query]\ndefault_bias = {value}\n"),
        );
        let (code, _, err) = sm(&home, &["query", "pool", "--all"]);
        assert_eq!(
            (code, err.as_str()),
            (2, format!("{why}\n").as_str()),
            "{value}"
        );
        let want = format!("warning: {}: query.default_bias: {why}\n", cfg.display());
        for args in [&["config"][..], &["doctor"], &["import", "catch-up"]] {
            let (code, _, err) = sm(&home, args);
            assert_eq!((code, err.as_str()), (0, want.as_str()), "{value} {args:?}");
        }
    }
    for (scopes, value) in [
        (
            "[scopes]\nacme = [\"GLX\"]\n",
            "[\"recency\", \"scope:acme\"]",
        ),
        (
            "[scopes]\n\"acme/web\" = [\"GLX\"]\n",
            "\"recency, scope:acme\"",
        ),
        ("", "[\"recency\", \"decided\"]"),
    ] {
        write(
            &cfg,
            &format!("[catch_up]\nenabled = false\n{scopes}[query]\ndefault_bias = {value}\n"),
        );
        let (code, out, err) = sm(&home, &["query", "pool", "--all"]);
        assert_eq!((code, err.as_str()), (0, ""), "{value}");
        assert!(out.contains("Pool size is 16 sockets"), "{value}: {out}");
        for args in [&["config"][..], &["doctor"], &["import", "catch-up"]] {
            let (code, _, err) = sm(&home, args);
            assert_eq!((code, err.as_str()), (0, ""), "{value} {args:?}");
        }
    }
}

#[test]
fn every_scope_reader_names_the_config_file_when_it_has_no_scopes_table() {
    let (_d, home) = scratch();
    let want = format!(
        "unknown scope: acme; {}/config.toml has no [scopes] table (README: Multi-repo scopes and maintenance)",
        home.display()
    );
    for args in [
        &["query", "pool", "--in", "acme"][..],
        &["list", "--in", "acme"],
    ] {
        let (code, out, err) = sm(&home, args);
        assert_eq!(
            (code, out.as_str(), err),
            (2, "", format!("{want}\n")),
            "{args:?}"
        );
    }
    let rpc = [
        r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"memory_query","arguments":{"query":"pool","in":"acme"}}}"#,
    ]
    .join("\n");
    let (_, out, _) = sm_stdin(&home, &["mcp"], &format!("{rpc}\n"));
    let reply: serde_json::Value = serde_json::from_str(out.lines().nth(1).unwrap()).unwrap();
    assert_eq!(
        reply["result"]["content"][0]["text"],
        format!("error: {want}")
    );
}

#[test]
fn a_default_bias_string_loads_and_splits_at_commas_as_before() {
    let (_d, home) = scratch();
    common::put(
        &home,
        &["--kind", "fact", "--tag", "derived", "pool limit 16"],
    );
    for (value, want) in [
        ("\"relevance\"", serde_json::json!(["relevance"])),
        ("\"recency,fact\"", serde_json::json!(["recency", "fact"])),
    ] {
        write(
            &home.join("config.toml"),
            &format!("[query]\ndefault_bias = {value}\n"),
        );
        let (code, out, err) = sm(&home, &["query", "pool limit", "--all", "--format", "json"]);
        assert_eq!((code, err.as_str()), (0, ""), "{value}");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["bias"], want, "{value}");
    }
}

#[test]
fn doctor_with_a_good_config_prints_what_it_printed_before() {
    let (_d, home) = scratch();
    write(&home.join("config.toml"), "[backup]\nkeep = 3\n");
    let (code, out, err) = sm(&home, &["doctor"]);
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (
            0,
            "integrity=ok rebuilt_fts=False total=0 active=0 superseded=0\n",
            ""
        )
    );
}

#[test]
fn unknown_sections_and_keys_are_named_by_config_and_doctor_only() {
    let (_d, home) = scratch();
    let cfg = home.join("config.toml");
    write(
        &cfg,
        "[upkep]\ntasklog_roots = []\n\n[upkeep]\ntasklog_root = []\n\n[prompt_hook]\nenabled = false\nlimt = 3\n\n\
         [prompt_hook.projects]\nANY = \"free key\"\n\n[repo_aliases]\nfoo = \"bar\"\n\n[keyword_aliases]\nk8s = \"kubernetes\"\n\n\
         [scopes]\n\"acme/web\" = [\"repo:web\"]\n\n[catch_up]\nenabled = false\n",
    );
    let p = cfg.display();
    let want = format!(
        "warning: {p}: unknown section [upkep]; sm config prints the keys in effect\n\
         warning: {p}: unknown key upkeep.tasklog_root; sm config prints the keys in effect\n\
         warning: {p}: unknown key prompt_hook.limt; sm config prints the keys in effect\n"
    );
    let (code, out, err) = sm(&home, &["config"]);
    assert_eq!((code, err.as_str()), (0, want.as_str()));
    assert!(out.starts_with("[repos]\n"), "{out}");
    let (code, _, err) = sm(&home, &["doctor"]);
    assert_eq!((code, err.as_str()), (0, want.as_str()));
    let (code, _, err) = sm(&home, &["query", "pool"]);
    assert_eq!((code, err.contains("unknown")), (0, false), "{err}");
    let (code, out, err) = sm_stdin(&home, &["hook", "prompt"], PROMPT);
    assert_eq!((code, out.as_str(), err.as_str()), (0, "", ""));
}

#[test]
fn a_store_that_cannot_be_opened_names_the_file() {
    let (_d, home) = scratch();
    std::fs::create_dir_all(home.join("memory.db")).unwrap();
    let (code, _, err) = sm(&home, &["query", "pool"]);
    assert_eq!(code, 1);
    assert_eq!(
        err,
        format!(
            "error: cannot open {}/memory.db: unable to open database file\n",
            home.display()
        )
    );
}

/// Overwrite every byte past the first page, as a disk fault would.
fn damage(db: &Path) {
    let mut b = std::fs::read(db).unwrap();
    assert!(b.len() > 4096, "{}", b.len());
    for x in &mut b[4096..] {
        *x = b'Z';
    }
    std::fs::write(db, b).unwrap();
}

#[test]
fn a_damaged_store_names_the_newest_backup_and_how_to_restore_it() {
    let (_d, home) = scratch();
    for i in 0..30 {
        common::put(
            &home,
            &[
                "--kind",
                "fact",
                "--tag",
                "verified",
                "--evidence",
                "t",
                &format!("pool limit {i} is the item body with some words in it"),
            ],
        );
    }
    let h = home.display();
    let db = home.join("memory.db");
    let (code, _, err) = {
        let keep = std::fs::read(&db).unwrap();
        damage(&db);
        let r = sm(&home, &["doctor"]);
        std::fs::write(&db, keep).unwrap();
        r
    };
    assert_eq!(code, 1);
    let lines: Vec<&str> = err.lines().collect();
    assert!(lines[0].starts_with("error: "), "{err}");
    assert_eq!(
        lines[1..],
        [format!(
            "{h}/memory.db looks damaged and {h}/backups holds no backup to restore"
        )]
    );
    assert_eq!(sm(&home, &["backup"]).0, 0);
    let backup = std::fs::read_dir(home.join("backups"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    damage(&db);
    for args in [&["doctor"][..], &["query", "pool"]] {
        let (code, _, err) = sm(&home, args);
        assert_eq!(code, 1, "{args:?}");
        let lines: Vec<&str> = err.lines().collect();
        assert!(lines[0].starts_with("error: "), "{err}");
        assert_eq!(
            lines[1..],
            [
                format!(
                    "{h}/memory.db looks damaged; to restore the newest backup that passes quick_check, {}:",
                    backup.display()
                ),
                format!("  1. mkdir -p {h}/damaged && mv {h}/memory.db* {h}/damaged/"),
                format!("  2. cp {} {h}/memory.db", backup.display()),
            ],
            "{args:?}"
        );
    }
}

#[test]
fn the_restore_steps_quote_a_home_path_that_holds_a_space() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("my home");
    std::fs::create_dir_all(&home).unwrap();
    for i in 0..30 {
        let body = format!("pool limit {i} is the item body with some words in it");
        common::put(&home, &["--kind", "fact", "--tag", "derived", &body]);
    }
    assert_eq!(sm(&home, &["backup"]).0, 0);
    let backup = std::fs::read_dir(home.join("backups"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    damage(&home.join("memory.db"));
    let (code, _, err) = sm(&home, &["doctor"]);
    assert_eq!(code, 1);
    let h = home.display();
    let lines: Vec<&str> = err.lines().collect();
    assert_eq!(
        lines[2..],
        [
            format!("  1. mkdir -p '{h}/damaged' && mv '{h}'/memory.db* '{h}/damaged'/"),
            format!("  2. cp '{}' '{h}/memory.db'", backup.display()),
        ],
        "{err}"
    );
}

/// The names in `<home>/backups`, sorted.
fn backup_names(home: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(home.join("backups"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    v.sort();
    v
}

/// A store with 30 items and one good backup under an old name, so no new copy can share it.
fn store_with_a_good_backup() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let (d, home) = scratch();
    for i in 0..30 {
        let body = format!("pool limit {i} is the item body with some words in it");
        common::put(&home, &["--kind", "fact", "--tag", "derived", &body]);
    }
    let (code, out, _) = sm(&home, &["backup"]);
    assert_eq!(code, 0, "{out}");
    let made = home.join("backups").join(&backup_names(&home)[0]);
    let good = home.join("backups/memory-20200101-000000-auto.db");
    std::fs::rename(made, &good).unwrap();
    (d, home, good)
}

#[test]
fn a_backup_of_a_damaged_store_keeps_no_copy_and_doctor_names_a_backup_that_passes() {
    let (_d, home, good) = store_with_a_good_backup();
    let db = home.join("memory.db");
    damage(&db);
    let (code, _, err) = sm(&home, &["backup", "--keep", "1"]);
    assert_eq!(code, 1, "{err}");
    assert_eq!(backup_names(&home), ["memory-20200101-000000-auto.db"]);
    let h = home.display();
    let steps = |backup: &Path| {
        vec![
            format!(
                "{h}/memory.db looks damaged; to restore the newest backup that passes quick_check, {}:",
                backup.display()
            ),
            format!("  1. mkdir -p {h}/damaged && mv {h}/memory.db* {h}/damaged/"),
            format!("  2. cp {} {h}/memory.db", backup.display()),
        ]
    };
    let (code, _, err) = sm(&home, &["doctor"]);
    assert_eq!(code, 1);
    assert_eq!(
        err.lines().skip(1).collect::<Vec<_>>(),
        steps(&good),
        "{err}"
    );
    // A damaged copy that an older build left under the normal name is passed over.
    std::fs::copy(&db, home.join("backups/memory-20990101-000000-auto.db")).unwrap();
    let (_, _, err) = sm(&home, &["doctor"]);
    assert_eq!(
        err.lines().skip(1).collect::<Vec<_>>(),
        steps(&good),
        "{err}"
    );
    std::fs::copy(&db, &good).unwrap();
    let (code, _, err) = sm(&home, &["doctor"]);
    assert_eq!(code, 1);
    assert_eq!(
        err.lines().skip(1).collect::<Vec<_>>(),
        [format!(
            "{h}/memory.db looks damaged and no backup in {h}/backups passes quick_check"
        )],
        "{err}"
    );
}

/// Name, size and modified time of each file in `<home>/backups`.
fn backup_listing(home: &Path) -> Vec<(String, u64, std::time::SystemTime)> {
    let mut v: Vec<_> = std::fs::read_dir(home.join("backups"))
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            let m = e.metadata().unwrap();
            let name = e.file_name().into_string().unwrap();
            (name, m.len(), m.modified().unwrap())
        })
        .collect();
    v.sort();
    v
}

#[test]
fn doctor_checks_a_backup_copy_without_writing_to_it() {
    use std::os::unix::fs::PermissionsExt;
    let (d, home, _) = store_with_a_good_backup();
    // SQLite reads `%`, `?` and `#` in a file URI as escapes, a query and a fragment.
    let home = {
        let moved = d.path().join("h#1%41?x");
        std::fs::rename(&home, &moved).unwrap();
        moved
    };
    let good = home.join("backups/memory-20200101-000000-auto.db");
    damage(&home.join("memory.db"));
    let h = home.display();
    let want = format!(
        "{h}/memory.db looks damaged; to restore the newest backup that passes quick_check, {}:",
        good.display()
    );
    for mode in [0o644, 0o444] {
        std::fs::set_permissions(&good, std::fs::Permissions::from_mode(mode)).unwrap();
        let before = backup_listing(&home);
        let (code, _, err) = sm(&home, &["doctor"]);
        assert_eq!(code, 1);
        assert_eq!(err.lines().nth(1), Some(want.as_str()), "{mode:o}: {err}");
        assert_eq!(backup_listing(&home), before, "{mode:o}");
    }
}

/// An index whose root page holds no cells: every SQL statement still runs, and only
/// `quick_check` finds the rows missing from the index.
#[test]
fn a_backup_whose_copy_fails_quick_check_prints_two_plain_lines() {
    let (_d, home, _good) = store_with_a_good_backup();
    let db = home.join("memory.db");
    let root: i64 = {
        let c = common::open(&home);
        c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
        c.query_row(
            "SELECT rootpage FROM sqlite_master WHERE type = 'index' AND tbl_name = 'items' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    let mut bytes = std::fs::read(&db).unwrap();
    let at = (root as usize - 1) * 4096;
    bytes[at + 3] = 0;
    bytes[at + 4] = 0;
    std::fs::write(&db, bytes).unwrap();
    let (code, out, err) = sm(&home, &["backup"]);
    let h = home.display();
    assert_eq!(
        (code, out.as_str(), err),
        (
            1,
            "",
            format!(
                "error: the backup copy of {h}/memory.db does not pass quick_check, so it was not kept\n\
                 {h}/memory.db looks damaged; run sm doctor\n"
            )
        )
    );
    assert_eq!(backup_names(&home), ["memory-20200101-000000-auto.db"]);
}

/// Two `sm backup --keep 1` runs on one store with the clock pinned to one second: each run
/// copies to a file of its own, so both pass, and `backups/` holds only that second's copy. Both
/// runs rotate out the same 200 older copies, so one finds some of them already gone.
#[cfg(feature = "parity")]
#[test]
fn two_backups_in_one_second_both_pass_and_leave_one_copy() {
    let (d, home, good) = store_with_a_good_backup();
    let seed = d.path().join("seed.db");
    std::fs::rename(&good, &seed).unwrap();
    for now in 1_700_000_000..1_700_000_005_i64 {
        for i in 0..200 {
            let old = format!(
                "memory-2019{:02}{:02}-000000-auto.db",
                i / 28 + 1,
                i % 28 + 1
            );
            std::fs::copy(&seed, home.join("backups").join(old)).unwrap();
        }
        let runs: Vec<_> = (0..2)
            .map(|_| {
                Command::new(env!("CARGO_BIN_EXE_sm"))
                    .args(["backup", "--keep", "1"])
                    .env("HOME", home.parent().unwrap())
                    .env("SIMPLE_MEMORY_HOME", &home)
                    .env("SM_NO_UPDATE_CHECK", "1")
                    .env("SM_PARITY_NOW", now.to_string())
                    .env("SM_UTC_OFFSET", "+00:00")
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        let stamp = chrono::DateTime::from_timestamp(now, 0)
            .unwrap()
            .format("%Y%m%d-%H%M%S");
        let name = format!("memory-{stamp}-auto.db");
        for run in runs {
            let out = run.wait_with_output().unwrap();
            let err = String::from_utf8(out.stderr).unwrap();
            assert_eq!((out.status.code(), err.as_str()), (Some(0), ""), "{now}");
            let out = String::from_utf8(out.stdout).unwrap();
            let first = out.lines().next().unwrap();
            let want = format!(
                "backup: {} quick_check=ok items=30",
                home.join("backups").join(&name).display()
            );
            assert_eq!(first, want, "{now}");
        }
        assert_eq!(backup_names(&home), [name.as_str()], "{now}");
    }
}

/// `<scratch>/<name>` with symlinks resolved, as the import resolves a config entry.
fn real_dir(d: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
    let p = d.path().join(name);
    std::fs::create_dir_all(&p).unwrap();
    p.canonicalize().unwrap()
}

#[test]
fn a_missing_task_log_root_or_handoff_dir_names_the_key_and_the_file_to_fix() {
    let (d, home) = scratch();
    std::fs::create_dir_all(d.path().join(".claude/projects")).unwrap();
    let gone = real_dir(&d, "work").join("gone");
    let h = home.display();
    for (key, list) in [
        (
            "tasklog_roots",
            "tasklog_roots = [\"{}\"]\nhandoff_dirs = []",
        ),
        (
            "handoff_dirs",
            "tasklog_roots = []\nhandoff_dirs = [\"{}\"]",
        ),
    ] {
        let list = list.replace("{}", gone.to_str().unwrap());
        write(&home.join("config.toml"), &format!("[upkeep]\n{list}\n"));
        let want = format!(
            "upkeep.{key} entry not found: {}; fix or remove it in [upkeep] {key} in {h}/config.toml",
            gone.display()
        );
        let (code, out, _) = sm(&home, &["import", "catch-up"]);
        assert_eq!(code, 1, "{out}");
        assert!(
            out.lines().any(|l| l == format!("imports: error: {want}")),
            "{out}"
        );
        let (code, _, err) = sm(&home, &["doctor", "--imports"]);
        assert_eq!((code, err), (1, format!("error: {want}\n")));
    }
}

#[test]
fn the_hook_shows_a_long_missing_entry_message_whole() {
    let (d, home) = scratch();
    std::fs::create_dir_all(d.path().join(".claude/projects")).unwrap();
    let gone = real_dir(&d, "work").join("g".repeat(200));
    write(
        &home.join("config.toml"),
        &format!(
            "[prompt_hook]\nmodel = \"none\"\n[catch_up]\nenabled = false\n[upkeep]\ntasklog_roots = [\"{}\"]\n",
            gone.display()
        ),
    );
    let want = format!(
        "upkeep.tasklog_roots entry not found: {}; fix or remove it in [upkeep] tasklog_roots in {}/config.toml",
        gone.display(),
        home.display()
    );
    let (code, out, _) = sm(&home, &["import", "catch-up"]);
    assert_eq!(code, 1, "{out}");
    assert!(
        out.lines().any(|l| l == format!("imports: error: {want}")),
        "{out}"
    );
    let (code, out, _) = sm_stdin(&home, &["hook", "prompt"], PROMPT);
    let hook: serde_json::Value = serde_json::from_str(&out).unwrap();
    let context = hook["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert_eq!(code, 0);
    assert!(
        context
            .lines()
            .any(|l| l.starts_with("sm catch-up failed at ")
                && l.ends_with(&format!(": {want}; run sm doctor --imports"))),
        "{context}"
    );
}

#[test]
fn a_repo_folder_given_as_a_task_log_root_gets_a_hint() {
    let (d, home) = scratch();
    std::fs::create_dir_all(d.path().join(".claude/projects")).unwrap();
    let code_dir = real_dir(&d, "code");
    let repo = code_dir.join("myrepo");
    write(
        &repo.join("docs/tasklog/2026-11-20/t1.yaml"),
        "id: t1\ntitle: pool sizing\nresults:\n  - pool size 16 works\n",
    );
    write(
        &home.join("config.toml"),
        &format!(
            "[upkeep]\ntasklog_roots = [\"{}\"]\nhandoff_dirs = []\n",
            repo.display()
        ),
    );
    let hint = format!(
        "{} is a repo folder: tasklog_roots takes the folder above it (set \"{}\")\n",
        repo.display(),
        code_dir.display()
    );
    let (code, out, err) = sm(&home, &["doctor", "--imports"]);
    assert_eq!((code, err.as_str()), (0, hint.as_str()), "{out}");
    assert!(out.contains("imports: tasklog_files=0 "), "{out}");
    let (code, out, err) = sm(&home, &["import", "catch-up"]);
    assert_eq!((code, err.as_str()), (0, hint.as_str()), "{out}");
    assert_eq!(
        sm(&home, &["import", "catch-up", "--quiet"]),
        (0, String::new(), String::new())
    );
}

/// Copy the built binary to `<dir>/sm`.
fn copy_sm(dir: &Path) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let to = dir.join("sm");
    std::fs::copy(env!("CARGO_BIN_EXE_sm"), &to).unwrap();
    to
}

fn hooks_print(bin: &Path, home: &Path, path: &str, harness: &str) -> (i32, String, String) {
    let out = Command::new(bin)
        .args(["hooks", "print", harness])
        .env("HOME", home.parent().unwrap())
        .env("SIMPLE_MEMORY_HOME", home)
        .env("PATH", path)
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

#[test]
fn the_printed_hook_command_runs_from_a_folder_whose_name_holds_a_space() {
    let (d, home) = scratch();
    let tools = d.path().join("My Tools/bin");
    let bin = copy_sm(&tools);
    let path = format!("{}:/usr/bin:/bin", tools.display());
    write(
        &home.join("config.toml"),
        "[prompt_hook]\nenabled = false\n[catch_up]\nenabled = false\n",
    );
    let (code, out, _) = hooks_print(&bin, &home, &path, "claude-code");
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let cmd = v["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(cmd, format!("'{}' hook prompt", bin.display()));
    let mut child = Command::new("sh")
        .args(["-c", &cmd])
        .env("HOME", home.parent().unwrap())
        .env("SIMPLE_MEMORY_HOME", &home)
        .env("PATH", &path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(PROMPT.as_bytes())
        .unwrap();
    let run = child.wait_with_output().unwrap();
    assert_eq!(
        run.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let (_, out, _) = hooks_print(&bin, &home, &path, "agy");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["sm-recall"]["PreInvocation"][0]["command"],
        format!("'{}' hook prompt --agy", bin.display())
    );
    let (_, out, _) = hooks_print(&bin, &home, &path, "mcp");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let server = &v["mcpServers"]["simple-memory"];
    assert_eq!(server["command"], bin.display().to_string());
    assert_eq!(server["args"], serde_json::json!(["mcp"]));
}

#[test]
fn hooks_print_names_this_binary_without_an_sm_on_path_and_the_sm_on_path_without_a_note() {
    let (d, home) = scratch();
    let bin = copy_sm(&d.path().join("real"));
    let bin = std::fs::canonicalize(bin).unwrap();
    let (code, out, err) = hooks_print(&bin, &home, "/usr/bin:/bin", "claude-code");
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
        format!("{} hook prompt", bin.display())
    );
    assert_eq!(
        err.lines().next().unwrap(),
        format!(
            "no sm on PATH: the command names this binary, {}",
            bin.display()
        )
    );
    let other = d.path().join("other");
    write(&other.join("sm"), "#!/bin/sh\n");
    std::os::unix::fs::PermissionsExt::set_mode(
        &mut std::fs::metadata(other.join("sm")).unwrap().permissions(),
        0o755,
    );
    std::fs::set_permissions(
        other.join("sm"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let path = format!("{}:/usr/bin:/bin", other.display());
    let (_, out, err) = hooks_print(&bin, &home, &path, "mcp");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let on_path = other.join("sm");
    assert_eq!(
        v["mcpServers"]["simple-memory"]["command"],
        on_path.display().to_string()
    );
    // Another sm on PATH (an npm wrapper that starts this binary, say): no note.
    assert_eq!(
        err,
        "merge into the top level of a project's .mcp.json (or of another MCP client's config): the fragment already holds the \"mcpServers\" key\n"
    );
    // The sm on PATH is this binary: no note.
    let (_, _, err) = hooks_print(
        &bin,
        &home,
        &format!("{}:/usr/bin", bin.parent().unwrap().display()),
        "agy",
    );
    assert_eq!(err, "merge into ~/.gemini/config/hooks.json\n");
}

#[test]
fn hooks_print_says_the_fragment_holds_the_hooks_key() {
    let (_d, home) = scratch();
    let (code, _, err) = sm(&home, &["hooks", "print", "claude-code"]);
    assert_eq!(code, 0);
    assert_eq!(
        err.lines().last().unwrap(),
        "merge into the top level of ~/.claude/settings.json (or a project's .claude/settings.json): the fragment already holds the \"hooks\" key"
    );
}

#[test]
fn init_tells_a_user_with_work_to_set_the_roots_and_names_the_key_that_turns_automemory_on() {
    let (d, home) = scratch();
    let claude = d.path().join(".claude/projects");
    let (code, out, _) = sm(&home, &["init"]);
    assert_eq!(code, 0);
    let cfg = home.join("config.toml");
    assert!(
        out.ends_with(&format!(
            "next: sm hooks print claude-code (or agy), then merge it into the hook file it names\n\
             next: if you have task logs or handoffs, set [upkeep] tasklog_roots and handoff_dirs in {}, then run sm import catch-up\n",
            cfg.display()
        )),
        "{out}"
    );
    assert!(!out.contains("exists now"), "{out}");
    std::fs::create_dir_all(&claude).unwrap();
    let (code, out, _) = sm_env(&home, &["init"], &[]);
    assert_eq!(code, 0);
    let line = format!(
        "{} exists now: set enabled = true under [automemory] and [capture] in {} to import its memory files and write session cards",
        claude.display(),
        cfg.display()
    );
    assert_eq!(out.lines().filter(|l| *l == line).count(), 1, "{out}");
}

#[test]
fn init_with_a_config_that_does_not_parse_prints_its_lines_and_warns_of_the_error() {
    let (d, home) = scratch();
    std::fs::create_dir_all(d.path().join(".claude/projects")).unwrap();
    let cfg = home.join("config.toml");
    write(&cfg, "[upkeep\n");
    let (code, out, err) = sm(&home, &["init"]);
    let h = home.display();
    assert_eq!(
        (code, out),
        (
            0,
            format!(
                "kept home {h}\ncreated database {h}/memory.db\n\
                 kept config {h}/config.toml (sm init never overwrites it)\n\
                 next: sm hooks print claude-code (or agy), then merge it into the hook file it names\n"
            )
        )
    );
    assert_eq!(
        err,
        format!(
            "warning: {h}/config.toml: line 1, column 8: Expected ']' at the end of a table declaration; fix the line, then run sm config\n"
        )
    );
}
