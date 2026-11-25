use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;
use rusqlite::types::ValueRef;

use crate::core::item::{Item, epoch_to_as_of, validate_as_of};
use crate::core::refs::{RefsArg, RepoRoots, extract_refs, merge_refs};
use crate::core::tags::append_tag;
use crate::error::{Base, SmError};
use crate::py::json::loads;
use crate::py::path::PyPath;
use crate::py::str::{ci_literal, slice, splitlines, strip};
use crate::py::value::{PyValue, repr_bytes};
use crate::sources::{TextLines, dict_get, mtime};

/// `<tag>.*?(?:</tag>|$)` under DOTALL and IGNORECASE. Python's `$` also matches before a final
/// `\n`; `\z` differs from it only by that `\n`, which the `.strip()` after every use removes.
fn noise_re(tag: &str) -> Regex {
    Regex::new(&format!(
        "(?s){}.*?(?:{}|\\z)",
        ci_literal(&format!("<{tag}>")),
        ci_literal(&format!("</{tag}>"))
    ))
    .expect("noise pattern")
}

static TASK_NOTIF_RE: LazyLock<Regex> = LazyLock::new(|| noise_re("task-notification"));
static SYS_REMINDER_RE: LazyLock<Regex> = LazyLock::new(|| noise_re("system-reminder"));

/// `strip_noise_blocks(prompt)`.
pub fn strip_noise_blocks(prompt: Option<&str>) -> String {
    let s = strip(prompt.unwrap_or(""));
    if s.is_empty() {
        return String::new();
    }
    let cleaned = TASK_NOTIF_RE.replace_all(s, "");
    let cleaned = SYS_REMINDER_RE.replace_all(&cleaned, "");
    strip(&cleaned).to_string()
}

/// `is_noise_prompt(prompt)`.
pub fn is_noise_prompt(prompt: Option<&str>) -> bool {
    strip_noise_blocks(prompt).is_empty()
}

fn extract_repo_from_cwd_or_folder(
    cwd: Option<&str>,
    project_folder: &str,
    repos: &RepoRoots,
) -> String {
    if let Some(cwd) = cwd.filter(|c| !c.is_empty()) {
        for repo in [repos.repo_from_path(cwd), repos.repo_from_key(cwd)] {
            if repo != "unassigned" {
                return repo;
            }
        }
    }
    repos.repo_from_key(project_folder)
}

