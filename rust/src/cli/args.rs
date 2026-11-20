//! The `sm` parser tree and its custom actions (comma list, bias list, version). Handlers read
//! the Namespace it returns by dest name.

use crate::py::argparse::{
    Action, ActionError, Arg, Conv, Kind, Namespace, Nargs, Parser, Subparsers, Value,
};
use crate::py::io::Out;
use crate::service::setup::HOOK_TARGETS;

/// `_CommaList`: `--refs A --refs B` gives the same value as `--refs A,B`.
fn comma_list(
    action: &Action,
    ns: &mut Namespace,
    values: Value,
    _out: &mut Out,
) -> Result<(), ActionError> {
    let dest = action.dest.as_deref().unwrap_or_default();
    let Value::Str(values) = values else {
        unreachable!("nargs=None gives one str")
    };
    let previous = match ns.get_or(dest, &Value::Str(String::new())) {
        Value::Str(s) => s.clone(),
        _ => String::new(),
    };
    let joined = if previous.is_empty() {
        values
    } else {
        format!("{previous},{values}")
    };
    ns.set(dest, Value::Str(joined));
    Ok(())
}

/// `_BiasList`: repeatable like `_CommaList`; the joined text is checked each time.
fn bias_list(
    _action: &Action,
    ns: &mut Namespace,
    values: Value,
    _out: &mut Out,
) -> Result<(), ActionError> {
    let Value::Str(values) = values else {
        unreachable!("nargs=None gives one str")
    };
    let previous = match ns.get_or("bias_text", &Value::Str(String::new())) {
        Value::Str(s) => s.clone(),
        _ => String::new(),
    };
    let raw = [previous.as_str(), values.as_str()]
        .into_iter()
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(",");
    let bias = crate::service::query::parse_bias(&raw).map_err(|e| ActionError::Arg(e.msg))?;
    ns.set("bias_text", Value::Str(raw));
    ns.set(
        "bias",
        Value::List(bias.into_iter().map(Value::Str).collect()),
    );
    Ok(())
}

/// `_Version`: prints `sm <version>` and exits 0. Rust prints its own package version
/// (an accepted difference: Python prints 0.3.2).
fn version(
    _action: &Action,
    _ns: &mut Namespace,
    _values: Value,
    out: &mut Out,
) -> Result<(), ActionError> {
    let _ = out.print(&format!("sm {}", env!("CARGO_PKG_VERSION")));
    Err(ActionError::Exit(0))
}

fn opt(help: &str) -> Arg {
    Arg {
        help: help.into(),
        ..Arg::default()
    }
}

fn default_str(default: &str, help: &str) -> Arg {
    Arg {
        default: Some(Value::Str(default.into())),
        help: help.into(),
        ..Arg::default()
    }
}

fn comma(help: &str) -> Arg {
    Arg {
        kind: Some(Kind::Custom(comma_list)),
        default: Some(Value::Str(String::new())),
        help: help.into(),
        ..Arg::default()
    }
}

fn opt_comma(help: &str) -> Arg {
    Arg {
        kind: Some(Kind::Custom(comma_list)),
        default: Some(Value::None),
        help: help.into(),
        ..Arg::default()
    }
}

fn flag(help: &str) -> Arg {
    Arg {
        kind: Some(Kind::StoreTrue),
        help: help.into(),
        ..Arg::default()
    }
}

fn int(default: Option<i64>, help: &str) -> Arg {
    Arg {
        conv: Conv::Int,
        default: Some(default.map_or(Value::None, Value::Int)),
        help: help.into(),
        ..Arg::default()
    }
}

fn choices(list: &[&str], default: Option<&str>, help: &str) -> Arg {
    Arg {
        choices: Some(list.iter().map(|s| s.to_string()).collect()),
        default: default.map(|d| Value::Str(d.into())),
        help: help.into(),
        ..Arg::default()
    }
}

