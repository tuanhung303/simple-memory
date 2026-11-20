use std::io::Write;

use crate::config::{config_toml, defaults, get_db_path, get_home};
use crate::error::SmError;
use crate::py::path::{PyPath, encode_text};
use crate::py::str::{splitlines, strip_chars};
use crate::py::value::PyValue;
use crate::store::connect::connect;

pub const CONFIG_HEADER: &str = "\
# simple-memory config. Each key shows its default as a comment; remove the `# ` to change it.
# `sm config` prints the config in effect. Keep project-specific aliases here, never in a repo.
";

/// Where each harness keeps its hooks, in the order `hooks print` offers them.
pub const HOOK_TARGETS: [(&str, &str); 3] = [
    (
        "claude-code",
        "the \"hooks\" object in ~/.claude/settings.json (or a project's .claude/settings.json)",
    ),
    ("agy", "~/.gemini/config/hooks.json"),
    (
        "mcp",
        "the \"mcpServers\" object of a project's .mcp.json, or of another MCP client's config",
    ),
];

/// `DEFAULTS["automemory"]["root"]`.
fn default_automemory_root() -> String {
    let field = |v: &PyValue, key: &str| match v {
        PyValue::Dict(pairs) => pairs
            .iter()
            .find(|(k, _)| matches!(k, PyValue::Str(s) if s == key))
            .map(|(_, v)| v.clone()),
        _ => None,
    };
    let root = field(&defaults(), "automemory").and_then(|a| field(&a, "root"));
    match root {
        Some(v) => v.py_str(),
        None => unreachable!("DEFAULTS holds automemory.root"),
    }
}

/// The commented defaults; automemory and capture are switched
/// off when Claude Code is absent.
pub fn config_text(claude_projects: &PyPath) -> Result<String, SmError> {
    let text = format!("{CONFIG_HEADER}\n{}", config_toml(&defaults(), true)?);
    if claude_projects.is_dir() {
        return Ok(text);
    }
    let mut lines: Vec<String> = Vec::new();
    let mut section = String::new();
    for line in splitlines(&text, false) {
        if line.starts_with('[') {
            section = strip_chars(line, "[]").to_string();
        }
        if (section == "automemory" || section == "capture") && line == "# enabled = true" {
            lines.push(format!(
                "enabled = false  # no {} at sm init",
                claude_projects.as_str()
            ));
        } else {
            lines.push(line.to_string());
        }
    }
    Ok(lines.join("\n") + "\n")
}

/// `run_init(home)`: create the home, the database and a commented config.toml; an existing
/// file is kept as it is. Returns one line per step, for the CLI to print.
pub fn run_init(home: Option<&str>) -> Result<Vec<String>, SmError> {
    let h = get_home(home)?;
    let mut done = Vec::new();
    done.push(format!(
        "{} home {}",
        if h.is_dir() { "kept" } else { "created" },
        h.as_str()
    ));
    h.mkdir(true, true)?;
    let db = get_db_path(Some(h.as_str()))?;
    let had_db = db.is_file();
    drop(connect(db.as_str(), None)?);
    done.push(format!(
        "{} database {}",
        if had_db { "kept" } else { "created" },
        db.as_str()
    ));
    let cfg = h.join("config.toml");
    let claude_projects = PyPath::new(&default_automemory_root()).expanduser()?;
    let os = cfg.to_os();
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&os)
    {
        Ok(mut fh) => {
            let bytes = encode_text(&config_text(&claude_projects)?)?;
            fh.write_all(&bytes)
                .map_err(|e| SmError::from_io(&e, None))?;
            done.push(format!("created config {}", cfg.as_str()));
            if !claude_projects.is_dir() {
                done.push(format!(
                    "no {}: automemory and session cards are off in {}",
                    claude_projects.as_str(),
                    cfg.as_str()
                ));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            done.push(format!(
                "kept config {} (sm init never overwrites it)",
                cfg.as_str()
            ));
        }
        Err(e) => return Err(SmError::from_io(&e, Some(&os))),
    }
    Ok(done)
}

