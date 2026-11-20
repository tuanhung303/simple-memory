//! The `mcp` command.

use crate::error::SmError;
use crate::py::argparse::Namespace;
use crate::py::io::Out;
use crate::service::mcp::serve;

pub fn run(_ns: &Namespace, out: &mut Out) -> Result<i32, SmError> {
    let stdin = std::io::stdin();
    let mut lock = stdin.lock();
    serve(&mut lock, out, None)
}
