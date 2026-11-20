//! Claude Code auto-memory files (`<root>/<project-dir>/memory/*.md`) parsed into items.
//! A file: optional frontmatter between the first two `---` lines (nested `metadata:` block with
//! `type`, `tag`, `evidence`, `as_of`, `modified`), then the body. The repo is resolved by the
//! caller.

use std::sync::LazyLock;

use crate::core::item::{Item, epoch_to_as_of, new_item_id, validate_as_of};
use crate::core::refs::item_refs;
use crate::core::tags::VALID_TAGS;
use crate::error::{Base, SmError};
use crate::py::json::loads;
use crate::py::path::{Errors, PyPath};
use crate::py::str::{PyRegex, lower, split, splitlines, strip};
use crate::py::value::PyValue;
use crate::py::yaml::safe_load;
use crate::sources::{TextLines, dict_get, mtime, sorted_paths};

pub const SOURCE_PREFIX: &str = "automemory:";
const FALLBACK_KEYS: [&str; 7] = [
    "name",
    "description",
    "type",
    "tag",
    "evidence",
    "as_of",
    "modified",
];
static FALLBACK_RE: LazyLock<PyRegex> = LazyLock::new(|| {
    PyRegex::new(&format!(r"^\s*({}):\s*(.*?)\s*$", FALLBACK_KEYS.join("|")))
        .expect("fallback pattern")
});
const CWD_SCAN_LINES: usize = 200;

/// The `MemoryFile` dataclass.
#[derive(Clone, Debug, PartialEq)]
pub struct MemoryFile {
    pub path: PyPath,
    pub project_dir: String,
    pub name: String,
    pub title: String,
    pub body: String,
    pub type_: String,
    pub tag: String,
    pub evidence: String,
    pub as_of: String,
    pub yaml_error: bool,
}

impl MemoryFile {
    /// `MemoryFile.source`.
    pub fn source(&self) -> String {
        format!("{SOURCE_PREFIX}{}/{}", self.project_dir, self.path.name())
    }
}

/// `discover(root)`: every memory file except the MEMORY.md index; a missing root is an error,
/// never zero files.
pub fn discover(root: &PyPath) -> Result<Vec<PyPath>, SmError> {
    if !root.is_dir() {
        return Err(SmError::new(
            Base::Os,
            "FileNotFoundError",
            format!("automemory root not found: {root}"),
        ));
    }
    let found = root
        .glob("*/memory/*.md")?
        .into_iter()
        .filter(|p| p.name() != "MEMORY.md" && p.is_file())
        .collect();
    Ok(sorted_paths(found))
}

fn split_frontmatter(text: &str) -> (Option<String>, String) {
    let lines = splitlines(text, true);
    if lines.is_empty() || strip(lines[0]) != "---" {
        return (None, text.to_string());
    }
    for i in 1..lines.len() {
        if strip(lines[i]) == "---" {
            return (Some(lines[1..i].concat()), lines[i + 1..].concat());
        }
    }
    (None, text.to_string())
}

fn unquote(value: &str) -> String {
    let v = strip(value);
    let chars: Vec<char> = v.chars().collect();
    if chars.len() >= 2
        && chars[0] == chars[chars.len() - 1]
        && (chars[0] == '"' || chars[0] == '\'')
    {
        return chars[1..chars.len() - 1].iter().collect();
    }
    v.to_string()
}

type Fields = Vec<(PyValue, PyValue)>;

/// `dict[key] = value` with Python's key equality: an equal key keeps its place.
fn set_key(d: &mut Fields, k: PyValue, v: PyValue) {
    match d.iter_mut().find(|(dk, _)| dk.key_eq(&k)) {
        Some(slot) => slot.1 = v,
        None => d.push((k, v)),
    }
}

