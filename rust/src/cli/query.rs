//! The `query` command with its formatters, and the refs helpers that `related` and the hook
//! use too.

use std::collections::BTreeSet;

use crate::core::rank::{Why, match_snippet};
use crate::core::tags::retired_label;
use crate::error::SmError;
use crate::py;
use crate::py::argparse::{Namespace, Value};
use crate::py::io::Out;
use crate::py::value::PyValue;
use crate::service::query::{
    Lookup, QueryArgs, StrList, item_text, item_tokens, lookup_json, next_cmds, run_lookup,
    split_list,
};
use crate::service::usage::log_usage;

pub const MAX_TOTAL_CHARS: usize = 2400;
pub const MIN_TEXT_CHARS: usize = 80;
pub const HEADER_REFS_CHARS: usize = 100;
pub const HOOK_TITLE_CHARS: usize = 160;
pub const HOOK_REFS_CHARS: usize = 60;

/// Leading refs that fit in max_chars (a longer first ref is
/// cut), then ` +N`.
pub fn compact_refs(refs: &str, max_chars: usize) -> String {
    let tokens = py::str::split(refs);
    let mut shown: Vec<String> = Vec::new();
    let mut used = 0;
    for tok in &tokens {
        let mut tok = tok.to_string();
        if shown.is_empty() && py::str::len(&tok) > max_chars {
            tok = format!(
                "{}...",
                py::str::slice(&tok, None, Some(max_chars as isize - 3))
            );
        }
        let cost = py::str::len(&tok) + usize::from(!shown.is_empty());
        if !shown.is_empty() && used + cost > max_chars {
            break;
        }
        shown.push(tok);
        used += cost;
    }
    let rest = tokens.len() - shown.len();
    let tail = if rest > 0 {
        format!(" +{rest}")
    } else {
        String::new()
    };
    format!("{}{tail}", shown.join(" "))
}

