//! The `hook` command: the prompt hook's output and its recall lines.

use std::io::Read;

use crate::cli::query::{
    HOOK_REFS_CHARS, HOOK_TITLE_CHARS, compact_refs, refs_without_source, shlex_quote,
};
use crate::config::load_config;
use crate::core::rank::clip;
use crate::core::item::Item;
use crate::error::SmError;
use crate::py;
use crate::py::argparse::Namespace;
use crate::py::io::Out;
use crate::py::json::{Opts, dumps, loads};
use crate::py::value::PyValue;
use crate::service::recall::{Runner, run_prompt_hook, subprocess_run};

/// One short line per item.
pub fn format_recall_lines(items: &[Item]) -> String {
    let mut lines = Vec::new();
    for it in items {
        let title = clip(&py::str::split(&it.title).join(" "), HOOK_TITLE_CHARS);
        let refs = refs_without_source(it);
        let refs_part = if refs.is_empty() {
            String::new()
        } else {
            format!(" (refs: {})", compact_refs(&refs, HOOK_REFS_CHARS))
        };
        lines.push(format!(
            "[{}] {} {} {} {}{refs_part}: {title}",
            it.id,
            it.repo,
            it.kind,
            it.tag,
            py::str::slice(&it.as_of, None, Some(10))
        ));
    }
    lines.join("\n")
}

/// `sys.stdin.read()`: UTF-8 with surrogateescape and universal newlines.
fn read_stdin() -> Result<String, SmError> {
    let mut buf = Vec::new();
    std::io::stdin()
        .read_to_end(&mut buf)
        .map_err(|e| SmError::from_io(&e, None))?;
    Ok(py::str::decode_surrogateescape(&buf)
        .replace("\r\n", "\n")
        .replace('\r', "\n"))
}

/// The hook context for one payload text, or the error `_hook_prompt` prints.
pub fn hook_context(
    stdin: &str,
    agy: bool,
    home: Option<&str>,
    runner: Runner,
) -> Result<String, SmError> {
    let payload = loads(if stdin.is_empty() { "{}" } else { stdin })?;
    let payload = if matches!(payload, PyValue::Dict(_)) {
        payload
    } else {
        PyValue::Dict(Vec::new())
    };
    let Some(found) = run_prompt_hook(&payload, agy, home, runner)? else {
        return Ok(String::new());
    };
    let mut parts = Vec::new();
    if !found.items.is_empty() {
        let q = &found.query;
        let source = if found.fallback.is_empty() {
            format!("query \"{q}\"")
        } else {
            format!("local keywords \"{q}\"; model {}", found.fallback)
        };
        let in_part = if found.scopes.is_empty() {
            String::new()
        } else {
            format!(" --in {}", shlex_quote(&found.scopes.join(",")))
        };
        parts.push(format!(
            "sm recall for this prompt ({source}): leads with their as_of dates, not facts. `sm show <id>` gives the full text; `sm query \"{q}\"{in_part} --limit 10` gives more.\n{}",
            format_recall_lines(&found.items)
        ));
    }
    if !found.notice.is_empty() {
        parts.push(found.notice);
    }
    Ok(parts.join("\n"))
}

/// Always exit 0 with valid hook output.
pub fn hook_prompt(agy: bool, out: &mut Out) -> Result<i32, SmError> {
    let context =
        match read_stdin().and_then(|text| hook_context(&text, agy, None, &subprocess_run)) {
            Ok(c) => c,
            Err(e) => {
                let msg = py::str::split(&e.msg).join(" ");
                // A config.toml that does not load turns recall off: say so where the agent reads.
                if load_config(None).err().as_ref() == Some(&e) {
                    out.eprint(&format!("sm hook: {msg}"))?;
                    format!("sm recall is off: {msg}")
                } else {
                    out.eprint(&format!(
                        "sm hook: {}",
                        py::str::slice(&msg, None, Some(200))
                    ))?;
                    String::new()
                }
            }
        };
    let s = |v: &str| PyValue::Str(v.to_string());
    if agy {
        let steps = if context.is_empty() {
            vec![]
        } else {
            vec![PyValue::Dict(vec![(s("ephemeralMessage"), s(&context))])]
        };
        out.print(&dumps(
            &PyValue::Dict(vec![(s("injectSteps"), PyValue::List(steps))]),
            &Opts::unicode(),
        )?)?;
    } else if !context.is_empty() {
        let inner = PyValue::Dict(vec![
            (s("hookEventName"), s("UserPromptSubmit")),
            (s("additionalContext"), s(&context)),
        ]);
        out.print(&dumps(
            &PyValue::Dict(vec![(s("hookSpecificOutput"), inner)]),
            &Opts::unicode(),
        )?)?;
    }
    Ok(0)
}

