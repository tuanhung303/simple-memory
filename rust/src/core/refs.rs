//! Patterns are the Python ones through py::str (convention 4); those with `\b` or lookaround
//! at an end run on `PyRegex`.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;

use crate::core::redact::refs_a_match_touches;
use crate::error::SmError;
use crate::py::path::PyPath;
use crate::py::str::{
    PyRegex, S_INNER, ci_class, lower, rstrip_chars, strip, strip_chars, translate,
};

fn py_re(p: &str) -> PyRegex {
    PyRegex::new(p).expect("refs pattern compiles")
}

static TASKLOG_PATTERNS: LazyLock<[PyRegex; 2]> = LazyLock::new(|| {
    [
        py_re(r"\b(?:\d{8}-)?\d{6}-[0-9a-f]{8}-[a-z0-9-]+-[0-9a-f]{4}\b"),
        py_re(r"\b[a-z0-9-]+-\d{8}-\d{6}-[0-9a-f]{6,12}\b"),
    ]
});
// re.IGNORECASE: `[0-9a-f]` is sre's case set, and `\b` keeps Python's case-blind \w set.
static UUID_RE: LazyLock<PyRegex> = LazyLock::new(|| {
    let h = ci_class(('0'..='9').chain('a'..='f'));
    py_re(&format!(
        r"\b{h}{{8}}-{h}{{4}}-{h}{{4}}-{h}{{4}}-{h}{{12}}\b"
    ))
});
static TICKET_RE: LazyLock<PyRegex> = LazyLock::new(|| py_re(r"\b[A-Z]+-?\d+\b"));
const NON_TICKET_REFS: [&str; 6] = ["UTF-8", "UTF-16", "UTF-32", "SHA-1", "SHA-256", "SHA-512"];
static SHA_RE: LazyLock<PyRegex> = LazyLock::new(|| py_re(r"\b[0-9a-f]{7,40}\b"));
static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&translate(r"https?://\S+")).expect("url pattern"));
static PATH_TOKEN_RE: LazyLock<PyRegex> = LazyLock::new(|| {
    py_re(
        r"(?<![A-Za-z0-9_:])(?:~|\.\.?|/[A-Za-z0-9_.-]+|[A-Za-z0-9_.-]+)(?:/[A-Za-z0-9_.-]+)+(?::\d+(?:-\d+)?)?",
    )
});
static LINE_SUFFIX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&translate(r":\d+(?:-\d+)?\z")).expect("line suffix pattern"));
static REF_SPLIT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("[{},]+", *S_INNER)).expect("ref split pattern"));
static NON_ALNUM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("[^A-Za-z0-9]").expect("encode pattern"));
static SESSION_SOURCE_RE: LazyLock<PyRegex> = LazyLock::new(|| {
    py_re(r"^(?:(?:session|turn):|handoff:\d{8}-\d{6}-|task:\d{8}-\d{6}-)([0-9a-f]{8})(?![0-9a-f])")
});

const FILE_EXTS: [&str; 21] = [
    ".py", ".md", ".yaml", ".yml", ".json", ".jsonl", ".sql", ".toml", ".sh", ".html", ".db",
    ".txt", ".env", ".ts", ".js", ".css", ".lock", ".cfg", ".ini", ".csv", ".parquet",
];
const KNOWN_ROOT_DIRS: [&str; 11] = [
    "src",
    "docs",
    "tests",
    "migrate",
    "eval",
    "skills",
    "tmp",
    "config",
    "scripts",
    "references",
    "branches",
];

fn matches<'t>(re: &PyRegex, text: &'t str) -> Vec<&'t str> {
    re.finditer(text).iter().map(|m| m.as_str()).collect()
}

