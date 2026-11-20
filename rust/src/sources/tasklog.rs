use std::borrow::Cow;

use crate::core::item::{Item, check_stored_as_of, epoch_to_as_of, new_item_id, validate_as_of};
use crate::core::refs::{RefsArg, extract_refs, merge_refs};
use crate::core::tags::append_tag;
use crate::error::{Base, SmError};
use crate::py::path::{Errors, PyPath};
use crate::py::str::{lower, split, strip};
use crate::py::value::PyValue;
use crate::py::yaml::safe_load;
use crate::sources::{
    SourceFile, dict_get as get, file_error, iterate, mtime, sorted_paths, truthy,
};

/// `str(data.get(key) or fallback)`.
fn get_str_or(data: &[(PyValue, PyValue)], key: &str, fallback: &str) -> Result<String, SmError> {
    match get(data, key) {
        Some(v) if truthy(v) => v.try_py_str(),
        _ => Ok(fallback.to_string()),
    }
}

fn format_section(heading: &str, value: Option<&PyValue>) -> Result<String, SmError> {
    let Some(value) = value.filter(|v| truthy(v)) else {
        return Ok(String::new());
    };
    if let PyValue::List(items) = value {
        let mut lines: Vec<String> = Vec::new();
        for x in items.iter().filter(|x| !matches!(x, PyValue::None)) {
            let text = x.try_py_str()?;
            if !strip(&text).is_empty() {
                lines.push(format!("- {text}"));
            }
        }
        if lines.is_empty() {
            return Ok(String::new());
        }
        return Ok(format!("{heading}:\n{}", lines.join("\n")));
    }
    let text = value.try_py_str()?;
    let text = strip(&text);
    if text.is_empty() {
        return Ok(String::new());
    }
    Ok(format!("{heading}:\n{text}"))
}

/// The folder above the first `docs` part of the resolved path (doctor calls it on an
/// unresolved path).
pub fn infer_tasklog_repo(path: &str) -> Result<String, SmError> {
    let parts = PyPath::new(path).resolve()?.parts();
    if let Some(idx) = parts.iter().position(|p| p == "docs")
        && idx > 0
    {
        return Ok(parts[idx - 1].clone());
    }
    Ok("unassigned".into())
}

