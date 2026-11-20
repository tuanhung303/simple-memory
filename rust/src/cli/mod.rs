//! The `sm` entry point: parse, dispatch, and the error paths.
//! The parser tree is in `args`; each command family's handler is in its own file.

pub mod amend;
pub mod args;
pub mod backup;
pub mod doctor;
pub mod hook;
pub mod import;
pub mod list;
pub mod mcp;
pub mod put;
pub mod query;
pub mod related;
pub mod retire;
pub mod setup;
pub mod show;
pub mod stats;
pub mod supersede;
pub mod transfer;
pub mod update;
pub mod upkeep;

use std::ffi::OsString;

use crate::error::{Base, SmError};
use crate::py::argparse::{Exit, Namespace};
use crate::py::io::{self, Out};

/// The process entry: `argv` holds the program name first, as `std::env::args_os()` gives it.
/// Returns the exit status, after the exit-time flush of stdout and stderr.
pub fn main(argv: Vec<OsString>) -> i32 {
    #[cfg(feature = "parity")]
    if argv.get(1).is_some_and(|a| a == "__parity") {
        return crate::parity::serve();
    }
    let args: Vec<String> = argv.iter().skip(1).map(|a| io::decode_argv(a)).collect();
    let mut out = Out::new();
    let code = run(&args, &mut out);
    out.finish(code)
}

/// `main(argv)` without the exit-time flush: parse, run the handler, map its error.
pub fn run(args: &[String], out: &mut Out) -> i32 {
    let ns = match args::build_parser().parse_args(args, out) {
        Ok(ns) => ns,
        Err(Exit(code)) => return code,
    };
    match dispatch(&ns, out) {
        Ok(code) => code,
        Err(e) => report_error(&e, out),
    }
}

/// The caught bases print `error: <one line>`; else the uncaught path: the
/// traceback header, then `<Class>: <msg>` as the last stderr line. Both exit 1.
fn report_error(e: &SmError, out: &mut Out) -> i32 {
    if caught(e) {
        let _ = out.eprint(&format!("error: {}", one_line(&e.msg)));
        return 1;
    }
    let name = qualified_name(&e.class);
    let last = if e.msg.is_empty() {
        name.to_string()
    } else {
        format!("{name}: {}", e.msg)
    };
    let _ = out.ewrite(&format!("Traceback (most recent call last):\n{last}\n"));
    1
}

/// The handler for the parsed command.
fn dispatch(ns: &Namespace, out: &mut Out) -> Result<i32, SmError> {
    match ns.get_str("command") {
        "put" => put::run(ns, out),
        "query" | "search" => query::run(ns, out),
        "show" => show::run(ns, out),
        "related" => related::run(ns, out),
        "supersede" => supersede::run(ns, out),
        "retire" => retire::run(ns, out),
        "amend" => amend::run(ns, out),
        "list" => list::run(ns, out),
        "import" if ns.get_str("source_kind") == "jsonl" => transfer::run(ns, out),
        "import" => import::run(ns, out),
        "export" => transfer::run(ns, out),
        "backup" => backup::run(ns, out),
        "upkeep" => upkeep::run(ns, out),
        "hook" => hook::run(ns, out),
        "stats" => stats::run(ns, out),
        "mcp" => mcp::run(ns, out),
        "init" | "config" | "hooks" => setup::run(ns, out),
        "doctor" => doctor::run(ns, out),
        "update" => update::run(ns, out),
        _ => Ok(0),
    }
}

/// The caught bases: ValueError, KeyError, FileNotFoundError, TypeError, sqlite3.Error, OSError.
fn caught(e: &SmError) -> bool {
    matches!(
        e.base,
        Base::Value | Base::Key | Base::Type | Base::Os | Base::Sqlite
    )
}

