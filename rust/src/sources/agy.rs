//! Read one agy (Antigravity CLI) conversation log into the `SessionScan` a session card needs,
//! and the ids of the conversations agy-dispatch started headless.
//!
//! A log is `<brain>/<conversation id>/.system_generated/logs/transcript.jsonl`, one JSON row per
//! step: `{"type", "source", "created_at", "content"?, "tool_calls"?: [{"name", "args"}]}`. Each
//! `args` value is a str, most of them JSON-encoded (`"\"/u/a.py\""`). The rows hold no cwd and
//! no link from a tool output to its call (agy 1.5.16, probed 2026-11-28).

use std::collections::HashSet;

use crate::error::SmError;
use crate::py::path::{Errors, PyPath};
use crate::py::str::{slice, strip};
use crate::py::value::PyValue;
use crate::sources::transcript::{
    COMMIT_LINE_RE, COMMIT_RE, Dialogue, MAX_MESSAGE_CHARS, SessionScan, TASK_ID_RE, add_unique,
    contains, epoch_of, newest, puts_to_store, row_of,
};
use crate::sources::{TextLines, dict_get};

/// The log of each conversation, below the brain folder (`[capture] agy_root`).
pub const LOG_GLOB: &str = "*/.system_generated/logs/transcript.jsonl";
/// agy-dispatch.sh appends one row per headless run here; its `task_dir` holds `session.id`.
pub const LEDGER: &str = "~/.gemini/agy-ledger.jsonl";
const WRITE_TOOLS: [&str; 4] = [
    "edit_file",
    "write_file",
    "replace_file_content",
    "write_to_file",
];

/// The conversation id of a log path: the folder three levels up.
pub fn conversation_id(log: &PyPath) -> String {
    log.parent().parent().parent().name().to_string()
}

fn str_of(v: Option<&PyValue>) -> &str {
    match v {
        Some(PyValue::Str(s)) => s,
        _ => "",
    }
}

/// The owner's text of a prompt row: the `<USER_REQUEST>` body, else the text before the
/// `<ADDITIONAL_METADATA>` agy appends (skill bodies, the clock).
fn request_text(content: &str) -> &str {
    let body = match content.split_once("<USER_REQUEST>") {
        Some((_, rest)) => rest.split_once("</USER_REQUEST>").map_or(rest, |(b, _)| b),
        None => content
            .split_once("<ADDITIONAL_METADATA>")
            .map_or(content, |(b, _)| b),
    };
    strip(body)
}

/// A tool argument as the tool got it: the JSON-decoded str, else the raw str.
fn arg_text(raw: &str) -> String {
    match crate::py::json::loads(raw) {
        Ok(PyValue::Str(s)) => s,
        _ => raw.to_string(),
    }
}

/// agy cuts a tool argument over 2048 chars to its head plus this marker.
const CUT_ARG: &str = "\n<truncated ";
/// agy keeps only a preview of a long tool output in the log; the note names the full file.
const CUT_OUTPUT: &str = "You can find the full output in a file at file://";

/// The full output a cut `GENERIC` row names, read only from this conversation's
/// `.system_generated/steps/` folder; "" when the row is whole or the file is gone.
fn full_output(log: &PyPath, content: &str) -> String {
    let Some((_, rest)) = content.split_once(CUT_OUTPUT) else {
        return String::new();
    };
    let named = rest.split(char::is_whitespace).next().unwrap_or("");
    let named = named.strip_suffix('.').unwrap_or(named);
    let steps = format!("{}/", log.parent().parent().join("steps").as_str());
    if !named.starts_with(&steps) || named.split('/').any(|part| part == "..") {
        return String::new();
    }
    let file = PyPath::new(named);
    if !file.is_file() {
        return String::new();
    }
    file.read_text(Errors::Replace).unwrap_or_default()
}

/// An argument that is one absolute path (a `file://` URI counts), else None.
fn arg_path(text: &str) -> Option<&str> {
    let p = text.strip_prefix("file://").unwrap_or(text);
    (p.starts_with('/') && !p.contains('\n')).then_some(p)
}

