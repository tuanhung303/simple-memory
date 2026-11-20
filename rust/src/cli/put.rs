//! The `put` command.

use crate::core::refs::RefsArg;
use crate::error::SmError;
use crate::py::argparse::Namespace;
use crate::py::io::Out;
use crate::py::str::strip;
use crate::service::put::{PutArgs, run_put};
use crate::service::upkeep::put_overlaps;

pub fn run(ns: &Namespace, out: &mut Out) -> Result<i32, SmError> {
    let body_text = ns.get_str_list("body").join(" ");
    let (item_id, created) = run_put(
        strip(&body_text),
        &PutArgs {
            tag: ns.get_str("tag"),
            kind: ns.get_str("kind"),
            title: ns.get_str("title"),
            evidence: ns.get_str("evidence"),
            refs: ns.get_str("refs"),
            tags: ns.get_str("tags"),
            source: ns.get_str("source"),
            repo: ns.get_str("repo"),
            as_of: ns.get_str("as_of"),
            supersedes: RefsArg::Str(ns.get_str("supersedes")),
            home: None,
            cwd: None,
        },
    )?;
    out.print(&item_id)?;
    if created {
        for line in put_overlaps(&item_id, None, None)? {
            out.eprint(&line)?;
        }
    }
    Ok(0)
}