pub fn run(ns: &Namespace, out: &mut Out) -> Result<i32, SmError> {
    hook_prompt(ns.get_bool("agy"), out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, title: &str, refs: &str, source: &str) -> Item {
        Item {
            id: id.into(),
            repo: "r".into(),
            kind: "fact".into(),
            title: title.into(),
            body: "b".into(),
            tag: "decided".into(),
            refs: refs.into(),
            tags: String::new(),
            source: source.into(),
            evidence: String::new(),
            as_of: "2026-11-01T10:00:00+00:00".into(),
            status: "active".into(),
            superseded_by: None,
            content_hash: String::new(),
            supersedes: Vec::new(),
        }
    }

    #[test]
    fn recall_lines_cut_long_titles_and_drop_the_source_ref() {
        let long = format!("{} tail", "w".repeat(170));
        let items = [
            item(
                "m_a",
                "Short\ttitle",
                "A-1 tasklog/x.yaml",
                "tasklog:tasklog/x.yaml",
            ),
            item("m_b", &long, "", ""),
        ];
        let text = format_recall_lines(&items);
        let lines: Vec<&str> = text.split('\n').collect();
        assert_eq!(
            lines[0],
            "[m_a] r fact decided 2026-11-01 (refs: A-1): Short title"
        );
        // 160 chars: 157 w and "..."
        assert_eq!(
            lines[1],
            format!("[m_b] r fact decided 2026-11-01: {}...", "w".repeat(157))
        );
    }

    #[test]
    fn recall_lines_show_each_items_evidence_tag() {
        let mut exec = item("m_e", "Ran check", "R5", "");
        exec.kind = "decision".into();
        exec.tag = "executed".into();
        let mut legacy = item("m_l", "Old note", "", "");
        legacy.repo = "global".into();
        legacy.tag = "n/a".into();
        let text = format_recall_lines(&[exec, legacy]);
        assert_eq!(
            text,
            "[m_e] r decision executed 2026-11-01 (refs: R5): Ran check\n\
             [m_l] global fact n/a 2026-11-01: Old note"
        );
    }

    #[test]
    fn no_items_gives_no_lines() {
        assert_eq!(format_recall_lines(&[]), "");
    }

    // ------------------------------------------------------------ ports of the Python recall tests (CLI hook)

    use crate::service::recall::port_tests::{
        PFake, PyHome, put_refs, put_titled, py_home, rows, set_config,
    };
    use serde_json::json;

    fn ctx(
        h: &PyHome,
        f: &PFake,
        payload: serde_json::Value,
        agy: bool,
    ) -> Result<String, SmError> {
        let r = |c: &[String], i: &str, t: f64, e: &[(&str, &str)], w: Option<&str>| {
            f.run(c, i, t, e, w)
        };
        hook_context(&payload.to_string(), agy, Some(&h.path), &r)
    }

    fn lines_after_header(context: &str) -> Vec<&str> {
        context.lines().skip(1).collect()
    }

    #[test]
    fn cli_hook_prints_valid_output_for_both_harnesses() {
        // test_cli_hook_prints_valid_output_for_both_harnesses (the context text; the JSON
        // wrappers and the "not json" path are in rust/tests/hook_catchup.rs)
        let h = py_home();
        let fake = PFake::out("R6 pruning");
        let p = json!({"prompt": "does R6 revert still", "cwd": "/nonexistent-sm-r2"});
        let claude = ctx(&h, &fake, p.clone(), false).unwrap();
        assert!(
            claude.contains("query \"R6 pruning\"") && claude.contains("R6 stays reverted"),
            "{claude}"
        );
        let agy = ctx(&h, &fake, p, true).unwrap();
        assert!(agy.contains("R6 stays reverted"), "{agy}");
        assert_eq!(ctx(&h, &fake, json!({"prompt": "ok"}), false).unwrap(), "");
        assert_eq!(ctx(&h, &fake, json!(null), false).unwrap(), "");
        assert_eq!(
            hook_context(
                "not json",
                false,
                Some(&h.path),
                &|_, _, _, _, _| unreachable!()
            )
            .unwrap_err()
            .class,
            "JSONDecodeError"
        );
    }

    #[test]
    fn cli_hook_injects_one_short_line_per_item() {
        // test_cli_hook_injects_one_short_line_per_item
        let h = py_home();
        let long = format!("R6 {}", "long title word ".repeat(40));
        put_titled(&h, &long, "decided");
        let context = ctx(
            &h,
            &PFake::out("R6"),
            json!({"prompt": "does R6 revert still", "cwd": "/nonexistent-sm-r2"}),
            false,
        )
        .unwrap();
        let lines = lines_after_header(&context);
        assert_eq!(lines.len(), 2);
        assert!(
            lines
                .iter()
                .all(|l| l.starts_with("[m_") && l.chars().count() < 260)
        );
        assert!(lines.iter().any(|l| l.ends_with("...")));
    }

    #[test]
    fn cli_hook_line_shows_compact_refs_before_the_title() {
        // test_cli_hook_line_shows_compact_refs_before_the_title
        let h = py_home();
        let refs = format!(
            "Q7 {}",
            (0..12)
                .map(|n| format!("skills/beta/part_{n}.py"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        put_refs(&h, "Q7 poll waits for sign-in", "decided", &refs);
        put_refs(&h, "Q7 second note with no extra refs", "decided", "");
        let context = ctx(
            &h,
            &PFake::out("Q7"),
            json!({"prompt": "what about Q7 now", "cwd": "/nonexistent-sm-r2"}),
            false,
        )
        .unwrap();
        let by_title = |title: &str| {
            lines_after_header(&context)
                .into_iter()
                .find(|l| l.rsplit(": ").next() == Some(title))
                .unwrap()
                .to_string()
        };
        assert!(
            by_title("Q7 poll waits for sign-in")
                .contains(" (refs: Q7 skills/beta/part_0.py skills/beta/part_1.py +10): ")
        );
        assert!(
            by_title("Q7 second note with no extra refs")
                .ends_with(" (refs: Q7): Q7 second note with no extra refs")
        );
    }

    #[test]
    fn cli_hook_line_cuts_a_first_ref_longer_than_the_cap() {
        // test_cli_hook_line_cuts_a_first_ref_longer_than_the_cap
        let h = py_home();
        let long_ref = format!("skills/gamma/{}tool.py", "deep/".repeat(14));
        put_refs(
            &h,
            "Q8 deep tool note",
            "decided",
            &format!("{long_ref} Q8"),
        );
        let context = ctx(
            &h,
            &PFake::out("Q8"),
            json!({"prompt": "what about Q8 now", "cwd": "/nonexistent-sm-r2"}),
            false,
        )
        .unwrap();
        let lines = lines_after_header(&context);
        assert_eq!(lines.len(), 1);
        let cut: String = long_ref.chars().take(57).collect();
        assert!(
            lines[0].contains(&format!(" (refs: {cut}... +1): Q8 deep tool note")),
            "{}",
            lines[0]
        );
    }

    #[test]
    fn cli_header_names_the_local_path_and_the_cause() {
        // test_cli_header_names_the_local_path_and_the_cause
        let h = py_home();
        set_config(&h, "[prompt_hook]\nmodel = \"none\"\n");
        let context = ctx(
            &h,
            &PFake::out("x"),
            json!({"prompt": "does R6 revert still", "cwd": "/nonexistent-sm-r2"}),
            false,
        )
        .unwrap();
        assert_eq!(
            context.lines().next().unwrap(),
            "sm recall for this prompt (local keywords \"r6 revert\"; model off): leads with their as_of dates, not facts. `sm show <id>` gives the full text; `sm query \"r6 revert\" --limit 10` gives more."
        );
    }

    #[test]
    fn cli_header_includes_in_scopes_only_when_scopes_matched() {
        let h = py_home();
        set_config(
            &h,
            "[prompt_hook]\nmin_idf = 0.0\n[prompt_hook.projects]\nGLX = \"Globex\"\n[scopes]\n\"acme/GLX\" = [\"GLX\"]\n",
        );
        put_titled(&h, "R6 GLX rollout note", "decided");
        let with_scope = ctx(
            &h,
            &PFake::out("R6 rollout"),
            json!({"prompt": "check GLX R6 rollout now", "cwd": "/nonexistent-sm-r2"}),
            false,
        )
        .unwrap();
        assert_eq!(
            with_scope.lines().next().unwrap(),
            "sm recall for this prompt (query \"R6 rollout\"): leads with their as_of dates, not facts. `sm show <id>` gives the full text; `sm query \"R6 rollout\" --in acme/GLX --limit 10` gives more."
        );
        let without_scope = ctx(
            &h,
            &PFake::out("R6 rollout"),
            json!({"prompt": "check generic R6 rollout now", "cwd": "/nonexistent-sm-r2"}),
            false,
        )
        .unwrap();
        assert_eq!(
            without_scope.lines().next().unwrap(),
            "sm recall for this prompt (query \"R6 rollout\"): leads with their as_of dates, not facts. `sm show <id>` gives the full text; `sm query \"R6 rollout\" --limit 10` gives more."
        );
    }

    fn catch_up_log(h: &PyHome, rows: &[serde_json::Value]) {
        std::fs::write(
            h.path.clone() + "/catchup.log",
            rows.iter().map(|r| format!("{r}\n")).collect::<String>(),
        )
        .unwrap();
    }

    fn failed_row() -> serde_json::Value {
        json!({
            "ts": "2026-11-04T01:00:00+00:00", "ok": false, "ms": 900,
            "parts": {
                "automemory": {"ok": true, "ms": 400, "counts": {"added": 1}, "error": null},
                "imports": {"ok": false, "ms": 500, "counts": {}, "error": format!("root missing: /u/none {}", "x".repeat(200))},
                "backup": {"ok": false, "ms": 0, "counts": {}, "error": "second error"},
            }
        })
    }

    fn ok_row() -> serde_json::Value {
        json!({"ts": "2026-11-04T01:10:00+00:00", "ok": true, "ms": 800, "parts": {}})
    }

    /// The context a first or later prompt of a session gets.
    fn session_context(h: &PyHome, session: &str, query: &str) -> String {
        let p = json!({"prompt": "does R6 revert still", "session_id": session, "cwd": "/nonexistent-sm-r2"});
        ctx(h, &PFake::out(query), p, false).unwrap()
    }

    #[test]
    fn failed_last_catch_up_adds_one_line_on_a_first_prompt_only() {
        // test_failed_last_catch_up_adds_one_line_on_a_first_prompt_only
        let h = py_home();
        catch_up_log(&h, &[ok_row(), failed_row()]);
        let expected = format!(
            "sm catch-up failed at 2026-11-04T01:00:00+00:00: root missing: /u/none {}; run sm doctor --imports",
            "x".repeat(98)
        );
        let first = session_context(&h, "sess-b", "R6 weakening");
        assert_eq!(first.lines().last().unwrap(), expected);
        assert!(first.contains("R6 stays reverted"));
        assert!(!session_context(&h, "sess-b", "R6 weakening").contains(&expected));
        assert_eq!(session_context(&h, "sess-c", "nothingmatches"), expected);
    }

    #[test]
    fn ok_last_catch_up_or_no_log_adds_nothing() {
        // test_ok_last_catch_up_or_no_log_adds_nothing (2 params)
        for rows in [vec![failed_row(), ok_row()], vec![]] {
            let h = py_home();
            if !rows.is_empty() {
                catch_up_log(&h, &rows);
            }
            let context = session_context(&h, "sess-d", "R6 weakening");
            assert!(
                !context.contains("catch-up") && context.contains("R6 stays reverted"),
                "{context}"
            );
            assert_eq!(session_context(&h, "sess-e", "nothingmatches"), "");
        }
    }

    #[test]
    fn non_json_last_catch_up_row_reads_as_a_failure() {
        // test_non_json_last_catch_up_row_reads_as_a_failure
        let h = py_home();
        std::fs::write(
            h.path.clone() + "/catchup.log",
            format!("{}\n{{torn\n", ok_row()),
        )
        .unwrap();
        assert_eq!(
            session_context(&h, "sess-f", "nothingmatches"),
            "sm catch-up failed at unknown time: last catchup.log row is not JSON; run sm doctor --imports"
        );
    }

    #[test]
    fn a_failed_catch_up_row_with_no_usable_ts_says_unknown_time() {
        // test_a_failed_catch_up_row_with_no_usable_ts_says_unknown_time (params None, 5, "")
        for ts in [json!(null), json!(5), json!("")] {
            let h = py_home();
            catch_up_log(
                &h,
                &[json!({"ts": ts, "ok": false, "parts": {"imports": {"error": "root missing"}}})],
            );
            assert_eq!(
                session_context(&h, "sess-ts", "nothingmatches"),
                "sm catch-up failed at unknown time: root missing; run sm doctor --imports"
            );
        }
    }

    #[test]
    fn failure_line_shows_on_a_first_prompt_with_no_query() {
        // test_failure_line_shows_on_a_first_prompt_with_no_query
        let h = py_home();
        let mut row = failed_row();
        row["parts"] = json!({"backup": {"ok": false, "error": "disk full"}});
        catch_up_log(&h, &[row]);
        assert_eq!(
            session_context(&h, "sess-g", ""),
            "sm catch-up failed at 2026-11-04T01:00:00+00:00: disk full; run sm doctor --imports"
        );
        assert_eq!(rows(&h).pop().unwrap()["path"], "none");
    }

    #[test]
    fn catch_up_start_error_never_breaks_the_hook_output() {
        // test_catch_up_start_error_never_breaks_the_hook_output. Python makes Popen raise
        // OSError("fork failed"); here the hook folder is read-only, so the stamp's touch raises
        // PermissionError and the row keeps that text. Python's stat of a stamp under a file
        // raises NotADirectoryError uncaught, as Rust's does, so a file at hook/ is no stand-in.
        // The model is "none" so the hook writes nothing else under hook/.
        use std::os::unix::fs::PermissionsExt;
        let h = py_home();
        set_config(
            &h,
            "[catch_up]\nenabled = true\n[prompt_hook]\nmodel = \"none\"\n",
        );
        std::fs::create_dir(h.path.clone() + "/hook").unwrap();
        std::fs::set_permissions(
            h.path.clone() + "/hook",
            std::fs::Permissions::from_mode(0o555),
        )
        .unwrap();
        let context = ctx(
            &h,
            &PFake::out("x"),
            json!({"prompt": "does R6 revert still", "cwd": "/nonexistent-sm-r2"}),
            false,
        )
        .unwrap();
        assert!(context.contains("R6 stays reverted"), "{context}");
        let first = rows(&h).remove(0);
        assert_eq!(
            first["catch_up_error"],
            format!(
                "[Errno 13] Permission denied: '{}/hook/catchup.stamp'",
                h.path
            )
        );
        std::fs::set_permissions(h.path + "/hook", std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(first["harness"], "claude");
        assert_eq!(first.as_object().unwrap().len(), 3);
    }
}