fn add_query_args(p: &mut Parser) {
    p.allow_dash_exclusions(true);
    p.add_argument(
        &["query"],
        Arg {
            nargs: Some(Nargs::OneOrMore),
            help: "Search query terms (pass '--' before a leading-dash query like '-R6')".into(),
            ..Arg::default()
        },
    );
    p.add_argument(
        &["--limit"],
        int(Some(4), "Maximum snippets to print (default: 4)"),
    );
    let recent = p.add_argument(&["--recent"], flag("Sort matching items newest first"));
    let bias = p.add_argument(
        &["--bias"],
        Arg {
            kind: Some(Kind::Custom(bias_list)),
            default: Some(Value::None),
            help:
                "Comma-separated ranking preferences; reorders, never filters \
                 (default: recency, fact, decision, learning, executed, verified, decided, client-stated, or [query] default_bias). \
                 recency, relevance (no preference), an evidence tag, a kind, or scope:<path>"
                    .into(),
            ..Arg::default()
        },
    );
    p.add_mutually_exclusive_group(vec![recent, bias]);
    p.add_argument(&["--tag"], comma("Filter by comma-separated evidence tags"));
    p.add_argument(&["--kind"], comma("Filter by comma-separated kinds"));
    p.add_argument(
        &["--source"],
        comma("Filter by comma-separated source prefixes (case-sensitive)"),
    );
    p.add_argument(
        &["--since"],
        default_str(
            "",
            "Keep items with as_of on or after ISO 8601 date or timestamp",
        ),
    );
    p.add_argument(
        &["--until"],
        default_str(
            "",
            "Keep items with as_of on or before ISO 8601 date or timestamp (date-only includes full day)",
        ),
    );
    let q_in = p.add_argument(
        &["--in"],
        comma("Keep items in comma-separated [scopes] paths; searches every repo"),
    );
    p.add_argument(
        &["--all"],
        Arg {
            dest: Some("all_repos".into()),
            ..flag("Search across all repos")
        },
    );
    let q_repo = p.add_argument(&["--repo"], default_str("", "Repo override"));
    p.add_mutually_exclusive_group(vec![q_in, q_repo]);
    p.add_argument(&["--include-history"], flag("Include superseded items"));
    p.add_argument(
        &["--format"],
        choices(
            &["text", "brief", "json"],
            Some("text"),
            "text: a header and a snippet per hit; brief: one line per hit; json (default: text)",
        ),
    );
}

