//! The `doctor` command.

use crate::error::SmError;
use crate::py::argparse::Namespace;
use crate::py::io::Out;
use crate::py::value::PyValue;
use crate::service::doctor::{repo_folder_roots, run_doctor};
use crate::sources::{dict_get, get_attr, truthy};

pub fn run(ns: &Namespace, out: &mut Out) -> Result<i32, SmError> {
    let (backfill, dry_run, imports) = (
        ns.get_bool("backfill_refs"),
        ns.get_bool("dry_run"),
        ns.get_bool("imports"),
    );
    let doc = run_doctor(ns.get_bool("rebuild_fts"), backfill, dry_run, imports, None)?;
    for line in crate::config::unknown_keys(None)? {
        out.eprint(&line)?;
    }
    if imports {
        for line in repo_folder_roots(&crate::config::load_config(None)?)? {
            out.eprint(&line)?;
        }
    }
    let c = &doc.check;
    out.print(&format!(
        "integrity={} rebuilt_fts={} total={} active={} superseded={}",
        c.integrity,
        if c.rebuilt_fts { "True" } else { "False" },
        c.total,
        c.active,
        c.superseded
    ))?;
    if backfill {
        let mode = if dry_run { "dry-run" } else { "applied" };
        let r = |k: &str| doc.refs.get(k).copied().unwrap_or(0);
        out.print(&format!(
            "backfill_refs ({mode}): empty_before={} changed={} empty_after={}",
            r("refs_empty_before"),
            r("refs_changed"),
            r("refs_empty_after")
        ))?;
    }
    if let Some((lag, am, row)) = &doc.lag {
        out.print(&format!(
            "imports: tasklog_files={} handoff_files={} not_stored={} unreadable={} stored_tasks_without_file={}",
            lag.tasklog_files,
            lag.handoff_files,
            lag.not_stored.len(),
            lag.unreadable.len(),
            lag.stored_tasks_without_file.len()
        ))?;
        for path in &lag.not_stored {
            out.print(&format!("not_stored: {path}"))?;
        }
        for line in &lag.unreadable {
            out.print(&format!("unreadable: {line}"))?;
        }
        for source in &lag.stored_tasks_without_file {
            out.print(&format!("stored_without_file: {source}"))?;
        }
        match am {
            None => out.print("automemory: off ([automemory] enabled = false)")?,
            Some(am) => {
                out.print(&format!(
                    "automemory: files={} not_stored={} stored_without_file={} unmapped={} unreadable={}",
                    am.automemory_files,
                    am.automemory_not_stored.len(),
                    am.automemory_stored_without_file.len(),
                    am.automemory_unmapped.len(),
                    am.automemory_unreadable.len()
                ))?;
                for path in &am.automemory_not_stored {
                    out.print(&format!("not_stored: {path}"))?;
                }
                for source in &am.automemory_stored_without_file {
                    out.print(&format!("stored_without_file: {source}"))?;
                }
                for path in &am.automemory_unmapped {
                    out.print(&format!("unmapped: {path}"))?;
                }
                for path in &am.automemory_unreadable {
                    out.print(&format!("unreadable: {path}"))?;
                }
            }
        }
        let shown = match row.as_ref().filter(|r| truthy(r)) {
            Some(r) => crate::py::json::dumps(r, &crate::py::json::Opts::unicode())?,
            None => "none".into(),
        };
        out.print(&format!("last_catchup: {shown}"))?;
        // `(row or {}).get("parts", {}).items()`, then `part.get("error")` on each part
        let empty = PyValue::Dict(Vec::new());
        let r = row.as_ref().filter(|r| truthy(r)).unwrap_or(&empty);
        let parts = get_attr(r, "parts")?.unwrap_or(&empty);
        let PyValue::Dict(items) = parts else {
            return Err(SmError::from_python(
                "AttributeError",
                format!(
                    "'{}' object has no attribute 'items'",
                    crate::py::value::type_name(parts)
                ),
            ));
        };
        for (name, part) in items {
            if let Some(e) = get_attr(part, "error")?.filter(|e| truthy(e)) {
                let err = match part {
                    PyValue::Dict(d) => dict_get(d, "error").unwrap_or(e),
                    _ => e,
                };
                out.print(&format!(
                    "catchup_error: {}: {}",
                    name.py_str(),
                    err.py_str()
                ))?;
            }
        }
    }
    Ok(if doc.check.integrity == "ok" { 0 } else { 1 })
}
