//! The home folder, config.toml with its defaults and notes,
//! `config_toml` (byte-identical to Python's), and the repo a cwd or project folder names.
//!
//! TOML goes through `crate::py::toml`, a port of Python 3.15.1's tomllib, so values and the
//! TOMLDecodeError text match Python's.

use std::cell::Cell;
use std::time::{Duration, Instant};

use indexmap::IndexMap;

use crate::core::refs::{RepoRoots, key_marker, key_segment, root_tail};
use crate::error::{Base, SmError};
#[cfg(test)]
use crate::parity::Call;
use crate::py::path::{Errors, PyPath};
use crate::py::value::PyValue;

/// `get_home(home)`: `home`, else `$SIMPLE_MEMORY_HOME` when set and not empty, else
/// `~/.simple-memory`, each through `expanduser`.
pub fn get_home(home: Option<&str>) -> Result<PyPath, SmError> {
    if let Some(h) = home {
        return PyPath::new(h).expanduser();
    }
    if let Some(env) = std::env::var_os("SIMPLE_MEMORY_HOME").filter(|v| !v.is_empty()) {
        return PyPath::new(&crate::py::io::decode_argv(&env)).expanduser();
    }
    PyPath::new("~/.simple-memory").expanduser()
}

/// `get_db_path(home)`: `<home>/memory.db`.
pub fn get_db_path(home: Option<&str>) -> Result<PyPath, SmError> {
    Ok(get_home(home)?.join("memory.db"))
}

fn s(v: &str) -> PyValue {
    PyValue::Str(v.into())
}

fn dict(pairs: Vec<(&str, PyValue)>) -> PyValue {
    PyValue::Dict(pairs.into_iter().map(|(k, v)| (s(k), v)).collect())
}

fn float(x: f64) -> PyValue {
    PyValue::Float(x)
}

/// `DEFAULTS`: every key `load_config` knows, with its default, in Python's order.
pub fn defaults() -> PyValue {
    let int = PyValue::int;
    let empty = || PyValue::Dict(Vec::new());
    dict(vec![
        ("repos", dict(vec![("roots", PyValue::List(vec![]))])),
        ("repo_aliases", empty()),
        ("keyword_aliases", empty()),
        (
            "prompt_hook",
            dict(vec![
                ("enabled", PyValue::Bool(true)),
                ("model", s("haiku")),
                ("timeout", float(8.0)),
                ("limit", int(4)),
                ("claude_bin", s("")),
                ("extra_stopwords", PyValue::List(vec![])),
                ("min_idf", float(3.0)),
                ("local_min_idf", float(crate::service::query::LOCAL_TAU)),
                ("projects", empty()),
            ]),
        ),
        (
            "catch_up",
            dict(vec![
                ("enabled", PyValue::Bool(true)),
                ("interval_minutes", int(10)),
            ]),
        ),
        (
            "upkeep",
            dict(vec![
                ("tasklog_roots", PyValue::List(vec![])),
                ("handoff_dirs", PyValue::List(vec![])),
            ]),
        ),
        (
            "capture",
            dict(vec![
                ("enabled", PyValue::Bool(true)),
                ("root", s("~/.claude/projects")),
                ("idle_minutes", int(60)),
                ("since_days", int(14)),
                ("min_prompts", int(5)),
                ("max_per_run", int(5)),
                ("model", s("haiku")),
                ("timeout", float(120.0)),
                ("dialogue_chars", int(30000)),
            ]),
        ),
        (
            "automemory",
            dict(vec![
                ("enabled", PyValue::Bool(true)),
                ("root", s("~/.claude/projects")),
            ]),
        ),
        (
            "query",
            dict(vec![(
                "default_bias",
                PyValue::List(
                    crate::core::rank::DEFAULT_BIAS
                        .iter()
                        .map(|v| s(v))
                        .collect(),
                ),
            )]),
        ),
        ("scopes", empty()),
        (
            "backup",
            dict(vec![("interval_hours", int(24)), ("keep", int(7))]),
        ),
    ])
}

/// `NOTES`: the one-line note `config_toml(commented=True)` writes above a section or key.
pub const NOTES: &[(&str, &str)] = &[
    (
        "repos",
        "Folders whose direct children are repos. A Claude Code project folder or session scratchpad whose name \
         encodes a path under a root maps to that child; any other cwd maps to its git repo.",
    ),
    (
        "repos.roots",
        "For example [\"~/code\"]. A root that does not exist holds no repos.",
    ),
    (
        "repo_aliases",
        "Repo name -> the name to store, for example \"my-repo-wt\" = \"my-repo\".",
    ),
    (
        "keyword_aliases",
        "Query phrase -> the word to search, for example \"k8s\" = \"kubernetes\".",
    ),
    (
        "prompt_hook",
        "Recall on each prompt: Haiku rewrites the prompt into a query (claude -p).",
    ),
    (
        "prompt_hook.claude_bin",
        "Path to the claude CLI; empty means the one on PATH.",
    ),
    (
        "prompt_hook.extra_stopwords",
        "Additional lowercase stopwords skipped by the local prompt-hook fallback.",
    ),
    (
        "prompt_hook.min_idf",
        "Minimum matched-term IDF sum for Haiku hook recall when scope has >= 20 items (0.0 disables; exact ref hits bypass).",
    ),
    (
        "prompt_hook.local_min_idf",
        "Minimum matched-term IDF sum for local prompt-hook fallback recall (0.0 disables).",
    ),
    (
        "prompt_hook.projects",
        "Project codes the query rewrite keeps, for example ACME = \"Acme, a repository\".",
    ),
    (
        "catch_up",
        "The prompt hook starts `sm import catch-up` in the background at most this often.",
    ),
    (
        "upkeep",
        "Where task logs (<root>/<repo>/docs/tasklog) and handoff files live.",
    ),
    (
        "capture",
        "Session cards for idle Claude Code sessions with real work and no item (Haiku).",
    ),
    (
        "automemory",
        "Import Claude Code memory files (<root>/<project>/memory/*.md).",
    ),
    (
        "query",
        "Default ranking preferences when `--bias` is omitted (`sm query`, `sm mcp`, `sm hook prompt`).",
    ),
    (
        "query.default_bias",
        "Same values as `--bias`: recency, relevance, any evidence tag, any kind, or scope:<path>.",
    ),
    (
        "backup",
        "Catch-up copies memory.db to backups/ every interval_hours and keeps the newest keep.",
    ),
    (
        "scopes",
        "Paths company, company/client or company/client/project -> matchers for `sm query --in` and `sm stats`: \"repo:<name>\", \
         or a code that stands as a whole word in the same case, for example \"acme/globex\" = [\"repo:globex-web\", \"GLX\"].",
    ),
];

fn note(key: &str) -> Option<&'static str> {
    NOTES.iter().find(|(k, _)| *k == key).map(|(_, n)| *n)
}

// ---------------------------------------------------------------- TOML