/// `" ".join(str(exc).split())`.
fn one_line(msg: &str) -> String {
    msg.split(crate::py::str::isspace)
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The name the traceback's last line gives: `module.Class` outside builtins
/// (`traceback.format_exception_only`).
fn qualified_name(class: &str) -> &str {
    match class {
        "SubprocessError" => "subprocess.SubprocessError",
        "TimeoutExpired" => "subprocess.TimeoutExpired",
        "CalledProcessError" => "subprocess.CalledProcessError",
        "YAMLError" => "yaml.error.YAMLError",
        "MarkedYAMLError" => "yaml.error.MarkedYAMLError",
        "ScannerError" => "yaml.scanner.ScannerError",
        "ParserError" => "yaml.parser.ParserError",
        "ComposerError" => "yaml.composer.ComposerError",
        "ConstructorError" => "yaml.constructor.ConstructorError",
        "ReaderError" => "yaml.reader.ReaderError",
        "JSONDecodeError" => "json.decoder.JSONDecodeError",
        "UnsupportedOperation" => "io.UnsupportedOperation",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(base: Base, class: &str, msg: &str) -> SmError {
        SmError {
            base,
            class: class.to_string().into(),
            msg: msg.into(),
        }
    }

    fn stderr_of(e: &SmError) -> (i32, String) {
        let mut out = Out::capture();
        let code = report_error(e, &mut out);
        out.finish(code);
        (code, out.captured().1)
    }

    #[test]
    fn caught_bases_print_one_error_line() {
        for base in [Base::Value, Base::Key, Base::Type, Base::Os, Base::Sqlite] {
            assert_eq!(
                stderr_of(&err(base, "X", "a\n b\x1c\tc ")),
                (1, "error: a b c\n".to_string())
            );
        }
    }

    #[test]
    fn uncaught_bases_print_traceback_and_class() {
        for base in [Base::Runtime, Base::Index, Base::Other] {
            let (code, text) = stderr_of(&err(base, "RuntimeError", "boom"));
            assert_eq!(
                (code, text.as_str()),
                (
                    1,
                    "Traceback (most recent call last):\nRuntimeError: boom\n"
                )
            );
        }
        let (_, text) = stderr_of(&err(Base::Other, "TimeoutExpired", "late"));
        assert_eq!(text.lines().last(), Some("subprocess.TimeoutExpired: late"));
        let (_, text) = stderr_of(&err(Base::Index, "IndexError", ""));
        assert_eq!(text.lines().last(), Some("IndexError"));
    }

    #[test]
    fn related_dispatches_and_fails_before_it_opens_a_home() {
        // `--limit 0` raises in run_related before connect, so no test reads ~/.simple-memory
        let mut out = Out::capture();
        let code = run(
            &["related".into(), "--limit".into(), "0".into(), "a".into()],
            &mut out,
        );
        out.finish(code);
        assert_eq!(code, 1);
        assert_eq!(
            out.captured().1.lines().last(),
            Some("error: --limit must be at least 1")
        );
    }

    #[test]
    fn import_jsonl_goes_to_transfer_and_other_imports_to_import() {
        let last = |argv: &[&str]| {
            let mut out = Out::capture();
            let code = run(
                &argv.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                &mut out,
            );
            out.finish(code);
            out.captured()
                .1
                .lines()
                .last()
                .unwrap_or_default()
                .to_string()
        };
        // Each ported line fails before the handler opens a home, so no test reads ~/.simple-memory.
        assert_eq!(
            last(&["import", "jsonl", "no-such-file.jsonl"]),
            "error: [Errno 2] No such file or directory: 'no-such-file.jsonl'"
        );
        assert_eq!(
            last(&["export", "--repo", "r", "--all"]),
            "sm export: error: argument --all: not allowed with argument --repo"
        );
        // `import tasklog` reads config.toml (never memory.db) before the path check.
        assert_eq!(
            last(&["import", "tasklog", "/nonexistent/p"]),
            "error: Tasklog path not found: /nonexistent/p"
        );
        assert_eq!(
            last(&["hooks", "print", "agy"]),
            "merge into ~/.gemini/config/hooks.json"
        );
        assert_eq!(
            last(&["put", "--tag", "decided", " "]),
            "error: Item body cannot be empty"
        );
    }

    #[test]
    fn parse_error_exits_2_before_any_handler() {
        let mut out = Out::capture();
        let code = run(
            &[
                "put".into(),
                "a".into(),
                "--tag".into(),
                "decided".into(),
                "b".into(),
            ],
            &mut out,
        );
        out.finish(code);
        assert_eq!(code, 2);
        assert_eq!(
            out.captured().1.lines().last(),
            Some("sm: error: unrecognized arguments: b")
        );
    }
}
