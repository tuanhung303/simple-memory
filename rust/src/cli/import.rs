//! The `import tasklog|handoff|proposal|automemory|sessions|catch-up` commands and their
//! summary output.

use crate::config::load_config;
use crate::error::SmError;
use crate::py::argparse::Namespace;
use crate::py::io::Out;
use crate::service::capture::capture_sessions;
use crate::service::catchup::run_catchup;
use crate::service::doctor::repo_folder_roots;
use crate::service::import_::{ImportResult, run_automemory_import, run_import};

use super::upkeep::manual_run_lock;

fn print_import_summary(res: &ImportResult, out: &mut Out) -> Result<(), SmError> {
    let mode = if res.dry_run { "dry-run" } else { "applied" };
    out.print(&format!(
        "import {} ({mode}): total={}",
        res.kind, res.total
    ))?;
    let join = |m: &indexmap::IndexMap<String, i64>| {
        let s: Vec<String> = m.iter().map(|(k, v)| format!("{k}={v}")).collect();
        if s.is_empty() {
            "none".to_string()
        } else {
            s.join(", ")
        }
    };
    out.print(&format!("by_repo: {}", join(&res.by_repo)))?;
    out.print(&format!("by_tag: {}", join(&res.by_tag)))?;
    for u in &res.unresolved_supersedes {
        out.print(&format!("unresolved superseded_by: {u}"))?;
    }
    for f in &res.failed_rows {
        out.print(&format!("failed_row: {f}"))?;
    }
    out.print(&format!(
        "total={} inserted={} deduped={} superseded={} rejected={} supersede_missing={} failed_rows={}",
        res.total,
        res.inserted,
        res.deduped,
        res.superseded,
        res.rejected,
        res.supersede_missing,
        res.failed_rows.len() + res.skipped.len()
    ))
}

fn automemory(ns: &Namespace, out: &mut Out) -> Result<i32, SmError> {
    let dry_run = ns.get_bool("dry_run");
    let Some(lock) = manual_run_lock(!dry_run, out)? else {
        return Ok(1);
    };
    let am = run_automemory_import(ns.get_opt_str("root"), dry_run, None)?;
    drop(lock);
    let mode = if am.dry_run { "dry-run" } else { "applied" };
    out.print(&format!(
        "import automemory ({mode}): files={} added={} updated={} retired={} unchanged={} unreadable={} unmapped={} yaml_errors={}",
        am.files,
        am.added,
        am.updated,
        am.retired,
        am.unchanged,
        am.unreadable.len(),
        am.unmapped.len(),
        am.yaml_errors
    ))?;
    for path in &am.unreadable {
        out.print(&format!("unreadable: {path}"))?;
    }
    for path in &am.unmapped {
        out.print(&format!("unmapped: {path}"))?;
    }
    Ok(0)
}

fn sessions(ns: &Namespace, out: &mut Out) -> Result<i32, SmError> {
    let dry_run = ns.get_bool("dry_run");
    let Some(lock) = manual_run_lock(!dry_run, out)? else {
        return Ok(1);
    };
    let res = capture_sessions(
        None,
        &load_config(None)?,
        dry_run,
        ns.get_opt_float("since_days"),
        ns.get_opt_int("limit"),
        None,
    )?;
    drop(lock);
    let mode = if dry_run { "dry-run" } else { "applied" };
    let counts: Vec<String> = res.counts.iter().map(|(k, v)| format!("{k}={v}")).collect();
    out.print(&format!("import sessions ({mode}): {}", counts.join(" ")))?;
    for (repo, n) in &res.by_repo {
        out.print(&format!("  repo {repo}: {n}"))?;
    }
    Ok(0)
}

fn catch_up(ns: &Namespace, out: &mut Out) -> Result<i32, SmError> {
    let quiet = ns.get_bool("quiet");
    if !quiet {
        // a config that does not parse: run_catchup gives its own error (or the lock skip)
        for line in crate::config::unknown_keys(None).unwrap_or_default() {
            out.eprint(&line)?;
        }
        let cfg = load_config(None).ok();
        for line in cfg
            .map(|c| repo_folder_roots(&c))
            .and_then(Result::ok)
            .unwrap_or_default()
        {
            out.eprint(&line)?;
        }
    }
    let Some(row) = run_catchup(None)? else {
        if !quiet {
            out.eprint("catch-up: another run is in progress; skipped")?;
        }
        return Ok(0);
    };
    if !quiet {
        for (name, part) in &row.parts {
            let counts: Vec<String> = part
                .counts
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            let state = match &part.error {
                None => "ok".to_string(),
                Some(e) => format!("error: {e}"),
            };
            let line = format!("{name}: {state} {}", counts.join(" "));
            out.print(line.trim_end_matches(' '))?;
        }
        let failed: i64 = row
            .parts
            .values()
            .filter_map(|p| p.counts.get("failed_rows"))
            .sum();
        match failed {
            0 => {}
            1 => out.eprint(
                "catch-up: 1 task log or handoff not imported; sm doctor --imports lists it",
            )?,
            n => out.eprint(&format!(
                "catch-up: {n} task logs or handoffs not imported; sm doctor --imports lists them"
            ))?,
        }
    }
    Ok(if row.ok { 0 } else { 1 })
}

pub fn run(ns: &Namespace, out: &mut Out) -> Result<i32, SmError> {
    match ns.get_str("source_kind") {
        "automemory" => automemory(ns, out),
        "sessions" => sessions(ns, out),
        "catch-up" => catch_up(ns, out),
        kind => {
            let path = ns.get_str("path").to_string();
            let res = run_import(
                kind,
                &path,
                ns.get_bool("dry_run"),
                ns.get_bool("apply"),
                None,
            )?;
            for s in &res.skipped {
                out.eprint(&format!("skipped: {s}"))?;
            }
            print_import_summary(&res, out)?;
            Ok(match res.skipped.len() {
                0 => 0,
                1 => {
                    out.eprint("1 file not imported; fix it and run the import again")?;
                    1
                }
                n => {
                    out.eprint(&format!(
                        "{n} files not imported; fix them and run the import again"
                    ))?;
                    1
                }
            })
        }
    }
}