/// `tomllib.loads(text)` as a dict (`crate::py::toml`, a port of tomllib).
pub fn toml_loads(text: &str) -> Result<PyValue, SmError> {
    crate::py::toml::loads(text)
}

fn get<'a>(d: &'a PyValue, key: &str) -> Option<&'a PyValue> {
    match d {
        PyValue::Dict(p) => p
            .iter()
            .find(|(k, _)| matches!(k, PyValue::Str(s) if s == key))
            .map(|(_, v)| v),
        _ => None,
    }
}

/// The path of a home's config.toml and the tables it holds, or None when there is no file. A file
/// that does not parse is an error that names the file, the place and the next step.
fn read_config(home: Option<&str>) -> Result<Option<(PyPath, PyValue)>, SmError> {
    let path = get_home(home)?.join("config.toml");
    if !path.is_file() {
        return Ok(None);
    }
    let raw = toml_loads(&path.read_text(Errors::Strict)?).map_err(|e| {
        // A TOMLDecodeError's text is `<problem> (at line L, column C)` or `(at end of document)`.
        let Some((what, at)) = e.msg.strip_suffix(')').and_then(|m| m.rsplit_once(" (at ")) else {
            return e;
        };
        let at = if at == "end of document" {
            "the end of the file"
        } else {
            at
        };
        let msg = format!("{path}: {at}: {what}; fix the line, then run sm config");
        SmError::new(e.base, e.class.clone(), msg)
    })?;
    Ok(Some((path, raw)))
}

/// `load_config(home)`: DEFAULTS, each section updated by the file's table of that name (a key
/// the file sets keeps the default's place; a new key goes last). Other sections are dropped. A
/// key whose default is a list reads a string as a list of that one string (`[query] default_bias`
/// keeps the comma-separated string it has always taken) and refuses any other value.
pub fn load_config(home: Option<&str>) -> Result<PyValue, SmError> {
    let mut default = defaults();
    let Some((cfg_path, raw)) = read_config(home)? else {
        return Ok(default);
    };
    if let PyValue::Dict(sections) = &mut default {
        for (key, section) in sections.iter_mut() {
            let PyValue::Str(name) = key else { continue };
            let (Some(PyValue::Dict(over)), PyValue::Dict(base)) = (get(&raw, name), section)
            else {
                continue;
            };
            for (k, v) in over {
                match base.iter_mut().find(|(bk, _)| bk.key_eq(k)) {
                    Some((_, slot @ PyValue::List(_)))
                        if matches!(v, PyValue::Str(_))
                            && !(name == "query" && k.key_eq(&s("default_bias"))) =>
                    {
                        *slot = PyValue::List(vec![v.clone()]);
                    }
                    Some((_, PyValue::List(_)))
                        if !matches!(v, PyValue::List(_) | PyValue::Str(_)) =>
                    {
                        return Err(SmError::value(format!(
                            "{cfg_path}: {name}.{} must be a list, for example []",
                            k.py_str()
                        )));
                    }
                    Some(slot) => slot.1 = v.clone(),
                    None => base.push((k.clone(), v.clone())),
                }
            }
        }
    }
    Ok(default)
}

/// One warning line per section or key config.toml sets that DEFAULTS does not name, and per
/// `[query] default_bias` word that a query would refuse. A table whose default is empty
/// (`[repo_aliases]`, `[scopes]`, `[prompt_hook.projects]`) takes any key.
pub fn unknown_keys(home: Option<&str>) -> Result<Vec<String>, SmError> {
    let Some((path, raw)) = read_config(home)? else {
        return Ok(Vec::new());
    };
    let mut found = Vec::new();
    unknown_in(&raw, &defaults(), "", &mut found);
    let mut lines: Vec<String> = found
        .into_iter()
        .map(|what| format!("warning: {path}: unknown {what}; sm config prints the keys in effect"))
        .collect();
    let words = match get(&raw, "query").and_then(|q| get(q, "default_bias")) {
        Some(PyValue::Str(v)) => v.split(',').map(|w| s(w.trim())).collect(),
        Some(PyValue::List(items)) => items.clone(),
        _ => Vec::new(),
    };
    for word in words {
        let bias = match &word {
            PyValue::Str(w) => crate::core::rank::parse_bias(w).ok(),
            _ => None,
        };
        match bias {
            // A scope word must name a [scopes] path, as a query resolves it.
            Some(b) => {
                if let Err(e) = crate::service::query::resolve_scope_biases(&raw, &b, &[]) {
                    lines.push(format!("warning: {path}: query.default_bias: {}", e.msg));
                }
            }
            None => lines.push(format!(
                "warning: {path}: query.default_bias: {} is not a bias; use {}, or scope:<path>",
                toml_value(&word).unwrap_or_else(|_| word.py_str()),
                crate::core::rank::bias_values().join(", ")
            )),
        }
    }
    Ok(lines)
}

fn unknown_in(set: &PyValue, known: &PyValue, prefix: &str, found: &mut Vec<String>) {
    let (PyValue::Dict(set), PyValue::Dict(names)) = (set, known) else {
        return;
    };
    if names.is_empty() {
        return;
    }
    for (k, v) in set {
        let name = format!("{prefix}{}", k.py_str());
        match names.iter().find(|(n, _)| n.key_eq(k)) {
            Some((_, default)) => unknown_in(v, default, &format!("{name}."), found),
            None if prefix.is_empty() && matches!(v, PyValue::Dict(_)) => {
                found.push(format!("section [{name}]"))
            }
            None => found.push(format!("key {name}")),
        }
    }
}

// ---------------------------------------------------------------- config_toml

/// `_toml_key`: a bare key when it is `[A-Za-z0-9_-]+`, else a JSON string (ensure_ascii=False).
pub fn toml_key(key: &str) -> String {
    if !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return key.to_string();
    }
    toml_key_json(key)
}

/// `_toml_value`: bool, int, float (repr), str (JSON), list; any other value is a TypeError.
pub fn toml_value(v: &PyValue) -> Result<String, SmError> {
    Ok(match v {
        PyValue::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        PyValue::Int(t) => crate::py::json::int_text(t)?,
        PyValue::Float(_) => v.py_repr(),
        PyValue::Str(st) => toml_key_json(st),
        PyValue::List(items) => {
            format!(
                "[{}]",
                items
                    .iter()
                    .map(toml_value)
                    .collect::<Result<Vec<_>, _>>()?
                    .join(", ")
            )
        }
        other => {
            other.str_digits_ok()?;
            return Err(SmError::type_(format!(
                "config value {} has no TOML form here",
                other.py_repr()
            )));
        }
    })
}

fn toml_key_json(st: &str) -> String {
    let mut out = String::new();
    crate::py::json::encode_str(st, false, &mut out);
    out
}

fn str_key(k: &PyValue) -> Result<&str, SmError> {
    match k {
        PyValue::Str(s) => Ok(s),
        other => Err(SmError::type_(format!(
            "expected string or bytes-like object, got '{}'",
            other.tp_name()
        ))),
    }
}