pub fn is_valid_path_token(token: &str) -> bool {
    if ["~/", "./", "../", "/"]
        .iter()
        .any(|p| token.starts_with(p))
    {
        return token.chars().count() > 2;
    }
    let parts: Vec<&str> = token.split('/').collect();
    if parts.len() >= 3 || KNOWN_ROOT_DIRS.contains(&parts[0]) {
        return true;
    }
    let last = parts[parts.len() - 1];
    match last.rfind('.') {
        Some(i) if i > 0 => FILE_EXTS.contains(&lower(&last[i..]).as_str()),
        _ => false,
    }
}

/// `extract_refs(text)`.
pub fn extract_refs(text: Option<&str>) -> Vec<String> {
    let Some(text) = text.filter(|t| !t.is_empty()) else {
        return Vec::new();
    };
    let mut found: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut add = |val: &str| {
        if !val.is_empty() && seen.insert(val.to_string()) {
            found.push(val.to_string());
        }
    };

    let no_urls = URL_RE.replace_all(text, " ").into_owned();

    let mut tasklog_spans: Vec<&str> = Vec::new();
    for pat in TASKLOG_PATTERNS.iter() {
        for m in matches(pat, &no_urls) {
            add(m);
            tasklog_spans.push(m);
        }
    }
    for m in matches(&TICKET_RE, &no_urls) {
        if !NON_TICKET_REFS.contains(&m) {
            add(m);
        }
    }
    for m in matches(&PATH_TOKEN_RE, &no_urls) {
        let raw = rstrip_chars(m, ".,;:)]'\"");
        let raw = LINE_SUFFIX_RE.replace(raw, "");
        if is_valid_path_token(&raw) {
            add(&raw);
        }
    }

    let mut masked = no_urls.clone();
    for tl in &tasklog_spans {
        masked = masked.replace(tl, " ");
    }
    let masked = UUID_RE.sub(&masked, " ", 0);

    for tok in matches(&SHA_RE, &masked) {
        if tok.len() == 8 && tok.bytes().all(|b| b.is_ascii_digit()) && tok.starts_with("20") {
            continue;
        }
        add(tok);
    }
    found
}

/// The `refs` argument of `split_refs`/`merge_refs`: a str, an iterable of values, or None.
#[derive(Clone, Debug)]
pub enum RefsArg<'a> {
    Str(&'a str),
    List(Vec<String>),
}

/// `split_refs(refs)`.
pub fn split_refs(refs: Option<&RefsArg>) -> Vec<String> {
    let tokens: Vec<String> = match refs {
        None => return Vec::new(),
        Some(RefsArg::Str("")) => return Vec::new(),
        Some(RefsArg::List(v)) if v.is_empty() => return Vec::new(),
        Some(RefsArg::Str(s)) => REF_SPLIT_RE.split(strip(s)).map(String::from).collect(),
        Some(RefsArg::List(v)) => v
            .iter()
            .flat_map(|item| {
                REF_SPLIT_RE
                    .split(strip(item))
                    .map(String::from)
                    .collect::<Vec<_>>()
            })
            .collect(),
    };
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for t in tokens {
        if !t.is_empty() && seen.insert(t.clone()) {
            out.push(t);
        }
    }
    out
}

/// `merge_refs(*sources)`: the refs of every source string and list element, each once, in
/// order. A string the merged list does not hold as written (it lost a token to the dedupe, or
/// a comma or more than one space parts its tokens) can hide a secret from the check on the
/// stored refs string. When one does, each such string and the merged list itself are read
/// (`refs_a_match_touches`), and every ref a match touches is dropped, wherever it stands; so
/// a key word two pairs share goes, and the value of each pair goes with it.
pub fn merge_refs(sources: &[Option<RefsArg>]) -> String {
    let mut combined: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    let mut cut: Vec<&str> = Vec::new();
    for src in sources.iter().flatten() {
        let raws: Vec<&str> = match src {
            RefsArg::Str(s) => vec![*s],
            RefsArg::List(v) => v.iter().map(String::as_str).collect(),
        };
        for raw in raws {
            let toks: Vec<&str> = REF_SPLIT_RE
                .split(strip(raw))
                .filter(|t| !t.is_empty())
                .collect();
            let mut lost = false;
            for tok in &toks {
                if seen.insert(tok.to_string()) {
                    combined.push(tok.to_string());
                } else {
                    lost = true;
                }
            }
            if toks.len() > 1 && (lost || toks.join(" ") != strip(raw)) {
                cut.push(raw);
            }
        }
    }
    if !cut.is_empty() {
        let merged = combined.join(" ");
        cut.push(&merged);
        let secret = refs_a_match_touches(cut);
        combined.retain(|t| !secret.contains(t));
    }
    combined.join(" ")
}

