//! The `backup` command.

use crate::config::load_config;
use crate::error::SmError;
use crate::py::argparse::{Namespace, Value};
use crate::py::int::{self, Int};
use crate::py::io::Out;
use crate::py::value::PyValue;
use crate::service::backup::run_backup;

/// `cfg[section][key]`; a missing key is Python's KeyError.
fn lookup<'a>(v: &'a PyValue, key: &str) -> Result<&'a PyValue, SmError> {
    match v {
        PyValue::Dict(pairs) => pairs
            .iter()
            .find(|(k, _)| matches!(k, PyValue::Str(s) if s == key))
            .map(|(_, v)| v)
            .ok_or_else(|| SmError::key(key)),
        _ => Err(SmError::type_("'NoneType' object is not subscriptable")),
    }
}

pub fn run(ns: &Namespace, out: &mut Out) -> Result<i32, SmError> {
    let keep = match ns.get("keep") {
        Value::None => int::from_value(lookup(lookup(&load_config(None)?, "backup")?, "keep")?)?,
        Value::Int(i) => Int::I64(*i),
        Value::BigInt(t) => Int::Big(t.clone()),
        other => panic!("attribute \"keep\" is not an int: {other:?}"),
    };
    let res = run_backup(&keep, None)?;
    out.print(&format!(
        "backup: {} quick_check={} items={}",
        res.path, res.quick_check, res.items
    ))?;
    for path in &res.deleted {
        out.print(&format!("deleted: {path}"))?;
    }
    Ok(0)
}