/// `config_toml(cfg, commented=...)`: one table per section, sub-tables after the section's plain
/// keys; `commented` writes each key as a comment with its note. Byte-identical to Python's.
pub fn config_toml(cfg: &PyValue, commented: bool) -> Result<String, SmError> {
    let PyValue::Dict(sections) = cfg else {
        return Err(SmError::new(
            Base::Other,
            "AttributeError",
            format!("'{}' object has no attribute 'items'", cfg.tp_name()),
        ));
    };
    let mut lines: Vec<String> = Vec::new();
    let pre = if commented { "# " } else { "" };
    for (section, values) in sections {
        let section = str_key(section)?;
        if !lines.is_empty() {
            lines.push(String::new());
        }
        if let Some(n) = note(section).filter(|_| commented) {
            lines.push(format!("# {n}"));
        }
        lines.push(format!("[{}]", toml_key(section)));
        let PyValue::Dict(items) = values else {
            return Err(SmError::new(
                Base::Other,
                "AttributeError",
                format!("'{}' object has no attribute 'items'", values.tp_name()),
            ));
        };
        for (key, value) in items.iter().filter(|(_, v)| !matches!(v, PyValue::Dict(_))) {
            let key = str_key(key)?;
            if let Some(n) = note(&format!("{section}.{key}")).filter(|_| commented) {
                lines.push(format!("# {n}"));
            }
            lines.push(format!("{pre}{} = {}", toml_key(key), toml_value(value)?));
        }
        for (key, table) in items {
            let PyValue::Dict(table) = table else {
                continue;
            };
            let key = str_key(key)?;
            lines.push(String::new());
            if let Some(n) = note(&format!("{section}.{key}")).filter(|_| commented) {
                lines.push(format!("# {n}"));
            }
            lines.push(format!("[{}.{}]", toml_key(section), toml_key(key)));
            for (k, v) in table {
                lines.push(format!(
                    "{pre}{} = {}",
                    toml_key(str_key(k)?),
                    toml_value(v)?
                ));
            }
        }
    }
    Ok(lines.join("\n") + "\n")
}

// ---------------------------------------------------------------- scopes

/// `[scopes]` as (path, matchers), sorted by level, so a path comes right before the paths below
/// it (`acme`, `acme/globex`, `acme-x`). Every ancestor of a key is a path too, with no matchers
/// of its own.
pub type Scopes = Vec<(String, Vec<String>)>;

/// The class of the error `scope_matchers` raises; `sm query` prints its text and exits 2.
pub const UNKNOWN_SCOPE: &str = "UnknownScope";

