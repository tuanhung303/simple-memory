//! The `list` command with its brief, text, and JSON formatters.

use std::collections::BTreeSet;

use crate::cli::query::{
    HEADER_REFS_CHARS, HOOK_TITLE_CHARS, MAX_TOTAL_CHARS, MIN_TEXT_CHARS, compact_refs,
    refs_without_source, shlex_quote,
};
use crate::core::item::Item;
use crate::core::rank::match_snippet;
use crate::core::tags::retired_label;
use crate::error::SmError;
use crate::py;
use crate::py::argparse::{Namespace, Value};
use crate::py::io::Out;
use crate::py::value::PyValue;
use crate::service::list::{
    DEFAULT_LIST_LIMIT, ListArgs, ListPage, list_json, list_next_cmds, run_list,
};
use crate::service::query::{StrList, item_text, item_tokens, split_list};
use crate::service::usage::log_usage;
use crate::store::search::StatusFilter;

fn int_text(v: &Value) -> String {
    match v {
        Value::Int(n) => n.to_string(),
        Value::BigInt(s) => s.clone(),
        other => panic!("expected an int value, got {other:?}"),
    }
}

fn s(v: &str) -> PyValue {
    PyValue::Str(v.to_string())
}

fn status_marker(it: &Item) -> Option<String> {
    match it.superseded_by.as_deref() {
        Some(by) if it.status == "superseded" && !by.is_empty() => {
            Some(format!("superseded_by={by}"))
        }
        _ if it.status == "superseded" => Some(format!("retired: {}", retired_label(&it.tags))),
        _ => None,
    }
}

fn list_flags(ns: &Namespace, next_offset: i128, page: &ListPage) -> Vec<String> {
    let mut flags: Vec<String> = Vec::new();
    let repo = ns.get_str("repo");
    let paths = split_list(StrList::Str(ns.get_str("in")));
    if !paths.is_empty() {
        flags.extend(["--in".to_string(), paths.join(",")]);
    } else if ns.get_bool("all_repos") {
        flags.push("--all".to_string());
    } else if !repo.is_empty() {
        flags.extend(["--repo".to_string(), shlex_quote(repo)]);
    }
    for (flag, dest) in [("--kind", "kind"), ("--tag", "tag")] {
        let vals = split_list(StrList::Str(ns.get_str(dest)));
        if !vals.is_empty() {
            flags.extend([flag.to_string(), vals.join(",")]);
        }
    }
    let sources = split_list(StrList::Str(ns.get_str("source")));
    if !sources.is_empty() {
        flags.extend(["--source".to_string(), shlex_quote(&sources.join(","))]);
    }
    for (flag, dest) in [("--since", "since"), ("--until", "until")] {
        let val = ns.get_str(dest);
        if !val.is_empty() {
            flags.extend([flag.to_string(), shlex_quote(val)]);
        }
    }
    if let Some(st) = ns.get_opt_str("status") {
        flags.extend(["--status".to_string(), st.to_string()]);
    } else if ns.get_bool("include_history") {
        flags.push("--include-history".to_string());
    }
    let format = ns.get_str("format");
    if format != "brief" {
        flags.extend(["--format".to_string(), format.to_string()]);
    }
    if page.limit != DEFAULT_LIST_LIMIT {
        flags.extend(["--limit".to_string(), page.limit.to_string()]);
    }
    flags.extend(["--offset".to_string(), next_offset.to_string()]);
    flags
}

fn footer_lines(page: &ListPage, ns: &Namespace) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let shown_end = page.offset as i128 + page.items.len() as i128;
    if shown_end < page.total as i128 {
        let remaining = page.total as i128 - shown_end;
        let flags = list_flags(ns, shown_end, page);
        lines.push(format!(
            "more: {remaining} remaining (showing {}-{} of {}) -> sm list {}",
            page.offset as i128 + 1,
            shown_end,
            page.total,
            flags.join(" ")
        ));
    }
    let nxt = list_next_cmds(page);
    if !nxt.is_empty() {
        lines.push(format!("next: {}", nxt.join("; ")));
    }
    lines
}