/// `os.path.split(p)` on posix.
fn path_split(p: &str) -> (&str, &str) {
    let i = p.rfind('/').map_or(0, |i| i + 1);
    let (head, tail) = p.split_at(i);
    if !head.is_empty() && head.bytes().any(|b| b != b'/') {
        return (head.trim_end_matches('/'), tail);
    }
    (head, tail)
}

/// `os.path.join(a, b)` on posix, for a relative `b`.
fn path_join(a: &str, b: &str) -> String {
    if b.starts_with('/') {
        b.to_string()
    } else if a.is_empty() || a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

/// `shutil._access_check(fn, os.F_OK | os.X_OK)`: exists, executable by the real uid, not a folder.
fn access_check(name: &str) -> bool {
    let os = crate::py::io::fs_path(name);
    let Ok(meta) = std::fs::metadata(&os) else {
        return false;
    };
    let Ok(c) = std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(os.as_os_str()))
    else {
        return false;
    };
    // SAFETY: access(2) reads a NUL-terminated path that `c` owns for the call.
    let ok = unsafe { libc::access(c.as_ptr(), libc::F_OK | libc::X_OK) } == 0;
    ok && !meta.is_dir()
}

/// `os.confstr("CS_PATH")`.
fn cs_path() -> String {
    // SAFETY: confstr with a null buffer returns the size it needs; the second call fills `buf`.
    unsafe {
        let n = libc::confstr(libc::_CS_PATH, std::ptr::null_mut(), 0);
        if n == 0 {
            return "/bin:/usr/bin".into();
        }
        let mut buf = vec![0u8; n];
        libc::confstr(libc::_CS_PATH, buf.as_mut_ptr().cast(), n);
        buf.truncate(n - 1);
        crate::py::str::decode_surrogateescape(&buf)
    }
}

/// `shutil.which(cmd, path=path)` on posix with the default mode.
pub fn which(cmd: &str, path: Option<&str>) -> Option<String> {
    let (dirname, cmd) = path_split(cmd);
    let dirs: Vec<String> = if !dirname.is_empty() {
        vec![dirname.to_string()]
    } else {
        let path = match path {
            Some(p) => p.to_string(),
            None => match std::env::var_os("PATH") {
                Some(p) => crate::py::io::decode_argv(&p),
                None => cs_path(),
            },
        };
        if path.is_empty() {
            return None;
        }
        path.split(':').map(String::from).collect()
    };
    // Python skips a folder it has seen; a second look gives the same answer, so none is kept.
    for dir in &dirs {
        let name = path_join(dir, cmd);
        if access_check(&name) {
            return Some(name);
        }
    }
    None
}

fn s(v: &str) -> PyValue {
    PyValue::Str(v.into())
}

fn dict(pairs: Vec<(&str, PyValue)>) -> PyValue {
    PyValue::Dict(pairs.into_iter().map(|(k, v)| (s(k), v)).collect())
}

