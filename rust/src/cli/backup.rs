//! The `backup` command.

use crate::config::{get_db_path, load_config};
use crate::error::{Base, SmError};
use crate::py::argparse::{Namespace, Value};
use crate::py::int::{self, Int};
use crate::py::io::Out;
use crate::service::backup::run_backup;
use crate::service::import_::cfg_get;

pub fn run(ns: &Namespace, out: &mut Out) -> Result<i32, SmError> {
    let keep = match ns.get("keep") {
        Value::None => int::from_value(cfg_get(cfg_get(&load_config(None)?, "backup")?, "keep")?)?,
        Value::Int(i) => Int::I64(*i),
        Value::BigInt(t) => Int::Big(t.clone()),
        other => panic!("attribute \"keep\" is not an int: {other:?}"),
    };
    let res = match run_backup(&keep, None) {
        // run_backup's one RuntimeError: the copy failed quick_check, whose cell list is SQLite's.
        Err(e) if e.is(Base::Runtime) => {
            let db = get_db_path(None)?;
            out.eprint(&format!(
                "error: the backup copy of {db} does not pass quick_check, so it was not kept"
            ))?;
            out.eprint(&format!("{db} looks damaged; run sm doctor"))?;
            return Ok(1);
        }
        res => res?,
    };
    out.print(&format!(
        "backup: {} quick_check={} items={}",
        res.path, res.quick_check, res.items
    ))?;
    for path in &res.deleted {
        out.print(&format!("deleted: {path}"))?;
    }
    Ok(0)
}