/// `item_refs(refs, title, body, evidence)`: its own refs, then those its text and evidence name.
pub fn item_refs(refs: &str, title: &str, body: &str, evidence: &str) -> String {
    merge_refs(&[
        Some(RefsArg::Str(refs)),
        Some(RefsArg::List(extract_refs(Some(&format!(
            "{title}\n{body}"
        ))))),
        Some(RefsArg::List(extract_refs(Some(evidence)))),
    ])
}

/// `encode_project_dir(path)`: each char outside [A-Za-z0-9] becomes `-`.
pub fn encode_project_dir(path: &str) -> String {
    NON_ALNUM_RE.replace_all(path, "-").into_owned()
}

/// `root_tail(root)`: a root as its path below the home folder, else (home too) its full path.
pub fn root_tail(root: &str) -> Result<String, SmError> {
    let path = PyPath::new(root).expanduser()?;
    let home = PyPath::home()?;
    if path.is_relative_to(&home) && path != home {
        return Ok(path.relative_to(&home)?.to_string());
    }
    Ok(path.to_string())
}

/// `key_marker(tail)`: `-code-` for `code`.
pub fn key_marker(tail: &str) -> String {
    format!("-{}-", strip_chars(&encode_project_dir(tail), "-"))
}

/// `key_segment(text, marker)`: the first `/` part that starts with `-` and holds `marker`.
pub fn key_segment<'a>(text: &'a str, marker: &str) -> &'a str {
    text.split('/')
        .find(|p| p.starts_with('-') && p.contains(marker))
        .unwrap_or("")
}

/// `RepoRoots`: (expanded root, its tail, child names longest first) per configured root.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoRoots {
    pub roots: Vec<(String, String, Vec<String>)>,
}

fn sort_names(names: &mut [String]) {
    names.sort_by(|a, b| {
        b.chars()
            .count()
            .cmp(&a.chars().count())
            .then_with(|| a.cmp(b))
    });
}

impl RepoRoots {
    /// `RepoRoots.scan(roots)`.
    pub fn scan<S: AsRef<str>>(roots: &[S]) -> Result<RepoRoots, SmError> {
        let mut out = Vec::new();
        for raw in roots {
            let root = PyPath::new(raw.as_ref()).expanduser()?;
            let mut names = Vec::new();
            if root.is_dir() {
                for p in root.iterdir()? {
                    let name = p.name().to_string();
                    if p.is_dir() && !name.starts_with('.') && name != "tmp" {
                        names.push(name);
                    }
                }
            }
            sort_names(&mut names);
            let tail = root_tail(root.as_str())?;
            out.push((root.to_string(), tail, names));
        }
        Ok(RepoRoots { roots: out })
    }

    /// `repo_from_key(key)`: the root child a Claude Code project key names, else "unassigned".
    pub fn repo_from_key(&self, key: &str) -> String {
        for (_, tail, names) in &self.roots {
            let marker = key_marker(tail);
            let seg = key_segment(key, &marker);
            let rest = if seg.is_empty() {
                String::new()
            } else {
                strip_chars(seg.split_once(marker.as_str()).map_or("", |(_, r)| r), "-").to_string()
            };
            for name in names {
                let enc = encode_project_dir(name);
                if rest == enc || rest.starts_with(&format!("{enc}-")) {
                    return name.clone();
                }
            }
        }
        "unassigned".into()
    }

