//! The `supersede` command.

use crate::error::SmError;
use crate::py::argparse::Namespace;
use crate::py::io::Out;
use crate::service::supersede::run_supersede;

pub fn run(ns: &Namespace, out: &mut Out) -> Result<i32, SmError> {
    let (old_id, new_id) = run_supersede(ns.get_str("old_id"), ns.get_str("new_id"), None)?;
    out.print(&format!("{old_id} -> {new_id}"))?;
    Ok(0)
}
