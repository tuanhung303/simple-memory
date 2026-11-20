//! The `stats` command and its text output.

use crate::error::SmError;
use crate::py::argparse::Namespace;
use crate::py::float::{format_fixed, format_g, repr};
use crate::py::io::Out;
use crate::service::stats::{Stats, run_stats};

fn pct(part: i64, whole: i64) -> String {
    if whole == 0 {
        "n/a".into()
    } else {
        format!("{}%", format_fixed(100.0 * part as f64 / whole as f64, 1))
    }
}

fn kv<K: std::fmt::Display>(m: impl IntoIterator<Item = (K, i64)>) -> String {
    m.into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn print_stats(st: &Stats, out: &mut Out) -> Result<(), SmError> {
    let store = &st.store;
    let breakdown = store.status_breakdown();
    let status = kv(breakdown.iter().map(|(k, v)| (k, *v)));
    let total: i64 = breakdown.values().sum();
    let line = format!("store: items={total} {status}");
    out.print(line.trim_end_matches(crate::py::str::isspace))?;
    let active = store.active;
    out.print(&format!(
        "  repos (active, top {} of {}): {}",
        store.repos.len(),
        store.repo_count,
        kv(store.repos.iter().map(|(k, v)| (k, *v)))
    ))?;
    out.print(&format!(
        "  kinds (active): {}",
        kv(store.kinds.iter().map(|(k, v)| (k, *v)))
    ))?;
    out.print(&format!(
        "  tags (active): {}",
        kv(store.tags.iter().map(|(k, v)| (k, *v)))
    ))?;
    out.print(&format!(
        "  unassigned={} ({}) legacy={} ({}) of active",
        store.unassigned,
        pct(store.unassigned, active),
        store.legacy,
        pct(store.legacy, active)
    ))?;
    let by_repo = kv(st.stale_repos.iter().map(|(k, v)| (k, *v)));
    let tags = crate::service::upkeep::STALE_TAGS.join(" or ");
    out.print(&format!(
        "  stale ({tags}, older than {} days): {}{}",
        crate::service::upkeep::STALE_DAYS,
        st.stale_total,
        if by_repo.is_empty() {
            String::new()
        } else {
            format!(" ({by_repo})")
        }
    ))?;
    if let Some((paths, outside)) = &st.scopes {
        out.print("scopes:")?;
        for (p, n) in paths {
            let indent = "  ".repeat(p.matches('/').count() + 1);
            out.print(&format!("{indent}{p} active={n}"))?;
        }
        out.print(&format!("  outside: active={outside}"))?;
    }
    let rec = &st.recall;
    let days = format_g(st.days);
    out.print(&format!(
        "recall (last {days} days, since {}): hook_runs={} with_injection={} injections={} injected_ids={}",
        st.since, rec.hook_runs, rec.with_injection, rec.injections, rec.injected_ids
    ))?;
    if !rec.failed_runs.is_empty() {
        let n: i64 = rec.failed_runs.values().sum();
        out.print(&format!(
            "  failed runs (nothing injected): {n} ({})",
            kv(rec.failed_runs.iter().map(|(k, v)| (k, *v)))
        ))?;
    }
    if !rec.fallback_runs.is_empty() {
        let n: i64 = rec.fallback_runs.values().sum();
        out.print(&format!(
            "  fallback runs (local keywords): {n} ({})",
            kv(rec.fallback_runs.iter().map(|(k, v)| (k, *v)))
        ))?;
        let paths: Vec<String> = rec
            .by_path
            .iter()
            .map(|(p, b)| {
                format!(
                    "{p} with_injection={} opened={} of {} ({})",
                    b.with_injection,
                    b.opened_ids,
                    b.tracked_ids,
                    pct(b.opened_ids, b.tracked_ids)
                )
            })
            .collect();
        out.print(&format!("  by path: {}", paths.join("; ")))?;
    }
    match &rec.tracked_since {
        None => out.print("  opened: no usage.log rows yet")?,
        Some(ts) => out.print(&format!(
            "  opened with sm show: {} of {} ids injected since usage.log began {ts} ({}); usage_rows={}",
            rec.opened_ids,
            rec.tracked_ids,
            pct(rec.opened_ids, rec.tracked_ids),
            rec.usage_rows
        ))?,
    }
    let bad: Vec<String> = [
        ("hook.log", rec.bad_rows.0),
        (crate::service::usage::USAGE_LOG, rec.bad_rows.1),
    ]
    .iter()
    .filter(|(_, v)| *v != 0)
    .map(|(k, v)| format!("{k}={v}"))
    .collect();
    if !bad.is_empty() {
        out.print(&format!("  unreadable rows: {}", bad.join(" ")))?;
    }
    out.print(&format!(
        "never opened, injected 5+ times since usage.log began: {}",
        st.never_opened.len()
    ))?;
    let none = |x: &Option<String>| x.clone().unwrap_or_else(|| "None".into());
    for (id, n, repo, kind, status) in &st.never_opened {
        out.print(&format!(
            "  [{id}] injections={n} repo={} kind={} status={status}",
            none(repo),
            none(kind)
        ))?;
    }
    let cu = &st.catch_up;
    let last = match &cu.last {
        Some((ts, ok)) => format!("{ts} {}", if *ok { "ok" } else { "failed" }),
        None => "none".into(),
    };
    out.print(&format!(
        "catch-up (last {days} days): runs={} failed={} last={last}",
        cu.runs, cu.failed
    ))?;
    if !cu.part_failures.is_empty() {
        out.print(&format!(
            "  part failures: {}",
            kv(cu.part_failures.iter().map(|(k, v)| (k, *v)))
        ))?;
    }
    if cu.bad_rows != 0 {
        out.print(&format!("  unreadable rows: catchup.log={}", cu.bad_rows))?;
    }
    let age = match st.backups.1 {
        None => "none".into(),
        Some(a) => format!("{}h", repr(a)),
    };
    out.print(&format!("backups: count={} newest_age={age}", st.backups.0))
}

pub fn run(ns: &Namespace, out: &mut Out) -> Result<i32, SmError> {
    let st = run_stats(ns.get_float("days"), None, None)?;
    if ns.get_str("format") == "json" {
        out.print(&crate::py::json::dumps(
            &st.value(),
            &crate::py::json::Opts::unicode(),
        )?)?;
    } else {
        print_stats(&st, out)?;
    }
    Ok(0)
}