/// Frontmatter keys, `metadata:` keys winning; on a YAML error, the known keys
/// read line by line.
fn fields(front: &str) -> Result<(Fields, bool), SmError> {
    let data = match safe_load(front) {
        Ok(v) => v,
        Err(e) if e.is_yaml_error() => {
            let mut found = Fields::new();
            for line in splitlines(front, false) {
                if let Some(m) = FALLBACK_RE.search(line) {
                    // the last line wins, so a nested metadata key beats a top-level one
                    let key = m.group(1).expect("group 1 always takes part").to_string();
                    set_key(
                        &mut found,
                        PyValue::Str(key),
                        PyValue::Str(unquote(m.group(2).unwrap_or(""))),
                    );
                }
            }
            return Ok((found, true));
        }
        Err(e) => return Err(SmError::from_python(e.class, e.message)),
    };
    let PyValue::Dict(data) = data else {
        return Ok((Fields::new(), false));
    };
    let meta = dict_get(&data, "metadata").cloned();
    let metadata = PyValue::Str("metadata".into());
    let mut merged: Fields = data
        .into_iter()
        .filter(|(k, _)| !k.key_eq(&metadata))
        .collect();
    if let Some(PyValue::Dict(meta)) = meta {
        for (k, v) in meta {
            set_key(&mut merged, k, v);
        }
    }
    Ok((merged, false))
}