    /// `repo_from_path(text)`: the first root child a `<root>/<repo>` path in `text` names.
    pub fn repo_from_path(&self, text: &str) -> String {
        for (_, tail, names) in &self.roots {
            let mut lowered: HashMap<String, &String> = HashMap::new();
            for n in names {
                lowered.insert(lower(n), n);
            }
            let re = Regex::new(&format!("{}/([A-Za-z0-9_.-]+)", regex::escape(tail)))
                .expect("escaped tail compiles");
            for c in re.captures_iter(text) {
                if let Some(n) = lowered.get(&lower(&c[1])) {
                    return (*n).clone();
                }
            }
        }
        "unassigned".into()
    }
}

/// `source_session_key(source)`: the 8 hex of the session a source names, or None.
pub fn source_session_key(source: Option<&str>) -> Option<String> {
    let src = source.unwrap_or("");
    SESSION_SOURCE_RE
        .search(src)
        .and_then(|m| m.group(1))
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_refs_drops_a_secret_pair_each_source_writes_before_the_dedupe() {
        // every importer and `sm put` build refs here; the dedupe once dropped the second
        // `password:`, so its value no longer had a key next to it
        let twice = "TKT-9 db password: Hunter2-example-Zz9 redis password: Hunter2-example-Yy8";
        assert_eq!(merge_refs(&[Some(RefsArg::Str(twice))]), "TKT-9 db redis");
        // a value the pair holds goes wherever it stands in the merged list
        assert_eq!(
            merge_refs(&[
                Some(RefsArg::List(vec![
                    "Hunter2-example-Zz9".into(),
                    "abc1234".into()
                ])),
                Some(RefsArg::Str("TKT-9 password: Hunter2-example-Zz9")),
            ]),
            "abc1234 TKT-9"
        );
        // a list element is one source: a pair inside it goes, the other elements stay
        let commits = vec![
            "abc1234".to_string(),
            "password: Hunter2-example-Zz9 password: Hunter2-example-Yy8".to_string(),
        ];
        assert_eq!(merge_refs(&[Some(RefsArg::List(commits))]), "abc1234");
        // a comma splits a string too: a secret that holds one is read before the split
        let comma = "TKT-1, https://user:pa,ss@host.example.com/x";
        assert_eq!(merge_refs(&[Some(RefsArg::Str(comma))]), "TKT-1");
        // plain repeated refs merge as before
        assert_eq!(
            merge_refs(&[
                Some(RefsArg::Str("TKT-1 docs/a.md TKT-1")),
                Some(RefsArg::List(vec!["TKT-1".into(), "abc1234".into()])),
            ]),
            "TKT-1 docs/a.md abc1234"
        );
    }

    #[test]
    fn merge_refs_drops_a_pair_whose_key_word_another_string_shares() {
        // the dedupe takes the second `password:`; the first pair, written as is, goes too
        let pairs = vec![
            "db password: Hunter2-example-Zz9".to_string(),
            "redis password: Hunter2-example-Yy8".to_string(),
        ];
        let tokens = vec![
            "token: Hunter2-example-Ww7".to_string(),
            "password: Hunter2-example-Zz9 token: Hunter2-example-Yy8".to_string(),
        ];
        // only commas part the tokens: each pair is read with one space between key and value
        let commas = "TKT-9,db,password:,Hunter2-example-Zz9,redis,password:,Hunter2-example-Yy8";
        // the read of the raw string drops `password= password`; `password= <value>` then
        // stands only in the merged list
        let key_words =
            "password,password: max_tokens=4096\tpassword= password Hunter2-example-Aa1  secret";
        let mut got = vec![
            merge_refs(&[Some(RefsArg::List(pairs))]),
            merge_refs(&[Some(RefsArg::List(tokens))]),
            merge_refs(&[Some(RefsArg::Str(commas))]),
            merge_refs(&[Some(RefsArg::Str(key_words))]),
        ];
        // a newline, CR LF, VT, U+0085 or U+2003 parts each pair as a comma does
        for sep in ["\n", "\r\n", "\u{b}", "\u{85}", "\u{2003}"] {
            let refs = format!(
                "TKT-9 db password:{sep}Hunter2-example-Zz9 redis password:{sep}Hunter2-example-Yy8"
            );
            got.push(merge_refs(&[Some(RefsArg::Str(&refs))]));
        }
        let parted = "TKT-9 db redis";
        assert_eq!(
            got,
            [
                "db redis",
                "",
                "TKT-9 db redis",
                "password: max_tokens=4096 secret",
                parted,
                parted,
                parted,
                parted,
                parted
            ]
        );
    }

    fn code_root(names: &[&str]) -> RepoRoots {
        RepoRoots {
            roots: vec![(
                "/u/ann/code".into(),
                "code".into(),
                names.iter().map(|s| s.to_string()).collect(),
            )],
        }
    }

    #[test]
    fn extract_refs_all_shapes() {
        let text = "Fixed R5, D9, PROJ-12 and SD-744 in commit dafd0a5 and 9990d733e1b2c3d4 \
            using UTF-8 and SHA-256 \
            for task 20261025-095016-3eb1b482-design-brainstorm-3537 \
            touching src/simple_memory/core/fold.py:12 and ~/.cache/memory.db. \
            Ignore https://example.com/a/b and ratio 18/20 or n/a.";
        let refs = extract_refs(Some(text));
        for want in [
            "20261025-095016-3eb1b482-design-brainstorm-3537",
            "R5",
            "D9",
            "PROJ-12",
            "SD-744",
            "dafd0a5",
            "9990d733e1b2c3d4",
            "src/simple_memory/core/fold.py",
            "~/.cache/memory.db",
        ] {
            assert!(refs.contains(&want.to_string()), "{want} in {refs:?}");
        }
        assert!(!refs.contains(&"UTF-8".to_string()));
        assert!(!refs.contains(&"SHA-256".to_string()));
        assert!(!refs.contains(&"18/20".to_string()));
        assert!(!refs.contains(&"n/a".to_string()));
        assert!(!refs.iter().any(|r| r.starts_with("https://")));
    }

    #[test]
    fn extract_refs_python_word_rules() {
        // `²` is \w in Python, so `R5²` holds no ticket; U+0301 is not, so `R5\u{301}` does.
        assert_eq!(extract_refs(Some("R5² R6\u{301}")), vec!["R6"]);
        assert_eq!(extract_refs(Some("src/a.py:١٢")), vec!["src/a.py"]);
        // An 8-digit token from 20 is a date, not a sha; 7 digits or another century are kept.
        assert_eq!(
            extract_refs(Some("20261104 2026110 dafd0a5 30261104")),
            vec!["2026110", "dafd0a5", "30261104"]
        );
    }

    #[test]
    fn extract_refs_empty_and_merge() {
        assert!(extract_refs(Some("")).is_empty());
        assert!(extract_refs(None).is_empty());
        let m = merge_refs(&[
            Some(RefsArg::Str("R5, D9")),
            Some(RefsArg::List(vec!["D9".into(), "dafd0a5".into()])),
            Some(RefsArg::Str("")),
        ]);
        assert_eq!(m, "R5 D9 dafd0a5");
        assert_eq!(
            split_refs(Some(&RefsArg::Str(" ,a\x1cb,,a "))),
            vec!["a", "b"]
        );
    }

    #[test]
    fn encode_project_dir_matches_claude_code_folder_names() {
        assert_eq!(
            encode_project_dir("/u/ann_b/code/web.app/.claude"),
            "-u-ann-b-code-web-app--claude"
        );
        assert_eq!(encode_project_dir(""), "");
        assert_eq!(encode_project_dir("é"), "-");
    }

    #[test]
    fn repo_from_key_maps_a_project_key_or_scratchpad_to_the_root_child() {
        let roots = code_root(&["simple-memory", "simple", "web"]);
        assert_eq!(roots.repo_from_key("-u-ann-code-web"), "web");
        assert_eq!(roots.repo_from_key("-u-ann-code-web-docs-notes"), "web");
        assert_eq!(
            roots.repo_from_key("-u-ann-code-web--claude-worktrees-x"),
            "web"
        );
        assert_eq!(
            roots.repo_from_key("-u-ann-code-simple-memory-tmp"),
            "simple-memory"
        );
        assert_eq!(
            roots.repo_from_key("/private/tmp/c-1/-u-ann-code-simple/sess-01/scratchpad/notes.md"),
            "simple"
        );
        assert_eq!(roots.repo_from_key("-u-ann-code"), "unassigned");
        assert_eq!(roots.repo_from_key("-u-ann-code-webapp"), "unassigned");
        assert_eq!(roots.repo_from_key("-u-bob-code-web"), "web");
        assert_eq!(roots.repo_from_key("-u-ann-vscode-web"), "unassigned");
        assert_eq!(roots.repo_from_key("/tmp/plain-folder"), "unassigned");
        assert_eq!(
            RepoRoots::default().repo_from_key("-u-ann-code-web"),
            "unassigned"
        );
    }

    #[test]
    fn repo_from_key_reads_only_a_segment_that_starts_with_a_dash_as_a_key() {
        let roots = code_root(&["simple", "web"]);
        assert_eq!(
            roots.repo_from_key("/u/ann/code/claude-code-web"),
            "unassigned"
        );
        assert_eq!(roots.repo_from_key("claude-code-web"), "unassigned");
        assert_eq!(
            roots.repo_from_key("/u/ann/code/claude-code-web/-u-ann-code-simple/scratchpad"),
            "simple"
        );
    }

    #[test]
    fn repo_from_path_reads_root_child_paths_in_text() {
        let roots = code_root(&["web", "api"]);
        assert_eq!(roots.repo_from_path("cwd: ~/code/API/src"), "api");
        assert_eq!(
            roots.repo_from_path("edited /u/ann/code/web/a.py then ~/code/api"),
            "web"
        );
        assert_eq!(roots.repo_from_path("~/code/other/x and code/web"), "web");
        assert_eq!(roots.repo_from_path("~/notes/web/x"), "unassigned");
    }

    #[test]
    fn root_tail_of_an_unknown_user_is_runtime_error() {
        assert_eq!(
            root_tail("~nosuchuser_q/x").unwrap_err().python_text(),
            "RuntimeError: Could not determine home directory."
        );
        assert_eq!(root_tail("/tmp//x/").unwrap(), "/tmp/x");
    }

    #[test]
    fn scan_lists_children_longest_first() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("code");
        for name in ["ab", "abc", ".hidden", "tmp"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        std::fs::write(root.join("file.txt"), "x").unwrap();
        let r = root.to_str().unwrap();
        let scanned = RepoRoots::scan(&[r, "/nonexistent/sm-test-root"]).unwrap();
        let names = &scanned.roots[0].2;
        assert_eq!(names, &["abc", "ab"]);
        assert!(scanned.roots[1].2.is_empty());
        assert_eq!(scanned.roots[1].1, "/nonexistent/sm-test-root");
    }

    #[test]
    fn source_session_key_shapes() {
        assert_eq!(
            source_session_key(Some("session:0123abcd-ffff")),
            Some("0123abcd".into())
        );
        assert_eq!(
            source_session_key(Some("task:20261025-095016-3eb1b482-x")),
            Some("3eb1b482".into())
        );
        assert_eq!(source_session_key(Some("session:0123abcde")), None);
        assert_eq!(source_session_key(None), None);
    }
}
