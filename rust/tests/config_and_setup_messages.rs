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
    assert_eq!((code, out.as_str()), (0, ""));
    // The hook keeps the first 200 chars of a message (cli/hook.rs).
    let cut: String = want.chars().take(200).collect();
    assert_eq!(err, format!("sm hook: {cut}\n"));
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
    write(&cfg, "[upkeep]\ntasklog_roots = \"/x/work\"\n");
    let (code, out, err) = sm(&home, &["doctor"]);
    assert_eq!((code, out.as_str()), (1, ""));
    assert_eq!(
        err,
        format!(
            "error: {}: upkeep.tasklog_roots must be a list, for example [\"/x/work\"]\n",
            cfg.display()
        )
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
                    "{h}/memory.db looks damaged; to restore the newest backup, {}:",
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

#[test]
fn a_damaged_file_that_is_not_the_store_gives_no_restore_steps() {
    let (d, home) = scratch();
    common::put(
        &home,
        &[
            "--kind",
            "fact",
            "--tag",
            "verified",
            "--evidence",
            "t",
            "a",
        ],
    );
    let old = d.path().join("old.db");
    write(&old, "not a database at all, only some text");
    let (code, _, err) = sm(&home, &["import", "legacy", old.to_str().unwrap()]);
    assert_eq!((code, err.as_str()), (1, "error: file is not a database\n"));
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
        "merge into the \"mcpServers\" object of a project's .mcp.json, or of another MCP client's config\n"
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
fn init_with_a_config_that_does_not_parse_prints_its_lines_and_names_the_error() {
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
            "error: {h}/config.toml: line 1, column 8: Expected ']' at the end of a table declaration; fix the line, then run sm config\n"
        )
    );
}