/// `scan_agy(path, dialogue_chars, after)`: stream one agy log into a SessionScan, as
/// `scan_transcript` does a Claude Code one. Owner prompts are `USER_INPUT` rows from
/// `USER_EXPLICIT`; agent text is `PLANNER_RESPONSE` content. A commit is a new
/// `[<branch> <sha>] <subject>` line in a `GENERIC` output row (or the full output file a cut
/// row names) after a step that ran `git commit` or whose command agy cut.
/// Tool paths come from every row; with `after`, prompts, commits, ids, writes and the dialogue
/// come only from rows later than it.
pub fn scan_agy(
    path: &PyPath,
    dialogue_chars: i64,
    after: Option<f64>,
) -> Result<SessionScan, SmError> {
    let mut scan = SessionScan {
        session_id: conversation_id(path),
        after,
        ..SessionScan::default()
    };
    let mut dialogue = Dialogue::new(dialogue_chars.div_euclid(2));
    let mut puts: Vec<(String, f64)> = Vec::new();
    let mut commit_step = false;
    let mut lines = TextLines::open(path)?;
    while let Some(raw) = lines.next_bytes()? {
        let prompt_row = contains(&raw, br#""type":"USER_INPUT""#);
        let step_row = contains(&raw, br#""type":"PLANNER_RESPONSE""#);
        let output_row = commit_step && contains(&raw, br#""type":"GENERIC""#);
        if !prompt_row && !step_row && !output_row {
            continue;
        }
        let Some(PyValue::Dict(row)) = row_of(&raw)? else {
            continue;
        };
        let ts = str_of(dict_get(&row, "created_at")).to_string();
        if !ts.is_empty() {
            scan.last_ts = ts;
        }
        let at = epoch_of(&scan.last_ts)?;
        let later = after.is_none_or(|t| at.is_some_and(|r| r > t));
        let content = str_of(dict_get(&row, "content"));
        let kind = str_of(dict_get(&row, "type"));
        if kind == "USER_INPUT" {
            let prompt = request_text(content);
            if !later || str_of(dict_get(&row, "source")) != "USER_EXPLICIT" || prompt.is_empty() {
                continue;
            }
            scan.prompts += 1;
            scan.prompt_times.push(at.unwrap_or(0.0));
            if scan.first_prompt.is_empty() {
                scan.first_prompt = prompt.to_string();
            }
            for m in TASK_ID_RE.finditer(prompt) {
                add_unique(&mut scan.task_ids, m.as_str());
            }
            dialogue.add(format!(
                "Owner: {}",
                slice(prompt, None, Some(MAX_MESSAGE_CHARS))
            ));
        } else if kind == "GENERIC" {
            if !later {
                continue;
            }
            let full = full_output(path, content);
            let mut found = COMMIT_LINE_RE.finditer(content);
            found.extend(COMMIT_LINE_RE.finditer(&full));
            for m in found {
                let sha = m.group(1).unwrap_or("");
                if scan.shas.iter().any(|s| s == sha) {
                    continue;
                }
                add_unique(&mut scan.shas, sha);
                add_unique(&mut scan.commit_subjects, strip(m.group(2).unwrap_or("")));
                scan.commits += 1;
                scan.commit_times.push(at.unwrap_or(0.0));
            }
        } else if kind == "PLANNER_RESPONSE" {
            commit_step = false;
            let text = strip(content);
            if later && !text.is_empty() {
                for m in TASK_ID_RE.finditer(text) {
                    add_unique(&mut scan.task_ids, m.as_str());
                }
                dialogue.add(format!(
                    "Agent: {}",
                    slice(text, None, Some(MAX_MESSAGE_CHARS))
                ));
            }
            let calls = match dict_get(&row, "tool_calls") {
                Some(PyValue::List(calls)) => calls.as_slice(),
                _ => &[],
            };
            for call in calls {
                let PyValue::Dict(call) = call else { continue };
                let name = str_of(dict_get(call, "name"));
                let Some(PyValue::Dict(args)) = dict_get(call, "args") else {
                    continue;
                };
                for (key, value) in args {
                    let PyValue::Str(value) = value else { continue };
                    let text = arg_text(value);
                    if let Some(p) = arg_path(&text) {
                        scan.tool_paths.push(p.to_string());
                        if later && WRITE_TOOLS.contains(&name) {
                            add_unique(&mut scan.files_written, p);
                        }
                    }
                    if name != "run_command" || str_of(Some(key)) != "CommandLine" {
                        continue;
                    }
                    // a cut command may hide its `git commit`; the output lines still decide
                    let may_commit = COMMIT_RE.search(&text).is_some() || text.contains(CUT_ARG);
                    if later && may_commit && !text.contains("--dry-run") {
                        commit_step = true;
                    }
                    if puts_to_store(&text) {
                        // no output row names its call, so a put that failed still covers
                        puts.push((String::new(), at.unwrap_or(f64::INFINITY)));
                    }
                }
            }
        }
    }
    scan.dialogue = dialogue.lines();
    scan.put_until = newest(&puts);
    Ok(scan)
}

/// The conversation ids agy-dispatch started (the `session.id` in each ledger row's
/// `task_dir`); empty when the ledger file is missing. A row whose folder or id file is gone
/// names no id.
pub fn dispatched_ids(ledger: &PyPath) -> Result<HashSet<String>, SmError> {
    let mut ids = HashSet::new();
    if !ledger.is_file() {
        return Ok(ids);
    }
    let mut lines = TextLines::open(ledger)?;
    while let Some(raw) = lines.next_bytes()? {
        let Some(PyValue::Dict(row)) = row_of(&raw)? else {
            continue;
        };
        let dir = str_of(dict_get(&row, "task_dir"));
        if dir.is_empty() {
            continue;
        }
        let id_file = PyPath::new(dir).join("session.id");
        if id_file.is_file() {
            let id = strip(&id_file.read_text(Errors::Strict)?).to_string();
            if !id.is_empty() {
                ids.insert(id);
            }
        }
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_text_keeps_the_owner_part() {
        assert_eq!(
            request_text("<USER_REQUEST>\n fix a.py \n</USER_REQUEST>\n<ADDITIONAL_METADATA>clock"),
            "fix a.py"
        );
        assert_eq!(
            request_text("plain ask\n<ADDITIONAL_METADATA>x"),
            "plain ask"
        );
        assert_eq!(request_text("<USER_REQUEST>\n\n</USER_REQUEST>"), "");
    }

    #[test]
    fn arg_path_takes_one_absolute_path() {
        assert_eq!(arg_text("\"/u/a.py\""), "/u/a.py");
        assert_eq!(arg_text("ls /u"), "ls /u");
        assert_eq!(arg_path("file:///u/code/web"), Some("/u/code/web"));
        assert_eq!(arg_path("/u/a.py"), Some("/u/a.py"));
        assert_eq!(arg_path("/// doc\n/// more"), None);
        assert_eq!(arg_path("cd /u/code/web"), None);
    }
}
