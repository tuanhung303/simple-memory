use std::sync::LazyLock;

use regex::Regex;

use crate::core::item::{Item, epoch_to_as_of, new_item_id, validate_as_of};
use crate::core::refs::{RefsArg, RepoRoots, extract_refs, merge_refs};
use crate::core::tags::append_tag;
use crate::error::{Base, SmError};
use crate::py::path::{Errors, PyPath};
use crate::py::str::ci_literal;
use crate::py::str::{RE_S, lower, splitlines, strip, translate};
use crate::sources::{mtime, sorted_paths};

static HEADING_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&translate(r"^(#{2,3})\s+(.+?)\s*$")).expect("heading pattern"));
static WRITTEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!("^{}{}*(.+)$", ci_literal("Written:"), *RE_S)).expect("written pattern")
});
static CWD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!("^{}{}*(.+)$", ci_literal("CWD:"), *RE_S)).expect("cwd pattern")
});
static TITLE_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        "^#{s}*(?:{}{s}*)?",
        ci_literal("HANDOFF:"),
        s = *RE_S
    ))
    .expect("title pattern")
});

const KEEP_PREFIXES: [&str; 10] = [
    "request",
    "done",
    "recap",
    "decision",
    "decisions",
    "accomplished",
    "what changed",
    "results",
    "open",
    "next",
];

fn should_keep_heading(heading_text: &str) -> bool {
    let h = lower(strip(heading_text));
    KEEP_PREFIXES
        .iter()
        .any(|p| h == *p || h.starts_with(&format!("{p} ")))
}

/// `_infer_repo_for_handoff(path, cwd_line, repos)`, with `path` already resolved.
fn infer_repo_for_handoff(path: &PyPath, cwd_line: &str, repos: &RepoRoots) -> String {
    let repo = repos.repo_from_key(path.parent().name());
    if repo != "unassigned" || cwd_line.is_empty() {
        return repo;
    }
    let from_path = repos.repo_from_path(cwd_line);
    if from_path != "unassigned" {
        from_path
    } else {
        repos.repo_from_key(cwd_line)
    }
}

/// `parse_handoff_file(path, repos=None)`.
pub fn parse_handoff_file(path: &str, repos: Option<&RepoRoots>) -> Result<Item, SmError> {
    let p = PyPath::new(path).expanduser()?.resolve()?;
    let default = RepoRoots::default();
    let known_repos = repos.unwrap_or(&default);
    let text = p.read_text(Errors::Replace)?;
    let lines = splitlines(&text, false);

    let mut title = String::new();
    let mut written_raw = String::new();
    let mut cwd_raw = String::new();
    for line in lines.iter().take(20) {
        let s = strip(line);
        if s.starts_with("# ") && title.is_empty() {
            title = strip(&TITLE_PREFIX_RE.replacen(s, 1, "")).to_string();
        }
        if let Some(m) = WRITTEN_RE.captures(s)
            && written_raw.is_empty()
        {
            written_raw = strip(&m[1]).to_string();
        }
        if let Some(m) = CWD_RE.captures(s)
            && cwd_raw.is_empty()
        {
            cwd_raw = strip(&m[1]).to_string();
        }
    }
    if title.is_empty() {
        title = p.stem().to_string();
    }

    let mut kept_lines: Vec<&str> = Vec::new();
    let mut keep_current = false;
    for line in &lines {
        if let Some(m) = HEADING_RE.captures(line) {
            let h_name = lower(strip(&m[2]));
            keep_current = should_keep_heading(&h_name);
            if keep_current {
                kept_lines.push(line);
            }
        } else if keep_current {
            kept_lines.push(line);
        }
    }
    let mut body = strip(&kept_lines.join("\n")).to_string();
    if body.is_empty() {
        body = strip(&text).to_string();
    }

    let mut tags_str = "handoff".to_string();
    let as_of = match validate_as_of(Some(&written_raw), true, true) {
        Ok(v) => v,
        Err(e) if e.is(Base::Value) => {
            let v = epoch_to_as_of(mtime(&p)?)?;
            tags_str = append_tag(Some(&tags_str), "as_of:mtime");
            v
        }
        Err(e) => return Err(e),
    };

    let repo = infer_repo_for_handoff(&p, &cwd_raw, known_repos);
    let refs = merge_refs(&[Some(RefsArg::List(extract_refs(Some(&format!(
        "{title}\n{body}"
    )))))]);

    let id = new_item_id(None);
    Ok(Item {
        refs,
        tags: tags_str,
        source: format!("handoff:{}", p.name()),
        evidence: p.to_string(),
        as_of,
        ..Item::bare(&id, &repo, "handoff", &title, &body, "derived")
    }
    .post_init())
}