/// `hook_snippet(harness, sm_bin)`: the settings fragment that runs `sm hook prompt` on each
/// prompt, or `sm mcp`; nothing is written.
pub fn hook_snippet(harness: &str, sm_bin: Option<&str>) -> Result<PyValue, SmError> {
    let binary = match sm_bin.filter(|b| !b.is_empty()) {
        Some(b) => b.to_string(),
        None => which("sm", None).unwrap_or_else(|| "sm".into()),
    };
    if harness == "mcp" {
        let server = dict(vec![
            ("type", s("stdio")),
            ("command", s(&binary)),
            ("args", PyValue::List(vec![s("mcp")])),
        ]);
        return Ok(dict(vec![(
            "mcpServers",
            dict(vec![("simple-memory", server)]),
        )]));
    }
    let command = format!("{binary} hook prompt");
    if harness == "claude-code" {
        let entry = dict(vec![
            ("type", s("command")),
            ("command", s(&command)),
            ("timeout", PyValue::int(15)),
        ]);
        let group = dict(vec![("hooks", PyValue::List(vec![entry]))]);
        return Ok(dict(vec![(
            "hooks",
            dict(vec![("UserPromptSubmit", PyValue::List(vec![group]))]),
        )]));
    }
    if harness == "agy" {
        let entry = dict(vec![
            ("type", s("command")),
            ("command", s(&format!("{command} --agy"))),
            ("timeout", PyValue::int(15)),
        ]);
        return Ok(dict(vec![(
            "sm-recall",
            dict(vec![("PreInvocation", PyValue::List(vec![entry]))]),
        )]));
    }
    let names: Vec<&str> = HOOK_TARGETS.iter().map(|(k, _)| *k).collect();
    Err(SmError::value(format!(
        "unknown harness {}; expected one of {}",
        crate::py::str::repr(harness),
        names.join(", ")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::py::json::{Opts, dumps};

    /// The first lines of Python's `_config_text` (at 555b8f2) and the blank line after.
    const PY_HEADER: &str = "# simple-memory config. Each key shows its default as a comment; remove the `# ` to change it.\n\
                             # `sm config` prints the config in effect. Keep project-specific aliases here, never in a repo.\n\n";

    fn tmp() -> (tempfile::TempDir, String) {
        let t = tempfile::tempdir().expect("a temp dir");
        let h = t.path().to_str().expect("a utf-8 temp path").to_string();
        (t, h)
    }

    fn one_line(v: &PyValue) -> String {
        dumps(v, &Opts::default()).unwrap()
    }

    #[test]
    fn config_text_turns_off_automemory_and_capture_only_when_claude_code_is_absent() {
        let (_t, d) = tmp();
        let present = PyPath::new(&d);
        let full = format!("{PY_HEADER}{}", config_toml(&defaults(), true).unwrap());
        assert_eq!(config_text(&present).unwrap(), full);
        let absent = PyPath::new(&format!("{d}/none"));
        let text = config_text(&absent).unwrap();
        let off = format!("enabled = false  # no {d}/none at sm init");
        let switched: Vec<&str> = text.lines().filter(|l| *l == off).collect();
        assert_eq!(switched.len(), 2);
        assert_eq!(text.replace(&off, "# enabled = true"), full);
        let mut section = "";
        for l in text.lines() {
            if l.starts_with('[') {
                section = l;
            }
            if l == off {
                assert!(
                    section == "[automemory]" || section == "[capture]",
                    "{section}"
                );
            }
        }
    }

    #[test]
    fn init_creates_home_database_and_config_then_keeps_them() {
        let (_t, d) = tmp();
        let h = format!("{d}/h");
        let lines = run_init(Some(&h)).unwrap();
        assert_eq!(
            lines[..3],
            [
                format!("created home {h}"),
                format!("created database {h}/memory.db"),
                format!("created config {h}/config.toml")
            ]
        );
        let c = rusqlite::Connection::open(format!("{h}/memory.db")).unwrap();
        assert_eq!(
            c.query_row("SELECT count(*) FROM items", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        let cfg = format!("{h}/config.toml");
        let claude = PyPath::new("~/.claude/projects").expanduser().unwrap();
        let note = format!(
            "no {}: automemory and session cards are off in {h}/config.toml",
            claude.as_str()
        );
        assert_eq!(
            lines[3..],
            if claude.is_dir() { vec![] } else { vec![note] }
        );
        assert_eq!(
            std::fs::read_to_string(&cfg).unwrap(),
            config_text(&claude).unwrap()
        );
        std::fs::write(&cfg, "[prompt_hook]\nlimit = 2\n").unwrap();
        assert_eq!(
            run_init(Some(&h)).unwrap(),
            [
                format!("kept home {h}"),
                format!("kept database {h}/memory.db"),
                format!("kept config {h}/config.toml (sm init never overwrites it)"),
            ]
        );
        assert_eq!(
            std::fs::read_to_string(&cfg).unwrap(),
            "[prompt_hook]\nlimit = 2\n"
        );
    }

    /// With `~/.claude/projects` present, init writes every default as a comment and adds no
    /// note. HOME is the process's, so the check runs in a child test process with its own HOME.
    #[test]
    fn init_with_claude_code_present_leaves_every_default_on() {
        if let Some(h) = std::env::var_os("SM_D2_INIT_CHILD") {
            let h = h.into_string().unwrap();
            let lines = run_init(Some(&h)).unwrap();
            assert_eq!(lines.len(), 3, "{lines:?}");
            let full = format!("{PY_HEADER}{}", config_toml(&defaults(), true).unwrap());
            assert_eq!(
                std::fs::read_to_string(format!("{h}/config.toml")).unwrap(),
                full
            );
            return;
        }
        let (_t, d) = tmp();
        std::fs::create_dir_all(format!("{d}/.claude/projects")).unwrap();
        let name = "service::setup::tests::init_with_claude_code_present_leaves_every_default_on";
        let st = std::process::Command::new(std::env::current_exe().unwrap())
            .args([name, "--exact", "--quiet"])
            .env("HOME", &d)
            .env("SM_D2_INIT_CHILD", format!("{d}/h"))
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&st.stdout);
        assert!(st.status.success() && text.contains("1 passed"), "{text}");
    }

    #[test]
    fn init_keeps_a_dangling_symlink_config_and_fails_on_a_file_home() {
        let (_t, d) = tmp();
        let h = format!("{d}/h");
        std::fs::create_dir(&h).unwrap();
        std::os::unix::fs::symlink(format!("{h}/elsewhere.toml"), format!("{h}/config.toml"))
            .unwrap();
        let lines = run_init(Some(&h)).unwrap();
        assert_eq!(
            lines[2],
            format!("kept config {h}/config.toml (sm init never overwrites it)")
        );
        assert!(!std::path::Path::new(&format!("{h}/elsewhere.toml")).exists());
        let f = format!("{d}/file");
        std::fs::write(&f, "not a folder").unwrap();
        let e = run_init(Some(&f)).unwrap_err();
        assert_eq!(
            e.python_text(),
            format!("FileExistsError: [Errno 17] File exists: '{f}'")
        );
    }

    #[test]
    fn init_keeps_a_config_toml_folder_since_open_x_says_it_exists() {
        let (_t, d) = tmp();
        std::fs::create_dir_all(format!("{d}/config.toml")).unwrap();
        let lines = run_init(Some(&d)).unwrap();
        assert_eq!(
            lines[2],
            format!("kept config {d}/config.toml (sm init never overwrites it)")
        );
    }

    #[test]
    fn hook_snippet_gives_each_harness_fragment() {
        let cc = hook_snippet("claude-code", Some("/x/bin/sm")).unwrap();
        assert_eq!(
            one_line(&cc),
            r#"{"hooks": {"UserPromptSubmit": [{"hooks": [{"type": "command", "command": "/x/bin/sm hook prompt", "timeout": 15}]}]}}"#
        );
        let agy = hook_snippet("agy", Some("/x/bin/sm")).unwrap();
        assert_eq!(
            one_line(&agy),
            r#"{"sm-recall": {"PreInvocation": [{"type": "command", "command": "/x/bin/sm hook prompt --agy", "timeout": 15}]}}"#
        );
        let mcp = hook_snippet("mcp", Some("/x/bin/sm")).unwrap();
        assert_eq!(
            one_line(&mcp),
            r#"{"mcpServers": {"simple-memory": {"type": "stdio", "command": "/x/bin/sm", "args": ["mcp"]}}}"#
        );
        let e = hook_snippet("vim", Some("/x/bin/sm")).unwrap_err();
        assert_eq!(
            e.python_text(),
            "ValueError: unknown harness 'vim'; expected one of claude-code, agy, mcp"
        );
    }

    /// With no binary (None or an empty string) the snippet names the `sm` found on PATH, else the
    /// bare `sm`. PATH is the process's, so each case runs in a child test process with its own.
    #[test]
    fn hook_snippet_without_a_binary_names_the_sm_on_path_or_a_bare_sm() {
        use std::os::unix::fs::PermissionsExt;
        if let Some(want) = std::env::var_os("SM_F5_HOOK_WANT") {
            let want = want.into_string().unwrap();
            for bin in [None, Some("")] {
                let v = one_line(&hook_snippet("mcp", bin).unwrap());
                assert_eq!(
                    v,
                    format!(
                        r#"{{"mcpServers": {{"simple-memory": {{"type": "stdio", "command": "{want}", "args": ["mcp"]}}}}}}"#
                    )
                );
            }
            return;
        }
        let (_t, d) = tmp();
        std::fs::create_dir(format!("{d}/bin")).unwrap();
        std::fs::create_dir(format!("{d}/empty")).unwrap();
        std::fs::write(format!("{d}/bin/sm"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            format!("{d}/bin/sm"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let name = "service::setup::tests::hook_snippet_without_a_binary_names_the_sm_on_path_or_a_bare_sm";
        for (path, want) in [
            (format!("{d}/bin"), format!("{d}/bin/sm")),
            (format!("{d}/empty"), "sm".to_string()),
        ] {
            let st = std::process::Command::new(std::env::current_exe().unwrap())
                .args([name, "--exact", "--quiet"])
                .env("PATH", &path)
                .env("SM_F5_HOOK_WANT", &want)
                .output()
                .unwrap();
            let text = String::from_utf8_lossy(&st.stdout);
            assert!(
                st.status.success() && text.contains("1 passed"),
                "PATH={path}: {text}"
            );
        }
    }

    #[test]
    fn which_finds_an_executable_file_on_the_path_only() {
        use std::os::unix::fs::PermissionsExt;
        let (_t, d) = tmp();
        for (dir, mode) in [("bin", 0o755), ("noexec", 0o644)] {
            std::fs::create_dir(format!("{d}/{dir}")).unwrap();
            std::fs::write(format!("{d}/{dir}/sm"), "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(
                format!("{d}/{dir}/sm"),
                std::fs::Permissions::from_mode(mode),
            )
            .unwrap();
        }
        std::fs::create_dir_all(format!("{d}/dir/sm")).unwrap();
        let found = Some(format!("{d}/bin/sm"));
        assert_eq!(
            which("sm", Some(&format!("{d}/noexec:{d}/dir:{d}/bin"))),
            found
        );
        assert_eq!(which("sm", Some(&format!("{d}/bin/"))), found);
        assert_eq!(which("sm", Some(&format!("{d}/noexec:{d}/dir"))), None);
        assert_eq!(which("sm", Some("")), None);
        assert_eq!(which(&format!("{d}/bin/sm"), Some("/nowhere")), found);
        assert_eq!(which(&format!("{d}/noexec/sm"), None), None);
    }

    /// `PATH=''` matches nothing, while `PATH=':'` looks in the current folder. The cwd is the
    /// process's, so the check runs in a child test process started in a folder holding `sm`.
    #[test]
    fn which_with_an_empty_path_finds_nothing_but_a_colon_looks_in_the_cwd() {
        use std::os::unix::fs::PermissionsExt;
        if std::env::var_os("SM_D2_WHICH_CHILD").is_some() {
            assert_eq!(which("sm", Some("")), None);
            assert_eq!(which("sm", Some(":")), Some("sm".to_string()));
            return;
        }
        let (_t, d) = tmp();
        std::fs::write(format!("{d}/sm"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(format!("{d}/sm"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let name = "service::setup::tests::which_with_an_empty_path_finds_nothing_but_a_colon_looks_in_the_cwd";
        let st = std::process::Command::new(std::env::current_exe().unwrap())
            .args([name, "--exact", "--quiet"])
            .env("SM_D2_WHICH_CHILD", "1")
            .current_dir(&d)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&st.stdout);
        assert!(st.status.success() && text.contains("1 passed"), "{text}");
    }

    #[test]
    fn path_split_and_join_follow_posixpath() {
        assert_eq!(path_split("sm"), ("", "sm"));
        assert_eq!(path_split("/a/b//sm"), ("/a/b", "sm"));
        assert_eq!(path_split("//sm"), ("//", "sm"));
        assert_eq!(path_join("", "sm"), "sm");
        assert_eq!(path_join("/a/", "sm"), "/a/sm");
        assert_eq!(path_join("/a", "sm"), "/a/sm");
    }
}
