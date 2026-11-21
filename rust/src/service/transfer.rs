use indexmap::IndexMap;

use crate::config::{aliases, get_db_path, load_config, resolve_repo, roots};
use crate::core::item::Item;
use crate::error::SmError;
use crate::py::io::{Out, repr};
use crate::py::json::{Opts, dumps_value, loads};
use crate::py::path::{Errors, PyPath};
use crate::py::str::strip;
use crate::py::value::PyValue;
use crate::store::connect::{connect, execute};
use crate::store::items::{ITEM_COLUMNS, content_hash_checked};
use crate::store::transfer::{ImportResult, export_items, import_items};

pub const STATUSES: [&str; 2] = ["active", "superseded"];

/// One export line's object: every column, in `_ITEM_COLUMNS` order.
pub fn item_json(it: &Item) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    for c in ITEM_COLUMNS {
        let v = match c {
            "id" => it.id.clone().into(),
            "repo" => it.repo.clone().into(),
            "kind" => it.kind.clone().into(),
            "title" => it.title.clone().into(),
            "body" => it.body.clone().into(),
            "refs" => it.refs.clone().into(),
            "tags" => it.tags.clone().into(),
            "source" => it.source.clone().into(),
            "tag" => it.tag.clone().into(),
            "evidence" => it.evidence.clone().into(),
            "as_of" => it.as_of.clone().into(),
            "status" => it.status.clone().into(),
            "superseded_by" => it
                .superseded_by
                .clone()
                .map_or(serde_json::Value::Null, Into::into),
            _ => it.content_hash.clone().into(),
        };
        m.insert(c.to_string(), v);
    }
    serde_json::Value::Object(m)
}

/// `run_export(out, repo=..., all_repos=..., include_history=...)`: one JSON object per item to
/// `out`, sorted by id; the item count. Scope: `repo`, else every repo with all_repos, else the
/// cwd's repo.
pub fn run_export(
    out: &mut Out,
    repo: &str,
    all_repos: bool,
    include_history: bool,
    home: Option<&str>,
    cwd: Option<&str>,
) -> Result<usize, SmError> {
    if !strip(repo).is_empty() && all_repos {
        return Err(SmError::value("--repo and --all exclude each other"));
    }
    let cfg = load_config(home)?;
    let scope = if !strip(repo).is_empty() {
        Some(strip(repo).to_string())
    } else if all_repos {
        None
    } else {
        Some(resolve_repo(cwd, &aliases(&cfg), &roots(&cfg)?)?)
    };
    let items = {
        let conn = connect(get_db_path(home)?.as_str(), None)?;
        export_items(&conn, scope.as_deref(), include_history)?
    };
    let opts = Opts {
        ensure_ascii: false,
        ..Opts::default()
    };
    for it in &items {
        out.write(&(dumps_value(&item_json(it), &opts) + "\n"))?;
    }
    if items.is_empty() {
        out.eprint(&match &scope {
            Some(repo) => format!("no items in repo {repo} (sm export --all exports every repo)"),
            None => "no items in any repo".to_string(),
        })?;
    }
    Ok(items.len())
}