/// A date, datetime or non-blank str as a local ISO timestamp; None when it is not
/// one (a ValueError), while other errors (OverflowError) still raise.
fn as_of_of(value: Option<&PyValue>) -> Result<Option<String>, SmError> {
    let text = match value {
        Some(v @ (PyValue::Date(_) | PyValue::DateTime(_))) => {
            v.isoformat().expect("a date has an isoformat")
        }
        Some(PyValue::Str(s)) if !strip(s).is_empty() => s.clone(),
        _ => return Ok(None),
    };
    match validate_as_of(Some(&text), false, false) {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.is(Base::Value) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `str(value)` with runs of whitespace as one space; None is "".
fn text_of(value: Option<&PyValue>) -> Result<String, SmError> {
    Ok(match value {
        None | Some(PyValue::None) => String::new(),
        Some(v) => split(&v.try_py_str()?).join(" "),
    })
}

/// `parse_memory_file(path)`: raises UnicodeDecodeError on a file that is not UTF-8.
pub fn parse_memory_file(path: &PyPath) -> Result<MemoryFile, SmError> {
    let text = path.read_text(Errors::Strict)?;
    let (front, body) = split_frontmatter(&text);
    let (fields, yaml_error) = match &front {
        Some(f) => fields(f)?,
        None => (Fields::new(), false),
    };
    let get = |k: &str| dict_get(&fields, k);
    let name = match text_of(get("name"))? {
        n if n.is_empty() => path.stem().to_string(),
        n => n,
    };
    let title = match text_of(get("description"))? {
        t if t.is_empty() => name.clone(),
        t => t,
    };
    let raw_tag = lower(&text_of(get("tag"))?);
    let as_of = match as_of_of(get("modified"))? {
        Some(v) => v,
        None => match as_of_of(get("as_of"))? {
            Some(v) => v,
            None => epoch_to_as_of(mtime(path)?)?,
        },
    };
    let type_ = match lower(&text_of(get("type"))?) {
        t if t.is_empty() => "unknown".to_string(),
        t => t,
    };
    Ok(MemoryFile {
        path: path.clone(),
        project_dir: path.parent().parent().name().to_string(),
        name,
        body: match strip(&body) {
            "" => title.clone(),
            b => b.to_string(),
        },
        title,
        type_,
        tag: if VALID_TAGS.contains(&raw_tag.as_str()) {
            raw_tag
        } else {
            "assumed".into()
        },
        evidence: text_of(get("evidence"))?,
        as_of,
        yaml_error,
    })
}

/// `to_item(mf, repo)`.
pub fn to_item(mf: &MemoryFile, repo: &str) -> Item {
    let kind = if mf.type_ == "feedback" {
        "decision"
    } else {
        "fact"
    };
    let mut tags: Vec<&str> = Vec::new();
    for t in ["automemory", mf.type_.as_str(), mf.name.as_str()] {
        if !tags.contains(&t) {
            tags.push(t);
        }
    }
    let id = new_item_id(None);
    Item {
        refs: item_refs("", &mf.title, &mf.body, &mf.evidence),
        tags: tags.join(", "),
        source: mf.source(),
        evidence: if mf.evidence.is_empty() {
            mf.path.to_string()
        } else {
            mf.evidence.clone()
        },
        as_of: mf.as_of.clone(),
        ..Item::bare(&id, repo, kind, &mf.title, &mf.body, &mf.tag)
    }
    .post_init()
}

/// `transcript_cwd(project)`: the `cwd` of the first transcript line that has one, newest
/// transcript first.
pub fn transcript_cwd(project: &PyPath) -> Result<Option<String>, SmError> {
    let mut transcripts = Vec::new();
    for p in project.glob("*.jsonl")? {
        let t = mtime(&p)?;
        transcripts.push((t, p));
    }
    // sorted(..., reverse=True) is stable: equal times keep the glob order.
    transcripts.sort_by(|a, b| b.0.total_cmp(&a.0));
    for (_, t) in &transcripts {
        let mut lines = TextLines::open(t)?;
        let mut n = 0;
        while let Some(line) = lines.next_line()? {
            if n >= CWD_SCAN_LINES {
                break;
            }
            n += 1;
            let row = match loads(&line) {
                Ok(r) => r,
                Err(e) if e.python_name() == "JSONDecodeError" => continue,
                Err(e) => return Err(e),
            };
            if let PyValue::Dict(row) = &row
                && let Some(PyValue::Str(cwd)) = dict_get(row, "cwd")
                && !strip(cwd).is_empty()
            {
                return Ok(Some(strip(cwd).to_string()));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &std::path::Path, rel: &str, text: &str) -> PyPath {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, text).unwrap();
        PyPath::new(p.to_str().unwrap())
    }

    fn set_mtime(p: &PyPath, secs: u64) {
        let f = fs::File::options().write(true).open(p.to_os()).unwrap();
        f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
            .unwrap();
    }

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir_in(crate::sources::TEST_TMP).unwrap()
    }

    const FULL: &str = "---\nname: alpha-rule\ndescription: \"Alpha rule: keep B short\"\nmetadata:\n  type: feedback\n  tag: client-stated\n  \
        evidence: \"owner in chat, 2026-10-01\"\n  as_of: 2026-10-01T08:00+00:00\n  modified: 2026-10-02T01:30:00.250Z\n---\n\nKeep B short.\n";

    #[test]
    fn a_field_int_past_4300_digits_raises_on_str() {
        // Python 3.15.1 (probe 2026-11-05): _text(str(value)) of 0x f*3572 raises, f*3571 is 4300 digits
        let d = tmp();
        let big = parse_memory_file(&write(
            d.path(),
            "p/memory/a.md",
            &format!("---\nname: 0x{}\n---\nbody\n", "f".repeat(3572)),
        ));
        assert_eq!(
            big.unwrap_err().python_text(),
            "ValueError: Exceeds the limit (4300 digits) for integer string conversion; use sys.set_int_max_str_digits() to increase the limit"
        );
        let ok = parse_memory_file(&write(
            d.path(),
            "p/memory/b.md",
            &format!("---\nname: 0x{}\n---\nbody\n", "f".repeat(3571)),
        ));
        assert_eq!(ok.unwrap().name.len(), 4300);
    }

    // Ports of the Python auto-memory parse tests, one test each.
    #[test]
    fn parse_keeps_metadata_and_maps_feedback_to_decision() {
        let d = tmp();
        let mf = parse_memory_file(&write(d.path(), "proj-a/memory/alpha-rule.md", FULL)).unwrap();
        let item = to_item(&mf, "r1");
        assert_eq!(
            (item.kind.as_str(), item.title.as_str(), item.body.as_str()),
            ("decision", "Alpha rule: keep B short", "Keep B short.")
        );
        assert_eq!(
            (item.tag.as_str(), item.evidence.as_str()),
            ("client-stated", "owner in chat, 2026-10-01")
        );
        assert_eq!(
            item.as_of,
            crate::core::item::validate_as_of(Some("2026-10-02T01:30:00Z"), false, false).unwrap(),
            "modified wins over as_of"
        );
        assert_eq!(item.source, "automemory:proj-a/alpha-rule.md");
        assert_eq!(item.tags, "automemory, feedback, alpha-rule");
        assert_eq!(item.repo, "r1");
        assert!(!mf.yaml_error);
    }

    #[test]
    fn parse_tag_outside_the_eight_becomes_assumed() {
        let d = tmp();
        for (meta, tag) in [
            ("  tag: unconfirmed\n", "assumed"),
            ("  tag: decision\n", "assumed"),
            ("", "assumed"),
            ("  tag: Verified\n", "verified"),
        ] {
            let f = write(
                d.path(),
                "p/memory/x.md",
                &format!("---\nname: x\nmetadata:\n  type: project\n{meta}---\nBody x\n"),
            );
            let item = to_item(&parse_memory_file(&f).unwrap(), "r1");
            assert_eq!(item.tag, tag, "{meta:?}");
            assert_eq!(
                item.evidence,
                f.as_str(),
                "no metadata.evidence: the file path is the evidence"
            );
            assert_eq!(item.kind, "fact");
        }
    }

    #[test]
    fn parse_as_of_falls_back_to_as_of_then_mtime() {
        let d = tmp();
        let f = write(
            d.path(),
            "p/memory/d.md",
            "---\nname: d\nmetadata:\n  type: user\n  as_of: 2026-09-05\n---\nD\n",
        );
        assert_eq!(
            parse_memory_file(&f).unwrap().as_of,
            crate::core::item::validate_as_of(Some("2026-09-05"), false, false).unwrap(),
            "an unquoted YAML date is a date object"
        );
        let g = write(
            d.path(),
            "p/memory/m.md",
            "---\nname: m\nmetadata:\n  modified: not a date\n---\nM\n",
        );
        set_mtime(&g, 1_767_225_600); // 2026-01-01T00:00:00Z
        assert_eq!(
            parse_memory_file(&g).unwrap().as_of,
            crate::core::item::epoch_to_as_of(1_767_225_600.0).unwrap()
        );
    }

    #[test]
    fn parse_yaml_error_reads_known_lines_and_strips_frontmatter() {
        let d = tmp();
        let f = write(
            d.path(),
            "p/memory/bad.md",
            "---\nname: bad\ndescription: Rule: a colon breaks YAML: here\nmetadata:\n  type: feedback\n  tag: verified\n  evidence: run 1\n---\nThe body.\n",
        );
        let mf = parse_memory_file(&f).unwrap();
        let item = to_item(&mf, "r1");
        assert!(mf.yaml_error);
        assert_eq!(
            (
                item.kind.as_str(),
                item.title.as_str(),
                item.body.as_str(),
                item.tag.as_str()
            ),
            (
                "decision",
                "Rule: a colon breaks YAML: here",
                "The body.",
                "verified"
            )
        );
    }

    #[test]
    fn parse_without_frontmatter_uses_stem_and_whole_text() {
        let d = tmp();
        let item = to_item(
            &parse_memory_file(&write(
                d.path(),
                "p/memory/plain-note.md",
                "Note: café crème\nline 2\n",
            ))
            .unwrap(),
            "r1",
        );
        assert_eq!(
            (
                item.title.as_str(),
                item.body.as_str(),
                item.kind.as_str(),
                item.tags.as_str()
            ),
            (
                "plain-note",
                "Note: café crème\nline 2",
                "fact",
                "automemory, unknown, plain-note"
            )
        );
    }

    #[test]
    fn parse_non_utf8_raises() {
        let d = tmp();
        let f = write(d.path(), "p/memory/latin.md", "");
        fs::write(f.to_os(), b"caf\xe9\n").unwrap();
        assert_eq!(
            parse_memory_file(&f).unwrap_err().python_name(),
            "UnicodeDecodeError"
        );
    }

    #[test]
    fn discover_skips_index_and_fails_on_missing_root() {
        let d = tmp();
        write(d.path(), "p/memory/MEMORY.md", "- index\n");
        let a = write(d.path(), "p/memory/a.md", "A\n");
        write(d.path(), "p/other/b.md", "B\n");
        let root = PyPath::new(d.path().to_str().unwrap());
        assert_eq!(discover(&root).unwrap(), [a]);
        let e = discover(&root.join("nope")).unwrap_err();
        assert_eq!(
            e.python_text(),
            format!(
                "FileNotFoundError: automemory root not found: {}/nope",
                root
            )
        );
    }

    #[test]
    fn transcript_cwd_reads_newest_transcript_first() {
        let d = tmp();
        let old = write(d.path(), "p/old.jsonl", "{\"cwd\": \"/a/old\"}\n");
        let new = write(
            d.path(),
            "p/new.jsonl",
            "{\"type\": \"x\"}\nnot json \"cwd\"\n{\"cwd\": \"/a/new\"}\n",
        );
        set_mtime(&old, 1_000);
        set_mtime(&new, 2_000);
        let root = PyPath::new(d.path().to_str().unwrap());
        assert_eq!(
            transcript_cwd(&root.join("p")).unwrap().as_deref(),
            Some("/a/new")
        );
        assert_eq!(transcript_cwd(&root.join("empty")).unwrap(), None);
    }

    #[test]
    fn parse_odd_frontmatter_shapes() {
        let d = tmp();
        let open = parse_memory_file(&write(d.path(), "p/memory/open.md", "---\nname: x\nBody\n"))
            .unwrap();
        assert_eq!(
            (open.body.as_str(), open.name.as_str()),
            ("---\nname: x\nBody", "open"),
            "no closing line: the whole file is the body"
        );
        let mf = parse_memory_file(&write(
            d.path(),
            "p/memory/listy.md",
            "---\n- a\n- b\n---\nL body\n",
        ))
        .unwrap();
        assert_eq!(
            (
                mf.name.as_str(),
                mf.type_.as_str(),
                mf.body.as_str(),
                mf.yaml_error
            ),
            ("listy", "unknown", "L body", false)
        );
        let empty = parse_memory_file(&write(
            d.path(),
            "p/memory/empty.md",
            "---\nname: e\ndescription: Only a title\n---\n\n",
        ))
        .unwrap();
        assert_eq!(empty.body, "Only a title");
        let both = parse_memory_file(&write(
            d.path(),
            "p/memory/both.md",
            "---\nname: b\ntype: user\ntag: verified\nmetadata:\n  tag: decided\n---\nB\n",
        ))
        .unwrap();
        assert_eq!(
            (both.type_.as_str(), both.tag.as_str()),
            ("user", "decided")
        );
    }

    #[test]
    fn parse_yaml_error_fallback_prefers_the_nested_key() {
        let d = tmp();
        let mf = parse_memory_file(&write(d.path(), "p/memory/n.md", "---\nname: n\ntag: verified\ndescription: a: b: c\nmetadata:\n  tag: decided\n---\nN\n")).unwrap();
        assert_eq!((mf.yaml_error, mf.tag.as_str()), (true, "decided"));
    }

    #[test]
    fn parse_horizontal_rule_without_frontmatter_keeps_whole_text() {
        let d = tmp();
        let mf = parse_memory_file(&write(
            d.path(),
            "p/memory/hr.md",
            "Top line\n---\nname: not frontmatter\n---\nEnd\n",
        ))
        .unwrap();
        assert_eq!(
            (mf.name.as_str(), mf.body.as_str()),
            ("hr", "Top line\n---\nname: not frontmatter\n---\nEnd")
        );
    }

    #[test]
    fn transcript_cwd_reads_only_the_first_200_lines() {
        let d = tmp();
        let old = write(d.path(), "p/old.jsonl", "{\"cwd\": \"/a/old\"}\n");
        let late = write(
            d.path(),
            "p/late.jsonl",
            &("{\"type\": \"x\"}\n".repeat(200) + "{\"cwd\": \"/a/late\"}\n"),
        );
        set_mtime(&old, 1_000);
        set_mtime(&late, 2_000);
        assert_eq!(
            transcript_cwd(&PyPath::new(d.path().join("p").to_str().unwrap()))
                .unwrap()
                .as_deref(),
            Some("/a/old")
        );
    }

    // Edge cases beyond the Python suite; expected values from Python 3.15.1.
    #[test]
    fn fallback_unquotes_and_errors_keep_their_class() {
        let d = tmp();
        let mf = parse_memory_file(&write(d.path(), "p/memory/q.md",
            "---\n  name:  'quoted name'  \ndescription: x: y\nmodified: \"2026-03-04T05:06:07+05:00\"\n---\nQ\n")).unwrap();
        assert_eq!(
            (mf.yaml_error, mf.name.as_str(), mf.as_of.as_str()),
            (
                true,
                "quoted name",
                crate::core::item::validate_as_of(Some("2026-03-04T05:06:07+05:00"), false, false)
                    .unwrap()
                    .as_str()
            )
        );
        let mf = parse_memory_file(&write(
            d.path(),
            "p/memory/w.md",
            "---\nname: \"a\\tb   c\"\ntype: 7\ntag: [x]\n---\n",
        ))
        .unwrap();
        assert_eq!(
            (
                mf.name.as_str(),
                mf.type_.as_str(),
                mf.tag.as_str(),
                mf.body.as_str()
            ),
            ("a b c", "7", "assumed", "a b c")
        );
        let e = parse_memory_file(&write(
            d.path(),
            "p/memory/m.md",
            "---\nmodified: 2026-13-01\n---\nM\n",
        ))
        .unwrap_err();
        assert_eq!(
            e.python_text(),
            "ValueError: month must be in 1..12, not 13",
            "a datetime ValueError is not a YAMLError"
        );
        let e = parse_memory_file(&write(
            d.path(),
            "p/memory/o.md",
            "---\nmodified: '9999-12-31T23:00:00-05:00'\n---\nO\n",
        ))
        .unwrap_err();
        assert_eq!(
            e.python_text(),
            "OverflowError: date value out of range",
            "only a ValueError falls through to the next as_of"
        );
    }

    #[test]
    fn transcript_cwd_errors_and_crlf() {
        let d = tmp();
        let p = d.path().join("p");
        fs::create_dir_all(p.join("dir.jsonl")).unwrap();
        let root = PyPath::new(p.to_str().unwrap());
        assert_eq!(
            transcript_cwd(&root).unwrap_err().python_name(),
            "IsADirectoryError"
        );
        fs::remove_dir(p.join("dir.jsonl")).unwrap();
        write(
            d.path(),
            "p/c.jsonl",
            "[1]\r{\"cwd\": \"  \"}\r\n{\"cwd\": 5}\r{\"cwd\": \" /c \"}",
        );
        assert_eq!(transcript_cwd(&root).unwrap().as_deref(), Some("/c"));
        std::os::unix::fs::symlink("nowhere", p.join("broken.jsonl")).unwrap();
        assert_eq!(
            transcript_cwd(&root).unwrap_err().python_name(),
            "FileNotFoundError"
        );
    }
}