/// `load_handoffs(path, repos=None)`.
pub fn load_handoffs(path: &str, repos: Option<&RepoRoots>) -> Result<Vec<Item>, SmError> {
    let p = PyPath::new(path).expanduser()?.resolve()?;
    let default = RepoRoots::default();
    let known_repos = repos.unwrap_or(&default);
    if p.is_file() {
        return Ok(vec![parse_handoff_file(p.as_str(), Some(known_repos))?]);
    }
    if p.is_dir() {
        let files: Vec<PyPath> = sorted_paths(p.rglob("*.md")?)
            .into_iter()
            .filter(|f| f.as_str().ends_with(".md"))
            .collect();
        return files
            .iter()
            .map(|f| parse_handoff_file(f.as_str(), Some(known_repos)))
            .collect();
    }
    Err(SmError::new(
        Base::Os,
        "FileNotFoundError",
        format!("Handoff path not found: {p}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    use crate::py::path::realpath;

    /// A temp folder under `TEST_TMP`, so the resolved paths below are the literal ones.
    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir_in(crate::sources::TEST_TMP).unwrap()
    }

    fn roots(names: &[&str]) -> RepoRoots {
        RepoRoots {
            roots: vec![(
                "/Users/devuser/Work/Repos".into(),
                "Work/Repos".into(),
                names.iter().map(|s| s.to_string()).collect(),
            )],
        }
    }

    fn s(p: &std::path::Path) -> String {
        p.to_str().unwrap().to_string()
    }

    #[test]
    fn handoff_sections_and_scratchpad_repo() {
        let d = tmp();
        let proj = d.path().join(
            "-private-tmp-sandbox-1000--Users-devuser-Work-Repos-simple-memory-1234-scratchpad",
        );
        fs::create_dir_all(&proj).unwrap();
        let ho = proj.join("20261025-020000-handoff-sample.md");
        fs::write(
            &ho,
            "# HANDOFF: Finish migration parser\n\nWritten: 2026-10-18T10:10:00Z (10:10 UTC)\n\n## For Mia\n\n\
             ### Request\nBuild the handoff parser for R7.\n\n### Read-back\nShould be excluded.\n\n### Recap\n\
             - Built parser in src/simple_memory/sources/handoff.py\n\n### State\nShould be excluded.\n\n\
             ### Open items\n- Add CLI wiring\n\n### Next\nRun pytest.\n",
        )
        .unwrap();
        fs::write(
            proj.join("handoff-no-written.md"),
            "# HANDOFF: Fallback mtime\n\nWritten: some unparseable date text\n\n### Request\nTest mtime fallback.\n",
        )
        .unwrap();
        let items = load_handoffs(&s(d.path()), Some(&roots(&["simple-memory", "anvil"]))).unwrap();
        assert_eq!(items.len(), 2);
        let it = items
            .iter()
            .find(|i| i.source == "handoff:20261025-020000-handoff-sample.md")
            .unwrap();
        assert_eq!(it.repo, "simple-memory");
        assert_eq!(it.kind, "handoff");
        assert_eq!(it.tag, "derived");
        assert_eq!(it.evidence, s(&ho));
        assert_eq!(it.title, "Finish migration parser");
        assert_eq!(
            it.as_of,
            crate::core::item::validate_as_of(Some("2026-10-18T10:10:00Z"), false, false).unwrap()
        );
        assert_eq!(
            it.body,
            "### Request\nBuild the handoff parser for R7.\n\n### Recap\n- Built parser in src/simple_memory/sources/handoff.py\n\n\
             ### Open items\n- Add CLI wiring\n\n### Next\nRun pytest."
        );
        assert!(it.refs.split(' ').any(|r| r == "R7"), "{}", it.refs);
        let m = items
            .iter()
            .find(|i| i.source == "handoff:handoff-no-written.md")
            .unwrap();
        assert_eq!(m.tags, "handoff, as_of:mtime");
        assert!(chrono::DateTime::parse_from_rfc3339(&m.as_of).is_ok());
    }

    #[test]
    fn title_written_and_cwd_follow_python_rules() {
        let d = tmp();
        let f = d.path().join("x.md");
        // `wrİtten:` matches because sre lowers U+0130 to `i`; the second `# ` line is ignored.
        fs::write(&f, "#  HANDOFF:   Spaced  \n# second\nwrİtten: 2026-10-02T03:04:05+05:00\ncwd: /Users/devuser/Work/Repos/design/x\n#### Done\nno\n## Done \nkept\n").unwrap();
        let it = parse_handoff_file(&s(&f), Some(&roots(&["design"]))).unwrap();
        assert_eq!(it.title, "Spaced");
        assert_eq!(
            it.as_of,
            crate::core::item::validate_as_of(Some("2026-10-02T03:04:05+05:00"), false, false)
                .unwrap()
        );
        assert_eq!(it.repo, "design");
        assert_eq!(it.body, "## Done \nkept");
        assert_eq!(it.tags, "handoff");
    }

    #[test]
    fn first_written_and_cwd_win_and_cwd_key_fallback() {
        let d = tmp();
        let f = d.path().join("w.md");
        fs::write(&f, "Written: 2026-10-01\nWritten: 2026-10-05\nCWD: -Users-devuser-Work-Repos-anvil-wt\nCWD: /Users/devuser/Work/Repos/design\n").unwrap();
        let it = parse_handoff_file(&s(&f), Some(&roots(&["anvil", "design"]))).unwrap();
        assert_eq!(
            it.as_of,
            crate::core::item::validate_as_of(Some("2026-10-01"), false, false).unwrap(),
            "a date alone is midnight in the local timezone"
        );
        assert_eq!(
            it.repo, "anvil",
            "a CWD that names no root path falls back to the key form"
        );
    }

    #[test]
    fn body_fallback_keeps_universal_newlines_and_a_file_path_loads() {
        let d = tmp();
        let f = d.path().join("n.md");
        fs::write(&f, "a\r\nb\rc\n").unwrap();
        let items = load_handoffs(&s(&f), None).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].body, "a\nb\nc");
    }

    #[test]
    fn written_overflow_is_not_a_value_error() {
        let d = tmp();
        let f = d.path().join("o.md");
        fs::write(&f, "Written: 9999-12-31T23:00:00-05:00\n").unwrap();
        assert_eq!(
            parse_handoff_file(&s(&f), None).unwrap_err().python_text(),
            "OverflowError: date value out of range"
        );
    }

    #[test]
    fn written_past_line_20_and_no_title_fall_back() {
        let d = tmp();
        let f = d.path().join("a.b.md");
        fs::write(
            &f,
            format!("{}Written: 2026-10-03\nplain text\n", "\n".repeat(20)),
        )
        .unwrap();
        let it = parse_handoff_file(&s(&f), None).unwrap();
        assert_eq!(it.title, "a.b");
        assert_eq!(it.body, "Written: 2026-10-03\nplain text");
        assert_eq!(it.tags, "handoff, as_of:mtime");
        assert_eq!(it.repo, "unassigned");
    }

    #[test]
    fn bad_bytes_are_replaced_and_newlines_are_universal() {
        let d = tmp();
        let f = d.path().join("b.md");
        fs::write(&f, b"# T \xff\r\n## Done\r\na\rb \xed\xa0\x80 \xf0\x9f\x98").unwrap();
        let it = parse_handoff_file(&s(&f), None).unwrap();
        assert_eq!(it.title, "T \u{fffd}");
        assert_eq!(it.body, "## Done\na\nb \u{fffd}\u{fffd}\u{fffd} \u{fffd}");
    }

    #[test]
    fn missing_folder_and_unknown_user_errors() {
        let d = tmp();
        let gone = s(&d.path().join("gone"));
        let e = load_handoffs(&gone, None).unwrap_err();
        assert_eq!(
            e.python_text(),
            format!("FileNotFoundError: Handoff path not found: {gone}")
        );
        let e = parse_handoff_file(&gone, None).unwrap_err();
        assert_eq!(
            e.python_text(),
            format!("FileNotFoundError: [Errno 2] No such file or directory: '{gone}'")
        );
        let e = parse_handoff_file(&s(d.path()), None).unwrap_err();
        assert_eq!(e.python_name(), "IsADirectoryError");
        let e = parse_handoff_file("~no_such_user_sm_test/x.md", None).unwrap_err();
        assert_eq!(
            e.python_text(),
            "RuntimeError: Could not determine home directory."
        );
    }

    #[test]
    fn heading_keep_rules() {
        for (h, keep) in [
            ("Request", true),
            ("  done  ", true),
            ("decisions made", true),
            ("requests", false),
            ("what  changed", false),
            ("next\tstep", false),
            ("open items", true),
            ("", false),
        ] {
            assert_eq!(should_keep_heading(h), keep, "{h:?}");
        }
    }

    #[test]
    fn realpath_walks_links_like_posixpath() {
        let d = tmp();
        let base = s(d.path());
        symlink("b", d.path().join("a")).unwrap();
        symlink("a", d.path().join("b")).unwrap();
        fs::create_dir_all(d.path().join("sub/inner")).unwrap();
        symlink("sub/inner", d.path().join("link")).unwrap();
        let rp = |rel: &str| realpath(&format!("{base}/{rel}")).unwrap();
        assert_eq!(rp("a"), format!("{base}/a"), "a link loop stays as it is");
        assert_eq!(
            rp("link/.."),
            format!("{base}/sub"),
            "`..` after a link is the target's parent"
        );
        assert_eq!(rp("a/x/.."), format!("{base}/a"));
        assert_eq!(rp("link/../../q"), format!("{base}/q"));
        symlink(format!("{base}/sub"), d.path().join("abs")).unwrap();
        assert_eq!(
            rp("abs/inner"),
            format!("{base}/sub/inner"),
            "an absolute target restarts at /"
        );
        assert_eq!(realpath("/").unwrap(), "/");
        assert_eq!(
            realpath("/a\0b").unwrap_err().python_text(),
            "ValueError: lstat: embedded null character in path"
        );
    }

    #[test]
    fn rglob_matches_every_kind_and_sorts_by_parts() {
        let d = tmp();
        let base = s(d.path());
        for f in [
            "a-b/x.md",
            "a/x.md",
            ".h.md",
            "X.MD",
            "dir.md/e.md",
            "real/r.md",
        ] {
            let p = d.path().join(f);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, "x").unwrap();
        }
        symlink("real", d.path().join("lnk")).unwrap();
        symlink("nowhere.md", d.path().join("broken.md")).unwrap();
        let got: Vec<String> = sorted_paths(PyPath::new(&base).rglob("*.md").unwrap())
            .into_iter()
            .map(|p| p.as_str()[base.len() + 1..].to_string())
            .collect();
        assert_eq!(
            got,
            [
                ".h.md",
                "a/x.md",
                "a-b/x.md",
                "broken.md",
                "dir.md",
                "dir.md/e.md",
                "real/r.md"
            ]
        );
    }

    #[test]
    fn read_text_is_strict_with_universal_newlines() {
        let d = tmp();
        let f = d.path().join("t.txt");
        fs::write(&f, b"a\r\nb\rc\n").unwrap();
        assert_eq!(
            PyPath::new(&s(&f)).read_text(Errors::Strict).unwrap(),
            "a\nb\nc\n"
        );
        fs::write(&f, b"ok \xe2\x82").unwrap();
        assert_eq!(
            PyPath::new(&s(&f))
                .read_text(Errors::Strict)
                .unwrap_err()
                .python_text(),
            "UnicodeDecodeError: 'utf-8' codec can't decode bytes in position 3-4: unexpected end of data"
        );
    }

    #[test]
    fn stem_name_and_parts() {
        for (name, stem) in [
            ("a.b.md", "a.b"),
            ("a.", "a"),
            ("..md", "..md"),
            (".a", ".a"),
            ("...", "..."),
            ("", ""),
        ] {
            assert_eq!(PyPath::new(name).stem(), stem, "{name:?}");
        }
        assert_eq!(PyPath::new("/x//y/./z").parts(), ["/", "x", "y", "z"]);
        assert_eq!(PyPath::new("//x").parts(), ["//", "x"]);
        assert_eq!(PyPath::new("/x").parent().name(), "");
        assert_eq!(PyPath::new("/").name(), "");
        assert_eq!(PyPath::new("").as_str(), ".");
    }
}
