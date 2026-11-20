//! The `retire` command.

use crate::error::SmError;
use crate::py::argparse::Namespace;
use crate::py::io::Out;
use crate::service::retire::run_retire;

pub fn run(ns: &Namespace, out: &mut Out) -> Result<i32, SmError> {
    let (id, reason) = run_retire(ns.get_str("id"), ns.get_str("reason"), None)?;
    out.print(&format!("{id} (retired: {reason})"))?;
    Ok(0)
}
