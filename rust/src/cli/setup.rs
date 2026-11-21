//! The `init`, `config` and `hooks print` commands.

use crate::config::{config_toml, get_home, load_config, unknown_keys};
use crate::error::SmError;
use crate::py::argparse::Namespace;
use crate::py::io::Out;
use crate::py::json::{Opts, dumps};
use crate::service::setup::{HOOK_TARGETS, hook_binary, hook_snippet, run_init};

pub fn run(ns: &Namespace, out: &mut Out) -> Result<i32, SmError> {
    match ns.get_str("command") {
        "init" => {
            for line in run_init(None)? {
                out.print(&line)?;
            }
            out.print("next: sm hooks print claude-code (or agy), then merge it into the hook file it names")?;
            match load_config(None) {
                Ok(_) => out.print(&format!(
                    "next: if you have task logs or handoffs, set [upkeep] tasklog_roots and handoff_dirs in {}, then run sm import catch-up",
                    get_home(None)?.join("config.toml")
                ))?,
                Err(e) => out.eprint(&format!("warning: {}", super::one_line(&e.msg)))?,
            }
        }
        "config" => {
            let cfg = load_config(None)?;
            for line in unknown_keys(None)? {
                out.eprint(&line)?;
            }
            out.write(&config_toml(&cfg, false)?)?
        }
        _ => {
            let harness = ns.get_str("harness");
            let (binary, note) = hook_binary()?;
            if let Some(note) = note {
                out.eprint(&note)?;
            }
            out.print(&dumps(
                &hook_snippet(harness, &binary)?,
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

    /// The `sm` on PATH is this binary, so `hooks print` adds no note about it. PATH is the
    /// process's, so the check runs in a child test process.
    #[test]
    fn hooks_print_names_the_file_to_merge_into_and_refuses_an_unknown_harness() {
        if std::env::var_os("SM_HOOKS_PRINT_CHILD").is_none() {
            let d = tempfile::tempdir().unwrap();
            let exe = std::env::current_exe().unwrap();
            std::os::unix::fs::symlink(&exe, d.path().join("sm")).unwrap();
            let name = "cli::setup::tests::hooks_print_names_the_file_to_merge_into_and_refuses_an_unknown_harness";
            let st = std::process::Command::new(exe)
                .args([name, "--exact", "--quiet"])
                .env("PATH", d.path())
                .env("SM_HOOKS_PRINT_CHILD", "1")
                .output()
                .unwrap();
            let text = String::from_utf8_lossy(&st.stdout);
            assert!(st.status.success() && text.contains("1 passed"), "{text}");
            return;
        }
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
            "merge into the top level of a project's .mcp.json (or of another MCP client's config): the fragment already holds the \"mcpServers\" key\n"
        );
        let (code, _, stderr) = run(&["hooks", "print", "vim"]);
        assert_eq!(code, 2);
        assert_eq!(
            stderr.lines().last().unwrap(),
            "sm hooks print: error: argument {claude-code,agy,mcp}: invalid choice: 'vim' (choose from 'claude-code', 'agy', 'mcp')"
        );
    }
}
