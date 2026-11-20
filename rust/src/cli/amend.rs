//! The `amend` command.

use crate::error::SmError;
use crate::py::argparse::Namespace;
use crate::py::io::Out;
use crate::service::amend::run_amend;
use crate::service::upkeep::put_overlaps;
use crate::store::items::AmendPatch;

pub fn run(ns: &Namespace, out: &mut Out) -> Result<i32, SmError> {
    let patch = AmendPatch {
        repo: ns.get_opt_str("repo"),
        kind: ns.get_opt_str("kind"),
        tag: ns.get_opt_str("tag"),
        evidence: ns.get_opt_str("evidence"),
        title: ns.get_opt_str("title"),
        body: ns.get_opt_str("body"),
        add_refs: ns.get_opt_str("add_refs"),
        remove_refs: ns.get_opt_str("remove_refs"),
        add_tags: ns.get_opt_str("add_tags"),
        remove_tags: ns.get_opt_str("remove_tags"),
        as_of: ns.get_opt_str("as_of"),
        ..Default::default()
    };
    let (new_id, body_changed) = run_amend(ns.get_str("id"), &patch, None)?;
    out.print(&new_id)?;
    if body_changed {
        for line in put_overlaps(&new_id, None, None)? {
            out.eprint(&line)?;
        }
    }
    Ok(0)
}