/// The `cwd` of the first of the first 30 lines that holds one; an OSError reads as no cwd.
fn first_cwd(jsonl_path: &PyPath) -> Result<Option<String>, SmError> {
    let mut lines = TextLines::open(jsonl_path)?;
    for _ in 0..30 {
        let Some(line) = lines.next_line()? else {
            break;
        };
        if !line.contains("\"cwd\"") {
            continue;
        }
        match loads(&line) {
            Ok(PyValue::Dict(obj)) => {
                if let Some(PyValue::Str(cwd)) = dict_get(&obj, "cwd") {
                    return Ok(Some(cwd.clone()));
                }
            }
            Ok(_) => {}
            Err(e) if e.python_name() == "JSONDecodeError" => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

fn index_session_repos(
    projects_dir: &PyPath,
    repos: &RepoRoots,
) -> Result<HashMap<String, String>, SmError> {
    let mut mapping = HashMap::new();
    if !projects_dir.is_dir() {
        return Ok(mapping);
    }
    for jsonl_path in projects_dir.glob("*/*.jsonl")? {
        let chat_id = jsonl_path.stem().to_string();
        let project_folder = jsonl_path.parent().name().to_string();
        let cwd = match first_cwd(&jsonl_path) {
            Ok(c) => c,
            Err(e) if e.is(Base::Os) => None,
            Err(e) => return Err(e),
        };
        mapping.insert(
            chat_id,
            extract_repo_from_cwd_or_folder(cwd.as_deref(), &project_folder, repos),
        );
    }
    Ok(mapping)
}

/// `readonly_sqlite_uri(db_path)`.
fn readonly_sqlite_uri(db_path: &str) -> Result<String, SmError> {
    let resolved = PyPath::new(db_path).expanduser()?.resolve()?;
    let wal = PyPath::new(&format!("{resolved}-wal")).to_os();
    let has_nonempty_wal = std::fs::metadata(&wal).is_ok_and(|m| m.is_file() && m.len() > 0);
    if has_nonempty_wal {
        return Ok(format!("file:{resolved}?mode=ro"));
    }
    Ok(format!("file:{resolved}?mode=ro&immutable=1"))
}

/// A cell as Python's sqlite3 returns it.
#[derive(Clone, Debug, PartialEq)]
enum Cell {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Cell {
    /// `str(value)`.
    fn py_str(&self) -> String {
        match self {
            Cell::Null => "None".into(),
            Cell::Int(n) => n.to_string(),
            Cell::Real(x) => crate::py::float::repr(*x),
            Cell::Text(s) => s.clone(),
            Cell::Blob(b) => repr_bytes(b),
        }
    }

    fn truthy(&self) -> bool {
        match self {
            Cell::Null => false,
            Cell::Int(n) => *n != 0,
            Cell::Real(x) => *x != 0.0,
            Cell::Text(s) => !s.is_empty(),
            Cell::Blob(b) => !b.is_empty(),
        }
    }
}

/// `sqlite3.connect(uri, uri=True)`: Python opens read-write-create and lets the URI's
/// `mode=ro` make it read-only.
fn open_uri(uri: &str) -> Result<rusqlite::Connection, SmError> {
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
        | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
        | rusqlite::OpenFlags::SQLITE_OPEN_URI
        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    rusqlite::Connection::open_with_flags(uri, flags)
        .map_err(|e| SmError::sqlite_open(&e, std::path::Path::new(uri)))
}

/// `conn.execute(sql).fetchall()`: each row's cells in column order.
fn fetch_rows(conn: &rusqlite::Connection, sql: &str) -> Result<Vec<Vec<Cell>>, SmError> {
    let mut stmt = conn.prepare(sql).map_err(|e| SmError::sqlite(&e))?;
    let names: Vec<String> = stmt.column_names().map(String::from).collect();
    let mut rows = stmt.query([]).map_err(|e| SmError::sqlite(&e))?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().map_err(|e| SmError::sqlite(&e))? {
        let mut cells = Vec::with_capacity(names.len());
        for (i, name) in names.iter().enumerate() {
            cells.push(match r.get_ref(i).map_err(|e| SmError::sqlite(&e))? {
                ValueRef::Null => Cell::Null,
                ValueRef::Integer(n) => Cell::Int(n),
                ValueRef::Real(x) => Cell::Real(x),
                ValueRef::Text(b) => match std::str::from_utf8(b) {
                    Ok(s) => Cell::Text(s.to_string()),
                    Err(_) => {
                        return Err(SmError::from_python(
                            "OperationalError",
                            format!(
                                "Could not decode to UTF-8 column '{name}' with text '{}'",
                                String::from_utf8_lossy(b)
                            ),
                        ));
                    }
                },
                ValueRef::Blob(b) => Cell::Blob(b.to_vec()),
            });
        }
        out.push(cells);
    }
    Ok(out)
}

/// `int(r["id"])` as its decimal text.
fn int_text(c: &Cell) -> Result<String, SmError> {
    Ok(match c {
        Cell::Int(n) => n.to_string(),
        Cell::Real(x) => crate::py::int::from_float(*x)?.text(),
        Cell::Text(s) => crate::py::int::from_str(s)?.text(),
        Cell::Blob(b) => crate::py::int::from_bytes(b)?.text(),
        Cell::Null => {
            return Err(SmError::type_(
                "int() argument must be a string, a bytes-like object or a real number, not 'NoneType'",
            ));
        }
    })
}

/// `str(value or "")`.
fn text_or_empty(c: &Cell) -> String {
    if c.truthy() {
        c.py_str()
    } else {
        String::new()
    }
}

/// `load_turns(db_path, projects_dir=None, repos=None)`.
pub fn load_turns(
    db_path: &str,
    projects_dir: Option<&str>,
    repos: Option<&RepoRoots>,
) -> Result<Vec<Item>, SmError> {
    let resolved = PyPath::new(db_path).expanduser()?.resolve()?;
    let uri = readonly_sqlite_uri(resolved.as_str())?;
    let db_mtime = if resolved.exists() {
        mtime(&resolved)?
    } else {
        0.0
    };
    let proj_root = match projects_dir {
        Some(d) => PyPath::new(d).expanduser()?.resolve()?,
        None => PyPath::new("~/.claude/projects").expanduser()?,
    };
    let default = RepoRoots::default();
    let known_repos = repos.unwrap_or(&default);
    let session_repos = index_session_repos(&proj_root, known_repos)?;

    let conn = open_uri(&uri)?;
    let rows = fetch_rows(
        &conn,
        "SELECT id, chat_id, user_prompt, assistant_response, created_at FROM turn_queue ORDER BY id",
    )?;
    drop(conn);

    let mut seen_pairs: HashSet<(String, String)> = HashSet::new();
    let mut grouped: IndexMap<String, Vec<Item>> = IndexMap::new();
    for r in &rows {
        let col = |i: usize| &r[i];
        let mut user_prompt = text_or_empty(col(2));
        let assistant_response = text_or_empty(col(3));
        if !seen_pairs.insert((user_prompt.clone(), assistant_response.clone())) {
            continue;
        }
        if is_noise_prompt(Some(&user_prompt)) {
            if strip(&user_prompt).is_empty() || strip(&assistant_response).chars().count() <= 500 {
                continue;
            }
            user_prompt = strip_noise_blocks(Some(&user_prompt));
        }

        let turn_id = int_text(col(0))?;
        let chat_id = match strip(&if col(1).truthy() {
            col(1).py_str()
        } else {
            "unknown".into()
        }) {
            "" => "unknown".to_string(),
            c => c.to_string(),
        };
        let repo = session_repos
            .get(&chat_id)
            .cloned()
            .unwrap_or_else(|| "unassigned".into());
        let mut tags_str = "turn".to_string();
        let as_of = match validate_as_of(Some(&text_or_empty(col(4))), true, true) {
            Ok(v) => v,
            Err(e) if e.is(Base::Value) => {
                tags_str = append_tag(Some(&tags_str), "as_of:mtime");
                epoch_to_as_of(db_mtime)?
            }
            Err(e) => return Err(e),
        };
        let prompt = strip(&user_prompt);
        let body = format!(
            "User:\n{prompt}\n\nAssistant:\n{}",
            strip(&assistant_response)
        );
        let title = if prompt.is_empty() {
            format!("turn:{turn_id}")
        } else {
            slice(splitlines(prompt, false)[0], None, Some(120)).to_string()
        };
        let refs = merge_refs(&[Some(RefsArg::List(extract_refs(Some(&body))))]);

        let item = Item {
            refs,
            tags: tags_str,
            source: format!("turn:{chat_id}"),
            evidence: turn_id.clone(),
            as_of,
            ..Item::bare(
                &format!("turn:{turn_id}"),
                &repo,
                "episode",
                &title,
                &body,
                "n/a",
            )
        }
        .post_init();
        grouped.entry(chat_id).or_default().push(item);
    }
    Ok(grouped.into_values().flatten().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn roots(names: &[&str]) -> RepoRoots {
        RepoRoots {
            roots: vec![(
                "/Users/devuser/Work/Repos".into(),
                "Work/Repos".into(),
                names.iter().map(|s| s.to_string()).collect(),
            )],
        }
    }

    fn db(path: &std::path::Path, rows: &[(i64, &str, &str, &str, &str)]) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE turn_queue (id INTEGER PRIMARY KEY, chat_id TEXT, user_prompt TEXT, assistant_response TEXT, created_at TEXT)",
        )
        .unwrap();
        for r in rows {
            conn.execute(
                "INSERT INTO turn_queue VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![r.0, r.1, r.2, r.3, r.4],
            )
            .unwrap();
        }
    }

    fn ids(items: &[Item]) -> Vec<&str> {
        items.iter().map(|i| i.id.as_str()).collect()
    }

    // a cwd on line 30 counts and one on line 31 does not; `"\u0063wd"` is never parsed without literal `"cwd"`.
    #[test]
    fn session_cwd_reads_30_lines_and_needs_the_literal_key() {
        let d = tempfile::tempdir_in(crate::sources::TEST_TMP).unwrap();
        let misc = d.path().join("p/misc");
        fs::create_dir_all(&misc).unwrap();
        let anvil = r#"{"cwd":"/Users/devuser/Work/Repos/anvil"}"#;
        fs::write(
            misc.join("l30.jsonl"),
            format!("{}{anvil}\n", "{\"x\":1}\n".repeat(29)),
        )
        .unwrap();
        fs::write(
            misc.join("l31.jsonl"),
            format!("{}{anvil}\n", "{\"x\":1}\n".repeat(30)),
        )
        .unwrap();
        fs::write(
            misc.join("esc.jsonl"),
            "{\"\\u0063wd\":\"/Users/devuser/Work/Repos/anvil\"}\n{\"cwd\":\"/Users/devuser/Work/Repos/design\"}\n",
        )
        .unwrap();
        let tq = d.path().join("t.db");
        db(
            &tq,
            &[
                (1, "l30", "P1", "R1", "2026-10-20"),
                (2, "l31", "P2", "R2", "2026-10-20"),
                (3, "esc", "P3", "R3", "2026-10-20"),
            ],
        );
        let projects = d.path().join("p");
        let turns = load_turns(
            tq.to_str().unwrap(),
            Some(projects.to_str().unwrap()),
            Some(&roots(&["anvil", "design"])),
        )
        .unwrap();
        let got: Vec<(&str, &str)> = turns
            .iter()
            .map(|i| (i.id.as_str(), i.repo.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("turn:1", "anvil"),
                ("turn:2", "unassigned"),
                ("turn:3", "design")
            ]
        );
    }

    #[test]
    fn turns_dedupe_noise_and_utc_to_local() {
        let d = tempfile::tempdir_in(crate::sources::TEST_TMP).unwrap();
        let projects = d.path().join("projects");
        let write = |rel: &str, cwd: &str| {
            let p = projects.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, format!("{{\"cwd\": \"{cwd}\"}}\n")).unwrap();
        };
        write(
            "-Users-devuser-Work-Repos-anvil/chat-1.jsonl",
            "/Users/devuser/Work/Repos/anvil",
        );
        write(
            "-private-tmp-sandbox-1000--Users-devuser-Work-Repos-simple-memory-abcd-scratchpad/chat-2.jsonl",
            "/private/tmp/sandbox-1000/scratchpad",
        );
        write(
            "misc-folder/chat-3.jsonl",
            "/private/tmp/sandbox-1000/-Users-devuser-Work-Repos-anvil/sess-abc-123/scratchpad/work",
        );
        let tq = d.path().join("turn_queue.db");
        db(
            &tq,
            &[
                (
                    1,
                    "chat-1",
                    "Fix R5 bug",
                    "Fixed in commit 1234567",
                    "2026-10-20 01:30:00",
                ),
                (
                    2,
                    "chat-1",
                    "Fix R5 bug",
                    "Fixed in commit 1234567",
                    "2026-10-20 01:31:00",
                ),
                (
                    3,
                    "chat-1",
                    "<task-notification>\n<task-id>t1</task-id>\n</task-notification>",
                    "ok",
                    "2026-10-20 01:32:00",
                ),
                (
                    4,
                    "chat-2",
                    "<system-reminder>reminder</system-reminder>",
                    "ack",
                    "2026-10-20 01:33:00",
                ),
                (
                    5,
                    "chat-2",
                    "Check D9 status",
                    "Done",
                    "2026-10-20 02:00:00",
                ),
                (
                    6,
                    "chat-unknown",
                    "Orphan prompt",
                    "Reply",
                    "2026-10-20 03:00:00",
                ),
                (
                    7,
                    "chat-3",
                    "Scratchpad cwd prompt",
                    "Reply",
                    "2026-10-20 04:00:00",
                ),
            ],
        );
        let turns = load_turns(
            tq.to_str().unwrap(),
            Some(projects.to_str().unwrap()),
            Some(&roots(&["simple-memory", "anvil"])),
        )
        .unwrap();
        assert_eq!(ids(&turns), ["turn:1", "turn:5", "turn:6", "turn:7"]);
        assert_eq!(turns[0].repo, "anvil");
        assert_eq!(
            turns[0].as_of,
            crate::core::item::validate_as_of(Some("2026-10-20 01:30:00"), true, false).unwrap()
        );
        assert_eq!(turns[0].source, "turn:chat-1");
        assert_eq!(turns[1].repo, "simple-memory");
        assert_eq!(turns[2].repo, "unassigned");
        assert_eq!(turns[3].repo, "anvil");
    }

    #[test]
    fn noise_prompt_kept_over_500_chars() {
        let d = tempfile::tempdir_in(crate::sources::TEST_TMP).unwrap();
        let tq = d.path().join("turn_queue_threshold.db");
        let (r500, r501) = ("a".repeat(500), "b".repeat(501));
        db(
            &tq,
            &[
                (
                    10,
                    "chat-1",
                    "<task-notification>done</task-notification>",
                    &r500,
                    "2026-10-20 01:00:00",
                ),
                (
                    11,
                    "chat-1",
                    "<task-notification>done</task-notification>",
                    &r501,
                    "2026-10-20 01:01:00",
                ),
                (
                    12,
                    "chat-1",
                    "<system-reminder>check</system-reminder>",
                    &r501,
                    "2026-10-20 01:02:00",
                ),
            ],
        );
        let none = d.path().join("no_projects");
        let turns = load_turns(
            tq.to_str().unwrap(),
            Some(none.to_str().unwrap()),
            Some(&roots(&["anvil"])),
        )
        .unwrap();
        assert_eq!(ids(&turns), ["turn:11", "turn:12"]);
        for t in &turns {
            assert!(
                !t.body.contains("<task-notification>") && !t.body.contains("<system-reminder>")
            );
            assert_eq!(t.body, format!("User:\n\n\nAssistant:\n{r501}"));
            assert_eq!(t.title, t.id, "an empty prompt titles the turn by id");
        }
    }

    #[test]
    fn noise_blocks_match_like_sre_ignorecase() {
        assert_eq!(
            strip_noise_blocks(Some("a <TASK-NOTIFİCATION>x</task-notıfication> b")),
            "a  b",
            "İ and ı fold to i under sre"
        );
        assert_eq!(
            strip_noise_blocks(Some("keep <system-reminder>open to the end\n")),
            "keep"
        );
        assert_eq!(
            strip_noise_blocks(Some(
                "<system-reminder>a</system-reminder>mid<system-reminder>b</system-reminder>"
            )),
            "mid"
        );
        assert_eq!(strip_noise_blocks(None), "");
        assert!(
            is_noise_prompt(Some(" \u{1c} ")),
            "\\x1c is Python whitespace"
        );
        assert!(!is_noise_prompt(Some("<task-notification")));
    }

    #[test]
    fn ids_dates_and_errors_as_python_reads_them() {
        let d = tempfile::tempdir_in(crate::sources::TEST_TMP).unwrap();
        let tq = d.path().join("t.db");
        let conn = rusqlite::Connection::open(&tq).unwrap();
        conn.execute_batch(
            "CREATE TABLE turn_queue (id, chat_id, user_prompt, assistant_response, created_at);
             INSERT INTO turn_queue VALUES (' 7 ', '  ', 'P1', 'R', 'nope');
             INSERT INTO turn_queue VALUES (8, NULL, 'line one\rline two', 3.5, NULL);",
        )
        .unwrap();
        let none = Some("/private/tmp/no-such-projects-t2");
        let turns = load_turns(tq.to_str().unwrap(), none, None).unwrap();
        let mt = epoch_to_as_of(mtime(&PyPath::new(tq.to_str().unwrap())).unwrap()).unwrap();
        // ORDER BY id puts the INTEGER 8 before the TEXT ' 7 '; both chat ids read as "unknown".
        assert_eq!(ids(&turns), ["turn:8", "turn:7"]);
        assert_eq!(
            (
                turns[1].evidence.as_str(),
                turns[1].source.as_str(),
                turns[1].title.as_str()
            ),
            ("7", "turn:unknown", "P1")
        );
        for t in &turns {
            assert_eq!(
                (t.tags.as_str(), t.as_of.as_str()),
                ("turn, as_of:mtime", mt.as_str())
            );
        }
        assert_eq!(
            (turns[0].title.as_str(), turns[0].body.as_str()),
            ("line one", "User:\nline one\rline two\n\nAssistant:\n3.5")
        );
        conn.execute_batch("INSERT INTO turn_queue VALUES ('x9', 'c', 'P3', 'R', '2026-10-20')")
            .unwrap();
        let e = load_turns(tq.to_str().unwrap(), none, None).unwrap_err();
        assert_eq!(
            e.python_text(),
            "ValueError: invalid literal for int() with base 10: 'x9'"
        );
        conn.execute_batch("DROP TABLE turn_queue").unwrap();
        drop(conn);
        assert_eq!(
            load_turns(tq.to_str().unwrap(), none, None)
                .unwrap_err()
                .python_text(),
            "OperationalError: no such table: turn_queue"
        );
    }

    #[test]
    fn readonly_uri_drops_immutable_only_for_a_nonempty_wal() {
        let d = tempfile::tempdir_in(crate::sources::TEST_TMP).unwrap();
        let f = d.path().join("check.db");
        std::fs::write(&f, b"sqlite").unwrap();
        let fs = f.to_str().unwrap();
        assert_eq!(
            readonly_sqlite_uri(fs).unwrap(),
            format!("file:{fs}?mode=ro&immutable=1")
        );
        std::fs::write(format!("{fs}-wal"), b"").unwrap();
        assert_eq!(
            readonly_sqlite_uri(fs).unwrap(),
            format!("file:{fs}?mode=ro&immutable=1")
        );
        std::fs::write(format!("{fs}-wal"), b"nonempty-wal").unwrap();
        assert_eq!(
            readonly_sqlite_uri(fs).unwrap(),
            format!("file:{fs}?mode=ro")
        );
    }
}
