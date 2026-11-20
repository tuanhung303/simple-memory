//! `re.IGNORECASE` patterns spell each cased letter as the class of chars Python's sre takes
//! for it (`py::str::ci_literal`, `ci_class`), since the regex crate folds case by other rules
//! (`(?i)i` misses U+0130 and U+0131 there).

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use rusqlite::types::ValueRef;

use crate::core::item::{Item, epoch_to_as_of, validate_as_of};
use crate::core::refs::{RefsArg, RepoRoots, extract_refs, merge_refs};
use crate::core::tags::{ALL_TAGS, LEGACY_TAG_MAP, append_tag, map_legacy_tag};
use crate::error::{Base, SmError};
use crate::py::path::PyPath;
use crate::py::str::{
    PyRegex, RE_S, ci_class, ci_literal, ci_set, lower, slice, split_sep, splitlines, strip,
    translate,
};
use crate::py::value::repr_bytes;
use crate::sources::mtime;

/// The members of `[A-Za-z0-9_-]`.
fn name_members() -> impl Iterator<Item = char> {
    ('A'..='Z')
        .chain('a'..='z')
        .chain('0'..='9')
        .chain(['_', '-'])
}

/// `[A-Za-z0-9_-]` under IGNORECASE: sre adds U+0130, U+0131, U+017F and U+212A.
static CI_NAME: LazyLock<Vec<char>> = LazyLock::new(|| ci_set(name_members()));

fn ci_name_char(c: char) -> bool {
    CI_NAME.binary_search(&c).is_ok()
}

// ---------------------------------------------------------------- patterns

static HEADER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&translate(r"^\s*\[([^\]]*\|[^\]]*)\]\s*")).expect("header pattern")
});
static BRACKET_TITLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&translate(r"^\s*\[([^\[\]\n]{2,100})\]\s*")).expect("bracket title pattern")
});
static KW_TAG_RE: LazyLock<PyRegex> = LazyLock::new(|| {
    PyRegex::new(&format!(
        "\\b{}({}+)",
        ci_literal("tag:"),
        ci_class(name_members())
    ))
    .expect("keyword tag pattern")
});
static AS_OF_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!("^{}{}+", ci_literal("as_of"), *RE_S)).expect("as_of prefix pattern")
});
static BY_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!("^{}{}+", ci_literal("by"), *RE_S)).expect("by prefix pattern")
});
static EVIDENCE_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!("^{}{}*", ci_literal("evidence:"), *RE_S)).expect("evidence prefix pattern")
});
static TAG_SPLIT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&translate(r"[\s,]+")).expect("tag split pattern"));

static KNOWN_TAG_NAMES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    let mut s: HashSet<&str> = ALL_TAGS.iter().copied().collect();
    s.extend(LEGACY_TAG_MAP.iter().map(|(k, _)| *k));
    s.extend(["user-stated", "observed", "heard"]);
    s
});
pub const PATH_ONLY_REPOS: [&str; 4] = ["design", "tmp", "demo", "apx"];

// ---------------------------------------------------------------- functions

/// `readonly_sqlite_uri(db_path)`.
pub fn readonly_sqlite_uri(db_path: &str) -> Result<String, SmError> {
    let resolved = PyPath::new(db_path).expanduser()?.resolve()?;
    let wal = PyPath::new(&format!("{resolved}-wal")).to_os();
    let has_nonempty_wal = std::fs::metadata(&wal).is_ok_and(|m| m.is_file() && m.len() > 0);
    if has_nonempty_wal {
        return Ok(format!("file:{resolved}?mode=ro"));
    }
    Ok(format!("file:{resolved}?mode=ro&immutable=1"))
}

/// The `re.search` start of `(?<![A-Za-z0-9_-])<name>(?![A-Za-z0-9_-])` under IGNORECASE, as
/// a byte offset (only compared with other offsets into the same text).
fn word_search(re: &Regex, text: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(m) = re.find_at(text, from) {
        let before_ok = !text[..m.start()]
            .chars()
            .next_back()
            .is_some_and(ci_name_char);
        let after_ok = !text[m.end()..].chars().next().is_some_and(ci_name_char);
        if before_ok && after_ok {
            return Some(m.start());
        }
        from = m.start() + text[m.start()..].chars().next().map_or(1, char::len_utf8);
    }
    None
}