/// One export line back to an Item, with its checks.
pub fn parse_line(line: &str, lineno: usize) -> Result<Item, SmError> {
    let row = loads(line).map_err(|e| {
        if e.class == "JSONDecodeError" {
            // exc.msg: the message without its `: line L column C (char P)` suffix
            let msg = e
                .msg
                .rfind(": line ")
                .map_or(e.msg.as_str(), |i| &e.msg[..i]);
            SmError::value(format!("line {lineno}: not JSON: {msg}"))
        } else {
            e
        }
    })?;
    let PyValue::Dict(pairs) = row else {
        return Err(SmError::type_(format!("line {lineno}: not a JSON object")));
    };
    // json.loads keeps a repeated key's first position and its last value, as dict does.
    let mut obj: IndexMap<String, PyValue> = IndexMap::new();
    for (k, v) in pairs {
        if let PyValue::Str(k) = k {
            obj.insert(k, v);
        }
    }
    let missing: Vec<String> = ITEM_COLUMNS
        .iter()
        .filter(|c| !obj.contains_key(**c))
        .map(|c| repr(c))
        .collect();
    let mut extra: Vec<&String> = obj
        .keys()
        .filter(|k| !ITEM_COLUMNS.contains(&k.as_str()))
        .collect();
    extra.sort();
    if !missing.is_empty() || !extra.is_empty() {
        let extra: Vec<String> = extra.iter().map(|k| repr(k)).collect();
        return Err(SmError::value(format!(
            "line {lineno}: missing keys [{}], unknown keys [{}]",
            missing.join(", "),
            extra.join(", ")
        )));
    }
    for c in ITEM_COLUMNS {
        let v = &obj[c];
        if !(matches!(v, PyValue::Str(_)) || (c == "superseded_by" && *v == PyValue::None)) {
            return Err(SmError::type_(format!(
                "line {lineno}: {c} must be a string"
            )));
        }
    }
    let s = |c: &str| match &obj[c] {
        PyValue::Str(s) => s.clone(),
        _ => String::new(),
    };
    let superseded_by = match &obj["superseded_by"] {
        PyValue::Str(s) => Some(s.clone()),
        _ => None,
    };
    if strip(&s("id")).is_empty() {
        return Err(SmError::value(format!("line {lineno}: id is empty")));
    }
    let status = s("status");
    if !STATUSES.contains(&status.as_str()) {
        return Err(SmError::value(format!(
            "line {lineno}: status {} is not active or superseded",
            repr(&status)
        )));
    }
    if let (true, Some(sb)) = (status == "active", &superseded_by) {
        return Err(SmError::value(format!(
            "line {lineno}: status active has superseded_by {}",
            repr(sb)
        )));
    }
    if s("content_hash") != content_hash_checked(&s("body"))? {
        return Err(SmError::value(format!(
            "line {lineno}: content_hash does not match the body"
        )));
    }
    Ok(Item {
        refs: s("refs"),
        tags: s("tags"),
        source: s("source"),
        evidence: s("evidence"),
        as_of: s("as_of"),
        status,
        superseded_by,
        content_hash: s("content_hash"),
        ..Item::bare(
            &s("id"),
            &s("repo"),
            &s("kind"),
            &s("title"),
            &s("body"),
            &s("tag"),
        )
    }
    .post_init())
}

/// `run_import_jsonl(path, dry_run=..., home=...)`: the import counts plus rows and dry_run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonlResult {
    pub res: ImportResult,
    pub rows: usize,
    pub dry_run: bool,
}

pub fn run_import_jsonl(
    path: &str,
    dry_run: bool,
    home: Option<&str>,
) -> Result<JsonlResult, SmError> {
    let text = PyPath::new(path).expanduser()?.read_text(Errors::Strict)?;
    // split on newline only: str.splitlines also splits on U+2028, which json.dumps leaves in a string
    let mut items = Vec::new();
    for (n, line) in text.split('\n').enumerate() {
        if !strip(line).is_empty() {
            items.push(parse_line(line, n + 1)?);
        }
    }
    let conn = connect(get_db_path(home)?.as_str(), None)?;
    if dry_run {
        // import_items then joins this transaction and does not commit; closing rolls it back
        execute(&conn, "BEGIN IMMEDIATE").map_err(|e| SmError::sqlite(&e))?;
    }
    let res = import_items(&conn, &items)?;
    drop(conn);
    Ok(JsonlResult {
        res,
        rows: items.len(),
        dry_run,
    })
}

#[cfg(test)]
mod tests {
    // Each home in a tempdir.
    use super::*;
    use crate::store::items::{get_item, put_item, retire_item, supersede_item};
    use crate::store::transfer::StatusDiffers;
    use serde_json::{Value, json};

    const FAKE_GH: &str = "ghp_a1B2c3D4e5a1B2c3D4e5a1B2c3D4e5a1B2c3D4e5";

    fn sha(t: &str) -> String {
        content_hash_checked(t).unwrap()
    }

    fn db(home: &str) -> rusqlite::Connection {
        connect(get_db_path(Some(home)).unwrap().as_str(), None).unwrap()
    }

    fn put(home: &str, id: &str, body: &str, repo: &str, f: impl FnOnce(&mut Item)) {
        let mut it = Item {
            as_of: "2026-10-01T09:00:00+00:00".into(),
            ..Item::bare(id, repo, "fact", &format!("T {id}"), body, "assumed")
        };
        f(&mut it);
        put_item(&db(home), &it.post_init(), None).unwrap();
    }

    fn export(home: &str, repo: &str, all: bool, hist: bool, cwd: Option<&str>) -> (usize, String) {
        let mut out = Out::capture();
        let n = run_export(&mut out, repo, all, hist, Some(home), cwd).unwrap();
        out.flush().unwrap();
        (n, out.captured().0)
    }