/// Refs minus the one equal to the source after its `prefix:`.
pub fn refs_without_source(it: &crate::core::item::Item) -> String {
    let source_value = it.source.split_once(':').map_or("", |(_, v)| v);
    py::str::split(&it.refs)
        .into_iter()
        .filter(|t| *t != source_value)
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn why_text(why: &Why) -> String {
    let mut parts: Vec<String> = why.ref_hits.iter().map(|r| format!("ref:{r}")).collect();
    if why.phrase {
        parts.push("phrase".into());
    }
    if why.accent {
        parts.push("accent".into());
    }
    let n_terms = why.terms_matched.len() + why.terms_missing.len();
    if n_terms > 0 {
        parts.push(format!("terms:{}/{n_terms}", why.terms_matched.len()));
    }
    parts.extend(why.bias.iter().map(|b| format!("bias:{b}")));
    if why.recent {
        parts.push("recent".into());
    }
    parts.join(",")
}

/// `shlex.quote(s)`: `[^\w@%+=:,./-]` with re.ASCII finds an unsafe char.
pub(crate) fn shlex_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    let safe = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if s.chars().all(safe) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

pub fn shell_arg(text: &str) -> String {
    if text.chars().any(|c| "\"$`\\!".contains(c)) {
        shlex_quote(text)
    } else {
        format!("\"{text}\"")
    }
}

/// A query that starts with '-' goes after '--'.
pub fn query_cmd(text: &str, flags: &[String]) -> String {
    let mut parts = vec!["sm query".to_string()];
    if text.starts_with('-') {
        parts.extend(flags.iter().cloned());
        parts.push("--".into());
        parts.push(shell_arg(text));
    } else {
        parts.push(shell_arg(text));
        parts.extend(flags.iter().cloned());
    }
    parts.join(" ")
}

fn bias_of(ns: &Namespace) -> Option<Vec<String>> {
    match ns.get("bias") {
        Value::List(_) => {
            if matches!(ns.get("bias_text"), Value::Str(_))
                && split_list(StrList::Str(ns.get_str("bias_text"))).is_empty()
            {
                return None;
            }
            Some(ns.get_str_list("bias"))
        }
        _ => None,
    }
}

/// (scope flags, filter flags) to carry into the footer commands.
fn query_flags(ns: &Namespace) -> (Vec<String>, Vec<String>) {
    let s = |v: &str| v.to_string();
    let repo = ns.get_str("repo");
    let paths = split_list(StrList::Str(ns.get_str("in")));
    let scope = if !paths.is_empty() {
        vec![s("--in"), paths.join(",")]
    } else if ns.get_bool("all_repos") {
        vec![s("--all")]
    } else if !repo.is_empty() {
        vec![s("--repo"), shlex_quote(repo)]
    } else {
        vec![]
    };
    let mut filters = Vec::new();
    for (flag, dest) in [("--tag", "tag"), ("--kind", "kind")] {
        let vals = split_list(StrList::Str(ns.get_str(dest)));
        if !vals.is_empty() {
            filters.extend([s(flag), vals.join(",")]);
        }
    }
    let sources = split_list(StrList::Str(ns.get_str("source")));
    if !sources.is_empty() {
        filters.extend([s("--source"), shlex_quote(&sources.join(","))]);
    }
    for (flag, dest) in [("--since", "since"), ("--until", "until")] {
        let val = ns.get_str(dest);
        if !val.is_empty() {
            filters.extend([s(flag), shlex_quote(val)]);
        }
    }
    if ns.get_bool("include_history") {
        filters.push(s("--include-history"));
    }
    if ns.get_bool("recent") {
        filters.push(s("--recent"));
    }
    if let Some(b) = bias_of(ns).filter(|b| !b.is_empty()) {
        filters.extend([s("--bias"), b.join(",")]);
    }
    (scope, filters)
}

/// `str(limit)` and `str(limit + n)` for the --limit flag, which may lie past i64.
#[derive(Clone, Debug)]
struct LimitText(String);

impl LimitText {
    fn of(v: &Value) -> LimitText {
        match v {
            Value::Int(n) => LimitText(n.to_string()),
            Value::BigInt(s) => LimitText(s.clone()),
            other => panic!("--limit holds {other:?}, not an int"),
        }
    }

    /// `limit + n` for a positive limit (a bad limit raised before any output) and n in 0..=10,
    /// by decimal digits, so it holds past i64.
    fn plus(&self, n: i64) -> String {
        let mut digits: Vec<u8> = self.0.bytes().map(|b| b - b'0').collect();
        let mut carry = n as u64;
        for d in digits.iter_mut().rev() {
            let sum = u64::from(*d) + carry;
            *d = (sum % 10) as u8;
            carry = sum / 10;
            if carry == 0 {
                break;
            }
        }
        let head = if carry > 0 {
            carry.to_string()
        } else {
            String::new()
        };
        head + &digits
            .iter()
            .map(|d| char::from(b'0' + d))
            .collect::<String>()
    }
}

fn footer_lines(lk: &Lookup, ns: &Namespace, limit: &LimitText) -> Vec<String> {
    let (scope, filters) = query_flags(ns);
    let with = |a: &[String], b: &[String]| -> Vec<String> { a.iter().chain(b).cloned().collect() };
    let mut lines = Vec::new();
    if !lk.missing.is_empty() {
        let parts: Vec<String> = lk
            .missing
            .iter()
            .map(|(term, n)| {
                if *n != 0 {
                    format!("{term}={n} -> {}", query_cmd(term, &with(&scope, &filters)))
                } else {
                    format!("{term}=0")
                }
            })
            .collect();
        lines.push(format!("missing: {}", parts.join("; ")));
    }
    let mut more = Vec::new();
    if lk.beyond_limit != 0 {
        let wider = limit.plus(lk.beyond_limit.min(10));
        let full = lk.pool_cap.is_some_and(|c| c != 0);
        let count = if full {
            format!("{}+", lk.beyond_limit)
        } else {
            lk.beyond_limit.to_string()
        };
        let pool = if full {
            format!(" (pool of {} full)", lk.pool_cap.unwrap_or(0))
        } else {
            String::new()
        };
        let flags = with(&with(&scope, &filters), &["--limit".to_string(), wider]);
        more.push(format!(
            "{count} beyond --limit {}{pool} -> {}",
            limit.0,
            query_cmd(&lk.query, &flags)
        ));
    }
    if !lk.other_repos.is_empty() {
        let plus = if lk.other_repos_cap.is_some_and(|c| c != 0) {
            "+"
        } else {
            ""
        };
        let counts = lk
            .other_repos
            .iter()
            .map(|(r, n)| format!("{r}={n}{plus}"))
            .collect::<Vec<_>>()
            .join(" ");
        more.push(format!(
            "other repos {counts} -> {}",
            query_cmd(&lk.query, &with(&["--all".to_string()], &filters))
        ));
    }
    if !more.is_empty() {
        lines.push(format!("more: {}", more.join("; ")));
    }
    if !lk.shared_refs.is_empty() {
        lines.push(format!(
            "refs: {}",
            lk.shared_refs
                .iter()
                .map(|(r, n)| format!("{r}={n}"))
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
    let nxt = next_cmds(lk);
    if !nxt.is_empty() {
        lines.push(format!("next: {}", nxt.join("; ")));
    }
    lines
}

/// One block per hit; hits and footer stay inside
/// MAX_TOTAL_CHARS, each text keeping at least min(len, MIN_TEXT_CHARS).
pub fn format_query_text(lk: &Lookup, footer: &[String]) -> String {
    let mut headers: Vec<String> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    for (i, s) in lk.hits.iter().enumerate() {
        let it = &s.item;
        let mut parts = vec![
            format!("[{}]", it.id),
            format!("#{}", i + 1),
            format!("repo={}", it.repo),
            format!("kind={}", it.kind),
            format!("tag={}", it.tag),
            format!("as_of={}", it.as_of),
            format!("~{} tok", item_tokens(it)),
            format!("why={}", why_text(&s.why)),
        ];
        if !it.source.is_empty() {
            parts.push(format!("source={}", it.source));
        }
        let refs = refs_without_source(it);
        if !refs.is_empty() {
            parts.push(format!("refs={}", compact_refs(&refs, HEADER_REFS_CHARS)));
        }
        match it.superseded_by.as_deref() {
            Some(by) if it.status == "superseded" && !by.is_empty() => {
                parts.push(format!("superseded_by={by}"))
            }
            _ if it.status == "superseded" => {
                parts.push(format!("retired: {}", retired_label(&it.tags)))
            }
            _ => {}
        }
        headers.push(parts.join(" "));
        texts.push(item_text(it));
    }
    let lens: Vec<i64> = texts.iter().map(|t| py::str::len(t) as i64).collect();
    let footer_chars: i64 = footer.iter().map(|l| py::str::len(l) as i64 + 1).sum();
    let total_overhead: i64 = headers
        .iter()
        .map(|h| py::str::len(h) as i64 + 4)
        .sum::<i64>()
        + footer_chars;
    let floor: i64 = lens.iter().map(|&n| n.min(MIN_TEXT_CHARS as i64)).sum();
    let avail_text = floor.max(MAX_TOTAL_CHARS as i64 - total_overhead);

    let mut caps = lens.clone();
    if caps.iter().sum::<i64>() > avail_text {
        let mut remaining = avail_text;
        let mut undecided: BTreeSet<usize> = (0..texts.len()).collect();
        while !undecided.is_empty() {
            let k = undecided.len() as i64;
            let share = remaining.div_euclid(k);
            let short: Vec<usize> = undecided
                .iter()
                .copied()
                .filter(|&i| lens[i] <= share)
                .collect();
            if short.is_empty() {
                let rem = remaining.rem_euclid(k);
                for (pos, &i) in undecided.iter().enumerate() {
                    let extra = i64::from((pos as i64) < rem);
                    caps[i] = (share + extra).max(0);
                }
                break;
            }
            for i in short {
                caps[i] = lens[i];
                remaining -= lens[i];
                undecided.remove(&i);
            }
        }
    }

    let mut blocks: Vec<String> = Vec::new();
    for (((s, header), text), &cap) in lk.hits.iter().zip(&headers).zip(&texts).zip(&caps) {
        let snippet = match_snippet(
            text,
            &s.why.terms_matched,
            &s.why.ref_hits,
            cap.max(0) as usize,
        );
        blocks.push(format!("{header}\n  {snippet}"));
    }
    blocks.extend(footer.iter().cloned());
    blocks.join("\n")
}

pub fn format_query_brief(lk: &Lookup, footer: &[String]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for (i, s) in lk.hits.iter().enumerate() {
        let it = &s.item;
        let mut title = py::str::split(&it.title).join(" ");
        if py::str::len(&title) > HOOK_TITLE_CHARS {
            title = format!(
                "{}...",
                py::str::rstrip(py::str::slice(
                    &title,
                    None,
                    Some(HOOK_TITLE_CHARS as isize - 3)
                ))
            );
        }
        lines.push(format!(
            "[{}] #{} {} {} {} ~{} tok: {title}",
            it.id,
            i + 1,
            it.kind,
            it.tag,
            py::str::slice(&it.as_of, None, Some(10)),
            item_tokens(it)
        ));
    }
    lines.extend(footer.iter().cloned());
    lines.join("\n")
}

fn print_query(
    lk: &Lookup,
    ns: &Namespace,
    limit: &LimitText,
    out: &mut Out,
) -> Result<(), SmError> {
    let format = ns.get_str("format");
    if format == "json" {
        return out.print(&py::json::dumps_value(
            &lookup_json(lk),
            &py::json::Opts::unicode(),
        ));
    }
    let footer = footer_lines(lk, ns, limit);
    if lk.hits.is_empty() && footer.is_empty() {
        return Ok(());
    }
    let text = if format == "brief" {
        format_query_brief(lk, &footer)
    } else {
        format_query_text(lk, &footer)
    };
    out.print(&text)
}

fn s(v: &str) -> PyValue {
    PyValue::Str(v.to_string())
}

pub fn run(ns: &Namespace, out: &mut Out) -> Result<i32, SmError> {
    let limit_text = LimitText::of(ns.get("limit"));
    let bias = bias_of(ns);
    let a = QueryArgs {
        // a limit past i64 saturates: the same slices, and the same ValueError when negative
        limit: ns.get_int("limit"),
        recent: ns.get_bool("recent"),
        tags: StrList::Str(ns.get_str("tag")),
        kinds: StrList::Str(ns.get_str("kind")),
        sources: StrList::Str(ns.get_str("source")),
        since: ns.get_str("since"),
        until: ns.get_str("until"),
        bias: bias.as_deref().map_or(StrList::None, StrList::List),
        all_repos: ns.get_bool("all_repos"),
        repo: ns.get_str("repo"),
        include_history: ns.get_bool("include_history"),
        scopes: StrList::Str(ns.get_str("in")),
        extra_scopes: StrList::None,
        home: None,
        cwd: None,
    };
    let lookup = match run_lookup(ns.get_str("query"), &a) {
        Err(e) if e.class == crate::config::UNKNOWN_SCOPE => {
            out.eprint(&e.msg)?;
            return Ok(2);
        }
        r => r?,
    };
    print_query(&lookup, ns, &limit_text, out)?;
    let ids: Vec<String> = lookup.hits.iter().map(|h| h.item.id.clone()).collect();
    let flags = PyValue::Dict(vec![
        (
            s("bias"),
            PyValue::List(lookup.bias.iter().map(|b| s(b)).collect()),
        ),
        (s("kind"), s(ns.get_str("kind"))),
        (s("tag"), s(ns.get_str("tag"))),
        (s("source"), s(ns.get_str("source"))),
        (s("since"), s(ns.get_str("since"))),
        (s("until"), s(ns.get_str("until"))),
        (s("format"), s(ns.get_str("format"))),
        (s("limit"), PyValue::Int(limit_text.0.clone())),
        (s("all"), PyValue::Bool(ns.get_bool("all_repos"))),
    ]);
    log_usage("query", &ids, lookup.repo.as_deref(), Some(&flags), None)?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::item::Item;
    use crate::core::rank::Scored;
    use indexmap::IndexMap;

    fn lookup(hits: Vec<Scored>) -> Lookup {
        Lookup {
            query: "q".into(),
            bias: vec![],
            repo: None,
            all_repos: false,
            scopes: vec![],
            limit: 4,
            hits,
            missing: vec![],
            beyond_limit: 0,
            pool_cap: None,
            other_repos: IndexMap::new(),
            other_repos_cap: None,
            shared_refs: vec![],
        }
    }

    fn hit(id: &str, body: &str) -> Scored {
        let mut it = Item::bare(id, "r", "fact", "", body, "decided");
        it.as_of = "2026-01-01T00:00:00+00:00".into();
        Scored {
            item: it,
            why: Why {
                terms_matched: vec!["q".into()],
                ..Default::default()
            },
        }
    }

    #[test]
    fn compact_refs_cuts_and_counts() {
        assert_eq!(compact_refs("a b c", 3), "a b +1");
        assert_eq!(compact_refs("abcdefgh x", 6), "abc... +1");
        assert_eq!(compact_refs("", 6), "");
        assert_eq!(compact_refs("ab", 2), "ab");
    }

    #[test]
    fn refs_without_source_drops_the_source_value() {
        let mut it = Item::bare("m", "r", "task", "", "", "decided");
        it.refs = "T-1 x:y y".into();
        it.source = "tasklog:T-1".into();
        assert_eq!(refs_without_source(&it), "x:y y");
        it.source = "nocolon".into();
        assert_eq!(refs_without_source(&it), "T-1 x:y y");
    }

    #[test]
    fn why_text_parts() {
        let w = Why {
            ref_hits: vec!["A-1".into()],
            phrase: true,
            accent: true,
            terms_matched: vec!["a".into()],
            terms_missing: vec!["b".into()],
            bias: vec!["decided".into()],
            recent: true,
            score: 0.0,
        };
        assert_eq!(
            why_text(&w),
            "ref:A-1,phrase,accent,terms:1/2,bias:decided,recent"
        );
        assert_eq!(why_text(&Why::default()), "");
    }

    #[test]
    fn shell_args_and_query_cmd() {
        assert_eq!(shell_arg("a b"), "\"a b\"");
        assert_eq!(shell_arg("it's $x"), "'it'\"'\"'s $x'");
        assert_eq!(shell_arg(""), "\"\"");
        assert_eq!(
            query_cmd("-R6", &["--all".into()]),
            "sm query --all -- \"-R6\""
        );
        assert_eq!(query_cmd("x", &["--all".into()]), "sm query \"x\" --all");
    }

    #[test]
    fn limit_text_adds_past_i64() {
        assert_eq!(
            LimitText("99999999999999999999999999999999999999999".into()).plus(10),
            "100000000000000000000000000000000000000009"
        );
        assert_eq!(
            LimitText::of(&Value::Int(i64::MAX)).plus(10),
            "9223372036854775817"
        );
        assert_eq!(LimitText::of(&Value::Int(4)).plus(3), "7");
        assert_eq!(LimitText::of(&Value::Int(95)).plus(10), "105");
        assert_eq!(LimitText::of(&Value::Int(1)).plus(0), "1");
    }

    #[test]
    fn text_budget_shares_the_space() {
        // two long texts and one short: the short keeps all, the long split the rest
        let lk = lookup(vec![
            hit("m_1", &"a".repeat(3000)),
            hit("m_2", "short"),
            hit("m_3", &"b".repeat(3000)),
        ]);
        let out = format_query_text(&lk, &[]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[3], "  short");
        let headers: usize = [0, 2, 4]
            .iter()
            .map(|&i| lines[i].chars().count() + 4)
            .sum();
        // 2400 - headers - 5 left for the two long ones, split evenly (odd char to the first)
        let rest = 2400 - headers - 5;
        assert_eq!(lines[1].chars().count(), 2 + (rest - rest / 2) - 3 + 3);
        assert!(lines[1].ends_with("..."));
        assert_eq!(lines[5].chars().count(), 2 + rest / 2);
        // a superseded hit names its successor, else the retired label
        let mut gone = hit("m_4", "x");
        gone.item.status = "superseded".into();
        assert!(
            format_query_text(&lookup(vec![gone.clone()]), &[]).contains("retired: file deleted")
        );
        let mut manual = gone.clone();
        manual.item.tags = "retired:manual".into();
        assert!(format_query_text(&lookup(vec![manual]), &[]).contains("retired: manual"));
        gone.item.superseded_by = Some("m_5".into());
        assert!(format_query_text(&lookup(vec![gone]), &[]).contains("superseded_by=m_5"));
    }

    #[test]
    fn brief_line_cuts_the_title() {
        let mut h = hit("m_1", "b");
        h.item.title = format!("{} {}", "t".repeat(170), "end");
        let line = format_query_brief(&lookup(vec![h]), &["next: x".into()]);
        assert_eq!(
            line,
            format!(
                "[m_1] #1 fact decided 2026-01-01 ~44 tok: {}...\nnext: x",
                "t".repeat(157)
            )
        );
    }

    #[test]
    fn query_flags_normalizes_sources_and_quotes_shell_metacharacters() {
        let argv: Vec<String> = [
            "query",
            "kiwi",
            "--repo",
            "my repo's",
            "--source",
            " task: , , agent:a b;echo ",
            "--since",
            "2026-05-01 09:00:00",
            "--until",
            "2026-05-02",
        ]
        .map(String::from)
        .to_vec();
        let ns = crate::cli::args::build_parser()
            .parse_args(&argv, &mut Out::capture())
            .unwrap();
        let (scope, filters) = query_flags(&ns);
        assert_eq!(scope, vec!["--repo", "'my repo'\"'\"'s'"]);
        assert_eq!(
            filters,
            vec![
                "--source",
                "'task:,agent:a b;echo'",
                "--since",
                "'2026-05-01 09:00:00'",
                "--until",
                "2026-05-02",
            ]
        );
        // Comma-only source is dropped from footer flags.
        let empty_src: Vec<String> = ["query", "kiwi", "--source", " , , "]
            .map(String::from)
            .to_vec();
        let ns_empty = crate::cli::args::build_parser()
            .parse_args(&empty_src, &mut Out::capture())
            .unwrap();
        assert_eq!(query_flags(&ns_empty), (vec![], vec![]));
    }
}
