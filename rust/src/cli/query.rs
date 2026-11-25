//! The `query` command with its formatters, and the refs helpers that `related` and the hook
//! use too.

use std::collections::BTreeSet;

use crate::core::item::Item;
use crate::core::rank::{Why, clip, match_snippet};
use crate::core::tags::retired_label;
use crate::error::SmError;
use crate::py;
use crate::py::argparse::{Namespace, Value};
use crate::py::io::Out;
use crate::py::value::PyValue;
use crate::service::query::{
    Lookup, QueryArgs, StrList, item_text, item_tokens, lookup_json, narrowing_flags, next_cmds,
    run_lookup, split_list,
};
use crate::service::recall::is_stopword;
use crate::service::usage::log_usage;
use crate::sources::s;
use crate::store::search::repo_names;

pub const MAX_TOTAL_CHARS: usize = 2400;
pub const MIN_TEXT_CHARS: usize = 80;
pub const HEADER_REFS_CHARS: usize = 100;
pub const HOOK_TITLE_CHARS: usize = 160;
pub const HOOK_REFS_CHARS: usize = 60;
pub const MAX_KNOWN_REPOS: usize = 5;

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

/// Status annotation for a superseded or retired item, shared by `query` and `list`.
pub(crate) fn status_marker(it: &crate::core::item::Item) -> Option<String> {
    match it.superseded_by.as_deref() {
        Some(by) if it.status == "superseded" && !by.is_empty() => {
            Some(format!("superseded_by={by}"))
        }
        _ if it.status == "superseded" => Some(format!("retired: {}", retired_label(&it.tags))),
        _ => None,
    }
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

/// (scope flags, filter flags) to carry into the footer commands; the filter flags start with
/// the `narrowing_flags`.
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
    let mut filters = narrowing_flags(&query_args(ns, None));
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

fn query_args<'a>(ns: &'a Namespace, bias: Option<&'a [String]>) -> QueryArgs<'a> {
    QueryArgs {
        // a limit past i64 saturates: the same slices, and the same ValueError when negative
        limit: ns.get_int("limit"),
        recent: ns.get_bool("recent"),
        tags: StrList::Str(ns.get_str("tag")),
        kinds: StrList::Str(ns.get_str("kind")),
        sources: StrList::Str(ns.get_str("source")),
        since: ns.get_str("since"),
        until: ns.get_str("until"),
        bias: bias.map_or(StrList::None, StrList::List),
        all_repos: ns.get_bool("all_repos"),
        repo: ns.get_str("repo"),
        include_history: ns.get_bool("include_history"),
        scopes: StrList::Str(ns.get_str("in")),
        extra_scopes: StrList::None,
        home: None,
        cwd: None,
    }
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
    let narrow = narrowing_flags(&query_args(ns, None));
    let other_flags = with(&["--all".to_string()], &filters);
    // a missing term that is the whole query would repeat the more: line's other repos command
    let whole = (!lk.other_repos.is_empty())
        .then(|| crate::core::fold::fold(Some(py::str::strip(&lk.query))));
    let mut lines = Vec::new();
    if !lk.missing.is_empty() {
        let parts: Vec<String> = lk
            .missing
            .iter()
            .map(|(term, n)| {
                // a stopword gets no suggested query
                let cmd = |flags: &[String]| {
                    if is_stopword(term) || (whole.as_ref() == Some(term) && flags == other_flags) {
                        String::new()
                    } else {
                        format!(" -> {}", query_cmd(term, flags))
                    }
                };
                if let Some(h) = lk.hidden.get(term) {
                    let rest = if h.filtered {
                        &filters[narrow.len()..]
                    } else {
                        &filters[..]
                    };
                    let mut flags = if h.other_repos {
                        with(&["--all".to_string()], rest)
                    } else {
                        with(&scope, rest)
                    };
                    if h.retired {
                        flags.push("--include-history".into());
                    }
                    format!("{term}: {} {}{}", h.n, h.by, cmd(&flags))
                } else if *n != 0 {
                    format!("{term}={n}{}", cmd(&with(&scope, &filters)))
                } else {
                    format!("{term}=0")
                }
            })
            .collect();
        lines.push(format!("missing: {}", parts.join("; ")));
    }
    let mut more = Vec::new();
    let full = lk.pool_cap.is_some_and(|c| c != 0);
    if lk.beyond_limit == 0 && full {
        // --limit took the whole pool: a wider limit shows no more, so the hint narrows the query
        more.push(format!(
            "matches may exist beyond the full pool of {} -> narrow the query: add a word, --kind, --tag or --since",
            lk.pool_cap.unwrap_or(0),
        ));
    }
    if lk.beyond_limit != 0 {
        let wider = limit.plus(lk.beyond_limit.min(10));
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
            query_cmd(&lk.query, &other_flags)
        ));
    }
    if !more.is_empty() {
        let line = format!("more: {}", more.join("; "));
        // no hit here: the other repos line comes first
        if lk.hits.is_empty() {
            lines.insert(0, line);
        } else {
            lines.push(line);
        }
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

/// One header line: id, rank, repo, kind, tag, as_of and tokens, then `why` when given, then
/// source, refs and status.
pub fn item_header(it: &Item, rank: i128, why: Option<String>) -> String {
    let mut parts = vec![
        format!("[{}]", it.id),
        format!("#{rank}"),
        format!("repo={}", it.repo),
        format!("kind={}", it.kind),
        format!("tag={}", it.tag),
        format!("as_of={}", it.as_of),
        format!("~{} tok", item_tokens(it)),
    ];
    parts.extend(why);
    if !it.source.is_empty() {
        parts.push(format!("source={}", it.source));
    }
    let refs = refs_without_source(it);
    if !refs.is_empty() {
        parts.push(format!("refs={}", compact_refs(&refs, HEADER_REFS_CHARS)));
    }
    if let Some(m) = status_marker(it) {
        parts.push(m);
    }
    parts.join(" ")
}

/// A cap per text so headers, texts and footer stay inside MAX_TOTAL_CHARS, each text keeping
/// at least min(len, MIN_TEXT_CHARS); short texts keep all, the long ones split the rest.
pub fn text_caps(headers: &[String], texts: &[String], footer: &[String]) -> Vec<usize> {
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
    caps.into_iter().map(|c| c.max(0) as usize).collect()
}

/// One block per hit; hits and footer stay inside
/// MAX_TOTAL_CHARS, each text keeping at least min(len, MIN_TEXT_CHARS).
pub fn format_query_text(lk: &Lookup, footer: &[String]) -> String {
    let headers: Vec<String> = lk
        .hits
        .iter()
        .enumerate()
        .map(|(i, s)| {
            item_header(
                &s.item,
                i as i128 + 1,
                Some(format!("why={}", why_text(&s.why))),
            )
        })
        .collect();
    let texts: Vec<String> = lk.hits.iter().map(|s| item_text(&s.item)).collect();
    let caps = text_caps(&headers, &texts, footer);
    let mut blocks: Vec<String> = Vec::new();
    for (((s, header), text), &cap) in lk.hits.iter().zip(&headers).zip(&texts).zip(&caps) {
        let snippet = match_snippet(text, &s.why.terms_matched, &s.why.ref_hits, cap);
        blocks.push(format!("{header}\n  {snippet}"));
    }
    blocks.extend(footer.iter().cloned());
    blocks.join("\n")
}

/// One brief line: id, rank, repo, kind, tag, date, tokens and status, then the clipped title.
pub fn brief_line(it: &Item, rank: i128) -> String {
    let title = clip(&py::str::split(&it.title).join(" "), HOOK_TITLE_CHARS);
    let marker = status_marker(it).map_or_else(String::new, |m| format!(" [{m}]"));
    format!(
        "[{}] #{rank} {} {} {} {} ~{} tok{marker}: {title}",
        it.id,
        it.repo,
        it.kind,
        it.tag,
        py::str::slice(&it.as_of, None, Some(10)),
        item_tokens(it)
    )
}

pub fn format_query_brief(lk: &Lookup, footer: &[String]) -> String {
    let mut lines: Vec<String> = lk
        .hits
        .iter()
        .enumerate()
        .map(|(i, s)| brief_line(&s.item, i as i128 + 1))
        .collect();
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

fn join_query_tokens(parts: &[String]) -> String {
    if parts.len() <= 1 {
        return parts.first().cloned().unwrap_or_default();
    }
    parts
        .iter()
        .map(|tok| {
            if let Some(rest) = tok.strip_prefix('-')
                && rest.chars().any(char::is_whitespace)
                && !rest.contains('"')
            {
                format!("-\"{rest}\"")
            } else {
                tok.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn run(ns: &Namespace, out: &mut Out) -> Result<i32, SmError> {
    let limit_text = LimitText::of(ns.get("limit"));
    let bias = bias_of(ns);
    let a = query_args(ns, bias.as_deref());
    let query = join_query_tokens(&ns.get_str_list("query"));
    let repo = py::str::strip(a.repo);
    if a.all_repos && !repo.is_empty() {
        out.eprint(&format!(
            "--all searches every repo, so --repo {repo} is not used"
        ))?;
    }
    let lookup = match run_lookup(&query, &a) {
        Err(e) if e.class == crate::config::UNKNOWN_SCOPE => {
            out.eprint(&e.msg)?;
            return Ok(2);
        }
        r => r?,
    };
    if !repo.is_empty() && !lookup.all_repos {
        let conn =
            crate::store::connect::connect(crate::config::get_db_path(None)?.as_str(), None)?;
        let mut names = repo_names(&conn)?;
        if !names.iter().any(|n| n == repo) {
            let typed = repo.to_lowercase();
            names.sort_by_key(|n| !n.to_lowercase().contains(&typed));
            let rest = names.len().saturating_sub(MAX_KNOWN_REPOS);
            names.truncate(MAX_KNOWN_REPOS);
            let more = if rest > 0 {
                format!(" (+{rest} more)")
            } else {
                String::new()
            };
            out.eprint(&format!(
                "no items in repo '{repo}'; known repos: {}{more}",
                names.join(", ")
            ))?;
        }
    }
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
            hidden: IndexMap::new(),
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
    fn clip_keeps_short_text_and_cuts_on_chars() {
        assert_eq!(clip("abc", 5), "abc");
        // exactly at the cap: no "..."
        assert_eq!(clip("abcde", 5), "abcde");
        // 17 chars, cap 10: the first 7 chars, then "..."
        assert_eq!(clip("Tiếng Việt có dấu", 10), "Tiếng V...");
        // cap 9: the first 6 chars "Tiếng " lose the trailing space
        assert_eq!(clip("Tiếng Việt có dấu", 9), "Tiếng...");
    }

    #[test]
    fn brief_line_cuts_the_title() {
        let mut h = hit("m_1", "b");
        h.item.title = format!("{} {}", "t".repeat(170), "end");
        let mut sup = hit("m_2", "b");
        sup.item.title = "Old note".into();
        sup.item.status = "superseded".into();
        sup.item.superseded_by = Some("m_9".into());
        let mut ret = hit("m_3", "b");
        ret.item.title = "Retired note".into();
        ret.item.status = "superseded".into();
        ret.item.tags = "retired:manual".into();
        let line = format_query_brief(&lookup(vec![h, sup, ret]), &["next: x".into()]);
        assert_eq!(
            line,
            format!(
                "[m_1] #1 r fact decided 2026-01-01 ~44 tok: {}...\n\
                 [m_2] #2 r fact decided 2026-01-01 ~3 tok [superseded_by=m_9]: Old note\n\
                 [m_3] #3 r fact decided 2026-01-01 ~4 tok [retired: manual]: Retired note\n\
                 next: x",
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