/// The word patterns of `match_repo_in_text`, longest name first, path-only repos left out.
fn repo_patterns(repos: &RepoRoots) -> Vec<(String, Regex)> {
    repos
        .names()
        .into_iter()
        .filter(|r| !PATH_ONLY_REPOS.contains(&lower(r).as_str()))
        .map(|r| {
            let re = Regex::new(&ci_literal(&r)).expect("an escaped name compiles");
            (r, re)
        })
        .collect()
}

fn match_repo_with(text: &str, repos: &RepoRoots, patterns: &[(String, Regex)]) -> String {
    if text.is_empty() {
        return "unassigned".into();
    }
    let from_path = repos.repo_from_path(text);
    if from_path != "unassigned" {
        return from_path;
    }
    let mut best: Option<(usize, &str)> = None;
    for (r, re) in patterns {
        if let Some(pos) = word_search(re, text)
            && best.is_none_or(|(b, _)| pos < b)
        {
            best = Some((pos, r));
        }
    }
    best.map_or_else(|| "unassigned".into(), |(_, r)| r.to_string())
}

/// `match_repo_in_text(text, repos)`.
#[cfg(test)]
pub fn match_repo_in_text(text: &str, repos: &RepoRoots) -> String {
    match_repo_with(text, repos, &repo_patterns(repos))
}

/// `parse_evidence_header(text)` -> (tag, as_of, by, evidence, body).
pub type Header = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
);

pub fn parse_evidence_header(text: Option<&str>) -> Header {
    let text = text.unwrap_or("");
    let Some(m) = HEADER_RE.captures(text) else {
        return (None, None, None, None, strip(text).to_string());
    };
    let parts: Vec<&str> = split_sep(&m[1], "|", Some(3))
        .expect("the separator is not empty")
        .into_iter()
        .map(strip)
        .collect();
    let raw_tag = parts.first().map(|s| s.to_string());
    let raw_as_of = parts
        .get(1)
        .map(|p| strip(&AS_OF_PREFIX_RE.replacen(p, 1, "")).to_string());
    let raw_by = parts
        .get(2)
        .map(|p| strip(&BY_PREFIX_RE.replacen(p, 1, "")).to_string());
    let raw_ev = if parts.len() > 3 {
        strip(&EVIDENCE_PREFIX_RE.replacen(parts[3], 1, "")).to_string()
    } else if parts.len() == 3 {
        raw_by.clone().unwrap_or_default()
    } else {
        String::new()
    };
    let remainder = strip(&text[m.get(0).expect("group 0").end()..]);
    let body = if remainder.is_empty() {
        strip(text)
    } else {
        remainder
    };
    (raw_tag, raw_as_of, raw_by, Some(raw_ev), body.to_string())
}

fn kw_tag_matches(text: &str) -> Vec<String> {
    KW_TAG_RE
        .finditer(text)
        .iter()
        .filter_map(|m| m.group(1))
        .map(String::from)
        .collect()
}

fn extract_keyword_tag(keywords: &str, text: &str) -> Option<String> {
    for part in [keywords, text] {
        for cand in kw_tag_matches(part) {
            let cand = lower(&cand);
            if KNOWN_TAG_NAMES.contains(cand.as_str()) {
                return Some(cand);
            }
        }
    }
    None
}

fn derive_title(explicit_title: &str, body: &str, fallback: &str) -> String {
    if !strip(explicit_title).is_empty() {
        return strip(explicit_title).to_string();
    }
    if let Some(m) = BRACKET_TITLE_RE.captures(body) {
        return strip(&m[1]).to_string();
    }
    let first_line = if strip(body).is_empty() {
        ""
    } else {
        strip(splitlines(body, false)[0])
    };
    if !first_line.is_empty() {
        return slice(first_line, None, Some(120)).to_string();
    }
    fallback.to_string()
}

fn collect_tags(parts: &[Option<&str>]) -> String {
    let mut tokens: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for part in parts.iter().flatten() {
        if part.is_empty() {
            continue;
        }
        let cleaned = KW_TAG_RE.sub(part, "", 0);
        for tok in TAG_SPLIT_RE.split(strip(&cleaned)) {
            if !tok.is_empty() && seen.insert(tok.to_string()) {
                tokens.push(tok.to_string());
            }
        }
    }
    tokens.join(" ")
}