pub fn build_parser() -> Parser {
    let mut parser = Parser::new(
        "sm",
        "simple-memory: single-file SQLite + FTS5 CLI memory store",
    );
    parser.add_argument(
        &["-v", "--version"],
        Arg {
            kind: Some(Kind::Custom(version)),
            nargs: Some(Nargs::Zero),
            help: "Print `sm <version>` and exit".into(),
            ..Arg::default()
        },
    );
    let mut sub = Subparsers::new(&parser, "command", true);

    let p_put = sub.add_parser("put", "Store a memory item with an evidence tag");
    p_put.add_argument(
        &["body"],
        Arg {
            nargs: Some(Nargs::ZeroOrMore),
            help: "Memory text to store".into(),
            ..Arg::default()
        },
    );
    p_put.add_argument(
        &["--kind"],
        default_str(
            "fact",
            "Item kind (fact, decision, learning, episode, task, handoff)",
        ),
    );
    p_put.add_argument(
        &["--tag"],
        default_str(
            "",
            "Evidence tag (executed, verified, decided, client-stated, derived, assumed, prior, planned)",
        ),
    );
    p_put.add_argument(
        &["--evidence"],
        default_str("", "Command, sha, file:line or quote behind the tag"),
    );
    p_put.add_argument(
        &["--refs"],
        comma("Explicit references (ticket ids, shas, paths)"),
    );
    p_put.add_argument(&["--tags"], comma("Freeform tags"));
    p_put.add_argument(
        &["--title"],
        default_str("", "Optional title (defaults to first line of body)"),
    );
    p_put.add_argument(&["--source"], default_str("", "Optional provenance source"));
    p_put.add_argument(
        &["--repo"],
        default_str("", "Repo override (defaults to current git repo)"),
    );
    p_put.add_argument(&["--as-of"], default_str("", "ISO 8601 timestamp override"));
    p_put.add_argument(
        &["--supersedes"],
        comma("Old item id(s) this item supersedes"),
    );

    let p_query = sub.add_parser(
        "query",
        "Search memory items (use '--' before a query starting with '-', e.g. sm query -- -R6)",
    );
    add_query_args(p_query);

    let p_search = sub.add_parser("search", "Alias for `sm query`");
    add_query_args(p_search);

    let p_show = sub.add_parser(
        "show",
        "Show full body and provenance of items, in the order given",
    );
    p_show.add_argument(
        &["ids"],
        Arg {
            nargs: Some(Nargs::OneOrMore),
            metavar: Some("ID".into()),
            help: "Item id(s)".into(),
            ..Arg::default()
        },
    );
    p_show.add_argument(
        &["--format"],
        choices(
            &["text", "json"],
            Some("text"),
            "Output format (default: text)",
        ),
    );

    let p_rel = sub.add_parser(
        "related",
        "Items linked to one item: its supersede chain, shared refs, the same session",
    );
    p_rel.add_argument(&["id"], opt("Item id"));
    p_rel.add_argument(
        &["--limit"],
        int(Some(10), "Maximum items to print (default: 10)"),
    );
    p_rel.add_argument(
        &["--format"],
        choices(
            &["text", "json"],
            Some("text"),
            "Output format (default: text)",
        ),
    );

    let p_sup = sub.add_parser("supersede", "Mark an old item superseded by a new item");
    p_sup.add_argument(&["old_id"], opt("Superseded item id"));
    p_sup.add_argument(&["new_id"], opt("Replacement item id"));

    let p_ret = sub.add_parser("retire", "Retire an active item with a reason");
    p_ret.add_argument(&["id"], opt("Item id to retire"));
    p_ret.add_argument(
        &["--reason"],
        default_str("", "Reason for retiring the item (default: manual)"),
    );

    let p_am = sub.add_parser(
        "amend",
        "Create a revised active copy of an item and supersede the old one",
    );
    p_am.add_argument(&["id"], opt("Active item id to amend"));
    p_am.add_argument(&["--repo"], opt("Replace repo"));
    p_am.add_argument(
        &["--kind"],
        opt("Replace kind (fact, decision, learning, episode, task, handoff)"),
    );
    p_am.add_argument(
        &["--tag"],
        opt("Replace evidence tag (executed, verified, decided, client-stated, derived, assumed, prior, planned)"),
    );
    p_am.add_argument(&["--evidence"], opt("Replace evidence text"));
    p_am.add_argument(&["--title"], opt("Replace title"));
    p_am.add_argument(&["--body"], opt("Replace body text"));
    p_am.add_argument(&["--add-refs"], opt_comma("Add comma-separated references"));
    p_am.add_argument(
        &["--remove-refs"],
        opt_comma("Remove comma-separated references"),
    );
    p_am.add_argument(&["--add-tags"], opt_comma("Add comma-separated tags"));
    p_am.add_argument(&["--remove-tags"], opt_comma("Remove comma-separated tags"));
    p_am.add_argument(&["--as-of"], opt("Replace ISO 8601 timestamp"));

    let p_list = sub.add_parser(
        "list",
        "Browse stored memory items chronologically with filters and pagination",
    );
    p_list.add_argument(
        &["--limit"],
        int(
            Some(crate::service::list::DEFAULT_LIST_LIMIT),
            "Maximum items to print (default: 20)",
        ),
    );
    p_list.add_argument(
        &["--offset"],
        int(Some(0), "Number of items to skip (default: 0)"),
    );
    let list_repo = p_list.add_argument(
        &["--repo"],
        default_str("", "Exact repo override (default: current git repo)"),
    );
    let list_all = p_list.add_argument(
        &["--all"],
        Arg {
            dest: Some("all_repos".into()),
            ..flag("List across all repos")
        },
    );
    let list_in = p_list.add_argument(
        &["--in"],
        comma("Keep items in comma-separated [scopes] paths; searches every repo"),
    );
    p_list.add_mutually_exclusive_group(vec![list_repo, list_all, list_in]);
    p_list.add_argument(&["--kind"], comma("Filter by comma-separated kinds"));
    p_list.add_argument(&["--tag"], comma("Filter by comma-separated evidence tags"));
    p_list.add_argument(
        &["--source"],
        comma("Filter by comma-separated source prefixes (case-sensitive)"),
    );
    p_list.add_argument(
        &["--since"],
        default_str(
            "",
            "Keep items with as_of on or after ISO 8601 date or timestamp",
        ),
    );
    p_list.add_argument(
        &["--until"],
        default_str(
            "",
            "Keep items with as_of on or before ISO 8601 date or timestamp (date-only includes full day)",
        ),
    );
    let list_status = p_list.add_argument(
        &["--status"],
        choices(
            &["active", "superseded", "retired", "all"],
            None,
            "Filter by status (default: active)",
        ),
    );
    let list_history = p_list.add_argument(
        &["--include-history"],
        flag("Include superseded and retired items (same as --status all)"),
    );
    p_list.add_mutually_exclusive_group(vec![list_status, list_history]);
    p_list.add_argument(
        &["--format"],
        choices(
            &["brief", "text", "json"],
            Some("brief"),
            "brief: one line per item; text: header and snippet; json (default: brief)",
        ),
    );

    let p_imp = sub.add_parser(
        "import",
        "Import items from files (tasklog, handoff, legacy, proposal, automemory, sessions, catch-up)",
    );
    let mut imp_sub = Subparsers::new(p_imp, "source_kind", true);
    for kind in ["tasklog", "handoff", "legacy", "proposal"] {
        let p_kind = imp_sub.add_parser(
            kind,
            &format!("Import {kind} items from a file or directory"),
        );
        p_kind.add_argument(&["path"], opt("File or directory path to import"));
        p_kind.add_argument(
            &["--dry-run"],
            flag("Print counts per repo and tag without writing"),
        );
        p_kind.add_argument(&["--apply"], flag("Apply proposal JSONL into the store"));
    }
    let p_am = imp_sub.add_parser(
        "automemory",
        "Sync Claude Code memory files (config [automemory] root)",
    );
    p_am.add_argument(
        &["--dry-run"],
        flag("Print the counts a sync would give; write nothing"),
    );
    p_am.add_argument(
        &["--root"],
        Arg {
            default: Some(Value::None),
            ..opt("Projects folder (default: [automemory] root)")
        },
    );
    let p_ses = imp_sub.add_parser(
        "sessions",
        "Write one session card per idle Claude Code session with real work and no item",
    );
    p_ses.add_argument(
        &["--dry-run"],
        flag("Print candidate counts by repo; write nothing"),
    );
    p_ses.add_argument(
        &["--since-days"],
        Arg {
            conv: Conv::Float,
            default: Some(Value::None),
            ..opt("Transcripts written in the last N days")
        },
    );
    p_ses.add_argument(
        &["--limit"],
        int(None, "At most N cards (default [capture] max_per_run)"),
    );
    let p_jl = imp_sub.add_parser(
        "jsonl",
        "Import an sm export file: new ids added, stored ids kept",
    );
    p_jl.add_argument(&["path"], opt("JSONL file that sm export wrote"));
    p_jl.add_argument(
        &["--dry-run"],
        flag("Print the counts an import would give; write nothing"),
    );
    let p_cu = imp_sub.add_parser(
        "catch-up",
        "Run every import, retirement, backup and prune once",
    );
    p_cu.add_argument(&["--quiet"], flag("Print nothing"));
    p_imp.add_subparsers(imp_sub);

    let p_exp = sub.add_parser(
        "export",
        "Write items as JSONL to stdout, one item per line, sorted by id",
    );
    p_exp.add_argument(&["--include-history"], flag("Include superseded items"));
    let exp_repo = p_exp.add_argument(
        &["--repo"],
        default_str("", "One repo (default: the current repo)"),
    );
    let exp_all = p_exp.add_argument(
        &["--all"],
        Arg {
            dest: Some("all_repos".into()),
            ..flag("Every repo")
        },
    );
    p_exp.add_mutually_exclusive_group(vec![exp_repo, exp_all]);

    let p_bak = sub.add_parser(
        "backup",
        "Copy memory.db to backups/memory-<time>-auto.db and check it",
    );
    p_bak.add_argument(
        &["--keep"],
        int(None, "Auto copies to keep (default: [backup] keep)"),
    );

    let p_hook = sub.add_parser(
        "hook",
        "Harness hook: recall items for the prompt (reads hook JSON on stdin)",
    );
    p_hook.add_argument(&["event"], choices(&["prompt"], None, "Hook event"));
    p_hook.add_argument(
        &["--agy"],
        flag("agy PreInvocation output instead of Claude Code UserPromptSubmit"),
    );

    let p_doc = sub.add_parser(
        "doctor",
        "Check database integrity and optionally rebuild FTS indexes",
    );
    p_doc.add_argument(&["--rebuild-fts"], flag("Rebuild FTS5 tables from items"));
    p_doc.add_argument(
        &["--backfill-refs"],
        flag("Fill refs each item's text (and an agent put's evidence) names; one transaction"),
    );
    p_doc.add_argument(
        &["--dry-run"],
        flag("With --backfill-refs: count, write nothing"),
    );
    p_doc.add_argument(
        &["--imports"],
        flag("List task logs, handoffs and Claude Code memory files on disk with no current item (config [upkeep], [automemory])"),
    );

    let p_up = sub.add_parser(
        "upkeep",
        "A session's newest handoff supersedes its older ones and the handoff it took up after /clear; --report lists likely duplicates",
    );
    p_up.add_argument(
        &["--dry-run"],
        flag("Count what would be superseded, write nothing"),
    );
    p_up.add_argument(
        &["--report"],
        flag("Read-only: same-title groups in one repo, near-duplicate title pairs, stale planned and assumed items"),
    );
    p_up.add_argument(
        &["--limit"],
        int(Some(50), "With --report: pairs to print (default: 50)"),
    );

    let p_stats = sub.add_parser(
        "stats",
        "Store counts, recall use, catch-up runs and backups; writes nothing",
    );
    p_stats.add_argument(
        &["--days"],
        Arg {
            conv: Conv::Float,
            default: Some(Value::Float(7.0)),
            ..opt("Log window in days (default: 7)")
        },
    );
    p_stats.add_argument(
        &["--format"],
        choices(
            &["text", "json"],
            Some("text"),
            "Output format (default: text)",
        ),
    );

    sub.add_parser(
        "init",
        "Create the home, the database and a commented config.toml (never overwrites it)",
    );
    sub.add_parser(
        "config",
        "Print the config in effect (defaults plus config.toml) as TOML",
    );
    let p_hooks = sub.add_parser(
        "hooks",
        "Print the hook settings a harness needs; writes no file",
    );
    let mut hooks_sub = Subparsers::new(p_hooks, "hooks_action", true);
    let p_hp = hooks_sub.add_parser(
        "print",
        "Print the JSON fragment to merge into the harness hook file",
    );
    p_hp.add_argument(
        &["harness"],
        choices(
            &HOOK_TARGETS.map(|(k, _)| k),
            None,
            "Harness to print the hook for",
        ),
    );
    p_hooks.add_subparsers(hooks_sub);

    sub.add_parser(
        "mcp",
        "Serve the memory tools over MCP on stdio (JSON-RPC); logs go to stderr",
    );

    let p_upd = sub.add_parser(
        "update",
        "Check for a newer simple-memory release and optionally upgrade",
    );
    p_upd.add_argument(&["--check"], flag("Check for updates without installing"));

    parser.add_subparsers(sub);
    parser
}

