//! The `export` and `import jsonl` commands.

use crate::error::SmError;
use crate::py::argparse::Namespace;
use crate::py::io::Out;
use crate::service::transfer::{run_export, run_import_jsonl};

pub fn run(ns: &Namespace, out: &mut Out) -> Result<i32, SmError> {
    if ns.get_str("command") == "export" {
        run_export(
            out,
            ns.get_str("repo"),
            ns.get_bool("all_repos"),
            ns.get_bool("include_history"),
            None,
            None,
        )?;
        return Ok(0);
    }
    let j = run_import_jsonl(ns.get_str("path"), ns.get_bool("dry_run"), None)?;
    let r = &j.res;
    let mode = if j.dry_run { "dry-run" } else { "applied" };
    out.print(&format!(
        "import jsonl ({mode}): rows={} added={} unchanged={} conflicts={} links_restored={} links_missing={} status_differs={} redacted={}",
        j.rows,
        r.added,
        r.unchanged,
        r.conflicts.len(),
        r.links_restored,
        r.links_missing.len(),
        r.status_differs.len(),
        r.redacted.len()
    ))?;
    for id in &r.conflicts {
        out.print(&format!("conflict: {id}"))?;
    }
    for d in &r.status_differs {
        out.print(&format!(
            "status_differs: {} store={} file={}",
            d.id, d.store, d.file
        ))?;
        if let Some(rep) = d.replacement.as_deref().filter(|s| !s.is_empty()) {
            out.print(&format!("  run: sm supersede {} {rep}", d.id))?;
        }
    }
    for id in &r.redacted {
        out.print(&format!("redacted: {id}"))?;
    }
    for link in &r.links_missing {
        out.print(&format!("link_missing: {link}"))?;
    }
    Ok(0)
}
