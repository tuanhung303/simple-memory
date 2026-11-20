//! The `show` command and its text and JSON output.

use crate::core::item::Item;
use crate::core::tags::retired_label;
use crate::error::SmError;
use crate::py;
use crate::py::argparse::Namespace;
use crate::py::io::Out;
use crate::service::mcp::item_json;
use crate::service::show::run_show_many;
use crate::service::usage::log_usage;

pub fn print_item(it: &Item, out: &mut Out) -> Result<(), SmError> {
    for (k, v) in [
        ("id", &it.id),
        ("repo", &it.repo),
        ("kind", &it.kind),
        ("title", &it.title),
        ("tag", &it.tag),
        ("evidence", &it.evidence),
        ("as_of", &it.as_of),
        ("source", &it.source),
        ("refs", &it.refs),
        ("tags", &it.tags),
        ("status", &it.status),
    ] {
        out.print(&format!("{k}: {v}"))?;
    }
    match it.superseded_by.as_deref() {
        Some(s) if !s.is_empty() => out.print(&format!("superseded_by: {s}"))?,
        _ if it.status == "superseded" => {
            out.print(&format!("retired: {}", retired_label(&it.tags)))?
        }
        _ => {}
    }
    out.print(&format!("\n{}", it.body))
}

pub fn run(ns: &Namespace, out: &mut Out) -> Result<i32, SmError> {
    let (found, missing) = run_show_many(&ns.get_str_list("ids"), None)?;
    if ns.get_str("format") == "json" {
        let arr = serde_json::Value::Array(found.iter().map(item_json).collect());
        out.print(&py::json::dumps_value(&arr, &py::json::Opts::unicode()))?;
    } else {
        for (n, it) in found.iter().enumerate() {
            if n > 0 {
                out.print("")?;
            }
            print_item(it, out)?;
        }
    }
    for id in &missing {
        out.eprint(&format!("not found: {id}"))?;
    }
    let ids: Vec<String> = found.iter().map(|it| it.id.clone()).collect();
    log_usage("show", &ids, None, None, None)?;
    Ok(if missing.is_empty() { 0 } else { 1 })
}