/// `parse_tasklog_file(path)`.
pub fn parse_tasklog_file(path: &str) -> Result<Item, SmError> {
    let p = PyPath::new(path).expanduser()?.resolve()?;
    let text = p.read_text(Errors::Strict)?;
    let data = match safe_load(&text) {
        Ok(v) => v,
        // the loader reads text already in memory, so each of its errors (a YAML error, an
        // impossible date, a value nested too deep) is a fault of this one file, not of the import
        Err(e) => {
            let msg = split(&e.message).join(" ");
            return Err(SmError::value(format!("Malformed YAML in {p}: {msg}")));
        }
    };
    let PyValue::Dict(data) = data else {
        return Err(SmError::type_(format!(
            "Tasklog file {p} does not contain a YAML mapping"
        )));
    };

    let task_id = strip(&get_str_or(&data, "id", p.stem())?).to_string();
    let title = strip(&get_str_or(&data, "title", &task_id)?).to_string();
    let repo = match strip(&get_str_or(&data, "repo", "")?) {
        "" => infer_tasklog_repo(p.as_str())?,
        r => r.to_string(),
    };
    let mut tags_str = strip(&get_str_or(&data, "state", "")?).to_string();
    let raw_as_of = strip(&get_str_or(&data, "as_of", "")?).to_string();
    let as_of = match validate_as_of(Some(&raw_as_of), false, false) {
        Ok(v) => v,
        Err(e) if e.is(Base::Value) => {
            let v = epoch_to_as_of(mtime(&p)?)?;
            tags_str = append_tag(Some(&tags_str), "as_of:mtime");
            v
        }
        Err(e) => return Err(e),
    };
    check_stored_as_of(&as_of)?;

    let sections = [
        format_section("Results", get(&data, "results"))?,
        format_section("Directions", get(&data, "directions"))?,
        format_section("Open", get(&data, "open"))?,
        format_section("Next", get(&data, "next"))?,
    ];
    let joined = sections
        .iter()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n\n");
    let body = match strip(&joined) {
        "" => title.clone(),
        b => b.to_string(),
    };

    let commits: Vec<String> = match get(&data, "commits").filter(|v| truthy(v)) {
        None => Vec::new(),
        Some(v) => {
            // one scalar, a string or a number (a short commit id of digits loads as an int),
            // is one commit id: the list of one, never one id per character
            let ids = match v {
                PyValue::Str(_) | PyValue::Int(_) | PyValue::Float(_) => vec![Cow::Borrowed(v)],
                _ => iterate(v)?,
            };
            let mut out = Vec::new();
            for c in ids.iter().filter(|c| truthy(c)) {
                let text = c.try_py_str()?;
                if !strip(&text).is_empty() {
                    out.push(strip(&text).to_string());
                }
            }
            out
        }
    };
    let continues = get_str_or(&data, "continues", "")?;
    let superseded_by = match get(&data, "superseded_by") {
        Some(raw) if !matches!(raw, PyValue::None) => {
            let s = raw.try_py_str()?;
            let s = strip(&s);
            if ["", "null", "none"].contains(&lower(s).as_str()) {
                None
            } else {
                Some(s.to_string())
            }
        }
        _ => None,
    };
    if let Some(sup) = &superseded_by {
        let clean = sup.strip_prefix("task:").unwrap_or(sup);
        tags_str = append_tag(Some(&tags_str), &format!("superseded_by:{clean}"));
    }
    let refs = merge_refs(&[
        Some(RefsArg::List(vec![task_id.clone()])),
        Some(RefsArg::List(commits.clone())),
        Some(RefsArg::List(
            if !continues.is_empty() && continues != "null" {
                vec![continues]
            } else {
                Vec::new()
            },
        )),
        Some(RefsArg::List(superseded_by.iter().cloned().collect())),
        Some(RefsArg::List(extract_refs(Some(&format!(
            "{title}\n{body}"
        ))))),
    ]);

    let (tag, evidence) = if commits.is_empty() {
        ("derived", p.to_string())
    } else {
        ("executed", commits.join(", "))
    };

    let id = new_item_id(None);
    Ok(Item {
        refs,
        tags: tags_str,
        source: format!("task:{task_id}"),
        evidence,
        as_of,
        superseded_by,
        ..Item::bare(&id, &repo, "task", &title, &body, tag)
    }
    .post_init())
}