    fn ids(home: &str, repo: &str, all: bool, hist: bool) -> Vec<String> {
        let text = export(home, repo, all, hist, None).1;
        text.lines()
            .map(|l| {
                serde_json::from_str::<Value>(l).unwrap()["id"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    fn row(id: &str, body: &str, kw: Value) -> Value {
        let mut r = json!({
            "id": id, "repo": "a", "kind": "fact", "title": format!("T {id}"), "body": body,
            "refs": "", "tags": "", "source": "", "tag": "assumed", "evidence": "",
            "as_of": "2026-10-01T09:00:00+00:00", "status": "active", "superseded_by": null,
            "content_hash": sha(body),
        });
        for (k, v) in kw.as_object().unwrap() {
            r[k] = v.clone();
        }
        r
    }

    fn write(dir: &std::path::Path, rows: &[Value]) -> String {
        let p = dir.join("in.jsonl");
        std::fs::write(
            &p,
            rows.iter()
                .map(|r| r.to_string() + "\n")
                .collect::<String>(),
        )
        .unwrap();
        p.to_str().unwrap().to_string()
    }

    fn import(path: &str, home: &str) -> JsonlResult {
        run_import_jsonl(path, false, Some(home)).unwrap()
    }

    fn status_of(home: &str, id: &str) -> (String, Option<String>) {
        let it = get_item(&db(home), id).unwrap().unwrap();
        (it.status, it.superseded_by)
    }

    fn sd(id: &str, store: &str, file: &str, replacement: Option<&str>) -> StatusDiffers {
        StatusDiffers {
            id: id.into(),
            store: store.into(),
            file: file.into(),
            replacement: replacement.map(String::from),
        }
    }

    fn tmp() -> (tempfile::TempDir, String) {
        let d = tempfile::tempdir().unwrap();
        let h = d.path().join("h").to_str().unwrap().to_string();
        (d, h)
    }

    #[test]
    fn export_every_column_sorted_by_id_and_history_flag() {
        let (_d, h) = tmp();
        put(&h, "m_c", "gamma", "a", |_| {});
        put(&h, "m_a", "alpha Đorđe café", "a", |i| {
            i.refs = "R5".into();
            i.tags = "x, y".into();
            i.source = "task:t1".into();
            i.evidence = "cmd".into();
        });
        put(&h, "m_b", "beta", "b", |_| {});
        supersede_item(&db(&h), "m_c", "m_a").unwrap();
        let text = export(&h, "", true, true, None).1;
        let rows: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            rows.iter()
                .map(|r| r["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["m_a", "m_b", "m_c"]
        );
        assert_eq!(
            rows[0],
            json!({
                "id": "m_a", "repo": "a", "kind": "fact", "title": "T m_a", "body": "alpha Đorđe café",
                "refs": "R5", "tags": "x, y", "source": "task:t1", "tag": "assumed", "evidence": "cmd",
                "as_of": "2026-10-01T09:00:00+00:00", "status": "active", "superseded_by": null,
                "content_hash": sha("alpha Đorđe café"),
            })
        );
        // the column order of the line is ITEM_COLUMNS minus rowid, as Python writes the dict
        assert!(text.starts_with("{\"id\": \"m_a\", \"repo\": \"a\", \"kind\": \"fact\", \"title\": \"T m_a\", \"body\": \"alpha Đorđe café\", \"refs\""));
        assert_eq!(
            (
                rows[2]["status"].as_str(),
                rows[2]["superseded_by"].as_str()
            ),
            (Some("superseded"), Some("m_a"))
        );
        assert_eq!(ids(&h, "", true, false), ["m_a", "m_b"]);
        assert_eq!(ids(&h, "a", false, true), ["m_a", "m_c"]);
    }

    #[test]
    fn export_writes_unicode_as_is_and_an_empty_store_as_nothing() {
        let (d, h) = tmp();
        let empty = d.path().join("empty").to_str().unwrap().to_string();
        assert_eq!(export(&empty, "", true, false, None), (0, String::new()));
        put(&h, "m_u", "café x", "a", |_| {});
        let text = export(&h, "", true, false, None).1;
        assert_eq!(text.matches('\n').count(), 1);
        assert!(text.contains("café"));
    }

    #[test]
    fn export_default_scope_is_the_cwd_repo() {
        let (d, h) = tmp();
        let repo = d.path().join("gh/alpha");
        std::fs::create_dir_all(&repo).unwrap();
        let st = std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&repo)
            .status()
            .unwrap();
        assert!(st.success());
        put(&h, "m_in", "in alpha", "alpha", |_| {});
        put(&h, "m_out", "in beta", "beta", |_| {});
        let text = export(&h, "", false, false, repo.to_str()).1;
        assert_eq!(text.lines().count(), 1);
        assert!(text.starts_with("{\"id\": \"m_in\""));
    }

    #[test]
    fn export_refuses_repo_with_all() {
        let (_d, h) = tmp();
        let mut out = Out::capture();
        let e = run_export(&mut out, "a", true, false, Some(&h), None).unwrap_err();
        assert_eq!(
            e.python_text(),
            "ValueError: --repo and --all exclude each other"
        );
        // a blank --repo is no repo
        assert!(run_export(&mut out, "  ", true, false, Some(&h), None).is_ok());
    }

    #[test]
    fn import_new_unchanged_and_conflict_ids() {
        let (d, h) = tmp();
        put(&h, "m_same", "same body", "a", |_| {});
        put(&h, "m_diff", "stored body", "a", |_| {});
        put(&h, "m_holder", "held body", "a", |_| {});
        let path = write(
            d.path(),
            &[
                row(
                    "m_new",
                    "new body",
                    json!({"refs": "R7", "source": "task:t9"}),
                ),
                row("m_same", "same body", json!({"title": "another title"})),
                row("m_diff", "file body", json!({})),
                row("m_dup", "held body", json!({})),
            ],
        );
        let r = import(&path, &h);
        assert_eq!(
            (r.rows, r.res.added, r.res.unchanged, r.res.conflicts),
            (4, 1, 1, vec!["m_diff".to_string(), "m_dup".into()])
        );
        let conn = db(&h);
        let new = get_item(&conn, "m_new").unwrap().unwrap();
        assert_eq!(
            (
                new.body.as_str(),
                new.refs.as_str(),
                new.source.as_str(),
                new.as_of.as_str()
            ),
            ("new body", "R7", "task:t9", "2026-10-01T09:00:00+00:00")
        );
        assert_eq!(
            get_item(&conn, "m_same").unwrap().unwrap().title,
            "T m_same"
        );
        assert_eq!(
            get_item(&conn, "m_diff").unwrap().unwrap().body,
            "stored body"
        );
        assert_eq!(get_item(&conn, "m_dup").unwrap(), None);
        drop(conn);
        let again = import(&path, &h);
        assert_eq!(
            (again.res.added, again.res.unchanged, again.res.conflicts),
            (0, 2, vec!["m_diff".to_string(), "m_dup".into()])
        );
    }

    #[test]
    fn import_restores_supersede_links_after_inserts() {
        let (d, h) = tmp();
        put(&h, "m_kept", "kept", "a", |_| {});
        let sup = |to: &str| json!({"status": "superseded", "superseded_by": to});
        let path = write(
            d.path(),
            &[
                row("m_a", "old", sup("m_z")),
                row("m_b", "older", sup("m_kept")),
                row("m_c", "gone", sup("m_nowhere")),
                row("m_k2", "kept", sup("m_kept")),
                row("m_z", "old", json!({})),
            ],
        );
        let r = import(&path, &h);
        assert_eq!(
            (r.res.added, r.res.links_restored, r.res.links_missing),
            (5, 3, vec!["m_c -> m_nowhere".to_string()])
        );
        assert_eq!(status_of(&h, "m_a").1.as_deref(), Some("m_z"));
        assert_eq!(status_of(&h, "m_b").1.as_deref(), Some("m_kept"));
        assert_eq!(status_of(&h, "m_c"), ("superseded".into(), None));
    }

    #[test]
    fn import_redacts_as_the_put_path() {
        let (d, h) = tmp();
        let body = format!("token {FAKE_GH} here");
        let path = write(
            d.path(),
            &[row(
                "m_s",
                &body,
                json!({"evidence": format!("curl -H {FAKE_GH}")}),
            )],
        );
        let r = import(&path, &h);
        assert_eq!((r.res.added, r.res.redacted), (1, vec!["m_s".to_string()]));
        let it = get_item(&db(&h), "m_s").unwrap().unwrap();
        assert_eq!(it.body, "token [REDACTED:github_token] here");
        assert_eq!(it.evidence, "curl -H [REDACTED:github_token]");
        assert_eq!(it.content_hash, sha("token [REDACTED:github_token] here"));
        let again = import(&path, &h);
        assert_eq!((again.res.unchanged, again.res.conflicts), (1, vec![]));
    }

    #[test]
    fn import_dry_run_counts_and_writes_nothing() {
        let (d, h) = tmp();
        put(&h, "m_same", "same body", "a", |_| {});
        let path = write(
            d.path(),
            &[
                row("m_new", "new body", json!({})),
                row("m_same", "same body", json!({})),
            ],
        );
        let r = run_import_jsonl(&path, true, Some(&h)).unwrap();
        assert_eq!((r.dry_run, r.res.added, r.res.unchanged), (true, 1, 1));
        assert_eq!(get_item(&db(&h), "m_new").unwrap(), None);
    }

    #[test]
    fn import_refuses_a_bad_line_and_writes_nothing() {
        let x = || row("m_x", "x", json!({}));
        let drop_tags = {
            let mut r = x();
            r.as_object_mut().unwrap().remove("tags");
            r.to_string()
        };
        let extra = {
            let mut r = x();
            r["rowid"] = json!("1");
            r.to_string()
        };
        let with = |kw: Value| row("m_x", "x", kw).to_string();
        let cases: Vec<(String, &str)> = vec![
            ("{not json".into(), "line 2: not JSON"),
            ("[1, 2]".into(), "line 2: not a JSON object"),
            (drop_tags, "missing keys ['tags']"),
            (extra, "unknown keys ['rowid']"),
            (with(json!({"refs": null})), "line 2: refs must be a string"),
            (
                with(json!({"status": "deleted"})),
                "status 'deleted' is not active or superseded",
            ),
            (
                with(json!({"content_hash": sha("y")})),
                "line 2: content_hash does not match the body",
            ),
            (row("", "x", json!({})).to_string(), "line 2: id is empty"),
            (row("  ", "x", json!({})).to_string(), "line 2: id is empty"),
            (
                with(json!({"superseded_by": "m_y"})),
                "line 2: status active has superseded_by 'm_y'",
            ),
            (with(json!({"kind": "note"})), "m_x: Invalid kind 'note'"),
            (with(json!({"tag": "n/a"})), "m_x: Invalid tag 'n/a'"),
            (
                with(json!({"tag": "verified"})),
                "m_x: Tag 'verified' needs non-empty evidence",
            ),
            (
                with(json!({"as_of": "2026-10-01 later"})),
                "m_x: Invalid ISO 8601 timestamp",
            ),
        ];
        for (line, message) in cases {
            let (d, h) = tmp();
            let p = d.path().join("in.jsonl");
            std::fs::write(&p, format!("{}\n{line}\n", row("m_ok", "ok", json!({})))).unwrap();
            let e = run_import_jsonl(p.to_str().unwrap(), false, Some(&h)).unwrap_err();
            assert!(
                ["ValueError", "TypeError"].contains(&e.class.as_ref()),
                "{line}: {}",
                e.python_text()
            );
            assert!(e.msg.contains(message), "{line}: {}", e.msg);
            assert_eq!(get_item(&db(&h), "m_ok").unwrap(), None, "{line}");
        }
    }

    #[test]
    fn import_keeps_na_tag_on_legacy_ids_and_adds_an_id_twice_once() {
        let (d, h) = tmp();
        let path = write(
            d.path(),
            &[row("legacy:old1", "legacy body", json!({"tag": "n/a"}))],
        );
        assert_eq!(import(&path, &h).res.added, 1);
        let (d, h) = tmp();
        let path = write(
            d.path(),
            &[
                row("m_d", "one", json!({})),
                row("m_d", "one", json!({})),
                row("m_d", "two", json!({})),
            ],
        );
        let r = import(&path, &h);
        assert_eq!(
            (r.res.added, r.res.unchanged, r.res.conflicts),
            (1, 1, vec!["m_d".to_string()])
        );
    }

    #[test]
    fn round_trip_into_an_empty_home_gives_the_same_lines() {
        let (d, src) = tmp();
        put(&src, "m_c", "gamma line", "a", |_| {});
        put(&src, "m_a", "alpha", "a", |i| {
            i.refs = "R5".into();
            i.source = "task:t1".into();
        });
        put(&src, "m_b", "beta", "b", |_| {});
        put(&src, "m_d", "delta", "a", |_| {});
        supersede_item(&db(&src), "m_c", "m_a").unwrap();
        supersede_item(&db(&src), "m_d", "m_b").unwrap();
        let first = export(&src, "", true, true, None).1;
        let p = d.path().join("x.jsonl");
        std::fs::write(&p, &first).unwrap();
        let dst = d.path().join("dst").to_str().unwrap().to_string();
        let r = import(p.to_str().unwrap(), &dst);
        assert_eq!(
            (r.res.added, r.res.links_restored, r.res.conflicts),
            (4, 2, vec![])
        );
        assert_eq!(export(&dst, "", true, true, None).1, first);
    }

    #[test]
    fn import_reports_status_differences_and_keeps_the_store_status() {
        let sup = |to: &str| json!({"status": "superseded", "superseded_by": to});
        // the file retires X, the store keeps it active
        let (d, h) = tmp();
        put(&h, "m_x", "old way", "a", |_| {});
        let r = import(
            &write(
                d.path(),
                &[
                    row("m_x", "old way", sup("m_y")),
                    row("m_y", "new way", json!({})),
                ],
            ),
            &h,
        );
        assert_eq!(
            (r.res.added, r.res.unchanged, r.res.conflicts.len()),
            (1, 0, 0)
        );
        assert_eq!(
            r.res.status_differs,
            vec![sd("m_x", "active", "superseded", Some("m_y"))]
        );
        assert_eq!(status_of(&h, "m_x"), ("active".into(), None));
        // the file keeps X active, the store retired it
        let (d, h) = tmp();
        put(&h, "m_x", "old way", "a", |_| {});
        put(&h, "m_y", "new way", "a", |_| {});
        supersede_item(&db(&h), "m_x", "m_y").unwrap();
        let r = import(
            &write(
                d.path(),
                &[
                    row("m_x", "old way", json!({})),
                    row("m_y", "new way", json!({})),
                ],
            ),
            &h,
        );
        assert_eq!((r.res.added, r.res.unchanged), (0, 1));
        assert_eq!(
            r.res.status_differs,
            vec![sd("m_x", "superseded", "active", None)]
        );
        assert_eq!(
            status_of(&h, "m_x"),
            ("superseded".into(), Some("m_y".into()))
        );
        // no replacement the store lacks; a matching link counts unchanged
        let (d, h) = tmp();
        put(&h, "m_x", "one", "a", |_| {});
        put(&h, "m_p", "two", "a", |_| {});
        put(&h, "m_q", "three", "a", |_| {});
        supersede_item(&db(&h), "m_q", "m_p").unwrap();
        let r = import(
            &write(
                d.path(),
                &[
                    row("m_x", "one", sup("m_nowhere")),
                    row("m_p", "two", json!({})),
                    row("m_q", "three", sup("m_p")),
                ],
            ),
            &h,
        );
        assert_eq!(r.res.unchanged, 2);
        assert_eq!(
            r.res.status_differs,
            vec![sd("m_x", "active", "superseded", None)]
        );
        // a retired item the file points at another replacement
        let (d, h) = tmp();
        put(&h, "m_x", "old way", "a", |_| {});
        put(&h, "m_p", "first", "a", |_| {});
        put(&h, "m_q", "second", "a", |_| {});
        supersede_item(&db(&h), "m_x", "m_p").unwrap();
        let r = import(
            &write(
                d.path(),
                &[
                    row("m_x", "old way", sup("m_q")),
                    row("m_p", "first", json!({})),
                    row("m_q", "second", json!({})),
                ],
            ),
            &h,
        );
        assert_eq!(
            (r.res.unchanged, r.res.status_differs),
            (2, vec![sd("m_x", "superseded", "superseded", Some("m_q"))])
        );
        assert_eq!(
            status_of(&h, "m_x"),
            ("superseded".into(), Some("m_p".into()))
        );
    }

    #[test]
    fn a_retired_item_with_no_replacement_exports_and_imports_again() {
        let (d, src) = tmp();
        put(&src, "m_gone", "file removed", "a", |_| {});
        assert_eq!(retire_item(&db(&src), "m_gone").unwrap(), 1);
        let p = d.path().join("x.jsonl");
        std::fs::write(&p, export(&src, "", true, true, None).1).unwrap();
        let dst = d.path().join("dst").to_str().unwrap().to_string();
        assert_eq!(import(p.to_str().unwrap(), &dst).res.added, 1);
        assert_eq!(status_of(&dst, "m_gone"), ("superseded".into(), None));
    }
}
