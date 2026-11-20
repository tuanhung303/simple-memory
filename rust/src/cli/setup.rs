//! The `init`, `config` and `hooks print` commands.

use crate::config::{config_toml, load_config};
use crate::error::SmError;
use crate::py::argparse::Namespace;
use crate::py::io::Out;
use crate::py::json::{Opts, dumps};
use crate::service::setup::{HOOK_TARGETS, hook_snippet, run_init};

pub fn run(ns: &Namespace, out: &mut Out) -> Result<i32, SmError> {
    match ns.get_str("command") {
        "init" => {
            for line in run_init(None)? {
                out.print(&line)?;
            }
            out.print("next: sm hooks print claude-code (or agy), then merge it into the hook file it names")?;
        }
        "config" => out.write(&config_toml(&load_config(None)?, false)?)?,
        _ => {
            let harness = ns.get_str("harness");
            out.print(&dumps(
                &hook_snippet(harness, None)?,
                &Opts::default().with_indent(2),
            )?)?;
            let target = HOOK_TARGETS
                .iter()
                .find(|(k, _)| *k == harness)
                .map(|(_, t)| *t);
            let Some(target) = target else {
                return Err(SmError::key(harness));
            };
            out.eprint(&format!("merge into {target}"))?;
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use crate::py::io::Out;

    fn run(argv: &[&str]) -> (i32, String, String) {
        let mut out = Out::capture();
        let code = crate::cli::run(
            &argv.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            &mut out,
        );
        out.finish(code);
        let (o, e) = out.captured();
        (code, o, e)
    }

    #[test]
    fn hooks_print_names_the_file_to_merge_into_and_refuses_an_unknown_harness() {
        let (code, stdout, stderr) = run(&["hooks", "print", "mcp"]);
        assert_eq!(code, 0);
        assert!(
            stdout.starts_with(
                "{\n  \"mcpServers\": {\n    \"simple-memory\": {\n      \"type\": \"stdio\",\n"
            ),
            "{stdout}"
        );
        assert!(
            stdout.ends_with("      \"args\": [\n        \"mcp\"\n      ]\n    }\n  }\n}\n"),
            "{stdout}"
        );
        assert_eq!(
            stderr,
            "merge into the \"mcpServers\" object of a project's .mcp.json, or of another MCP client's config\n"
        );
        let (code, _, stderr) = run(&["hooks", "print", "vim"]);
        assert_eq!(code, 2);
        assert_eq!(
            stderr.lines().last().unwrap(),
            "sm hooks print: error: argument harness: invalid choice: 'vim' (choose from 'claude-code', 'agy', 'mcp')"
        );
    }
}