/// `load_tasklogs(path)`: each file with its item.
pub fn load_tasklogs(path: &str) -> Result<Vec<SourceFile>, SmError> {
    let p = PyPath::new(path).expanduser()?.resolve()?;
    let parse = |f: PyPath| {
        let item = parse_tasklog_file(f.as_str()).map_err(|e| file_error(f.as_str(), e))?;
        Ok((f, item))
    };
    if p.is_file() {
        return Ok(vec![parse(p)?]);
    }
    if p.is_dir() {
        return sorted_paths(p.rglob("*.yaml")?)
            .into_iter()
            .map(parse)
            .collect();
    }
    Err(SmError::new(
        Base::Os,
        "FileNotFoundError",
        format!("Tasklog path not found: {p}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &std::path::Path, rel: &str, data: &[u8]) -> String {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, data).unwrap();
        p.to_str().unwrap().to_string()
    }

    // Python 3.15.1 (probe 2026-11-05): int() refuses a 4301-digit decimal while YAML loads, and
    // str() refuses a hex, octal or binary int past 4300 decimal digits, nested ones included
    #[test]
    fn ints_past_4300_digits_raise_python_value_error() {
        let d = tempfile::tempdir_in(crate::sources::TEST_TMP).unwrap();
        let parse = |yaml: String| {
            parse_tasklog_file(&write(d.path(), "r/docs/tasklog/a.yaml", yaml.as_bytes()))
        };
        // the loader's error is malformed YAML for the file
        let a = std::fs::canonicalize(d.path())
            .unwrap()
            .join("r/docs/tasklog/a.yaml");
        let parse_msg = format!(
            "ValueError: Malformed YAML in {}: Exceeds the limit (4300 digits) for integer string conversion: value has 4301 digits; \
             use sys.set_int_max_str_digits() to increase the limit",
            a.display()
        );
        let str_msg = "ValueError: Exceeds the limit (4300 digits) for integer string conversion; use sys.set_int_max_str_digits() to increase the limit";
        assert_eq!(
            parse(format!("id: x\ntitle: {}\n", "9".repeat(4301)))
                .unwrap_err()
                .python_text(),
            parse_msg
        );
        assert_eq!(
            parse(format!("id: x\nnext: [[0b{}]]\n", "1".repeat(14_300)))
                .unwrap_err()
                .python_text(),
            str_msg
        );
        assert_eq!(
            parse(format!("id: 0x{}\n", "f".repeat(3572)))
                .unwrap_err()
                .python_text(),
            str_msg
        );
        assert_eq!(
            parse(format!("id: x\ntitle: {}\n", "9".repeat(4300)))
                .unwrap()
                .title,
            "9".repeat(4300)
        );
    }

    #[test]
    fn tasklog_parser() {
        let d = tempfile::tempdir_in(crate::sources::TEST_TMP).unwrap();
        let tl = "anvil/docs/tasklog/2026-10-25";
        write(d.path(), &format!("{tl}/010203-abcdef12-sample-task-9999.yaml"),
            b"id: 20261025-010203-abcdef12-sample-task-9999\ntitle: Sample task for R5\nstate: done\nrepo: anvil\n\
              as_of: 2026-10-25T10:30:00+00:00\ncommits:\n  - a1b2c3d\nresults:\n  - Shipped R5 fix in src/simple_memory/cli.py\n\
              open:\n  - Follow up on D9\nnext: Run acceptance suite\n");
        write(d.path(), &format!("{tl}/020304-bcdef123-no-commits-8888.yaml"),
            b"id: 20261025-020304-bcdef123-no-commits-8888\ntitle: Investigation without commits\nstate: done\nrepo: anvil\n\
              as_of: not-an-iso-date\nsuperseded_by: 20261025-010203-abcdef12-sample-task-9999\nresults:\n  - Checked logs\n");
        let items: Vec<Item> = load_tasklogs(d.path().join("anvil/docs/tasklog").to_str().unwrap())
            .unwrap()
            .into_iter()
            .map(|(_, it)| it)
            .collect();
        assert_eq!(items.len(), 2);
        let it = &items[0];
        assert_eq!(
            (
                it.repo.as_str(),
                it.kind.as_str(),
                it.tag.as_str(),
                it.evidence.as_str()
            ),
            ("anvil", "task", "executed", "a1b2c3d")
        );
        assert_eq!(it.source, "task:20261025-010203-abcdef12-sample-task-9999");
        assert_eq!(
            it.as_of,
            crate::core::item::validate_as_of(Some("2026-10-25T10:30:00+00:00"), false, false)
                .unwrap()
        );
        assert_eq!(
            it.body,
            "Results:\n- Shipped R5 fix in src/simple_memory/cli.py\n\nOpen:\n- Follow up on D9\n\nNext:\nRun acceptance suite"
        );
        for r in [
            "20261025-010203-abcdef12-sample-task-9999",
            "a1b2c3d",
            "R5",
            "D9",
        ] {
            assert!(it.refs.split(' ').any(|x| x == r), "{r} in {}", it.refs);
        }
        let it2 = &items[1];
        assert_eq!(it2.tag, "derived");
        assert_eq!(
            it2.tags,
            "done, as_of:mtime, superseded_by:20261025-010203-abcdef12-sample-task-9999"
        );
        assert_eq!(
            it2.superseded_by.as_deref(),
            Some("20261025-010203-abcdef12-sample-task-9999")
        );

        let bad = write(d.path(), "malformed.yaml", b"title: [unclosed\n  bad: :\n");
        let e = load_tasklogs(&bad).unwrap_err();
        assert!(e.is(Base::Value));
        assert!(
            e.msg
                .starts_with(&format!("Malformed YAML in {bad}: while parsing a flow")),
            "{}",
            e.msg
        );
        assert!(
            !e.msg.contains('\n'),
            "the YAML message is folded onto one line"
        );
    }

    #[test]
    fn repo_and_fallbacks_from_the_path() {
        let d = tempfile::tempdir_in(crate::sources::TEST_TMP).unwrap();
        let p = write(
            d.path(),
            "proj/docs/tasklog/t-1.yaml",
            b"results: [one, '', null, 0, two]\n",
        );
        let it = parse_tasklog_file(&p).unwrap();
        assert_eq!(
            (it.repo.as_str(), it.title.as_str(), it.source.as_str()),
            ("proj", "t-1", "task:t-1")
        );
        assert_eq!(it.body, "Results:\n- one\n- 0\n- two");
        assert_eq!(it.evidence, p);
        assert_eq!(infer_tasklog_repo("/docs/x").unwrap(), "/");
        assert_eq!(infer_tasklog_repo("/a/b/x").unwrap(), "unassigned");
    }

    #[test]
    fn commits_iterate_as_python_iterates() {
        let d = tempfile::tempdir_in(crate::sources::TEST_TMP).unwrap();
        let it = parse_tasklog_file(&write(
            d.path(),
            "a.yaml",
            b"id: a\ncommits: abc\nsuperseded_by: false\n",
        ))
        .unwrap();
        assert_eq!(
            (it.evidence.as_str(), it.tag.as_str()),
            ("abc", "executed"),
            "a commits value that is one scalar, a string or a number, is one commit id, stored as \
             the one-item list of that value is stored"
        );
        assert_eq!(it.superseded_by.as_deref(), Some("False"));
        assert_eq!(it.tags, "as_of:mtime, superseded_by:False");
        let it = parse_tasklog_file(&write(
            d.path(),
            "b.yaml",
            b"id: b\ncommits: {x1: 1, 0: 2}\nsuperseded_by: 'task:t-9'\n",
        ))
        .unwrap();
        assert_eq!(it.evidence, "x1");
        assert_eq!(it.tags, "as_of:mtime, superseded_by:t-9");
        assert_eq!(it.superseded_by.as_deref(), Some("task:t-9"));
        let it = parse_tasklog_file(&write(
            d.path(),
            "c.yaml",
            b"id: c\nsuperseded_by: ' None '\ncontinues: 'null'\n",
        ))
        .unwrap();
        assert_eq!((it.superseded_by.clone(), it.refs.as_str()), (None, "c"));
        let it = parse_tasklog_file(&write(
            d.path(),
            "z.yaml",
            b"id: z\ncommits: [0, '', ' ab1 ', null, 7]\n",
        ))
        .unwrap();
        assert_eq!(
            it.evidence, "ab1, 7",
            "a falsy commit is skipped before str()"
        );
        let it = parse_tasklog_file(&write(d.path(), "d.yaml", b"id: d\ncommits: 5\n")).unwrap();
        assert_eq!(
            (it.evidence.as_str(), it.tag.as_str(), it.refs.as_str()),
            ("5", "executed", "d 5"),
            "a number is one commit id"
        );
        let it =
            parse_tasklog_file(&write(d.path(), "f.yaml", b"id: f\ncommits: 1.5e+3\n")).unwrap();
        assert_eq!(
            it.evidence, "1500.0",
            "a float is one commit id, as str() gives it"
        );
        let it = parse_tasklog_file(&write(d.path(), "g.yaml", b"id: g\ncommits: 1e10\n")).unwrap();
        assert_eq!(
            it.evidence, "1e10",
            "YAML 1.1 reads 1e10 as a str; a commits value that is one scalar, a string or a \
             number, is one commit id"
        );
        let w = write(d.path(), "w.yaml", b"id: w\ncommits: '  '\n");
        let it = parse_tasklog_file(&w).unwrap();
        assert_eq!(
            (it.evidence.as_str(), it.tag.as_str()),
            (w.as_str(), "derived"),
            "a blank string is no commits"
        );
        let e = parse_tasklog_file(&write(d.path(), "e.yaml", b"id: e\ncommits: 2026-10-25\n"))
            .unwrap_err();
        assert_eq!(
            e.python_text(),
            "TypeError: 'datetime.date' object is not iterable"
        );
    }

    #[test]
    fn errors_keep_their_python_class() {
        let d = tempfile::tempdir_in(crate::sources::TEST_TMP).unwrap();
        let m = write(d.path(), "m.yaml", b"id: m\nas_of: 2026-13-01\n");
        let e = parse_tasklog_file(&m).unwrap_err();
        let m = std::fs::canonicalize(&m).unwrap();
        assert_eq!(
            e.python_text(),
            format!(
                "ValueError: Malformed YAML in {}: month must be in 1..12, not 13",
                m.display()
            ),
            "a datetime ValueError of the loader is malformed YAML for the file"
        );
        let e = parse_tasklog_file(&write(
            d.path(),
            "o.yaml",
            b"id: o\nas_of: '9999-12-31T23:00:00-05:00'\n",
        ))
        .unwrap_err();
        assert_eq!(
            e.python_text(),
            "OverflowError: date value out of range",
            "only a ValueError falls back to the mtime"
        );
        let e = parse_tasklog_file(&write(d.path(), "u.yaml", b"id: u\ntitle: \xff\xfe\n"))
            .unwrap_err();
        assert_eq!(
            e.python_text(),
            "UnicodeDecodeError: 'utf-8' codec can't decode byte 0xff in position 13: invalid start byte"
        );
        for (name, text) in [("l.yaml", "- a\n"), ("n.yaml", ""), ("s.yaml", "text\n")] {
            let p = write(d.path(), name, text.as_bytes());
            let e = parse_tasklog_file(&p).unwrap_err();
            assert_eq!(
                e.python_text(),
                format!("TypeError: Tasklog file {p} does not contain a YAML mapping")
            );
        }
        let gone = d.path().join("gone");
        let gs = gone.to_str().unwrap();
        assert_eq!(
            load_tasklogs(gs).unwrap_err().python_text(),
            format!("FileNotFoundError: Tasklog path not found: {gs}")
        );
    }

    #[test]
    fn crlf_and_datetime_values() {
        let d = tempfile::tempdir_in(crate::sources::TEST_TMP).unwrap();
        let it = parse_tasklog_file(&write(
            d.path(),
            "c.yaml",
            b"id: c\r\nas_of: 2026-10-25 10:30:00\r\nnext: [2026-10-25]\r\n",
        ))
        .unwrap();
        assert_eq!(
            it.as_of,
            crate::core::item::validate_as_of(Some("2026-10-25 10:30:00"), false, false).unwrap(),
            "a naive datetime is read in local time"
        );
        assert_eq!(it.body, "Next:\n- 2026-10-25");
    }

    #[test]
    fn format_section_values() {
        let s = |v: PyValue| format_section("H", Some(&v)).unwrap();
        assert_eq!(s(PyValue::None), "");
        assert_eq!(s(PyValue::Str("  ".into())), "");
        assert_eq!(s(PyValue::Int("0".into())), "");
        assert_eq!(s(PyValue::Float(-0.0)), "");
        assert_eq!(s(PyValue::Bool(true)), "H:\nTrue");
        assert_eq!(
            s(PyValue::List(vec![PyValue::None, PyValue::Str(" ".into())])),
            ""
        );
        assert_eq!(
            s(PyValue::List(vec![
                PyValue::List(vec![PyValue::Str("x".into())]),
                PyValue::Bool(false),
                PyValue::Float(1.5)
            ])),
            "H:\n- ['x']\n- False\n- 1.5"
        );
        assert_eq!(
            s(PyValue::Dict(vec![(
                PyValue::Str("a".into()),
                PyValue::int(1)
            )])),
            "H:\n{'a': 1}"
        );
    }
}