pub fn format_list_brief(page: &ListPage, footer: &[String]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for (i, it) in page.items.iter().enumerate() {
        let rank = page.offset as i128 + i as i128 + 1;
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
        let marker = status_marker(it).map_or_else(String::new, |m| format!(" [{m}]"));
        lines.push(format!(
            "[{}] #{rank} {} {} {} {} ~{} tok{marker}: {title}",
            it.id,
            it.repo,
            it.kind,
            it.tag,
            py::str::slice(&it.as_of, None, Some(10)),
            item_tokens(it)
        ));
    }
    lines.extend(footer.iter().cloned());
    lines.join("\n")
}

pub fn format_list_text(page: &ListPage, footer: &[String]) -> String {
    let mut headers: Vec<String> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    for (i, it) in page.items.iter().enumerate() {
        let rank = page.offset as i128 + i as i128 + 1;
        let mut parts = vec![
            format!("[{}]", it.id),
            format!("#{rank}"),
            format!("repo={}", it.repo),
            format!("kind={}", it.kind),
            format!("tag={}", it.tag),
            format!("as_of={}", it.as_of),
            format!("~{} tok", item_tokens(it)),
        ];
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
    for ((header, text), &cap) in headers.iter().zip(&texts).zip(&caps) {
        let snippet = match_snippet(text, &[], &[], cap.max(0) as usize);
        blocks.push(format!("{header}\n  {snippet}"));
    }
    blocks.extend(footer.iter().cloned());
    blocks.join("\n")
}

fn print_list(page: &ListPage, ns: &Namespace, out: &mut Out) -> Result<(), SmError> {
    let format = ns.get_str("format");
    if format == "json" {
        return out.print(&py::json::dumps_value(
            &list_json(page),
            &py::json::Opts::unicode(),
        ));
    }
    let footer = footer_lines(page, ns);
    if page.items.is_empty() && footer.is_empty() {
        return Ok(());
    }
    let text = if format == "text" {
        format_list_text(page, &footer)
    } else {
        format_list_brief(page, &footer)
    };
    out.print(&text)
}

pub fn run(ns: &Namespace, out: &mut Out) -> Result<i32, SmError> {
    let status = match ns.get_opt_str("status") {
        Some("active") => StatusFilter::Active,
        Some("superseded") => StatusFilter::Superseded,
        Some("retired") => StatusFilter::Retired,
        Some("all") => StatusFilter::All,
        Some(other) => return Err(SmError::value(format!("Invalid status '{other}'"))),
        None if ns.get_bool("include_history") => StatusFilter::All,
        None => StatusFilter::Active,
    };
    let a = ListArgs {
        limit: ns.get_int("limit"),
        offset: ns.get_int("offset"),
        repo: ns.get_str("repo"),
        all_repos: ns.get_bool("all_repos"),
        scopes: StrList::Str(ns.get_str("in")),
        kinds: StrList::Str(ns.get_str("kind")),
        tags: StrList::Str(ns.get_str("tag")),
        sources: StrList::Str(ns.get_str("source")),
        since: ns.get_str("since"),
        until: ns.get_str("until"),
        status,
        ..Default::default()
    };
    let page = match run_list(&a) {
        Err(e) if e.class == crate::config::UNKNOWN_SCOPE => {
            out.eprint(&e.msg)?;
            return Ok(2);
        }
        r => r?,
    };
    print_list(&page, ns, out)?;
    let ids: Vec<String> = page.items.iter().map(|it| it.id.clone()).collect();
    let flags = PyValue::Dict(vec![
        (s("kind"), s(ns.get_str("kind"))),
        (s("tag"), s(ns.get_str("tag"))),
        (s("source"), s(ns.get_str("source"))),
        (s("since"), s(ns.get_str("since"))),
        (s("until"), s(ns.get_str("until"))),
        (s("status"), s(page.status.as_str())),
        (s("format"), s(ns.get_str("format"))),
        (s("limit"), PyValue::Int(int_text(ns.get("limit")))),
        (s("offset"), PyValue::Int(int_text(ns.get("offset")))),
        (s("all"), PyValue::Bool(ns.get_bool("all_repos"))),
    ]);
    log_usage("list", &ids, page.repo.as_deref(), Some(&flags), None)?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_item(id: &str, title: &str, body: &str) -> Item {
        let mut it = Item::bare(id, "anvil", "fact", title, body, "decided");
        it.as_of = "2026-06-15T10:00:00+00:00".into();
        it
    }

    fn parse(argv: &[&str]) -> Namespace {
        let args: Vec<String> = argv.iter().map(|s| (*s).to_string()).collect();
        crate::cli::args::build_parser()
            .parse_args(&args, &mut Out::capture())
            .unwrap()
    }

    #[test]
    fn brief_and_text_format_status_markers_and_i64_max_offset_safely() {
        let active = sample_item("m_1", "Active title", "Active body");
        let mut sup = sample_item("m_2", "Old title", "Old body");
        sup.status = "superseded".into();
        sup.superseded_by = Some("m_1".into());
        let mut ret = sample_item("m_3", "Retired title", "Retired body");
        ret.status = "superseded".into();
        ret.tags = "retired:manual".into();
        ret.source = "task:T-1".into();
        ret.refs = "T-1 R-9".into();

        let page = ListPage {
            items: vec![active, sup, ret],
            total: 5,
            limit: 3,
            offset: 0,
            status: StatusFilter::All,
            repo: Some("anvil".into()),
            all_repos: false,
            scopes: vec![],
        };
        let ns = parse(&[
            "list",
            "--repo",
            "my repo",
            "--source",
            " task: , agent:a b ",
            "--since",
            "2026-06-01 09:00:00",
            "--until",
            "2026-06-30",
            "--include-history",
            "--limit",
            "3",
        ]);
        let footer = footer_lines(&page, &ns);
        assert_eq!(
            footer,
            vec![
                "more: 2 remaining (showing 1-3 of 5) -> sm list --repo 'my repo' --source 'task:,agent:a b' --since '2026-06-01 09:00:00' --until 2026-06-30 --include-history --limit 3 --offset 3",
                "next: sm show m_1 m_2 m_3",
            ]
        );

        let brief = format_list_brief(&page, &footer);
        let lines: Vec<&str> = brief.lines().collect();
        assert_eq!(
            lines[0],
            "[m_1] #1 anvil fact decided 2026-06-15 ~6 tok: Active title"
        );
        assert_eq!(
            lines[1],
            "[m_2] #2 anvil fact decided 2026-06-15 ~5 tok [superseded_by=m_1]: Old title"
        );
        assert_eq!(
            lines[2],
            "[m_3] #3 anvil fact decided 2026-06-15 ~7 tok [retired: manual]: Retired title"
        );

        let text = format_list_text(&page, &[]);
        assert!(text.contains("source=task:T-1 refs=R-9 retired: manual"));
        assert!(text.contains("superseded_by=m_1"));

        // i64::MAX offset does not overflow rank or footer comparisons.
        let huge = ListPage {
            items: vec![sample_item("m_max", "Edge", "Edge body")],
            total: i64::MAX,
            limit: 1,
            offset: i64::MAX,
            status: StatusFilter::Active,
            repo: None,
            all_repos: true,
            scopes: vec![],
        };
        let ns_all = parse(&["list", "--all", "--offset", "99999999999999999999"]);
        assert_eq!(footer_lines(&huge, &ns_all), vec!["next: sm show m_max"]);
        assert!(format_list_brief(&huge, &[]).starts_with(&format!("[m_max] #{} ", 1u64 << 63)));
    }
}