#[cfg(test)]
#[path = "args_tests.rs"]
mod args_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::py::argparse::Exit;

    #[test]
    fn version_prints_the_package_version_and_exits_0() {
        for flag in ["--version", "-v"] {
            let mut out = Out::capture();
            assert_eq!(
                build_parser().parse_args(&[flag.into()], &mut out),
                Err(Exit(0))
            );
            out.finish(0);
            assert_eq!(out.captured().0, "sm 0.6.0\n");
        }
    }

    #[test]
    fn namespace_getters_read_parsed_values() {
        let argv: Vec<String> = [
            "import",
            "sessions",
            "--since-days",
            "٣.٥",
            "--limit",
            "1_0",
        ]
        .map(String::from)
        .to_vec();
        let ns = build_parser()
            .parse_args(&argv, &mut Out::capture())
            .expect("parses");
        assert_eq!(ns.get_str("source_kind"), "sessions");
        assert_eq!(ns.get_opt_float("since_days"), Some(3.5));
        assert_eq!(ns.get_opt_int("limit"), Some(10));
        assert!(!ns.get_bool("dry_run"));
        let ns = build_parser()
            .parse_args(&["put".into(), "a".into(), "b".into()], &mut Out::capture())
            .expect("parses");
        assert_eq!(ns.get_str_list("body"), vec!["a", "b"]);
        assert_eq!(ns.get_str("refs"), "");
        assert_eq!(ns.get_opt_str("version"), None);
    }

    #[test]
    fn query_empty_bias_preserves_bias_text_for_config_override() {
        let empty: Vec<String> = ["query", "x", "--bias", ""].map(String::from).to_vec();
        let ns = build_parser()
            .parse_args(&empty, &mut Out::capture())
            .expect("parses empty bias");
        assert_eq!(ns.get_str("command"), "query");
        assert_eq!(ns.get_str("bias_text"), "");

        let commas: Vec<String> = ["query", "x", "--bias", ",,"].map(String::from).to_vec();
        let ns_commas = build_parser()
            .parse_args(&commas, &mut Out::capture())
            .expect("parses comma-only bias");
        assert_eq!(ns_commas.get_str("bias_text"), ",,");
    }
}