/// `_resolve_legacy_as_of(hdr_as_of, updated_at, period, fallback_mtime)` -> (as_of, used_mtime).
fn resolve_legacy_as_of(
    hdr_as_of: Option<&str>,
    updated_at: &str,
    period: &str,
    fallback_mtime: f64,
) -> Result<(String, bool), SmError> {
    for (cand, assume_utc) in [
        (hdr_as_of, false),
        (Some(updated_at), true),
        (Some(period), false),
    ] {
        let Some(cand) = cand.filter(|c| !strip(c).is_empty()) else {
            continue;
        };
        match validate_as_of(Some(cand), assume_utc, true) {
            Ok(v) => return Ok((v, false)),
            Err(e) if e.is(Base::Value) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok((epoch_to_as_of(fallback_mtime)?, true))
}

/// A cell as Python's sqlite3 returns it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Cell {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Cell {
    /// `str(value)`.
    pub(crate) fn py_str(&self) -> String {
        match self {
            Cell::Null => "None".into(),
            Cell::Int(n) => n.to_string(),
            Cell::Real(x) => crate::py::float::repr(*x),
            Cell::Text(s) => s.clone(),
            Cell::Blob(b) => repr_bytes(b),
        }
    }

    pub(crate) fn truthy(&self) -> bool {
        match self {
            Cell::Null => false,
            Cell::Int(n) => *n != 0,
            Cell::Real(x) => *x != 0.0,
            Cell::Text(s) => !s.is_empty(),
            Cell::Blob(b) => !b.is_empty(),
        }
    }
}

/// A `sqlite3.Row`: `r[name]` is the first column whose name equals `name` ignoring ASCII case,
/// while `name in r.keys()` is exact.
pub(crate) struct Row {
    pub(crate) cols: Vec<(String, Cell)>,
}

impl Row {
    fn has(&self, name: &str) -> bool {
        self.cols.iter().any(|(k, _)| k == name)
    }

    pub(crate) fn get(&self, name: &str) -> &Cell {
        &self
            .cols
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .expect("the key is a column")
            .1
    }

    /// `str(r[name] or "") if name in row_keys else ""`.
    fn text(&self, name: &str) -> String {
        if !self.has(name) {
            return String::new();
        }
        let v = self.get(name);
        if v.truthy() {
            v.py_str()
        } else {
            String::new()
        }
    }
}

/// `sqlite3.connect(uri, uri=True)`: Python opens read-write-create and lets the URI's
/// `mode=ro` make it read-only.
pub(crate) fn open_uri(uri: &str) -> Result<rusqlite::Connection, SmError> {
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
        | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
        | rusqlite::OpenFlags::SQLITE_OPEN_URI
        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    rusqlite::Connection::open_with_flags(uri, flags)
        .map_err(|e| SmError::sqlite_open(&e, std::path::Path::new(uri)))
}

pub(crate) fn fetch_rows(conn: &rusqlite::Connection, sql: &str) -> Result<Vec<Row>, SmError> {
    let mut stmt = conn.prepare(sql).map_err(|e| SmError::sqlite(&e))?;
    let names: Vec<String> = stmt.column_names().map(String::from).collect();
    let mut rows = stmt.query([]).map_err(|e| SmError::sqlite(&e))?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().map_err(|e| SmError::sqlite(&e))? {
        let mut cols = Vec::with_capacity(names.len());
        for (i, name) in names.iter().enumerate() {
            let cell = match r.get_ref(i).map_err(|e| SmError::sqlite(&e))? {
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
            };
            cols.push((name.clone(), cell));
        }
        out.push(Row { cols });
    }
    Ok(out)
}

/// `load_legacy(db_path, repos=None)`.
pub fn load_legacy(db_path: &str, repos: Option<&RepoRoots>) -> Result<Vec<Item>, SmError> {
    let resolved = PyPath::new(db_path).expanduser()?.resolve()?;
    let uri = readonly_sqlite_uri(resolved.as_str())?;
    let default = RepoRoots::default();
    let known_repos = repos.unwrap_or(&default);
    let db_mtime = if resolved.exists() {
        mtime(&resolved)?
    } else {
        0.0
    };
    let patterns = repo_patterns(known_repos);

    let table_specs = [
        ("memories", "fact", "fact"),
        ("learnings", "insight", "learning"),
        ("episodes", "narrative", "episode"),
    ];
    let conn = open_uri(&uri)?;
    let existing: HashSet<String> =
        fetch_rows(&conn, "SELECT name FROM sqlite_master WHERE type='table'")?
            .into_iter()
            .map(|r| r.cols[0].1.py_str())
            .collect();
    let mut items = Vec::new();
    for (table, text_col, kind) in table_specs {
        if !existing.contains(table) {
            continue;
        }
        for r in fetch_rows(&conn, &format!("SELECT rowid AS _rowid, * FROM {table}"))? {
            let raw_id = if r.has("id")
                && r.get("id") != &Cell::Null
                && !strip(&r.get("id").py_str()).is_empty()
            {
                r.get("id").py_str()
            } else {
                r.get("_rowid").py_str()
            };
            let item_id = format!("legacy:{table}:{raw_id}");
            let raw_text = r.text(text_col);
            let keywords = r.text("keywords");
            let category = r.text("category");
            let topic = r.text("topic");
            let old_status = r.text("status");
            let context = r.text("context");
            let entities = r.text("entities");
            let explicit_title = r.text("title");
            let updated_at = r.text("updated_at");
            let period = r.text("period");
            let stance = strip(&r.text("stance")).to_string();

            let (mut hdr_tag, hdr_as_of, hdr_by, hdr_ev, mut body) =
                parse_evidence_header(Some(&raw_text));
            if hdr_tag.is_none() {
                hdr_tag = extract_keyword_tag(&keywords, &raw_text);
            }
            let (tag, extra_unknown_tag) = map_legacy_tag(hdr_tag.as_deref());
            let mut evidence = hdr_ev.unwrap_or_default();
            if (tag == "executed" || tag == "verified") && evidence.is_empty() {
                evidence = format!("legacy:{table}:{raw_id}");
            }
            let (as_of, used_mtime) =
                resolve_legacy_as_of(hdr_as_of.as_deref(), &updated_at, &period, db_mtime)?;

            let source = match hdr_by.as_deref() {
                Some(by) if !by.is_empty() => format!("legacy:{table}:{by}"),
                _ => format!("legacy:{table}"),
            };
            let title = derive_title(&explicit_title, &body, &raw_id);
            if kind == "episode" && !stance.is_empty() {
                body = if body.is_empty() {
                    format!("State: {stance}")
                } else {
                    format!("{body}\nState: {stance}")
                };
            }
            let mut tags_str = collect_tags(&[
                Some(&category),
                Some(&topic),
                Some(&old_status),
                extra_unknown_tag.as_deref(),
                Some(&keywords),
            ]);
            if used_mtime {
                tags_str = append_tag(Some(&tags_str), "as_of:mtime");
            }
            let search_blob = [
                raw_id.as_str(),
                &explicit_title,
                &context,
                &entities,
                &raw_text,
                &stance,
                &keywords,
            ]
            .into_iter()
            .filter(|x| !x.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
            let repo = match_repo_with(&search_blob, known_repos, &patterns);
            let refs = merge_refs(&[Some(RefsArg::List(extract_refs(Some(&format!(
                "{title}\n{body}\n{evidence}"
            )))))]);

            items.push(
                Item {
                    refs,
                    tags: tags_str,
                    source,
                    evidence,
                    as_of,
                    ..Item::bare(&item_id, &repo, kind, &title, &body, &tag)
                }
                .post_init(),
            );
        }
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots(names: &[&str]) -> RepoRoots {
        let mut v: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        v.sort_by(|a, b| {
            b.chars()
                .count()
                .cmp(&a.chars().count())
                .then_with(|| a.cmp(b))
        });
        RepoRoots {
            roots: vec![("/Users/devuser/Work/Repos".into(), "Work/Repos".into(), v)],
        }
    }

    fn db(dir: &tempfile::TempDir, name: &str, sql: &str) -> String {
        let p = dir.path().join(name);
        rusqlite::Connection::open(&p)
            .unwrap()
            .execute_batch(sql)
            .unwrap();
        p.to_str().unwrap().to_string()
    }

    const SCHEMA: &str = "
        CREATE TABLE memories (id TEXT PRIMARY KEY, category TEXT, fact TEXT, keywords TEXT, updated_at TEXT);
        CREATE TABLE learnings (id TEXT PRIMARY KEY, category TEXT, insight TEXT, context TEXT, keywords TEXT, updated_at TEXT);
        CREATE TABLE episodes (id TEXT PRIMARY KEY, topic TEXT, title TEXT, period TEXT, status TEXT, narrative TEXT,
            entities TEXT, stance TEXT, keywords TEXT, updated_at TEXT);";

    #[test]
    fn headers_keywords_and_repos() {
        let d = tempfile::tempdir_in(crate::sources::TEST_TMP).unwrap();
        let p = db(&d, "legacy_synth.db", &format!("{SCHEMA}
            INSERT INTO memories VALUES ('fact.1', 'arch', '[inferred | 2026-10-10T08:00+00:00 | by opus | evidence: commit 1234567] Repo anvil uses WAL mode.', 'sqlite wal', '2026-10-10 01:00:00');
            INSERT INTO memories VALUES ('fact.2', 'ops', '[user-stated | as_of 2026-10-11T09:00+00:00 | by sonnet | evidence: chat] Keep simple-memory local.', 'local', '2026-10-11 02:00:00');
            INSERT INTO learnings VALUES ('learn.1', 'dev', '[speculated | 2026-10-12T10:00+00:00 | by haiku | evidence: note] Check R5 timeout.', 'Apply when testing', 'timeout', '2026-10-12 03:00:00');
            INSERT INTO learnings VALUES ('learn.2', 'dev', 'Plain untagged insight with keyword tag in keywords column.', '', 'retry tag:not-a-tag tag:decided', '2026-10-13 04:00:00');
            INSERT INTO episodes VALUES ('ep.1', 'release', 'Initial rollout', '2026-09-01', 'historic', 'Untagged episode with no repo match anywhere.', '', '', 'rollout', '2026-09-01 00:00:00');"));
        let items = load_legacy(&p, Some(&roots(&["simple-memory", "anvil", "design"]))).unwrap();
        let by: std::collections::HashMap<&str, &Item> =
            items.iter().map(|i| (i.id.as_str(), i)).collect();
        assert_eq!(items.len(), 5);
        let f1 = by["legacy:memories:fact.1"];
        assert_eq!(
            (f1.tag.as_str(), f1.repo.as_str(), f1.evidence.as_str()),
            ("derived", "anvil", "commit 1234567")
        );
        assert_eq!(
            f1.as_of,
            crate::core::item::validate_as_of(Some("2026-10-10T08:00+00:00"), false, false)
                .unwrap()
        );
        assert_eq!(f1.source, "legacy:memories:opus");
        assert_eq!(f1.body, "Repo anvil uses WAL mode.");
        assert!(f1.refs.split(' ').any(|r| r == "1234567"), "{}", f1.refs);
        let f2 = by["legacy:memories:fact.2"];
        assert_eq!(
            (f2.tag.as_str(), f2.tags.as_str(), f2.repo.as_str()),
            ("n/a", "ops user-stated local", "simple-memory")
        );
        let x = load_legacy(
            &db(
                &d,
                "exec.db",
                &format!(
                    "{SCHEMA}
            INSERT INTO memories VALUES ('fact.3', '', '[executed | 2026-10-12] Ran it.', '', '');"
                ),
            ),
            None,
        )
        .unwrap();
        assert_eq!(
            (x[0].tag.as_str(), x[0].evidence.as_str()),
            ("executed", "legacy:memories:fact.3")
        );
        let l1 = by["legacy:learnings:learn.1"];
        assert_eq!(l1.tag, "prior");
        assert!(l1.refs.split(' ').any(|r| r == "R5"));
        let l2 = by["legacy:learnings:learn.2"];
        assert_eq!(
            (l2.tag.as_str(), l2.tags.as_str()),
            ("decided", "dev retry")
        );
        let ep = by["legacy:episodes:ep.1"];
        assert_eq!(
            (ep.tag.as_str(), ep.repo.as_str(), ep.tags.as_str()),
            ("n/a", "unassigned", "release historic rollout")
        );
        assert_eq!(
            ep.as_of,
            crate::core::item::validate_as_of(Some("2026-09-01 00:00:00"), true, false).unwrap()
        );
        assert_eq!(ep.title, "Initial rollout");
    }

    #[test]
    fn episode_stance_appended() {
        let d = tempfile::tempdir_in(crate::sources::TEST_TMP).unwrap();
        let p = db(&d, "stance.db", &format!("{SCHEMA}
            INSERT INTO episodes VALUES ('ep.with_stance', 'ops', 'Migration prep', '2026-10-20', 'active', 'Completed schema check.', '', 'ready for dry-run', 'schema', '2026-10-20 01:00:00');
            INSERT INTO episodes VALUES ('ep.empty_stance', 'ops', 'No stance row', '2026-10-20', 'active', 'Only narrative line.', '', '   ', 'schema', '2026-10-20 01:00:00');
            INSERT INTO episodes VALUES ('ep.only', 'ops', '', '', '', '', '', ' s ', '', '');"));
        let items = load_legacy(&p, Some(&roots(&["anvil"]))).unwrap();
        assert_eq!(
            items[0].body,
            "Completed schema check.\nState: ready for dry-run"
        );
        assert_eq!(items[1].body, "Only narrative line.");
        assert_eq!(items[2].body, "State: s");
        assert_eq!(
            items[2].title, "ep.only",
            "an empty body falls back to the raw id"
        );
        assert_eq!(items[2].tags, "ops, as_of:mtime");
    }

    #[test]
    fn repo_matching_and_wal_uri() {
        let r = roots(&["simple-memory", "anvil", "design", "demo", "tmp", "apx"]);
        assert_eq!(
            match_repo_in_text("We updated the design of the tmp demo apx system", &r),
            "unassigned"
        );
        assert_eq!(
            match_repo_in_text("Edited ~/Work/Repos/design/tokens.json", &r),
            "design"
        );
        assert_eq!(
            match_repo_in_text("Using acme-design skill and non-anvil helper", &r),
            "unassigned"
        );
        assert_eq!(
            match_repo_in_text("Using acme-design skill in anvil repo", &r),
            "anvil"
        );
        assert_eq!(
            match_repo_in_text("simple-memory then anvil", &r),
            "simple-memory"
        );
        assert_eq!(
            match_repo_in_text("anvil then simple-memory", &r),
            "anvil",
            "the earliest match wins"
        );
        assert_eq!(
            match_repo_in_text("İanvil Kanvil", &r),
            "unassigned",
            "sre's IGNORECASE name class holds U+0130 and U+212A"
        );
        assert_eq!(match_repo_in_text("ANVİL", &roots(&["anvil"])), "anvil");
        assert_eq!(match_repo_in_text("", &r), "unassigned");
        assert_eq!(
            match_repo_in_text("aaa aa", &roots(&["aa"])),
            "aa",
            "every start is tried, overlaps included"
        );
        assert_eq!(match_repo_in_text("aaa", &roots(&["aa"])), "unassigned");

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

    #[test]
    fn name_class_under_ignorecase() {
        // sre: `[A-Za-z0-9_-]` with re.I adds U+0130, U+0131, U+017F and U+212A (Python 3.15.1:
        // [c for c in map(chr, range(0x110000)) if re.fullmatch('[A-Za-z0-9_-]', c, re.I)]).
        let extra: Vec<char> = CI_NAME.iter().copied().filter(|c| !c.is_ascii()).collect();
        assert_eq!(extra, ['\u{130}', '\u{131}', '\u{17F}', '\u{212A}']);
        assert_eq!(CI_NAME.len(), 64 + 4);
        assert_eq!(kw_tag_matches("x tag:KİT,tag:a"), ["KİT", "a"]);
    }

    #[test]
    fn header_fields() {
        let h = |t: &str| parse_evidence_header(Some(t));
        assert_eq!(
            h("[verified|AS_OF  2026-10-13|BY  haiku] Two fields then by."),
            (
                Some("verified".into()),
                Some("2026-10-13".into()),
                Some("haiku".into()),
                Some("haiku".into()),
                "Two fields then by.".into()
            )
        );
        assert_eq!(
            h("[a|b|c|evidence: d|e] x").3,
            Some("d|e".into()),
            "split stops after 3 pipes"
        );
        assert_eq!(
            h("[a|b] ").4,
            "[a|b]",
            "an empty remainder keeps the whole text"
        );
        assert_eq!(h("[a|b").4, "[a|b");
        assert_eq!(h("[a|b").0, None);
        assert_eq!(
            h("[x|aſ_of 1]").1,
            Some("1".into()),
            "U+017F matches `s` under IGNORECASE"
        );
    }

    #[test]
    fn keyword_tag_title_tags_and_as_of() {
        assert_eq!(
            extract_keyword_tag("tag:nope tag:Verified", ""),
            Some("verified".into())
        );
        assert_eq!(
            extract_keyword_tag("xtag:prior", "a_tag:prior"),
            None,
            "`\\b` needs a word boundary"
        );
        assert_eq!(
            extract_keyword_tag("é tag:observed", ""),
            Some("observed".into())
        );
        assert_eq!(
            extract_keyword_tag("tag:DECİDED", "tag:ſpeculated"),
            None,
            "lower() keeps both out of the known names"
        );
        assert_eq!(
            derive_title("", &format!("[{}]", "x".repeat(101)), "fb"),
            format!("[{}]", "x".repeat(101))
        );
        assert_eq!(derive_title("", "[ab] rest", "fb"), "ab");
        assert_eq!(derive_title("", &"é".repeat(130), "fb"), "é".repeat(120));
        assert_eq!(derive_title("", "\n\nthird", "fb"), "fb");
        assert_eq!(
            collect_tags(&[Some(",lead"), Some("trail,"), None, Some("tag:x a lead")]),
            "lead trail a"
        );
        assert_eq!(
            resolve_legacy_as_of(Some("bad"), "2026-10-11 02:00:00", "", 0.0).unwrap(),
            (
                crate::core::item::validate_as_of(Some("2026-10-11 02:00:00"), true, false)
                    .unwrap(),
                false
            )
        );
        assert_eq!(
            resolve_legacy_as_of(None, " ", "x", 86400.5).unwrap(),
            (crate::core::item::epoch_to_as_of(86400.5).unwrap(), true)
        );
        assert_eq!(
            resolve_legacy_as_of(Some("9999-12-31T23:00:00-05:00"), "2026-10-10", "", 0.0)
                .unwrap_err()
                .python_text(),
            "OverflowError: date value out of range",
            "only a ValueError moves on to the next candidate"
        );
    }

    #[test]
    fn cell_types_upper_id_and_decode_errors() {
        let d = tempfile::tempdir_in(crate::sources::TEST_TMP).unwrap();
        let p = db(
            &d,
            "types.db",
            "CREATE TABLE memories (id, category, fact);
            INSERT INTO memories VALUES (7, 0, 'int id');
            INSERT INTO memories VALUES (2.5, 0.0, x'00ff');
            INSERT INTO memories VALUES (x'626964', 1e300, 12345);
            INSERT INTO memories VALUES ('  ', 'c', 'blank id');",
        );
        let items = load_legacy(&p, None).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "legacy:memories:7",
                "legacy:memories:2.5",
                "legacy:memories:b'bid'",
                "legacy:memories:4"
            ]
        );
        assert_eq!(items[1].body, "b'\\x00\\xff'");
        assert_eq!(
            (
                items[0].tags.as_str(),
                items[1].tags.as_str(),
                items[2].tags.as_str()
            ),
            ("as_of:mtime", "as_of:mtime", "1e+300, as_of:mtime"),
            "0 and 0.0 are falsy"
        );

        let p = db(
            &d,
            "upper.db",
            "CREATE TABLE memories (ID TEXT, fact TEXT); INSERT INTO memories VALUES ('up', 'x');",
        );
        assert_eq!(
            load_legacy(&p, None).unwrap()[0].id,
            "legacy:memories:1",
            "`id in row_keys` is exact"
        );

        let p = db(
            &d,
            "bad.db",
            "CREATE TABLE memories (id TEXT, fact TEXT); INSERT INTO memories VALUES ('b', CAST(x'ff41' AS TEXT));",
        );
        assert_eq!(
            load_legacy(&p, None).unwrap_err().python_text(),
            "OperationalError: Could not decode to UTF-8 column 'fact' with text '\u{fffd}A'"
        );
        let f = d.path().join("text.db");
        std::fs::write(
            &f,
            "plain text, not a database file, long enough to hold a header".repeat(2),
        )
        .unwrap();
        assert_eq!(
            load_legacy(f.to_str().unwrap(), None)
                .unwrap_err()
                .python_text(),
            "DatabaseError: file is not a database"
        );
        let gone = d.path().join("gone.db");
        assert_eq!(
            load_legacy(gone.to_str().unwrap(), None)
                .unwrap_err()
                .python_text(),
            "OperationalError: unable to open database file"
        );
        assert!(!gone.exists(), "mode=ro creates no file");
    }
}