/// `[scopes]` of a loaded config; a bad path or value is a ValueError naming the path.
pub fn scopes(cfg: &PyValue) -> Result<Scopes, SmError> {
    let Some(PyValue::Dict(pairs)) = get(cfg, "scopes") else {
        return Ok(Vec::new());
    };
    let level_ok = |l: &str| {
        !l.is_empty()
            && l.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    };
    let mut out = Scopes::new();
    for (k, v) in pairs {
        let path = k.py_str();
        let bad = |what: &str| {
            SmError::value(format!(
                "[scopes] path {}: {what}",
                crate::py::str::repr(&path)
            ))
        };
        if !path.split('/').all(level_ok) {
            return Err(bad("each level must match [A-Za-z0-9_.-]+"));
        }
        let matchers = match v {
            PyValue::List(items) => items
                .iter()
                .map(|m| match m {
                    PyValue::Str(m) if !m.is_empty() => Some(m.clone()),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>(),
            _ => None,
        }
        .ok_or_else(|| bad("the value must be a list of non-empty strings"))?;
        out.push((path, matchers));
    }
    let keys: Vec<String> = out.iter().map(|(k, _)| k.clone()).collect();
    for key in &keys {
        let mut up = key.as_str();
        while let Some((parent, _)) = up.rsplit_once('/') {
            if !out.iter().any(|(k, _)| k == parent) {
                out.push((parent.to_string(), Vec::new()));
            }
            up = parent;
        }
    }
    out.sort_by(|a, b| a.0.split('/').cmp(b.0.split('/')));
    Ok(out)
}

/// The matchers of each path and of every path below it, deduplicated in first-seen order; an
/// unknown path is an `UNKNOWN_SCOPE` error that lists the known ones, or with no scopes, names
/// the config.toml and the line to add (`sm init` writes an empty `[scopes]` header, and a second
/// header breaks the file).
pub fn scope_matchers<S: AsRef<str>>(scopes: &Scopes, paths: &[S]) -> Result<Vec<String>, SmError> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for p in paths.iter().map(AsRef::as_ref) {
        if !scopes.iter().any(|(k, _)| k == p) {
            let known: Vec<&str> = scopes.iter().map(|(k, _)| k.as_str()).collect();
            let msg = if known.is_empty() {
                format!(
                    "unknown scope: {p}; {} lists no scopes: add {} = [\"repo:<name>\"] under a single [scopes] header (README: Multi-repo scopes and maintenance)",
                    get_home(None)?.join("config.toml"),
                    toml_key(p)
                )
            } else {
                format!("unknown scope: {p}; known: {}", known.join(", "))
            };
            return Err(SmError::new(Base::Value, UNKNOWN_SCOPE, msg));
        }
        let below = format!("{p}/");
        for (k, m) in scopes {
            if k == p || k.starts_with(&below) {
                for matcher in m {
                    if seen.insert(matcher.as_str()) {
                        out.push(matcher.clone());
                    }
                }
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- repos

/// Repo name -> stored name (`repo_aliases`).
pub type Aliases = IndexMap<String, String>;

/// `repo_aliases` of a loaded config: its str-to-str pairs.
pub fn aliases(cfg: &PyValue) -> Aliases {
    match get(cfg, "repo_aliases") {
        Some(PyValue::Dict(p)) => p
            .iter()
            .filter_map(|(k, v)| match (k, v) {
                (PyValue::Str(k), PyValue::Str(v)) => Some((k.clone(), v.clone())),
                _ => None,
            })
            .collect(),
        _ => Aliases::new(),
    }
}

/// `cfg["repos"]["roots"]`, iterated as `RepoRoots.scan` iterates it (the value is passed
/// raw): a str gives its chars and a dict its keys; a value that cannot be iterated, or an item
/// that is not a str, is Python's TypeError (`Path(item)`).
pub fn roots(cfg: &PyValue) -> Result<Vec<String>, SmError> {
    let repos = get(cfg, "repos").ok_or_else(|| SmError::key("repos"))?;
    root_strs(get(repos, "roots").ok_or_else(|| SmError::key("roots"))?)
}

fn root_strs(raw: &PyValue) -> Result<Vec<String>, SmError> {
    crate::sources::iterate(raw)?
        .iter()
        .map(|item| match item.as_ref() {
            PyValue::Str(s) => Ok(s.clone()),
            other => Err(SmError::type_(format!(
                "argument should be a str or an os.PathLike object where __fspath__ returns a str, not '{}'",
                crate::py::value::type_name(other)
            ))),
        })
        .collect()
}

/// `repo_roots(cfg)`.
pub fn repo_roots(cfg: &PyValue) -> Result<RepoRoots, SmError> {
    RepoRoots::scan(&roots(cfg)?)
}

/// `project_dir_repo(name, repo_aliases, roots)`: the repo a Claude Code project folder name (or
/// a path holding one) names, else "unassigned".
pub fn project_dir_repo<S: AsRef<str>>(
    name: &str,
    repo_aliases: &Aliases,
    roots: &[S],
) -> Result<String, SmError> {
    let repo = RepoRoots::scan(roots)?.repo_from_key(name);
    if repo == "unassigned" {
        return Ok(repo);
    }
    Ok(repo_aliases.get(&repo).cloned().unwrap_or(repo))
}

/// `subprocess.run(argv, capture_output=True, text=True, check=False, timeout=timeout)` inside
/// `except OSError, subprocess.SubprocessError`: None when the command cannot
/// start or times out (only the child is killed), else whether it exited 0 and its stdout. A
/// UnicodeDecodeError of either pipe still raises, as in `Popen.communicate`.
fn run_text(argv: &[String], timeout: f64) -> Result<Option<(bool, String)>, SmError> {
    use crate::service::capture::{Run, is_subprocess_error, run_process};
    match run_process(&Run {
        argv: argv.to_vec(),
        timeout: Some(timeout),
        text: true,
        ..Default::default()
    }) {
        Ok(done) => Ok(Some((done.returncode == 0, done.stdout))),
        Err(e) if e.is(crate::error::Base::Os) || is_subprocess_error(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

thread_local! {
    /// The time `git_common_dir` took on this thread since `take_git_ms` last read it.
    static GIT_TIME: Cell<Duration> = const { Cell::new(Duration::ZERO) };
}

/// The milliseconds `git_common_dir` took on this thread since the last call (hook.log `git_ms`).
pub fn take_git_ms() -> i64 {
    GIT_TIME.with(Cell::take).as_millis() as i64
}

const GIT_TIMEOUT_S: f64 = 2.0;
/// A git call that starts at or after the deadline still gets this long, as the store's lock wait
/// gets one 1 ms try.
const GIT_LAST_TRY_S: f64 = 0.1;

/// `git -C <base> rev-parse --git-common-dir` with Python's 2 s timeout (the time left before the
/// deadline, at least GIT_LAST_TRY_S, when that is shorter), text mode; None on any OSError,
/// timeout or non-zero exit. A run that the time left killed is a TimeoutExpired: the prompt hook
/// logs it as its budget.
fn git_common_dir(base: &PyPath, deadline: Option<Instant>) -> Result<Option<String>, SmError> {
    let argv = ["git", "-C", base.as_str(), "rev-parse", "--git-common-dir"].map(String::from);
    let timeout = deadline.map_or(GIT_TIMEOUT_S, |d| {
        d.saturating_duration_since(Instant::now())
            .as_secs_f64()
            .clamp(GIT_LAST_TRY_S, GIT_TIMEOUT_S)
    });
    let started = Instant::now();
    let run = run_text(&argv, timeout);
    let took = started.elapsed();
    GIT_TIME.with(|t| t.set(t.get() + took));
    let run = run?;
    // run_text gives None on a timeout or a command that cannot start; only a timeout lasts the
    // whole timeout, which starts after the spawn.
    if run.is_none() && timeout < GIT_TIMEOUT_S && took.as_secs_f64() >= timeout {
        return Err(crate::service::capture::timeout_expired("git", timeout));
    }
    Ok(common_dir_of(run))
}

/// `proc.stdout.strip()` when the run exited 0 and printed something.
fn common_dir_of(run: Option<(bool, String)>) -> Option<String> {
    let (true, out) = run? else { return None };
    let text = crate::py::str::strip(&out);
    (!text.is_empty()).then(|| text.to_string())
}

/// `resolve_repo(cwd, repo_aliases, roots)`: a cwd (or its resolved path) holding a project-folder
/// key under a root names that root's child; else the git repo's folder name; else "unassigned".
pub fn resolve_repo<S: AsRef<str>>(
    cwd: Option<&str>,
    repo_aliases: &Aliases,
    roots: &[S],
) -> Result<String, SmError> {
    resolve_repo_until(cwd, repo_aliases, roots, None)
}

/// `resolve_repo` whose git call ends by the deadline (the prompt hook's).
pub fn resolve_repo_until<S: AsRef<str>>(
    cwd: Option<&str>,
    repo_aliases: &Aliases,
    roots: &[S],
    deadline: Option<Instant>,
) -> Result<String, SmError> {
    let raw_base = match cwd {
        Some(c) => PyPath::new(c).expanduser()?,
        None => PyPath::cwd()?,
    };
    let base = raw_base.resolve()?;
    let markers = roots
        .iter()
        .map(|r| Ok(key_marker(&root_tail(r.as_ref())?)))
        .collect::<Result<Vec<_>, SmError>>()?;
    for candidate in [raw_base.as_str(), base.as_str()] {
        if markers
            .iter()
            .any(|m| !key_segment(candidate, m).is_empty())
        {
            let repo = project_dir_repo(candidate, repo_aliases, roots)?;
            // a key that names no root child (a deleted repo) falls through to git
            if repo != "unassigned" {
                return Ok(repo);
            }
        }
    }
    let mut repo_name = String::new();
    if let Some(out) = git_common_dir(&base, deadline)? {
        let common = base.join(&out).resolve()?;
        repo_name = if common.name() == ".git" {
            common.parent().name().to_string()
        } else {
            common.name().to_string()
        };
    }
    if repo_name.is_empty() {
        return Ok("unassigned".into());
    }
    Ok(repo_aliases.get(&repo_name).cloned().unwrap_or(repo_name))
}

/// The bridge's `encode` of a loaded config: a `datetime.time` has no protocol form, so it raises
/// the bridge's TypeError, as the Python parity reference reports it.
#[cfg(test)]
fn to_wire(v: &PyValue) -> Result<serde_json::Value, SmError> {
    use serde_json::{Value, json};
    Ok(match v {
        PyValue::None => Value::Null,
        PyValue::Bool(b) => json!(b),
        PyValue::Int(t) => t
            .parse::<i64>()
            .map_or_else(|_| json!({"__int": t}), |i| json!(i)),
        PyValue::Float(x) if x.is_nan() => json!({"__float": "nan"}),
        PyValue::Float(x) if x.is_infinite() => {
            json!({"__float": if *x > 0.0 { "inf" } else { "-inf" }})
        }
        PyValue::Float(x) => json!(x),
        PyValue::Str(st) => json!(st),
        PyValue::Date(d) => json!({"__date": d.isoformat()}),
        PyValue::DateTime(d) => json!({"__datetime": d.isoformat('T')}),
        PyValue::Time(_) => return Err(SmError::type_("no protocol form for time")),
        PyValue::List(x) | PyValue::Tuple(x) => {
            Value::Array(x.iter().map(to_wire).collect::<Result<_, _>>()?)
        }
        PyValue::Dict(p) => Value::Object(
            p.iter()
                .map(|(k, v)| Ok((k.py_str(), to_wire(v)?)))
                .collect::<Result<_, SmError>>()?,
        ),
        other => json!(other.py_repr()),
    })
}

/// `py.config.load(name, content, commented)`: a fresh scratch folder of this call's own, holding
/// `name` (a path under it) as the home with `content` as config.toml (bytes; None: no file), then
/// load_config and config_toml of it. The folder goes when the call ends, so no two runs share one.
#[cfg(test)]
fn load_case(call: &Call) -> Result<serde_json::Value, SmError> {
    let name = call.str(0, "home")?;
    let scratch = tempfile::tempdir().map_err(|e| SmError::from_io(&e, None))?;
    let dir = scratch.path().join(crate::py::io::fs_path(name));
    std::fs::create_dir_all(&dir).map_err(|e| SmError::from_io(&e, Some(&dir)))?;
    let home = dir
        .to_str()
        .ok_or_else(|| SmError::value("parity: home is not UTF-8"))?;
    if let Some(c) = call.arg(1, "content").filter(|c| !c.is_null()) {
        let PyValue::Bytes(b) = crate::py::json::protocol_value(c)? else {
            return Err(SmError::type_("parity: content must be bytes"));
        };
        let f = dir.join("config.toml");
        std::fs::write(&f, b).map_err(|e| SmError::from_io(&e, Some(&f)))?;
    }
    let commented = call.opt_bool(2, "commented")?.unwrap_or(false);
    let text = |r: Result<serde_json::Value, SmError>| {
        r.unwrap_or_else(|e| serde_json::json!({"err": e.python_text()}))
    };
    let cfg = load_config(Some(home));
    // The reference's `_big` takes `str()` of each int, which raises past 4,300 digits.
    if let Ok(c) = &cfg {
        c.str_digits_ok()?;
    }
    let toml = cfg
        .as_ref()
        .map_err(Clone::clone)
        .and_then(|c| config_toml(c, commented))
        .map(serde_json::Value::from);
    Ok(serde_json::json!({"load": text(cfg.and_then(|c| to_wire(&c))), "toml": text(toml)}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home_with(toml: &str) -> tempfile::TempDir {
        let h = tempfile::tempdir().expect("a temp dir");
        std::fs::write(h.path().join("config.toml"), toml).expect("write config.toml");
        h
    }

    #[test]
    fn defaults_round_trip_through_config_toml() {
        let text = config_toml(&defaults(), false).unwrap();
        assert!(text.starts_with("[repos]\nroots = []\n\n[repo_aliases]\n\n[keyword_aliases]\n\n[prompt_hook]\nenabled = true\n"));
        assert!(text.contains(
            "timeout = 8.0\nlimit = 4\nclaude_bin = \"\"\nextra_stopwords = []\nmin_idf = 3.0\nlocal_min_idf = 8.0\n\n[prompt_hook.projects]\n\n[catch_up]"
        ));
        assert!(text.ends_with("[backup]\ninterval_hours = 24\nkeep = 7\n"));
        assert_eq!(
            config_toml(&toml_loads(&text).unwrap(), false).unwrap(),
            text
        );
    }

    #[test]
    fn load_merges_known_sections_in_place_and_keeps_file_order() {
        let h = home_with(
            "[backup]\nzeta = 1\nkeep = 3\nalpha = 2\n[nope]\nx = 1\n[prompt_hook.projects]\nZZ = \"z\"\nAA = \"a\"\n",
        );
        let cfg = load_config(h.path().to_str()).unwrap();
        let text = config_toml(&cfg, false).unwrap();
        assert!(
            text.ends_with("[backup]\ninterval_hours = 24\nkeep = 3\nzeta = 1\nalpha = 2\n"),
            "{text}"
        );
        assert!(
            text.contains("[prompt_hook.projects]\nZZ = \"z\"\nAA = \"a\"\n"),
            "{text}"
        );
        assert!(!text.contains("nope"));
    }

    #[test]
    fn values_and_keys_without_a_toml_form() {
        assert_eq!(toml_key("a-b_9"), "a-b_9");
        assert_eq!(toml_key("a.b"), "\"a.b\"");
        assert_eq!(toml_key(""), "\"\"");
        assert_eq!(toml_key("ë"), "\"ë\"");
        assert_eq!(toml_value(&PyValue::Float(1e16)).unwrap(), "1e+16");
        assert_eq!(
            toml_value(&PyValue::None).unwrap_err().python_text(),
            "TypeError: config value None has no TOML form here"
        );
    }

    #[test]
    fn toml_ints_in_every_radix_and_past_i64() {
        let v = toml_loads(
            "a = 0xff\nb = 0o17\nc = 0b101\nd = 99999999999999999999\ne = 0xffffffffffffffffffff\n",
        )
        .unwrap();
        assert_eq!(
            v.py_repr(),
            "{'a': 255, 'b': 15, 'c': 5, 'd': 99999999999999999999, 'e': 1208925819614629174706175}"
        );
    }

    // Ported config tests with absolute roots and explicit homes so no test sets
    // HOME or SIMPLE_MEMORY_HOME.
    const OWNER_CONFIG: &str = "\n[prompt_hook]\nenabled = true\nmodel = \"haiku\"\ntimeout = 8.0\nlimit = 4\n\n[prompt_hook.projects]\nACME = \"Acme, a repository\"\nops = \"the ops repo\"\n";
    const OWNER_SNIPPET: &str = "\n[repos]\nroots = [\"~/Work/Repos\"]\n\n[repo_aliases]\nanvil-wt = \"anvil\"\n\n[keyword_aliases]\n\"queue lag\" = \"queue-worker\"\n\n[upkeep]\ntasklog_roots = [\"~/Work/Repos\", \"~/ops-logs\"]\nhandoff_dirs = [\"~/Work/Repos/anvil/tmp/handoffs\"]\n";

    fn key_of(path: &std::path::Path) -> String {
        crate::core::refs::encode_project_dir(path.to_str().expect("utf-8 temp path"))
    }

    #[test]
    fn owner_snippet_gives_the_effective_config() {
        let h = home_with(&format!("{OWNER_CONFIG}{OWNER_SNIPPET}"));
        let cfg = load_config(h.path().to_str()).unwrap();
        assert_eq!(
            get(&cfg, "repos").unwrap().py_repr(),
            "{'roots': ['~/Work/Repos']}"
        );
        assert_eq!(
            get(&cfg, "repo_aliases").unwrap().py_repr(),
            "{'anvil-wt': 'anvil'}"
        );
        assert_eq!(
            get(&cfg, "keyword_aliases").unwrap().py_repr(),
            "{'queue lag': 'queue-worker'}"
        );
        assert_eq!(
            get(&cfg, "prompt_hook").unwrap().py_repr(),
            "{'enabled': True, 'model': 'haiku', 'timeout': 8.0, 'limit': 4, 'claude_bin': '', \
             'extra_stopwords': [], 'min_idf': 3.0, 'local_min_idf': 8.0, 'projects': {'ACME': 'Acme, a repository', 'ops': 'the ops repo'}}"
        );
        assert_eq!(
            get(&cfg, "upkeep").unwrap().py_repr(),
            "{'tasklog_roots': ['~/Work/Repos', '~/ops-logs'], 'handoff_dirs': ['~/Work/Repos/anvil/tmp/handoffs']}"
        );
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(load_config(empty.path().to_str()).unwrap(), defaults());
    }

    #[test]
    fn project_dir_decode_covers_the_old_hardcoded_root_cases() {
        let h = tempfile::tempdir().unwrap();
        let root = h.path().join("Work/Repos");
        for name in [
            "anvil",
            "anvil-wt",
            "zenith",
            "zenith-local",
            "tmp",
            ".cache",
        ] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        let roots = [root.to_str().unwrap().to_string()];
        let aliases: Aliases = [("anvil-wt".to_string(), "anvil".to_string())]
            .into_iter()
            .collect();
        let k = key_of(&root);
        for (name, repo) in [
            (format!("{k}-anvil"), "anvil"),
            (format!("{k}-anvil-skills-x"), "anvil"),
            (format!("{k}-anvil--claude-worktrees-u5"), "anvil"),
            (format!("{k}-anvil-wt"), "anvil"),
            (format!("{k}-anvil-wt-gone-branch"), "anvil"),
            (format!("{k}-zenith-local-polls"), "zenith-local"),
            (format!("{k}-zenith-abc-QRT"), "zenith"),
            (
                format!("-private-tmp-u-1000-{k}-zenith-1234-scratchpad"),
                "zenith",
            ),
            (format!("{k}-tmp-x"), "unassigned"),
            (format!("{k}--cache"), "unassigned"),
            (format!("{k}-nope"), "unassigned"),
            (k.clone(), "unassigned"),
            ("-private-tmp".to_string(), "unassigned"),
        ] {
            assert_eq!(
                project_dir_repo(&name, &aliases, &roots).unwrap(),
                repo,
                "{name}"
            );
        }
        // A scratchpad cwd maps through the key it holds.
        let pad = h
            .path()
            .join("u-1000")
            .join(format!("{k}-anvil-wt"))
            .join("sess/scratchpad");
        std::fs::create_dir_all(&pad).unwrap();
        assert_eq!(
            resolve_repo(pad.to_str(), &aliases, &roots).unwrap(),
            "anvil"
        );
    }

    #[test]
    fn a_repo_folder_whose_name_holds_the_root_marker_maps_through_git() {
        let h = tempfile::tempdir().unwrap();
        let git = |p: &std::path::Path| {
            std::fs::create_dir_all(p).unwrap();
            assert!(
                std::process::Command::new("git")
                    .args(["init", "-q"])
                    .arg(p)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        let code = h.path().join("code");
        git(&code.join("claude-code-hooks"));
        let roots = [code.to_str().unwrap().to_string()];
        assert_eq!(
            resolve_repo(
                code.join("claude-code-hooks").to_str(),
                &Aliases::new(),
                &roots
            )
            .unwrap(),
            "claude-code-hooks"
        );
        let gone = h
            .path()
            .join(format!("{}-gone", key_of(&code)))
            .join("repo");
        git(&gone);
        assert_eq!(
            resolve_repo(gone.to_str(), &Aliases::new(), &roots).unwrap(),
            "repo"
        );
        let al: Aliases = [("repo".to_string(), "REPO".to_string())]
            .into_iter()
            .collect();
        assert_eq!(resolve_repo(gone.to_str(), &al, &roots).unwrap(), "REPO");
        // Outside any git repo and key: unassigned.
        assert_eq!(
            resolve_repo(h.path().to_str(), &Aliases::new(), &roots).unwrap(),
            "unassigned"
        );
    }

    #[test]
    fn config_and_resolve_repo_through_aliases() {
        let h = tempfile::tempdir().unwrap();
        let home = h.path().to_str().unwrap();
        assert_eq!(
            get_db_path(Some(home)).unwrap().as_str(),
            format!("{home}/memory.db")
        );
        let code = h.path().join("code");
        std::fs::create_dir_all(code.join("proj-wt")).unwrap();
        std::fs::create_dir_all(code.join("proj")).unwrap();
        std::fs::write(
            h.path().join("config.toml"),
            format!("[repos]\nroots = [\"{}\"]\n[repo_aliases]\n\"proj-wt\" = \"proj\"\n[keyword_aliases]\n\"k8s\" = \"kubernetes\"\n", code.display()),
        )
        .unwrap();
        let cfg = load_config(Some(home)).unwrap();
        let (al, rs) = (aliases(&cfg), roots(&cfg).unwrap());
        assert_eq!(al.get("proj-wt").map(String::as_str), Some("proj"));
        let k = key_of(&code);
        let pad = h
            .path()
            .join("sandbox-1000")
            .join(format!("{k}-proj-wt"))
            .join("sess-01/scratchpad");
        std::fs::create_dir_all(&pad).unwrap();
        assert_eq!(resolve_repo(pad.to_str(), &al, &rs).unwrap(), "proj");
        assert_eq!(
            resolve_repo(pad.to_str(), &al, &[] as &[String]).unwrap(),
            "unassigned"
        );
        let unknown = h
            .path()
            .join("sandbox-1000")
            .join(format!("{k}-nonexistent-repo"))
            .join("sess-02/scratchpad");
        std::fs::create_dir_all(&unknown).unwrap();
        assert_eq!(
            resolve_repo(unknown.to_str(), &al, &rs).unwrap(),
            "unassigned"
        );
    }

    #[test]
    fn round_trips_owner_values_and_commented_defaults() {
        let owner = OWNER_CONFIG.replace(
            "ops = \"the ops repo\"",
            "ops = \"the ops repo\"\n\"Ünï x\" = \"a \\\" b \\\\ c\"",
        );
        let h = home_with(&format!("{owner}{OWNER_SNIPPET}"));
        let cfg = load_config(h.path().to_str()).unwrap();
        assert!(
            get(get(&cfg, "prompt_hook").unwrap(), "projects")
                .unwrap()
                .py_repr()
                .contains("'Ünï x': 'a \" b \\\\ c'")
        );
        assert_eq!(toml_loads(&config_toml(&cfg, false).unwrap()).unwrap(), cfg);
        let text = config_toml(&defaults(), true).unwrap();
        let loaded = toml_loads(&text).unwrap();
        assert_eq!(
            get(&loaded, "prompt_hook").unwrap().py_repr(),
            "{'projects': {}}"
        );
        assert_eq!(get(&loaded, "backup").unwrap().py_repr(), "{}");
        let uncommented: Vec<&str> = text
            .lines()
            .map(|l| match l.strip_prefix("# ") {
                Some(rest)
                    if rest
                        .split_once(" = ")
                        .is_some_and(|(k, _)| !k.contains(' ') || k.starts_with('"')) =>
                {
                    rest
                }
                _ => l,
            })
            .collect();
        assert_eq!(toml_loads(&uncommented.join("\n")).unwrap(), defaults());
        let bad = dict(vec![("x", dict(vec![("when", PyValue::None)]))]);
        assert!(
            config_toml(&bad, false)
                .unwrap_err()
                .msg
                .contains("no TOML form")
        );
    }

    #[test]
    fn loads_raise_tomllibs_text_and_values_keep_their_python_form() {
        // Python 3.15.1 probes: tomllib.loads and config._toml_value on the same input.
        assert_eq!(
            toml_loads("\u{feff}[backup]\nkeep = 1\n")
                .unwrap_err()
                .python_text(),
            "TOMLDecodeError: Invalid statement (at line 1, column 1)"
        );
        assert_eq!(
            toml_loads("x = 0x\n").unwrap_err().python_text(),
            "TOMLDecodeError: Expected newline or end of document after a statement (at line 1, column 6)"
        );
        assert_eq!(
            toml_loads("x = [0b]\n").unwrap_err().python_text(),
            "TOMLDecodeError: Unclosed array (at line 1, column 7)"
        );
        assert_eq!(toml_loads("x = 0x0\n").unwrap().py_repr(), "{'x': 0}");
        let t = toml_loads("t = 07:32\n").unwrap();
        assert_eq!(t.py_repr(), "{'t': datetime.time(7, 32)}");
        let PyValue::Dict(p) = t else {
            panic!("a dict")
        };
        assert_eq!(
            toml_value(&p[0].1).unwrap_err().python_text(),
            "TypeError: config value datetime.time(7, 32) has no TOML form here"
        );
        let big = PyValue::Int("1".repeat(4301));
        let limit = "ValueError: Exceeds the limit (4300 digits) for integer string conversion; use sys.set_int_max_str_digits() to increase the limit";
        assert_eq!(toml_value(&big).unwrap_err().python_text(), limit);
        assert_eq!(
            toml_value(&PyValue::List(vec![PyValue::Dict(vec![(s("a"), big)])]))
                .unwrap_err()
                .python_text(),
            limit
        );
    }

    #[test]
    fn git_stderr_with_an_undecodable_byte_raises_even_on_a_non_zero_exit() {
        // Python 3.15.1: config.resolve_repo('/nonexistent-sm-f4/\udcff', {}, []) raises
        // UnicodeDecodeError "'utf-8' codec can't decode byte 0xff in position 44: invalid start byte".
        // git -C exits 128 and its stderr holds the path; the position depends on git's wording.
        let err = resolve_repo(
            Some("/nonexistent-sm-f4/\u{10fdff}"),
            &Aliases::new(),
            &[] as &[String],
        )
        .unwrap_err();
        let text = err.python_text();
        assert!(
            text.starts_with(
                "UnicodeDecodeError: 'utf-8' codec can't decode byte 0xff in position "
            ),
            "{text}"
        );
        assert!(text.ends_with(": invalid start byte"), "{text}");
        assert_eq!(
            resolve_repo(
                Some("/nonexistent-sm-f4/x"),
                &Aliases::new(),
                &[] as &[String]
            )
            .unwrap(),
            "unassigned"
        );
    }

    fn sh_run(script: &str, ms: u64) -> Result<Option<(bool, String)>, SmError> {
        run_text(&["sh", "-c", script].map(String::from), ms as f64 / 1000.0)
    }

    #[test]
    fn the_common_dir_is_the_stripped_stdout_of_a_run_that_exited_0() {
        assert_eq!(
            common_dir_of(Some((true, " ../.git\u{2028}\n".into()))),
            Some("../.git".to_string())
        );
        assert_eq!(common_dir_of(Some((true, " \n".into()))), None);
        assert_eq!(common_dir_of(Some((false, ".git\n".into()))), None);
        assert_eq!(common_dir_of(None), None);
    }

    #[test]
    fn run_text_reads_output_past_the_pipe_size_while_waiting() {
        // Python's communicate reads while the child runs; 200,000 bytes fill a 64 KiB pipe.
        let got = sh_run("head -c 200000 /dev/zero | tr '\\0' a", 20_000)
            .unwrap()
            .unwrap();
        assert!(got.0);
        assert_eq!(got.1.len(), 200_000);
    }

    #[test]
    fn run_text_gives_the_exit_status_and_universal_newlines() {
        assert_eq!(
            sh_run("printf 'x\\r\\ny\\rz'; exit 2", 20_000).unwrap(),
            Some((false, "x\ny\nz".to_string()))
        );
        assert_eq!(
            sh_run("exit 0", 20_000).unwrap(),
            Some((true, String::new()))
        );
    }

    #[test]
    fn run_text_decodes_both_pipes_strictly_whatever_the_exit_status() {
        // Python 3.15.1: subprocess.run(["sh", "-c", ...], capture_output=True, text=True) raises
        // UnicodeDecodeError for the first bad pipe, stdout before stderr.
        let err = |script: &str| sh_run(script, 20_000).unwrap_err().python_text();
        assert_eq!(
            err("printf 'a\\377'; exit 3"),
            "UnicodeDecodeError: 'utf-8' codec can't decode byte 0xff in position 1: invalid start byte"
        );
        assert_eq!(
            err("printf 'ok'; printf '\\377' >&2; exit 0"),
            "UnicodeDecodeError: 'utf-8' codec can't decode byte 0xff in position 0: invalid start byte"
        );
        assert_eq!(
            err("printf 'ab\\377'; printf '\\377' >&2; exit 1"),
            "UnicodeDecodeError: 'utf-8' codec can't decode byte 0xff in position 2: invalid start byte"
        );
    }

    #[test]
    fn run_text_is_none_on_a_timeout_or_a_command_that_cannot_start() {
        let started = std::time::Instant::now();
        assert_eq!(sh_run("exec sleep 5", 100).unwrap(), None);
        // The child exits at once but its grandchild keeps both pipes open past the timeout.
        assert_eq!(sh_run("(sleep 2 &); exit 0", 300).unwrap(), None);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        let missing = run_text(&["/nonexistent-sm-f4/git".to_string()], 1.0);
        assert_eq!(missing.unwrap(), None);
    }

    #[test]
    fn roots_from_a_config_value_of_any_type() {
        // Python 3.15.1: config.repo_roots(config.load_config(home)) for each `roots` below.
        let got = |toml: &str| {
            let h = home_with(&format!("[repos]\nroots = {toml}\n"));
            match load_config(h.path().to_str()) {
                Ok(cfg) => roots(&cfg).map_err(|e| e.python_text()),
                Err(e) => Err(e.msg.replace(&h.path().display().to_string(), "<home>")),
            }
        };
        let not_a_list = |example: &str| {
            Err(format!(
                "<home>/config.toml: repos.roots must be a list, for example {example}"
            ))
        };
        let not_a_path = |t: &str| {
            Err(format!(
                "TypeError: argument should be a str or an os.PathLike object where __fspath__ returns a str, not '{t}'"
            ))
        };
        assert_eq!(
            got("[\"/x\", \"~/y\"]"),
            Ok(vec!["/x".to_string(), "~/y".to_string()])
        );
        assert_eq!(got("[07:32]"), not_a_path("time"));
        assert_eq!(got("[\"/x\", 1]"), not_a_path("int"));
        assert_eq!(got("[1979-05-27]"), not_a_path("date"));
        assert_eq!(got("[[\"a\"]]"), not_a_path("list"));
        // A value that is not a list stops at load, so it is never walked as an iterable.
        assert_eq!(got("07:32"), not_a_list("[]"));
        assert_eq!(got("5"), not_a_list("[]"));
        // a string for a list key is read as a one-item list
        assert_eq!(got("\"ab\""), Ok(vec!["ab".to_string()]));
        assert_eq!(got("{ \"/r\" = 1 }"), not_a_list("[]"));
    }

    #[test]
    fn load_case_leaves_the_path_its_home_names_alone() {
        // The bridge runs many py.config.load cases, and two runs at once must not share a folder:
        // the home each case names is a name under a scratch folder of its own, never a cwd path.
        let kept = std::path::Path::new("target/f4-load-case-sentinel");
        std::fs::create_dir_all(kept).unwrap();
        std::fs::write(kept.join("keep.txt"), "x").unwrap();
        let call = Call {
            args: vec![serde_json::json!("target/f4-load-case-sentinel")],
            kwargs: serde_json::Map::new(),
        };
        let got = load_case(&call);
        let survived = kept.join("keep.txt").exists();
        std::fs::remove_dir_all("target/f4-load-case-sentinel").unwrap();
        assert!(got.is_ok());
        assert!(
            survived,
            "load_case removed the folder its home argument names"
        );
    }

    #[test]
    fn query_default_bias_round_trips_and_overrides() {
        let text = config_toml(&defaults(), false).unwrap();
        assert!(
            text.contains("[query]\ndefault_bias = [\"recency\", \"fact\", \"decision\", \"learning\", \"executed\", \"verified\", \"decided\", \"client-stated\"]\n"),
            "{text}"
        );
        let h = home_with("[query]\ndefault_bias = [\"executed\", \"recency\"]\n");
        let cfg = load_config(h.path().to_str()).unwrap();
        let q = get(&cfg, "query").unwrap();
        assert_eq!(
            get(q, "default_bias").unwrap().py_repr(),
            "['executed', 'recency']"
        );
    }

    #[test]
    fn a_config_that_does_not_parse_names_the_file_the_place_and_the_next_step() {
        let got = |toml: &str| {
            let h = home_with(toml);
            let msg = load_config(h.path().to_str()).unwrap_err().msg;
            msg.replace(&format!("{}/config.toml", h.path().display()), "<cfg>")
        };
        assert_eq!(
            got("[repos]\nroots = ~/code\n"),
            "<cfg>: line 2, column 9: invalid value: text needs quotes and a list needs [ ]; fix the line, then run sm config"
        );
        assert_eq!(
            got("[prompt_hook]\nlimit = 2\n[prompt_hook.projects]\n[prompt_hook.projects]\n"),
            "<cfg>: line 4, column 22: [prompt_hook.projects] is declared twice; keep one header and put the keys under it; fix the line, then run sm config"
        );
        assert_eq!(
            got("[backup]\nkeep = \"3\n"),
            "<cfg>: line 2, column 10: Illegal character '\\n'; fix the line, then run sm config"
        );
        assert_eq!(
            got("[backup]\nkeep = [1, 2"),
            "<cfg>: the end of the file: Unclosed array; fix the line, then run sm config"
        );
    }

    #[test]
    fn every_list_key_reads_a_string_as_one_item_and_refuses_another_type_at_load() {
        let PyValue::Dict(sections) = defaults() else {
            panic!("defaults is a dict")
        };
        let mut checked = Vec::new();
        for (section, values) in &sections {
            let PyValue::Dict(values) = values else {
                continue;
            };
            for (key, default) in values {
                if !matches!(default, PyValue::List(_)) {
                    continue;
                }
                let name = format!("{}.{}", section.py_str(), key.py_str());
                let h = home_with(&format!(
                    "[{}]\n{} = \"/a b\"\n",
                    section.py_str(),
                    key.py_str()
                ));
                let cfg = format!("{}/config.toml", h.path().display());
                if name == "query.default_bias" {
                    // the comma-separated string default_bias has always taken still loads
                    let got = load_config(h.path().to_str()).unwrap();
                    assert_eq!(
                        get(get(&got, "query").unwrap(), "default_bias"),
                        Some(&s("/a b"))
                    );
                } else {
                    // a string for a list key is read as a one-item list; the refusal of a string
                    // stopped every command for a config that loaded before
                    let got = load_config(h.path().to_str()).unwrap();
                    let got = get(get(&got, &section.py_str()).unwrap(), &key.py_str());
                    assert_eq!(got, Some(&PyValue::List(vec![s("/a b")])), "{cfg}");
                }
                for other in ["5", "true", "{ a = 1 }"] {
                    let h = home_with(&format!(
                        "[{}]\n{} = {other}\n",
                        section.py_str(),
                        key.py_str()
                    ));
                    assert_eq!(
                        load_config(h.path().to_str()).unwrap_err().msg,
                        format!(
                            "{}/config.toml: {name} must be a list, for example []",
                            h.path().display()
                        )
                    );
                }
                // the example in the message loads when pasted
                let h = home_with(&format!("[{}]\n{} = []\n", section.py_str(), key.py_str()));
                assert!(load_config(h.path().to_str()).is_ok(), "{name}");
                checked.push(name);
            }
        }
        assert_eq!(
            checked,
            [
                "repos.roots",
                "prompt_hook.extra_stopwords",
                "upkeep.tasklog_roots",
                "upkeep.handoff_dirs",
                "query.default_bias"
            ]
        );
        let h = home_with("[upkeep]\ntasklog_roots = []\nhandoff_dirs = [\"/x\"]\n");
        assert!(load_config(h.path().to_str()).is_ok());
    }

    #[test]
    fn scope_matchers_deduplicates_preserving_first_seen_order() {
        let h = home_with(
            "[scopes]\nacme = [\"repo:ops\", \"GLX\"]\n\"acme/globex\" = [\"GLX\", \"repo:web\", \"GLX\"]\n\"acme/globex/web\" = [\"repo:web\", \"WEB\"]\n",
        );
        let cfg = load_config(h.path().to_str()).unwrap();
        let sc = scopes(&cfg).unwrap();
        assert_eq!(
            scope_matchers(&sc, &["acme", "acme/globex"]).unwrap(),
            vec!["repo:ops", "GLX", "repo:web", "WEB"]
        );
        assert_eq!(
            scope_matchers(&sc, &["acme/globex/web", "acme"]).unwrap(),
            vec!["repo:web", "WEB", "repo:ops", "GLX"]
        );
    }
}
