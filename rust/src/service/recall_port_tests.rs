//! Rust ports of the Python recall, recall state and recall sequence tests
//! (Python main 555b8f2). Expected values come from those tests.
//!
//! Python patches module constants and the clock; Rust has constants and a real clock. The ports
//! reach the same observable values another way, each marked where it happens:
//! - `LOCAL_TAU` is 8.0, so a store holds filler items until `ln(N / df)` clears the floor.
//! - A clock step is a backoff `until` moved earlier in hook/backoff.json (`shift_window`).
//! - The hook's catch-up start is off in these homes; `rust/tests/hook_catchup.rs` runs the real one.

use std::cell::RefCell;

use serde_json::{Value as Json, json};

use super::*;
use crate::service::capture::{Run, run_process};
use crate::service::put::{PutArgs, run_put};
use crate::store::connect::connect;
use crate::store::items::put_items_batch;

pub(crate) struct PyHome {
    pub(crate) _dir: tempfile::TempDir,
    pub(crate) path: String,
}

impl PyHome {
    pub(crate) fn file(&self, rel: &str) -> String {
        format!("{}/{rel}", self.path)
    }
}

/// `(home / "config.toml").write_text(text)`, with the catch-up start off unless the text says.
pub(crate) fn set_config(h: &PyHome, text: &str) {
    let off = if text.contains("[catch_up]") {
        ""
    } else {
        "[catch_up]\nenabled = false\n"
    };
    std::fs::write(h.file("config.toml"), format!("{off}{text}")).unwrap();
}

/// `run_put(text, tag=..., kind=..., repo=..., home=...)`: the new id.
pub(crate) fn put(h: &PyHome, text: &str, tag: &str, kind: &str, repo: &str) -> String {
    let a = PutArgs {
        tag,
        kind,
        repo,
        home: Some(&h.path),
        ..Default::default()
    };
    run_put(text, &a).unwrap().0
}

/// `run_put(text, title=text, ...)`: the title is not cut to 120 chars.
pub(crate) fn put_titled(h: &PyHome, text: &str, tag: &str) -> String {
    let a = PutArgs {
        tag,
        title: text,
        repo: "global",
        home: Some(&h.path),
        ..Default::default()
    };
    run_put(text, &a).unwrap().0
}

pub(crate) fn put_refs(h: &PyHome, text: &str, tag: &str, refs: &str) -> String {
    let a = PutArgs {
        tag,
        refs,
        repo: "global",
        home: Some(&h.path),
        ..Default::default()
    };
    run_put(text, &a).unwrap().0
}

/// `n` items of `repo` with the given titles, in one transaction. A title is also the body.
pub(crate) fn bulk(h: &PyHome, repo: &str, titles: impl Iterator<Item = String>) {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let conn = connect(&h.file("memory.db"), None).unwrap();
    let items: Vec<Item> = titles
        .map(|t| {
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Item::new(&format!("m_bulk_{n:08}"), repo, "fact", &t, &t, "decided")
        })
        .collect();
    put_items_batch(&conn, &items, false).unwrap();
}

/// Items that no test prompt holds: `n` in repo global, each with its own words.
pub(crate) fn fillers(h: &PyHome, n: usize) {
    bulk(h, "global", (0..n).map(|i| format!("pad{i} padx{i}")));
}

/// The Python `home` fixture: one decision item, the project code ZZ, the catch-up start off.
/// `n_fillers` extra items stand in for the patched `LOCAL_TAU = 0.0`: with 120 of them each held
/// word has idf `ln(121)` = 4.8, so the two words r6 and revert sum to 9.6 >= 8.0.
pub(crate) fn py_home_with(n_fillers: usize) -> PyHome {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap().to_string();
    let h = PyHome { _dir: dir, path };
    set_config(
        &h,
        "[prompt_hook]\n[prompt_hook.projects]\nZZ = \"Zeta Zone project\"\n",
    );
    put(
        &h,
        "R6 stays reverted: narrowing prune_stale() loses old catches",
        "decided",
        "decision",
        "global",
    );
    fillers(&h, n_fillers);
    h
}

pub(crate) fn py_home() -> PyHome {
    py_home_with(120)
}

pub(crate) fn pl(v: Json) -> PyValue {
    loads(&v.to_string()).unwrap()
}

/// One recorded runner call. `env` keeps the two names the hook sets.
pub(crate) struct Rec {
    pub(crate) cmd: Vec<String>,
    pub(crate) input: String,
    pub(crate) timeout: f64,
    pub(crate) env: Vec<(String, String)>,
    pub(crate) cwd: Option<String>,
}

pub(crate) enum Ans {
    Out(i32, String),
    Fail(&'static str, String),
}

/// `FakeClaude`: answers (code, stdout) with stderr `boom`, or raises, and keeps each call.
pub(crate) struct PFake {
    pub(crate) ans: Ans,
    pub(crate) calls: RefCell<Vec<Rec>>,
}

impl PFake {
    pub(crate) fn out(stdout: &str) -> PFake {
        PFake {
            ans: Ans::Out(0, stdout.into()),
            calls: RefCell::new(Vec::new()),
        }
    }

    pub(crate) fn exit(code: i32) -> PFake {
        PFake {
            ans: Ans::Out(code, String::new()),
            calls: RefCell::new(Vec::new()),
        }
    }

    pub(crate) fn timeout() -> PFake {
        PFake {
            ans: Ans::Fail("TimeoutExpired", String::new()),
            calls: RefCell::new(Vec::new()),
        }
    }

    pub(crate) fn fail(class: &'static str, msg: &str) -> PFake {
        PFake {
            ans: Ans::Fail(class, msg.into()),
            calls: RefCell::new(Vec::new()),
        }
    }

    pub(crate) fn n(&self) -> usize {
        self.calls.borrow().len()
    }

    pub(crate) fn run(
        &self,
        cmd: &[String],
        input: &str,
        timeout: f64,
        env: &[(&str, &str)],
        cwd: Option<&str>,
    ) -> Result<Completed, SmError> {
        let env = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        self.calls.borrow_mut().push(Rec {
            cmd: cmd.to_vec(),
            input: input.into(),
            timeout,
            env,
            cwd: cwd.map(String::from),
        });
        match &self.ans {
            Ans::Out(code, out) => Ok(Completed {
                returncode: *code,
                stdout: out.clone(),
                stderr: "boom".into(),
            }),
            Ans::Fail("TimeoutExpired", _) => Err(timeout_expired("claude", timeout)),
            Ans::Fail(class, msg) => Err(SmError::from_python(*class, msg.clone())),
        }
    }
}

pub(crate) fn run_hook(
    h: &PyHome,
    f: &PFake,
    payload: &PyValue,
    agy: bool,
) -> Result<Option<Recall>, SmError> {
    let r =
        |c: &[String], i: &str, t: f64, e: &[(&str, &str)], w: Option<&str>| f.run(c, i, t, e, w);
    run_prompt_hook(payload, agy, Some(&h.path), &r)
}

pub(crate) fn hook_ok(h: &PyHome, f: &PFake, payload: Json) -> Option<Recall> {
    run_hook(h, f, &pl(payload), false).unwrap()
}

pub(crate) fn rows(h: &PyHome) -> Vec<Json> {
    std::fs::read_to_string(h.file("hook.log"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// The newest row without the keys that vary per run.
pub(crate) fn local_row(h: &PyHome) -> Json {
    let mut row = rows(h).pop().unwrap();
    for k in [
        "ts",
        "ms",
        "rewrite_ms",
        "db_ms",
        "git_ms",
        "fin_ms",
        "session",
        "pos",
        "harness",
    ] {
        row.as_object_mut().unwrap().remove(k);
    }
    row
}

fn text_of(r: &Recall) -> Vec<String> {
    r.items.iter().map(|it| it.body.clone()).collect()
}

fn backoff_file(h: &PyHome) -> String {
    h.file("hook/backoff.json")
}

fn session_file(h: &PyHome, session: &str) -> String {
    format!(
        "{}/hook/claude-{}.json",
        h.path,
        &sha_hex(session).unwrap()[..16]
    )
}

fn write_state(path: &str, text: &str) {
    std::fs::create_dir_all(std::path::Path::new(path).parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// The clock stepping `secs` forward: the open window's `until` moves `secs` earlier.
fn shift_window(h: &PyHome, secs: f64) {
    let p = PyPath::new(&backoff_file(h));
    let mut b = Backoff::read(&p).unwrap();
    b.until -= secs;
    b.write(&p).unwrap();
}

/// `time.strftime("%H:%M", time.localtime(t))`.
fn hm(t: f64) -> String {
    clock::strftime_hm(&clock::localtime(Some(t)).unwrap()).unwrap()
}

/// The notice names `until` = the failing prompt's time + 600 s. The clock is read before and
/// after the call, so a minute boundary inside the call gives one of two texts.
fn one_of_until(got: &str, make: impl Fn(&str) -> String, before: f64, after: f64) {
    let (a, b) = (make(&hm(before + 600.0)), make(&hm(after + 600.0)));
    assert!(got == a || got == b, "{got:?} is neither {a:?} nor {b:?}");
}

const ERROR_NOTICE_PRE: &str = "sm recall: the query model failed twice in a row";
const ERROR_NOTICE_POST: &str = "(10-minute backoff). Fix: put `claude` on PATH or set [prompt_hook] claude_bin; to use local keywords only, set [prompt_hook] model = \"none\".";

fn error_notice(paren: &str, end: &str) -> String {
    format!("{ERROR_NOTICE_PRE}{paren}, so local keywords stand in until {end} {ERROR_NOTICE_POST}")
}

fn timeout_notice(end: &str) -> String {
    format!(
        "sm recall: the query model timed out twice in a row, so local keywords stand in until {end} (10-minute backoff); then sm tries the model again."
    )
}

// ------------------------------------------------------------------ needs_recall and model input

#[test]
fn needs_recall_skips_commands_wrappers_and_short_replies() {
    // test_needs_recall_skips_commands_wrappers_and_short_replies
    assert!(!needs_recall(""));
    assert!(!needs_recall("/compact"));
    assert!(!needs_recall(
        "<task-notification> agent run done with exit 0 </task-notification>"
    ));
    assert!(!needs_recall("ok go"));
    assert!(needs_recall("drop editor config"));
}

const PASTED_LED: &str = "\n\n<pasted_content id=\"p1\">\nrow A 10\nrow B 12\n</pasted_content id=\"p1\">\nwhy does row B differ from the QX7 deck?";

#[test]
fn needs_recall_keeps_a_prompt_led_by_pasted_or_owner_markup() {
    // test_needs_recall_keeps_a_prompt_led_by_pasted_or_owner_markup (3 params)
    for prompt in [
        PASTED_LED,
        "<launch-selected-element>\n<element>div.card</element>\n</launch-selected-element>\nmake this card wider please",
        "<div class=\"a\">x</div> fix this layout please",
    ] {
        assert!(needs_recall(prompt), "{prompt:?}");
    }
}

#[test]
fn needs_recall_skips_prompts_that_claude_code_writes() {
    // test_needs_recall_skips_prompts_that_claude_code_writes (10 params)
    for prompt in [
        "<task-notification>\n<task-id>a1</task-id>\n<status>completed</status>\n</task-notification>",
        "<command-name>/model</command-name>\n<command-message>model</command-message>\n<command-args></command-args>",
        "<command-message>app-checkpoint</command-message>\n<command-name>/app-checkpoint</command-name>",
        "<local-command-stdout>Set model to opus and more words</local-command-stdout>",
        "<teammate-message teammate_id=\"t1\" summary=\"done\">lane A done with three words</teammate-message>",
        "<system-reminder>\nThe user opened the file a.py in the IDE.\n</system-reminder>",
        "<bash-input>ls -la some dir</bash-input>",
        "<bash-stdout>a.txt b.txt c.txt</bash-stdout><bash-stderr></bash-stderr>",
        "<local-command-caveat>Caveat: the messages below came from local commands.</local-command-caveat>",
        "<artifact-content-authored-by-others/>This session is being continued from a previous conversation. Summary: A did B.",
    ] {
        assert!(!needs_recall(prompt), "{prompt:?}");
    }
}

#[test]
fn a_pasted_led_prompt_reaches_the_model() {
    // test_pasted_led_prompt_reaches_the_model
    let h = py_home();
    let fake = PFake::out("QX7 deck row\n");
    hook_ok(
        &h,
        &fake,
        json!({"prompt": PASTED_LED, "cwd": "/nonexistent-sm-r2"}),
    );
    assert_eq!(fake.n(), 1);
    assert!(
        fake.calls.borrow()[0]
            .input
            .contains("why does row B differ from the QX7 deck?")
    );
}

fn long_log() -> String {
    (0..260)
        .map(|i| {
            format!(
                "2026-01-01 10:{:02}:00 INFO step {i} rows={} status=ok",
                i % 60,
                i * 7
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

const QUESTION: &str = "why did the QX7 pipeline fail?";

#[test]
fn model_input_keeps_head_and_tail_of_a_long_prompt() {
    // test_model_input_keeps_head_and_tail_of_a_long_prompt
    let prompt = format!("{}\n{QUESTION}", long_log());
    let sent = rewrite_input(&prompt);
    assert!(len(&prompt) > 12000);
    assert_eq!(len(&sent), 2000);
    assert!(sent.starts_with("2026-01-01 10:00:00 INFO step 0 rows=0 status=ok\n"));
    assert!(sent.contains("\n[...]\n"));
    assert!(sent.ends_with(&format!("status=ok\n{QUESTION}")));
}

#[test]
fn model_input_puts_owner_words_before_the_pasted_block() {
    // test_model_input_puts_owner_words_before_the_pasted_block
    let pasted = (0..400)
        .map(|i| format!("line {i} of the pasted report"))
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = format!(
        "\n\n<pasted_content id=\"p1\">\n{pasted}\n</pasted_content id=\"p1\">\n{QUESTION}"
    );
    let sent = rewrite_input(&prompt);
    assert_eq!(len(&sent), 2000);
    assert!(sent.starts_with(&format!(
        "{QUESTION}\n\n[pasted]\nline 0 of the pasted report\nline 1 of"
    )));
    assert!(!sent.contains("pasted_content"));
}

#[test]
fn model_input_takes_owner_words_from_around_several_blocks() {
    // test_model_input_takes_owner_words_from_around_several_blocks
    let prompt = "compare <pasted_content id=\"a\">alpha rows</pasted_content id=\"a\"> with <pasted_content id=\"b\">beta rows</pasted_content id=\"b\"> for QX7";
    assert_eq!(
        rewrite_input(prompt),
        "compare with for QX7\n\n[pasted]\nalpha rows\n\nbeta rows"
    );
}

#[test]
fn model_input_boundaries() {
    // test_model_input_boundaries (5 params)
    let x2000 = "x".repeat(2000);
    for (prompt, expected) in [
        ("-R6 status now".to_string(), "-R6 status now".to_string()),
        (x2000.clone(), x2000),
        (
            "fix QX7 <pasted_content id=\"c\">\ntrace row one\ntrace row two".into(),
            "fix QX7\n\n[pasted]\ntrace row one\ntrace row two".into(),
        ),
        (
            "<pasted_content id=\"d\">\nonly pasted words here\n</pasted_content id=\"d\">".into(),
            "only pasted words here".into(),
        ),
    ] {
        assert_eq!(rewrite_input(&prompt), expected);
    }
}

#[test]
fn model_input_cuts_long_owner_words_and_drops_the_paste() {
    // test_model_input_cuts_long_owner_words_and_drops_the_paste
    let owner = format!("{} middle {}", "a".repeat(1500), "z".repeat(1500));
    let sent = rewrite_input(&format!(
        "{owner}\n<pasted_content id=\"e\">pasted body</pasted_content id=\"e\">"
    ));
    assert_eq!(len(&sent), 2000);
    assert!(sent.starts_with(&format!("{}\n[...]\n", "a".repeat(996))));
    assert!(sent.ends_with(&"z".repeat(997)));
    assert!(!sent.contains("pasted body"));
}

fn rewrite(prompt: &str, f: &PFake) -> Result<String, SmError> {
    let r =
        |c: &[String], i: &str, t: f64, e: &[(&str, &str)], w: Option<&str>| f.run(c, i, t, e, w);
    rewrite_query(prompt, "SYS", "haiku", 8.0, "c", None, &r)
}

#[test]
fn rewrite_query_sends_the_question_after_a_long_paste() {
    // test_rewrite_query_sends_the_question_after_a_long_paste
    let fake = PFake::out("QX7 pipeline failure\n");
    rewrite(&format!("{}\n{QUESTION}", long_log()), &fake).unwrap();
    assert!(fake.calls.borrow()[0].input.ends_with(QUESTION));
}

#[test]
fn build_rewrite_prompt_adds_projects_only_when_given() {
    // test_build_rewrite_prompt_adds_projects_only_when_given
    assert!(
        !build_rewrite_prompt(&PyValue::Dict(vec![]))
            .unwrap()
            .contains("Project codes")
    );
    let with =
        build_rewrite_prompt(&PyValue::Dict(vec![(s("ZZ"), s("Zeta Zone project"))])).unwrap();
    assert!(with.contains("- ZZ: Zeta Zone project"));
}

#[test]
fn rewrite_query_sends_prompt_on_stdin_with_hooks_off_and_cleans_output() {
    // test_rewrite_query_sends_prompt_on_stdin_with_hooks_off_and_cleans_output
    let fake = PFake::out("\n\"R6 pruning revert\"\nextra line\n");
    let r = |c: &[String], i: &str, t: f64, e: &[(&str, &str)], w: Option<&str>| {
        fake.run(c, i, t, e, w)
    };
    assert_eq!(
        rewrite_query("-R6 status now", "SYS", "haiku", 8.0, "claude", None, &r).unwrap(),
        "R6 pruning revert"
    );
    let calls = fake.calls.borrow();
    assert_eq!(calls[0].input, "-R6 status now");
    assert!(!calls[0].cmd.contains(&"-R6 status now".to_string()));
    assert!(
        calls[0]
            .cmd
            .contains(&"{\"disableAllHooks\":true}".to_string())
    );
    assert!(
        calls[0]
            .env
            .contains(&("SM_PROMPT_HOOK".into(), "0".into()))
    );
    assert_eq!(calls[0].timeout, 8.0);
    assert_eq!(rewrite("x y z", &PFake::out("NONE\n")).unwrap(), "");
    let e = rewrite("x y z", &PFake::exit(1)).unwrap_err();
    assert_eq!(
        (e.class.as_ref(), e.msg.as_str()),
        ("RuntimeError", "claude exited 1: boom")
    );
}

#[test]
fn rewrite_query_turns_off_nonessential_claude_traffic() {
    // test_rewrite_query_turns_off_nonessential_claude_traffic (params: inherited None and "0").
    // rewrite_query sets the pair itself with no read of the inherited value, so the inherited
    // value cannot matter; the process env is shared by parallel tests and is not set here.
    let fake = PFake::out("R6\n");
    rewrite("x y z", &fake).unwrap();
    let env = fake.calls.borrow()[0].env.clone();
    assert_eq!(
        env.iter()
            .filter(|(k, _)| k == "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC")
            .map(|(_, v)| v.as_str())
            .collect::<Vec<_>>(),
        ["1"]
    );
}

// ------------------------------------------------------------------ run_prompt_hook basics

#[test]
fn run_prompt_hook_queries_with_project_codes_and_logs() {
    // test_run_prompt_hook_queries_with_project_codes_and_logs
    let h = py_home();
    let fake = PFake::out("R6 pruning revert\n");
    let found = hook_ok(
        &h,
        &fake,
        json!({"prompt": "does R6 revert still", "cwd": "/u/work/zz"}),
    )
    .unwrap();
    assert_eq!((found.notice.as_str(), found.fallback.as_str()), ("", ""));
    assert_eq!(found.query, "R6 pruning revert");
    assert!(found.items[0].body.contains("R6"));
    let calls = fake.calls.borrow();
    let cmd = &calls[0].cmd;
    let system = &cmd[cmd.iter().position(|a| a == "--system-prompt").unwrap() + 1];
    assert!(system.contains("- ZZ: Zeta Zone project"));
    assert_eq!(calls[0].timeout, 8.0);
    // the model runs in the hook's own folder (rewrite_query passes cwd=home), not the prompt's
    assert_eq!(calls[0].cwd.as_deref(), Some(h.path.as_str()));
    let last = rows(&h).pop().unwrap();
    assert_eq!(last["query"], "R6 pruning revert");
    assert_eq!(last["ids"], json!([found.items[0].id]));
}

#[test]
fn run_prompt_hook_passes_configured_timeout_and_logs_it() {
    // test_run_prompt_hook_passes_configured_timeout_and_logs_it
    let h = py_home();
    set_config(&h, "[prompt_hook]\ntimeout = 2.5\n");
    let fake = PFake::timeout();
    let found = hook_ok(
        &h,
        &fake,
        json!({"prompt": "does R6 revert still", "cwd": "/nonexistent-sm-r2"}),
    )
    .unwrap();
    assert_eq!(found.fallback, "timeout");
    assert_eq!(fake.calls.borrow()[0].timeout, 2.5);
    assert_eq!(
        rows(&h).pop().unwrap()["error"],
        "rewrite timed out after 2.5 s"
    );
}

#[test]
fn run_prompt_hook_off_switches() {
    // test_run_prompt_hook_off_switches: the `enabled = false` half. The SM_PROMPT_HOOK=0 half
    // sets a process variable that parallel tests read; rust/tests/hook_catchup.rs runs it.
    let h = py_home();
    set_config(&h, "[prompt_hook]\nenabled = false\n");
    let fake = PFake::out("R6");
    assert_eq!(
        hook_ok(&h, &fake, json!({"prompt": "does R6 revert still"})),
        None
    );
    assert_eq!(fake.n(), 0);
}

fn agy_transcript(h: &PyHome, lines: &[Json]) -> String {
    let path = h.file("t.jsonl");
    std::fs::write(
        &path,
        lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
    )
    .unwrap();
    path
}

#[test]
fn agy_prompt_from_transcript_recalls_once_per_prompt() {
    // test_agy_prompt_from_transcript_recalls_once_per_prompt
    let h = py_home();
    let t = agy_transcript(
        &h,
        &[
            json!({"type": "USER_INPUT", "content": "first ask here"}),
            json!({"type": "PLANNER_RESPONSE", "content": "x"}),
            json!({"type": "USER_INPUT", "content": "is R6 reverted yet"}),
        ],
    );
    let payload =
        pl(json!({"conversationId": "c1", "transcriptPath": t, "cwd": "/nonexistent-sm-r2"}));
    assert_eq!(extract_prompt(&payload).unwrap(), "is R6 reverted yet");
    let fake = PFake::out("R6 pruning");
    assert!(run_hook(&h, &fake, &payload, true).unwrap().is_some());
    assert!(run_hook(&h, &fake, &payload, true).unwrap().is_none());
    assert_eq!(fake.n(), 1);
    let mut f = std::fs::OpenOptions::new().append(true).open(&t).unwrap();
    use std::io::Write;
    writeln!(
        f,
        "{}",
        json!({"type": "USER_INPUT", "content": "how does R6 revert work"})
    )
    .unwrap();
    assert!(run_hook(&h, &fake, &payload, true).unwrap().is_some());
    assert_eq!(fake.n(), 2);
}

#[test]
fn agy_transcript_unwraps_request_skips_short_slash_and_system_and_resets_on_shrink() {
    let h = session_home();
    let wrapped_ok = agy_transcript(
        &h,
        &[json!({
            "type": "USER_INPUT",
            "source": "USER_EXPLICIT",
            "content": "<USER_REQUEST>\nok\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: 2026-11-19T00:00:00+00:00.\n</ADDITIONAL_METADATA>"
        })],
    );
    let p_ok = pl(json!({
        "conversationId": "c-wrap",
        "transcriptPath": wrapped_ok,
        "cwd": "/u/work/zz"
    }));
    assert_eq!(extract_prompt(&p_ok).unwrap(), "ok");
    let fake = PFake::out("K9");
    assert_eq!(run_hook(&h, &fake, &p_ok, true).unwrap(), None);
    assert_eq!(fake.n(), 0);

    let wrapped_slash = agy_transcript(
        &h,
        &[json!({
            "type": "USER_INPUT",
            "source": "USER_EXPLICIT",
            "content": "<USER_REQUEST>\n/compact\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: 2026-11-19T00:00:00+00:00.\n</ADDITIONAL_METADATA>"
        })],
    );
    let p_slash = pl(json!({
        "conversationId": "c-wrap",
        "transcriptPath": wrapped_slash,
        "cwd": "/u/work/zz"
    }));
    assert_eq!(extract_prompt(&p_slash).unwrap(), "/compact");
    assert_eq!(run_hook(&h, &fake, &p_slash, true).unwrap(), None);
    assert_eq!(fake.n(), 0);

    // Normal request followed by a SYSTEM entry: extracts only USER_EXPLICIT unwrapped text.
    let t = agy_transcript(
        &h,
        &[
            json!({
                "type": "USER_INPUT",
                "source": "USER_EXPLICIT",
                "content": "<USER_REQUEST>\nK9 deploy status check\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: 2026-11-19T00:00:00+00:00.\n</ADDITIONAL_METADATA>"
            }),
            json!({
                "type": "USER_INPUT",
                "source": "SYSTEM",
                "content": "internal system prompt should be ignored"
            }),
        ],
    );
    let p = pl(json!({
        "conversationId": "c-shrink",
        "transcriptPath": t,
        "cwd": "/u/work/zz"
    }));
    assert_eq!(extract_prompt(&p).unwrap(), "K9 deploy status check");
    let first: Vec<String> = run_hook(&h, &fake, &p, true)
        .unwrap()
        .unwrap()
        .items
        .into_iter()
        .map(|it| it.id)
        .collect();
    assert_eq!(first.len(), 2);

    // Next turn appends a new request: pos=later, dropped=first.
    append(
        &t,
        &format!(
            "{}\n",
            json!({
                "type": "USER_INPUT",
                "source": "USER_EXPLICIT",
                "content": "<USER_REQUEST>\nK9 deploy next step check\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: 2026-11-19T00:01:00+00:00.\n</ADDITIONAL_METADATA>"
            })
        ),
    );
    let second: Vec<String> = run_hook(&h, &fake, &p, true)
        .unwrap()
        .unwrap()
        .items
        .into_iter()
        .map(|it| it.id)
        .collect();
    assert!(disjoint(&second, &first));

    // Transcript shrinks (shorter than previous transcript_size): resets pos to first and clears injected.
    std::fs::write(
        &t,
        format!(
            "{}\n",
            json!({"type": "USER_INPUT", "content": "<USER_REQUEST>\nK9 fresh ask\n</USER_REQUEST>"})
        ),
    )
    .unwrap();
    let after_shrink: Vec<String> = run_hook(&h, &fake, &p, true)
        .unwrap()
        .unwrap()
        .items
        .into_iter()
        .map(|it| it.id)
        .collect();
    assert_eq!(after_shrink, first);
    let r = rows(&h);
    assert_eq!(
        r.iter().map(|x| x["pos"].clone()).collect::<Vec<_>>(),
        [json!("first"), json!("later"), json!("first")]
    );
}

#[test]
fn agy_unwrap_edges_direct_payload_legacy_gate_and_omitted_transcript_preserve_state() {
    let h = session_home();
    // Unclosed <USER_REQUEST> bounded by fallback wrapper tags, literal </USER_REQUEST> in body,
    // and mid-sentence <USER_REQUEST> without leading wrapper.
    assert_eq!(
        unwrap_agy_user_input(
            "<USER_REQUEST>\nK9 unclosed meta\n<ADDITIONAL_METADATA>\nx\n</ADDITIONAL_METADATA>"
        ),
        "K9 unclosed meta"
    );
    assert_eq!(
        unwrap_agy_user_input("<USER_REQUEST>\nK9 unclosed settings\n<USER_SETTINGS_CHANGE>\nx"),
        "K9 unclosed settings"
    );
    assert_eq!(
        unwrap_agy_user_input(
            "<USER_REQUEST>\ncheck </USER_REQUEST> for K9 deploy note\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nx"
        ),
        "check </USER_REQUEST> for K9 deploy note"
    );
    assert_eq!(
        unwrap_agy_user_input("prefix <USER_REQUEST> mid mention"),
        "prefix <USER_REQUEST> mid mention"
    );

    // Direct wrapped lastUserInput unwraps before needs_recall / rewrite, and omitting
    // transcriptPath on Turn 2 must NOT reset transcript_size or bypass same-prompt gating.
    let tr = agy_transcript(
        &h,
        &[json!({
            "type": "USER_INPUT",
            "source": "USER_EXPLICIT",
            "content": "<USER_REQUEST>\nK9 deploy status check\n</USER_REQUEST>"
        })],
    );
    let tr_size = std::fs::metadata(&tr).unwrap().len() as i128;
    let fake = PFake::out("K9");
    let turn1 = run_hook(
        &h,
        &fake,
        &pl(json!({
            "conversationId": "c-omit",
            "transcriptPath": tr,
            "lastUserInput": "<USER_REQUEST>\nK9 deploy status check\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nx",
            "cwd": "/u/work/zz"
        })),
        true,
    )
    .unwrap()
    .unwrap();
    let turn1_ids: Vec<String> = turn1.items.iter().map(|it| it.id.clone()).collect();
    assert_eq!(turn1_ids.len(), 2);

    // Same prompt with omitted transcriptPath and null transcriptPath: gated (returns None),
    // keeps transcript_size > 0 and keeps injected ids.
    for extra in [json!({}), json!({"transcriptPath": null})] {
        let mut obj = serde_json::Map::new();
        obj.insert("conversationId".into(), json!("c-omit"));
        obj.insert(
            "lastUserInput".into(),
            json!(
                "<USER_REQUEST>\nK9 deploy status check\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nx"
            ),
        );
        obj.insert("cwd".into(), json!("/u/work/zz"));
        for (k, v) in extra.as_object().unwrap() {
            obj.insert(k.clone(), v.clone());
        }
        assert_eq!(
            run_hook(&h, &fake, &pl(serde_json::Value::Object(obj)), true).unwrap(),
            None
        );
    }
    assert_eq!(fake.n(), 1);
    let state_path = PyPath::new(&format!(
        "{}/hook/agy-{}.json",
        h.path,
        &sha_hex("c-omit").unwrap()[..16]
    ));
    let (st, sha) = SessionState::read_with_prompt(&state_path)
        .unwrap()
        .unwrap();
    assert_eq!(st.transcript_size, tr_size);
    assert_eq!(st.injected.len(), 2);
    assert_eq!(
        sha.as_deref(),
        Some(sha_hex("K9 deploy status check").unwrap().as_str())
    );

    // A new prompt via lastUserInput with omitted transcriptPath preserves prior transcript_size
    // and carries forward prior injected ids, so a subsequent transcript shrink resets injected
    // and re-injects the first-turn items.
    let turn2 = run_hook(
        &h,
        &fake,
        &pl(json!({
            "conversationId": "c-omit",
            "lastUserInput": "<USER_REQUEST>\nK9 followup without path\n</USER_REQUEST>",
            "cwd": "/u/work/zz"
        })),
        true,
    )
    .unwrap()
    .unwrap();
    let turn2_ids: Vec<String> = turn2.items.iter().map(|it| it.id.clone()).collect();
    assert_eq!(turn2_ids.len(), 2);
    assert!(disjoint(&turn2_ids, &turn1_ids));
    let (st_after_t2, _) = SessionState::read_with_prompt(&state_path)
        .unwrap()
        .unwrap();
    assert_eq!(st_after_t2.transcript_size, tr_size);
    assert_eq!(st_after_t2.injected.len(), 4);

    std::fs::write(&tr, b"{}").unwrap();
    assert!((std::fs::metadata(&tr).unwrap().len() as i128) < tr_size);
    let turn3 = run_hook(
        &h,
        &fake,
        &pl(json!({
            "conversationId": "c-omit",
            "transcriptPath": tr,
            "lastUserInput": "<USER_REQUEST>\nK9 shrunk transcript check\n</USER_REQUEST>",
            "cwd": "/u/work/zz"
        })),
        true,
    )
    .unwrap()
    .unwrap();
    let turn3_ids: Vec<String> = turn3.items.iter().map(|it| it.id.clone()).collect();
    assert_eq!(turn3_ids, turn1_ids);
    let (st_after_t3, _) = SessionState::read_with_prompt(&state_path)
        .unwrap()
        .unwrap();
    assert_eq!(st_after_t3.transcript_size, 2);
    assert_eq!(st_after_t3.injected.len(), 2);

    // Direct wrapped slash command via lastUserInput is skipped after unwrapping.
    assert_eq!(
        run_hook(
            &h,
            &fake,
            &pl(json!({
                "conversationId": "c-omit",
                "lastUserInput": "<USER_REQUEST>\n/compact\n</USER_REQUEST>",
                "cwd": "/u/work/zz"
            })),
            true,
        )
        .unwrap(),
        None
    );

    // Legacy plain-text hook/agy-<sha16> prompt digest gates same-prompt repeats before .json exists.
    let legacy_file = format!(
        "{}/hook/agy-{}",
        h.path,
        &sha_hex("c-legacy").unwrap()[..16]
    );
    write_state(&legacy_file, &sha_hex("K9 legacy prompt gate").unwrap());
    assert_eq!(
        run_hook(
            &h,
            &fake,
            &pl(json!({
                "conversationId": "c-legacy",
                "lastUserInput": "K9 legacy prompt gate",
                "cwd": "/u/work/zz"
            })),
            true,
        )
        .unwrap(),
        None
    );
    assert_eq!(fake.n(), 3);

    // Same-turn error guard: when run_query fails after Haiku rewrite (e.g. invalid config),
    // open_agy_session has already saved the prompt digest so the next PreInvocation is gated.
    let err_home = session_home();
    std::fs::write(
        err_home.file("config.toml"),
        "[query]\ndefault_bias = [\"bogus\"]\n",
    )
    .unwrap();
    let fake_err = PFake::out("K9");
    let err_payload = pl(json!({
        "conversationId": "c-err-gate",
        "lastUserInput": "<USER_REQUEST>\nK9 same turn error gate check\n</USER_REQUEST>",
        "cwd": "/u/work/zz"
    }));
    assert!(run_hook(&err_home, &fake_err, &err_payload, true).is_err());
    assert_eq!(fake_err.n(), 1);
    assert_eq!(
        run_hook(&err_home, &fake_err, &err_payload, true).unwrap(),
        None
    );
    assert_eq!(fake_err.n(), 1);
}

/// A git checkout `<home>/<rel>` whose folder name is its repo key.
pub(crate) fn checkout(h: &PyHome, rel: &str) -> String {
    let dir = h.file(rel);
    std::fs::create_dir_all(&dir).unwrap();
    let ok = std::process::Command::new("git")
        .args(["init", "-q", &dir])
        .status()
        .unwrap()
        .success();
    assert!(ok);
    dir
}

#[test]
fn agy_payload_gives_prompt_and_workspace() {
    // test_agy_payload_gives_prompt_and_workspace. Python spies on run_query's cwd; here the cwd's
    // repo shows in the result: an agy workspace and a Claude cwd are git checkouts named zz and
    // repo, each holding one item, and the hook finds the item of its own repo only.
    let h = py_home();
    put(
        &h,
        "Quokka deploy plan for the zz repo",
        "decided",
        "fact",
        "zz",
    );
    put(
        &h,
        "Quokka deploy plan for the claude repo",
        "decided",
        "fact",
        "repo",
    );
    let (zz, repo) = (checkout(&h, "work/zz"), checkout(&h, "claude/repo"));
    let fake = PFake::out("quokka");
    let agy = pl(
        json!({"conversationId": "c2", "lastUserInput": "does R6 revert still", "workspacePaths": [zz]}),
    );
    let got = run_hook(&h, &fake, &agy, true).unwrap().unwrap();
    assert_eq!(
        (
            got.query.as_str(),
            got.notice.as_str(),
            got.fallback.as_str()
        ),
        ("quokka", "", "")
    );
    assert_eq!(text_of(&got), ["Quokka deploy plan for the zz repo"]);
    let claude = hook_ok(
        &h,
        &fake,
        json!({"prompt": "does R6 revert still", "cwd": repo}),
    )
    .unwrap();
    assert_eq!(text_of(&claude), ["Quokka deploy plan for the claude repo"]);
}

// ------------------------------------------------------------------ session dedupe and compaction

const BOUNDARY: &str =
    "{\"type\":\"system\",\"subtype\":\"compact_boundary\",\"content\":\"Conversation compacted\"}";

/// `session_home`: `limit = 2` and five K9 items.
fn session_home() -> PyHome {
    let h = py_home();
    set_config(
        &h,
        "[prompt_hook]\nlimit = 2\n[catch_up]\nenabled = false\n",
    );
    for n in 0..5 {
        put(
            &h,
            &format!("K9 note number {n} about the deploy"),
            "decided",
            "fact",
            "global",
        );
    }
    h
}

fn transcript(h: &PyHome, lines: &[&str]) -> String {
    let path = h.file("s.jsonl");
    std::fs::write(
        &path,
        lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
    )
    .unwrap();
    path
}

fn append(path: &str, text: &str) {
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
}

/// The ids a K9 prompt injects.
fn ask(h: &PyHome, t: Option<&str>, session: &str) -> Vec<String> {
    let mut p = json!({"prompt": "K9 deploy now please", "cwd": "/u/work/zz"});
    if !session.is_empty() {
        p["session_id"] = json!(session);
    }
    if let Some(t) = t {
        p["transcript_path"] = json!(t);
    }
    hook_ok(h, &PFake::out("K9"), p)
        .unwrap()
        .items
        .into_iter()
        .map(|it| it.id)
        .collect()
}

fn ask1(h: &PyHome, t: &str) -> Vec<String> {
    ask(h, Some(t), "sess-aaaa-1111")
}

fn poss(h: &PyHome) -> Vec<Json> {
    rows(h).iter().map(|r| r["pos"].clone()).collect()
}

fn disjoint(a: &[String], b: &[String]) -> bool {
    a.iter().all(|x| !b.contains(x))
}

#[test]
fn later_prompt_never_repeats_an_id_injected_earlier_in_the_session() {
    // test_later_prompt_never_repeats_an_id_injected_earlier_in_the_session
    let h = session_home();
    let t = transcript(&h, &["{\"type\":\"user\"}"]);
    let (first, second, third) = (ask1(&h, &t), ask1(&h, &t), ask1(&h, &t));
    assert_eq!((first.len(), second.len(), third.len()), (2, 2, 1));
    let all: std::collections::HashSet<_> = first.iter().chain(&second).chain(&third).collect();
    assert_eq!(all.len(), 5);
    assert_eq!(ask1(&h, &t), Vec::<String>::new());
    let r = rows(&h);
    assert_eq!(
        poss(&h),
        [
            json!("first"),
            json!("later"),
            json!("later"),
            json!("later")
        ]
    );
    assert!(
        r.iter()
            .all(|x| x["session"] == "sess-aaa" && x["path"] == "haiku")
    );
    assert_eq!(r[0]["dropped"], json!([]));
    assert_eq!(r[1]["dropped"], json!(first));
    assert_eq!(
        r[2]["dropped"],
        json!([first.clone(), second.clone()].concat())
    );
    assert!(
        r.iter()
            .all(|x| x["ms"].is_i64() && x["rewrite_ms"].is_i64())
    );
    // sha256("sess-aaaa-1111").hexdigest()[:16], computed with Python hashlib for this test
    let names: Vec<String> = std::fs::read_dir(h.file("hook"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, ["claude-c696667409300325.json"]);
}

#[test]
fn compact_boundary_in_new_bytes_resets_injected_and_makes_the_prompt_first() {
    // test_compact_boundary_in_new_bytes_resets_injected_and_makes_the_prompt_first
    let h = session_home();
    let t = transcript(&h, &["{\"type\":\"user\"}"]);
    let first = ask1(&h, &t);
    append(
        &t,
        "{\"type\":\"assistant\",\"note\":\"compact_boundary is only a word here\"}\n",
    );
    assert!(disjoint(&ask1(&h, &t), &first));
    append(&t, &format!("{BOUNDARY}\n"));
    assert_eq!(ask1(&h, &t), first);
    assert_eq!(poss(&h), [json!("first"), json!("later"), json!("first")]);
}

#[test]
fn boundary_split_across_scan_chunks_is_found() {
    // test_boundary_split_across_scan_chunks_is_found. Python patches the chunk to 7 bytes; the
    // chunk is 1 MiB here, so the appended text puts the mark's first 10 bytes before the 1 MiB
    // line and the rest after it. The mark starts 17 bytes into BOUNDARY.
    let h = session_home();
    let t = transcript(&h, &["{\"type\":\"user\"}"]);
    let first = ask1(&h, &t);
    let pad_len = SCAN_CHUNK_BYTES - 10 - 17;
    let pad = format!(
        "{{\"type\":\"user\",\"pad\":\"{}\"}}\n",
        "x".repeat(pad_len - "{\"type\":\"user\",\"pad\":\"\"}\n".len())
    );
    assert_eq!(pad.len(), pad_len);
    append(&t, &format!("{pad}{BOUNDARY}\n"));
    assert_eq!(ask1(&h, &t), first);
}

#[test]
fn boundary_written_before_the_first_run_is_not_read() {
    // test_boundary_written_before_the_first_run_is_not_read
    let h = session_home();
    let t = transcript(&h, &[BOUNDARY, "{\"type\":\"user\"}"]);
    let first = ask1(&h, &t);
    assert!(disjoint(&ask1(&h, &t), &first));
    assert_eq!(poss(&h), [json!("first"), json!("later")]);
}

#[test]
fn shrunk_or_missing_transcript_resets_the_session() {
    // test_shrunk_or_missing_transcript_resets_the_session
    let h = session_home();
    let t = transcript(&h, &["{\"type\":\"user\",\"text\":\"a long first line\"}"]);
    let first = ask1(&h, &t);
    std::fs::write(&t, "{\"t\":1}\n").unwrap();
    assert_eq!(ask1(&h, &t), first);
    let second_later = ask1(&h, &t);
    assert!(disjoint(&second_later, &first));
    std::fs::remove_file(&t).unwrap();
    assert_eq!(ask1(&h, &t), first);
    assert_eq!(
        poss(&h),
        [
            json!("first"),
            json!("first"),
            json!("later"),
            json!("first")
        ]
    );
}

#[test]
fn payload_with_no_session_id_keeps_todays_behavior() {
    // test_payload_with_no_session_id_keeps_todays_behavior
    let h = session_home();
    let t = transcript(&h, &["{\"type\":\"user\"}"]);
    let first = ask(&h, Some(&t), "");
    assert_eq!(ask(&h, Some(&t), ""), first);
    for r in rows(&h) {
        assert_eq!(
            (r["session"].clone(), r["pos"].clone(), r["dropped"].clone()),
            (Json::Null, Json::Null, json!([]))
        );
    }
    assert_eq!(rows(&h).len(), 2);
    assert!(!std::path::Path::new(&h.file("hook")).exists());
}

#[test]
fn agy_tracks_session_dedupe_and_keeps_same_prompt_gating() {
    // Spec change (2026-11-19): agy tracks per-conversationId state (pos, injected dedupe) in
    // hook/agy-<sha16>.json when conversationId is non-empty, replacing the old no-dedupe rule.
    let h = session_home();
    let payload = pl(
        json!({"conversationId": "c9", "lastUserInput": "K9 deploy now please", "session_id": "sess-x", "cwd": "/u/work/zz"}),
    );
    let fake = PFake::out("K9");
    let first = run_hook(&h, &fake, &payload, true).unwrap().unwrap();
    let first_ids: Vec<String> = first.items.iter().map(|it| it.id.clone()).collect();
    assert_eq!(first_ids.len(), 2);
    // Same prompt in the same PreInvocation tool loop is still gated once.
    assert_eq!(run_hook(&h, &fake, &payload, true).unwrap(), None);
    let again = pl(
        json!({"conversationId": "c9", "lastUserInput": "K9 deploy again now", "session_id": "sess-x", "cwd": "/u/work/zz"}),
    );
    let second = run_hook(&h, &fake, &again, true).unwrap().unwrap();
    let second_ids: Vec<String> = second.items.iter().map(|it| it.id.clone()).collect();
    assert_eq!(second_ids.len(), 2);
    assert!(disjoint(&second_ids, &first_ids));
    assert_eq!(fake.n(), 2);
    let names: Vec<String> = std::fs::read_dir(h.file("hook"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(names.iter().all(|n| !n.starts_with("claude-")), "{names:?}");
    assert!(
        names
            .iter()
            .any(|n| n.starts_with("agy-") && n.ends_with(".json")),
        "{names:?}"
    );
    let r = rows(&h);
    assert_eq!(
        (
            r[0]["harness"].clone(),
            r[0]["session"].clone(),
            r[0]["pos"].clone(),
            r[0]["dropped"].clone()
        ),
        (json!("agy"), json!("c9"), json!("first"), json!([]))
    );
    assert_eq!(
        (
            r[1]["harness"].clone(),
            r[1]["session"].clone(),
            r[1]["pos"].clone(),
            r[1]["dropped"].clone()
        ),
        (json!("agy"), json!("c9"), json!("later"), json!(first_ids))
    );

    // Payload with no conversationId still gates same-prompt repeats, with no session dedupe.
    let no_id_1 = pl(json!({"lastUserInput": "K9 deploy no id", "cwd": "/u/work/zz"}));
    let no_id_first: Vec<String> = run_hook(&h, &fake, &no_id_1, true)
        .unwrap()
        .unwrap()
        .items
        .into_iter()
        .map(|it| it.id)
        .collect();
    assert_eq!(run_hook(&h, &fake, &no_id_1, true).unwrap(), None);
    let no_id_2 = pl(json!({"lastUserInput": "K9 deploy again no id", "cwd": "/u/work/zz"}));
    let no_id_second: Vec<String> = run_hook(&h, &fake, &no_id_2, true)
        .unwrap()
        .unwrap()
        .items
        .into_iter()
        .map(|it| it.id)
        .collect();
    assert_eq!(no_id_second, no_id_first);
    let tail: Vec<_> = rows(&h).into_iter().skip(2).collect();
    assert_eq!(tail.len(), 2);
    for row in tail {
        assert_eq!(
            (
                row["harness"].clone(),
                row["session"].clone(),
                row["pos"].clone(),
                row["dropped"].clone()
            ),
            (json!("agy"), Json::Null, Json::Null, json!([]))
        );
    }
}

// ------------------------------------------------------------------ backoff: the clock steps by shift_window

fn prompt_text(h: &PyHome, f: &PFake, agy: bool, text: &str) -> Option<Recall> {
    let key = if agy { "lastUserInput" } else { "prompt" };
    let p = pl(json!({key: text, "conversationId": text, "cwd": "/nonexistent-sm-r2"}));
    run_hook(h, f, &p, agy).unwrap()
}

fn prompt(h: &PyHome, f: &PFake) -> Option<Recall> {
    prompt_text(h, f, false, "does R6 revert still")
}

type FailingCase = (&'static str, fn() -> PFake);

fn failing_cases() -> Vec<FailingCase> {
    vec![("timeout", PFake::timeout), ("error", || PFake::exit(1))]
}

#[test]
fn two_failures_in_a_row_skip_the_model_for_ten_minutes() {
    // test_two_failures_in_a_row_skip_the_model_for_ten_minutes (params: timeout, error).
    // Clock +599 s and +2 s: the window has 600 s, so the next prompt is inside it, then
    // `shift_window` moves `until` 601 s earlier.
    for (cause, make) in failing_cases() {
        let h = py_home();
        let failing = make();
        assert_eq!(
            [
                prompt(&h, &failing).unwrap().fallback,
                prompt(&h, &failing).unwrap().fallback
            ],
            [cause, cause]
        );
        assert_eq!(failing.n(), 2);
        assert_eq!(prompt(&h, &failing).unwrap().fallback, "backoff");
        assert_eq!(failing.n(), 2);
        let r = rows(&h);
        let got: Vec<_> = r
            .iter()
            .map(|x| (x["path"].clone(), x["fallback"].clone()))
            .collect();
        assert_eq!(
            got,
            [
                (json!("local"), json!(cause)),
                (json!("local"), json!(cause)),
                (json!("local"), json!("backoff"))
            ]
        );
        assert!(r[2]["rewrite_ms"] == 0 && r[2].get("error").is_none());
        shift_window(&h, 601.0);
        let ok = PFake::out("R6 weakening");
        assert!(prompt(&h, &ok).is_some());
        assert_eq!(ok.n(), 1);
        assert_eq!(rows(&h).pop().unwrap()["path"], "haiku");
        assert!(!std::path::Path::new(&backoff_file(&h)).exists());
    }
}

#[test]
fn one_failure_after_the_window_skips_again_and_agy_shares_the_backoff() {
    // test_one_failure_after_the_window_skips_again_and_agy_shares_the_backoff. The agy prompt
    // holds one word the store has (r6): 3200 fillers give it idf ln(3201) = 8.07 >= 8.0.
    let h = py_home_with(3200);
    let failing = PFake::exit(1);
    prompt(&h, &failing);
    prompt(&h, &failing);
    shift_window(&h, 601.0);
    assert_eq!(prompt(&h, &failing).unwrap().fallback, "error");
    assert_eq!(failing.n(), 3);
    assert_eq!(
        prompt_text(&h, &failing, true, "R6 agy ask again")
            .unwrap()
            .fallback,
        "backoff"
    );
    assert_eq!(failing.n(), 3);
    let fb: Vec<_> = rows(&h).iter().map(|r| r["fallback"].clone()).collect();
    assert_eq!(fb, ["error", "error", "error", "backoff"].map(|x| json!(x)));
}

#[test]
fn none_answer_is_not_a_failure() {
    // test_none_answer_is_not_a_failure
    let h = py_home();
    let (failing, none) = (PFake::exit(1), PFake::out("NONE"));
    prompt(&h, &failing);
    assert_eq!(prompt(&h, &none), None);
    prompt(&h, &failing);
    assert_eq!(prompt(&h, &failing).unwrap().fallback, "error");
    assert_eq!(failing.n(), 3);
    let r = rows(&h);
    assert_eq!(
        r.iter().map(|x| x["path"].clone()).collect::<Vec<_>>(),
        ["local", "none", "local", "local"].map(|x| json!(x))
    );
    assert_eq!(r[1]["query"], "NONE");
}

/// The prompt's notice; "" when the hook injects nothing. Also the clock before and after.
fn notice(h: &PyHome, session: &str, f: &PFake, text: &str) -> (String, f64, f64) {
    let before = clock::now_epoch();
    let p = pl(json!({"prompt": text, "session_id": session, "cwd": "/nonexistent-sm-r2"}));
    let found = run_hook(h, f, &p, false).unwrap();
    (
        found.map(|r| r.notice).unwrap_or_default(),
        before,
        clock::now_epoch(),
    )
}

const R6_TEXT: &str = "does R6 revert still";

#[test]
fn the_backoff_notice_shows_once_per_window_per_session_from_the_prompt_that_opens_it() {
    // test_the_backoff_notice_shows_once_per_window_per_session_from_the_prompt_that_opens_it
    let h = py_home();
    let failing = PFake::exit(1);
    assert_eq!(notice(&h, "s-one", &failing, R6_TEXT).0, "");
    let (got, b, a) = notice(&h, "s-one", &failing, R6_TEXT);
    one_of_until(
        &got,
        |end| error_notice(" (claude exited 1: boom)", end),
        b,
        a,
    );
    let make = |end: &str| error_notice(" (claude exited 1: boom)", end);
    assert_eq!(notice(&h, "s-one", &failing, R6_TEXT).0, "");
    let (two, b2, a2) = notice(&h, "s-two", &failing, "does R6 pruning still apply");
    one_of_until(&two, make, b2.min(b), a2.max(a));
    assert_eq!(notice(&h, "s-two", &failing, R6_TEXT).0, "");
    assert_eq!(failing.n(), 2);
    let tail: Vec<_> = rows(&h)
        .iter()
        .rev()
        .take(3)
        .map(|r| (r["path"].clone(), r["fallback"].clone()))
        .collect();
    assert_eq!(tail, vec![(json!("local"), json!("backoff")); 3]);
}

#[test]
fn a_timeout_backoff_notice_gives_no_fix_and_a_new_window_shows_it_again() {
    // test_a_timeout_backoff_notice_gives_no_fix_and_a_new_window_shows_it_again
    let h = py_home();
    let slow = PFake::timeout();
    notice(&h, "s-one", &slow, R6_TEXT);
    let (got, b, a) = notice(&h, "s-one", &slow, R6_TEXT);
    one_of_until(&got, timeout_notice, b, a);
    shift_window(&h, 601.0);
    let (third, b3, a3) = notice(&h, "s-one", &slow, R6_TEXT);
    assert!(third.contains("timed out twice in a row"));
    let (s2, b4, a4) = notice(&h, "s-two", &slow, R6_TEXT);
    assert!(
        s2.contains(&hm(b3 + 600.0)) || s2.contains(&hm(a3 + 600.0)),
        "{s2} {b4} {a4}"
    );
}

#[test]
fn agy_sees_the_backoff_notice_once_per_window() {
    // agy tracks per-conversationId backoff notice once per window,
    // so three turns in one conversationId see [false, true, false], and a second conversationId
    // sees the notice once too. (3200 fillers, as above.)
    let h = py_home_with(3200);
    let failing = PFake::exit(1);
    let notices: Vec<bool> = (0..3)
        .map(|n| {
            let p = pl(json!({
                "lastUserInput": format!("R6 agy turn {n}"),
                "conversationId": "c1",
                "cwd": "/nonexistent-sm-r2"
            }));
            run_hook(&h, &failing, &p, true)
                .unwrap()
                .is_some_and(|r| !r.notice.is_empty())
        })
        .collect();
    assert_eq!(notices, [false, true, false]);
    let other = pl(json!({
        "lastUserInput": "R6 agy other",
        "conversationId": "c2",
        "cwd": "/nonexistent-sm-r2"
    }));
    assert!(
        run_hook(&h, &failing, &other, true)
            .unwrap()
            .is_some_and(|r| !r.notice.is_empty())
    );
    let again = pl(json!({
        "lastUserInput": "R6 agy other two",
        "conversationId": "c2",
        "cwd": "/nonexistent-sm-r2"
    }));
    assert!(
        !run_hook(&h, &failing, &again, true)
            .unwrap()
            .is_some_and(|r| !r.notice.is_empty())
    );
}

#[test]
fn a_window_with_no_error_prints_no_parenthetical() {
    // test_a_window_with_no_error_prints_no_parenthetical
    let h = py_home();
    write_state(
        &backoff_file(&h),
        &format!(
            "{{\"failures\": 2, \"until\": {}}}",
            clock::now_epoch() + 300.0
        ),
    );
    let (n, ..) = notice(&h, "s-old", &PFake::exit(1), R6_TEXT);
    assert!(
        n.starts_with(
            "sm recall: the query model failed twice in a row, so local keywords stand in until "
        ),
        "{n}"
    );
}

#[test]
fn a_streak_with_any_other_error_gets_the_other_errors_text() {
    // test_a_streak_with_any_other_error_gets_the_other_errors_text (2 params)
    for (first, second, last_error) in [
        (
            PFake::exit(1),
            PFake::timeout(),
            "rewrite timed out after 8.0 s",
        ),
        (PFake::timeout(), PFake::exit(1), "claude exited 1: boom"),
    ] {
        let h = py_home();
        notice(&h, "s-mix", &first, R6_TEXT);
        let (n, ..) = notice(&h, "s-mix", &second, R6_TEXT);
        assert!(
            n.starts_with(&format!(
                "sm recall: the query model failed twice in a row ({last_error}), so local keywords"
            )),
            "{n}"
        );
    }
}

#[test]
fn a_window_after_a_state_with_no_cause_gets_the_other_errors_text() {
    // test_a_window_after_a_state_with_no_cause_gets_the_other_errors_text
    let h = py_home();
    write_state(
        &backoff_file(&h),
        "{\"failures\": 1, \"until\": 0, \"error\": \"claude exited 1\"}",
    );
    let (n, ..) = notice(&h, "s-old", &PFake::timeout(), R6_TEXT);
    assert!(
        n.starts_with(
            "sm recall: the query model failed twice in a row (rewrite timed out after 8.0 s)"
        ),
        "{n}"
    );
}

#[test]
fn model_none_ignores_an_active_backoff() {
    // test_model_none_ignores_an_active_backoff
    let h = py_home();
    write_state(
        &backoff_file(&h),
        &format!(
            "{{\"failures\": 2, \"until\": {}, \"error\": \"claude exited 1: boom\"}}",
            clock::now_epoch() + 300.0
        ),
    );
    set_config(&h, "[prompt_hook]\nmodel = \"none\"\n");
    let found = hook_ok(
        &h,
        &PFake::out("x"),
        json!({"prompt": R6_TEXT, "session_id": "s-off", "cwd": "/nonexistent-sm-r2"}),
    )
    .unwrap();
    assert_eq!(
        (found.fallback.as_str(), found.notice.as_str()),
        ("off", "")
    );
}

// ------------------------------------------------------------------ local fallback

fn cause_cases() -> Vec<(PFake, &'static str, &'static str)> {
    vec![
        (PFake::timeout(), "timeout", "rewrite timed out after 8.0 s"),
        (PFake::exit(1), "error", "claude exited 1: boom"),
        (
            PFake::fail("FileNotFoundError", "no such file: claude"),
            "error",
            "no such file: claude",
        ),
    ]
}

#[test]
fn a_failed_rewrite_falls_back_to_local_keywords() {
    // test_a_failed_rewrite_falls_back_to_local_keywords (3 params)
    for (fake, cause, error) in cause_cases() {
        let h = py_home();
        let found = hook_ok(
            &h,
            &fake,
            json!({"prompt": "does R6 revert still", "cwd": "/nonexistent-sm-r2"}),
        )
        .unwrap();
        assert_eq!(
            (
                found.query.as_str(),
                found.fallback.as_str(),
                found.notice.as_str()
            ),
            ("r6 revert", cause, "")
        );
        assert_eq!(
            text_of(&found),
            ["R6 stays reverted: narrowing prune_stale() loses old catches"]
        );
        assert_eq!(
            local_row(&h),
            json!({"path": "local", "fallback": cause, "error": error, "dropped": [], "query": "r6 revert", "ids": [found.items[0].id]})
        );
    }
}

#[test]
fn model_none_answer_still_injects_nothing() {
    // test_model_none_answer_still_injects_nothing
    let h = py_home();
    assert_eq!(
        hook_ok(
            &h,
            &PFake::out("NONE"),
            json!({"prompt": "does R6 revert still", "cwd": "/nonexistent-sm-r2"})
        ),
        None
    );
    assert_eq!(rows(&h).pop().unwrap()["path"], "none");
}

#[test]
fn model_none_config_runs_local_only_with_no_model_call_no_backoff_and_no_notice() {
    // test_model_none_config_runs_local_only_with_no_model_call_no_backoff_and_no_notice
    let h = py_home();
    set_config(&h, "[prompt_hook]\nmodel = \"none\"\n");
    let fake = PFake::out("R6");
    for session in ["s-one", "s-two"] {
        let found = hook_ok(&h, &fake, json!({"prompt": "does R6 revert still", "session_id": session, "cwd": "/nonexistent-sm-r2"})).unwrap();
        assert_eq!(
            (
                found.query.as_str(),
                found.fallback.as_str(),
                found.notice.as_str()
            ),
            ("r6 revert", "off", "")
        );
    }
    assert_eq!(fake.n(), 0);
    assert!(!std::path::Path::new(&backoff_file(&h)).exists());
    let mut row = local_row(&h);
    row.as_object_mut().unwrap().remove("ids");
    assert_eq!(
        row,
        json!({"path": "local", "fallback": "off", "dropped": [], "query": "r6 revert"})
    );
}

#[test]
fn no_word_the_store_holds_injects_nothing() {
    // test_no_word_the_store_holds_injects_nothing
    let h = py_home();
    set_config(&h, "[prompt_hook]\nmodel = \"none\"\n");
    assert_eq!(
        hook_ok(
            &h,
            &PFake::out("x"),
            json!({"prompt": "zzqx wwvy please", "cwd": "/nonexistent-sm-r2"})
        ),
        None
    );
    assert_eq!(
        local_row(&h),
        json!({"path": "local", "fallback": "off", "dropped": [], "query": "", "ids": []})
    );
}

/// `floor_store` at `LOCAL_TAU` 8.0: the R6 item, 30 `alpha bravo note number n` items, `extra`
/// filler items in scope and `other` items of another repo. Python's `LOCAL_TAU` is 2.5 and
/// each test's comment gives its idf values; here `ln(N / df)` is scaled past 8.0 by `extra`.
fn floor_store(extra: usize, other: usize) -> PyHome {
    let h = py_home_with(0);
    bulk(
        &h,
        "global",
        (0..30).map(|n| format!("alpha bravo note number {n}")),
    );
    fillers(&h, extra);
    bulk(
        &h,
        "other-repo",
        (0..other).map(|n| format!("alpha bravo elsewhere {n}")),
    );
    h
}

fn local(prompt: &str, h: &PyHome, limit: i64) -> (String, Vec<String>) {
    let (q, items) = local_query(
        prompt,
        limit,
        0,
        Some(&PyValue::Dict(vec![])),
        "",
        Some(&h.path),
        Some(&h.path),
        None,
    )
    .unwrap();
    (q, items.into_iter().map(|it| it.body).collect())
}

#[test]
fn floor_lets_one_rare_matched_word_carry_an_item() {
    // test_floor_lets_one_rare_matched_word_carry_an_item. 4000 in scope: concisex has df 1,
    // idf ln(4000) = 8.29 >= 8.0.
    let h = floor_store(4000 - 32, 40);
    put(
        &h,
        "Concisex lite mode for the hook",
        "decided",
        "fact",
        "global",
    );
    assert_eq!(
        local("about concisex again now please", &h, 4),
        (
            "concisex".to_string(),
            vec!["Concisex lite mode for the hook".to_string()]
        )
    );
}

#[test]
fn floor_drops_items_that_match_only_common_words() {
    // test_floor_drops_items_that_match_only_common_words. 31 in scope: alpha, bravo and note
    // have df 30, idf ln(31/30) = 0.033 each. With the 450 other-repo items in the count each
    // would be ln(481/30) = 2.77, sum 8.32 >= 8.0, so the item would pass.
    let h = floor_store(0, 450);
    assert_eq!(
        local("alpha bravo note again", &h, 4),
        (String::new(), vec![])
    );
}

#[test]
fn floor_keeps_the_search_order_among_items_that_clear_it() {
    // test_floor_keeps_the_search_order_among_items_that_clear_it. 7000 in scope: harrierx
    // idf ln(7000) = 8.85, kestrelx ln(7000/2) = 8.16, both >= 8.0.
    let h = floor_store(7000 - 34, 40);
    let long_h = format!(
        "harrierx {}",
        (0..60)
            .map(|n| format!("filler{n}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let long_k = format!(
        "kestrelx {}",
        (0..60)
            .map(|n| format!("pad{n}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    for body in [long_h.as_str(), "kestrelx kestrelx", long_k.as_str()] {
        put(&h, body, "decided", "fact", "global");
    }
    // Python's prompt has a third word, alpha. At N = 7000 the 30 alpha items outrank long_h and
    // long_k inside the 20-deep search, so Python itself keeps one item for that prompt (a run
    // of Python main 555b8f2 on the same store gave the kept list below). The order claim is
    // checked on the prompt without alpha, whose expected values are from the same Python run.
    let (query, kept) = local("harrierx kestrelx", &h, 4);
    let a = QueryArgs {
        limit: 20,
        home: Some(&h.path),
        cwd: Some(&h.path),
        ..Default::default()
    };
    let ranked: Vec<String> = run_query(&query, &a, None)
        .unwrap()
        .into_iter()
        .map(|it| it.body)
        .collect();
    let want = vec!["kestrelx kestrelx".to_string(), long_h, long_k];
    assert_eq!(ranked[..3], want[..]);
    assert_eq!(kept, want);
    assert_eq!(
        local("harrierx kestrelx alpha", &h, 4),
        (
            "harrierx kestrelx alpha".to_string(),
            vec!["kestrelx kestrelx".to_string()]
        )
    );
}

#[test]
fn floor_searches_past_the_limit_for_an_item_that_clears_it() {
    // test_floor_searches_past_the_limit_for_an_item_that_clears_it. 4000 in scope: the short
    // kestrelx item ranks first with idf ln(4000/2) = 7.6 < 8.0; harrierx (8.29) ranks below it.
    let h = floor_store(4000 - 34, 40);
    let long_h = format!(
        "harrierx {}",
        (0..60)
            .map(|n| format!("filler{n}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    for body in [long_h.as_str(), "kestrelx kestrelx", "kestrelx pad"] {
        put(&h, body, "decided", "fact", "global");
    }
    assert_eq!(
        local("harrierx kestrelx", &h, 1),
        ("harrierx kestrelx".to_string(), vec![long_h])
    );
}

#[test]
fn a_multi_word_project_code_is_one_floor_unit() {
    // test_a_multi_word_project_code_is_one_floor_unit. 4000 in scope: the unit "the forge" has
    // df 1, idf ln(4000) = 8.29 >= 8.0; scored word by word, only "forge" (df 21, ln(4000/21) =
    // 5.25) would count and the item would miss the floor.
    let h = floor_store(4000 - 52, 40);
    bulk(
        &h,
        "global",
        (0..20).map(|n| format!("forge build entry {n}")),
    );
    put(&h, "the Forge runbook", "decided", "fact", "global");
    let projects = PyValue::Dict(vec![(s("the Forge"), s("Example Forge"))]);
    let (q, items) = local_query(
        "the Forge now please",
        4,
        0,
        Some(&projects),
        "",
        Some(&h.path),
        Some(&h.path),
        None,
    )
    .unwrap();
    assert_eq!(
        (
            q.as_str(),
            items.into_iter().map(|it| it.body).collect::<Vec<_>>()
        ),
        ("the Forge", vec!["the Forge runbook".to_string()])
    );
}

fn terms_of(prompt: &str, projects: &[(&str, &str)]) -> (Vec<String>, Vec<String>) {
    local_candidates(
        prompt,
        &PyValue::Dict(projects.iter().map(|(k, v)| (s(k), s(v))).collect()),
    )
    .unwrap()
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

#[test]
fn local_candidates_put_owner_words_before_pasted_words() {
    // test_local_candidates_put_owner_words_before_pasted_words
    let prompt = "<pasted_content id=\"p1\">\npasted gamma alpha beta\n</pasted_content id=\"p1\">\nwhy does gamma fail Gamma";
    assert_eq!(
        terms_of(prompt, &[]),
        (strs(&["gamma", "fail", "pasted", "alpha", "beta"]), vec![])
    );
}

fn words(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("w{i:02}")).collect()
}

#[test]
fn local_candidates_keep_prompt_order_up_to_40_owner_words() {
    // test_local_candidates_keep_prompt_order_up_to_40_owner_words
    assert_eq!(terms_of(&words(40).join(" "), &[]).0, words(40));
}

#[test]
fn local_candidates_take_the_tail_then_the_head_past_40_owner_words() {
    // test_local_candidates_take_the_tail_then_the_head_past_40_owner_words
    let w = words(41);
    assert_eq!(terms_of(&w.join(" "), &[]).0, [&w[21..], &w[..20]].concat());
    let w = words(60);
    assert_eq!(terms_of(&w.join(" "), &[]).0, [&w[40..], &w[..20]].concat());
}

#[test]
fn local_candidates_fill_only_the_slots_owner_words_leave() {
    // test_local_candidates_fill_only_the_slots_owner_words_leave
    let owner = (0..35)
        .map(|n| format!("o{n:02}"))
        .collect::<Vec<_>>()
        .join(" ");
    let pasted = (0..30)
        .map(|n| format!("p{n:02}"))
        .collect::<Vec<_>>()
        .join(" ");
    let (terms, _) = terms_of(
        &format!("<pasted_content>{pasted}</pasted_content>{owner}"),
        &[],
    );
    assert_eq!(
        terms[terms.len() - 6..],
        ["o34", "p00", "p01", "p02", "p03", "p04"]
    );
    assert_eq!(terms.len(), 40);
}

#[test]
fn local_candidates_drop_accented_words_stopwords_and_one_letter_words() {
    let prompt = "please check Café deploy now à la x AND runner here";
    assert_eq!(
        terms_of(prompt, &[]),
        (strs(&["check", "deploy", "la", "runner"]), vec![])
    );
}

#[test]
fn local_stopwords_filter_english_defaults_and_configured_extras() {
    let prompt = "else tra customstop question one two things maybe using runner";
    assert_eq!(
        terms_of(prompt, &[]),
        (
            strs(&["else", "tra", "customstop", "question", "runner"]),
            vec![]
        )
    );
}

#[test]
fn project_codes_match_case_sensitively_as_whole_words() {
    // test_project_codes_match_case_sensitively_as_whole_words
    let projects = [("CD", "Cobalt Dock"), ("ZZ", "Zeta Zone")];
    assert_eq!(
        terms_of("check CD runner and ZZtop", &projects),
        (strs(&["check", "cd", "runner", "zztop"]), strs(&["CD"]))
    );
    assert_eq!(
        terms_of("run cd in the folder", &projects),
        (strs(&["run", "cd", "folder"]), vec![])
    );
    assert_eq!(
        terms_of("run myCD and CD_x", &projects),
        (strs(&["run", "mycd", "cd_x"]), vec![])
    );
}

#[test]
fn local_query_counts_only_active_items_in_the_searchs_repo_scope() {
    // test_local_query_counts_only_active_items_in_the_searchs_repo_scope
    let h = py_home();
    put(
        &h,
        "Kestrel runner moved to arm64 hosts",
        "decided",
        "fact",
        "global",
    );
    let old = put(&h, "Ospreyx cache note", "decided", "fact", "global");
    let new = put(
        &h,
        "Kestrel cache note, current",
        "decided",
        "fact",
        "global",
    );
    crate::service::supersede::run_supersede(&old, &new, Some(&h.path)).unwrap();
    put(
        &h,
        "Harrierx only in another repo",
        "decided",
        "fact",
        "other-repo",
    );
    let q = |repo: &str, cwd: Option<&str>| {
        local_query(
            "kestrel ospreyx harrierx zzqx runner",
            4,
            0,
            None,
            repo,
            Some(&h.path),
            cwd,
            None,
        )
        .unwrap()
        .0
    };
    assert_eq!(q("", Some(&h.path)), "kestrel runner");
    assert_eq!(q("other-repo", None), "kestrel harrierx runner");
}

#[test]
fn local_query_keeps_a_project_code_found_in_the_store() {
    // test_local_query_keeps_a_project_code_found_in_the_store
    let h = py_home();
    put(&h, "ZZ deploy runner notes", "decided", "fact", "global");
    let q = |p: &str| {
        local_query(p, 4, 0, None, "", Some(&h.path), Some(&h.path), None)
            .unwrap()
            .0
    };
    assert_eq!(q("ZZ deploy now please"), "ZZ deploy");
    assert_eq!(q("zz deploy now please"), "zz deploy");
}

#[test]
fn local_query_drops_a_project_code_the_store_does_not_hold() {
    // test_local_query_drops_a_project_code_the_store_does_not_hold
    let h = py_home();
    put(&h, "deploy runner notes", "decided", "fact", "global");
    let q = |p: &str| {
        local_query(p, 4, 0, None, "", Some(&h.path), Some(&h.path), None)
            .unwrap()
            .0
    };
    assert_eq!(q("ZZ deploy runner"), "deploy runner");
    assert_eq!(q("ZZ zzqx"), "");
}

#[test]
fn local_query_keeps_at_most_12_held_words_in_candidate_order() {
    // test_local_query_keeps_at_most_12_held_words_in_candidate_order
    let h = py_home();
    let ws: Vec<String> = (0..15).map(|n| format!("wq{n:02}")).collect();
    put(&h, &ws.join(" "), "decided", "fact", "global");
    let q = local_query(
        &ws.join(" "),
        4,
        0,
        None,
        "",
        Some(&h.path),
        Some(&h.path),
        None,
    )
    .unwrap()
    .0;
    assert_eq!(q, ws[..12].join(" "));
}

#[test]
fn a_word_of_a_kept_code_is_not_also_a_term() {
    // test_a_word_of_a_kept_code_is_not_also_a_term
    let h = py_home();
    put(&h, "the Forge deploy runbook", "decided", "fact", "global");
    let projects = PyValue::Dict(vec![
        (s("the Forge"), s("Example Forge")),
        (s("CD"), s("Cobalt Dock")),
    ]);
    let q = local_query(
        "the Forge deploy now please",
        4,
        0,
        Some(&projects),
        "",
        Some(&h.path),
        Some(&h.path),
        None,
    )
    .unwrap()
    .0;
    assert_eq!(q, "the Forge deploy");
}

// ------------------------------------------------------------------ errors and locks

fn hold_migration_lock(h: &PyHome) -> rusqlite::Connection {
    let conn = connect(&h.file("memory.db"), None).unwrap();
    let latest: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    conn.execute_batch(&format!("PRAGMA user_version = {}", latest - 1))
        .unwrap();
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    conn
}

fn break_store(h: &PyHome) {
    for n in ["memory.db", "memory.db-wal", "memory.db-shm"] {
        let _ = std::fs::remove_file(h.file(n));
    }
    std::fs::create_dir(h.file("memory.db")).unwrap();
}

#[test]
fn hook_lock_wait_gets_only_the_time_left_in_its_budget() {
    // test_hook_lock_wait_gets_only_the_time_left_in_its_budget. Python patches the store wait
    // to 20 s and the budget to 1 s. Rust has 15 s and 12 s: the wait must end near 12 s, not 15.
    // The bound is the 12 s budget plus 2 s, with the busy handler's stretch on this host taken
    // from a 12 s reference wait beside it (busy_reference); the 15 s store wait goes past it.
    let h = py_home();
    let writer = hold_migration_lock(&h);
    let reference = crate::store::connect::busy_reference(&h.file("memory.db"), 12_000);
    let started = Instant::now();
    let found = hook_ok(
        &h,
        &PFake::out("R6 revert\n"),
        json!({"prompt": "does R6 revert still", "cwd": "/nonexistent-sm-r2"}),
    );
    let elapsed = started.elapsed().as_secs_f64();
    let reference = reference.join().unwrap();
    writer.execute_batch("ROLLBACK").unwrap();
    assert!(elapsed < reference + 2.0, "{elapsed} {reference}");
    assert_eq!(found, None);
    let mut row = rows(&h).pop().unwrap();
    for k in ["ts", "ms", "rewrite_ms", "db_ms", "git_ms", "fin_ms"] {
        row.as_object_mut().unwrap().remove(k);
    }
    assert_eq!(
        row,
        json!({"harness": "claude", "session": null, "pos": null, "path": "error", "dropped": [], "query": "R6 revert", "ids": [], "error": "budget"})
    );
}

/// A connection in EXCLUSIVE locking mode keeps the store's lock from its first write until it
/// closes, so every other connect waits for it.
fn hold_exclusive_lock(h: &PyHome) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(h.file("memory.db")).unwrap();
    conn.execute_batch("PRAGMA locking_mode = EXCLUSIVE; CREATE TABLE held (x)")
        .unwrap();
    conn
}

#[test]
fn an_exclusive_lock_holder_never_keeps_the_hook_past_its_budget() {
    // The store wait reads the wall clock, so the hook ends within its budget although SQLite's
    // busy handler sleeps run long on a busy host (busy_reference): a 12 s wait that trusts the
    // handler took 12.86 s on a nice -n 15 run here (2026-11-06).
    // The test bites only when the busy handler oversleeps by more than about 0.3 to 0.5 s: on a
    // quiet host a wait without the 100 ms tries can also end within the bound.
    let h = py_home();
    let holder = hold_exclusive_lock(&h);
    let reference = crate::store::connect::busy_reference(&h.file("memory.db"), 12_000);
    let found = hook_ok(
        &h,
        &PFake::out("R6 revert\n"),
        json!({"prompt": "does R6 revert still", "cwd": "/nonexistent-sm-r2"}),
    );
    let reference_ms = (reference.join().unwrap() * 1000.0) as i64;
    drop(holder);
    assert_eq!(found, None);
    let row = rows(&h).pop().unwrap();
    assert_eq!(
        (&row["path"], &row["error"]),
        (&json!("error"), &json!("budget"))
    );
    // The budget, plus the last 100 ms try, its 25 ms pause and the row's own work, plus a quarter
    // of the handler's stretch beside it for host stalls. A wait that trusts the handler ends near
    // the reference (heavy load: 12.50 to 12.64 s, reference 12.51 to 12.63 s), so it still fails here.
    let ms = row["ms"].as_i64().unwrap();
    let bound = 12_250 + (reference_ms - 12_000) / 4;
    assert!(ms < bound, "{ms} {bound} {reference_ms}");
}

#[test]
fn a_lock_in_the_local_search_logs_local_error_and_keeps_the_fallback() {
    // test_a_lock_in_the_local_search_logs_local_error_and_keeps_the_fallback. Python's wait
    // ends first and logs "lock"; Rust's wait is longer than the budget, so it logs "budget".
    let h = py_home();
    set_config(&h, "[prompt_hook]\nmodel = \"none\"\n");
    let writer = hold_migration_lock(&h);
    let found = hook_ok(
        &h,
        &PFake::out("x"),
        json!({"prompt": R6_TEXT, "cwd": "/nonexistent-sm-r2"}),
    );
    writer.execute_batch("ROLLBACK").unwrap();
    assert_eq!(found, None);
    assert_eq!(
        local_row(&h),
        json!({"path": "local", "fallback": "off", "local_error": "budget", "dropped": [], "query": "", "ids": []})
    );
}

#[test]
fn a_lock_in_the_local_search_keeps_the_rewrite_error() {
    // test_a_lock_in_the_local_search_keeps_the_rewrite_error (local_error "budget", as above)
    let h = py_home();
    let writer = hold_migration_lock(&h);
    let found = hook_ok(
        &h,
        &PFake::exit(1),
        json!({"prompt": R6_TEXT, "cwd": "/nonexistent-sm-r2"}),
    );
    writer.execute_batch("ROLLBACK").unwrap();
    assert_eq!(found, None);
    assert_eq!(
        local_row(&h),
        json!({"path": "local", "fallback": "error", "error": "claude exited 1: boom", "local_error": "budget", "dropped": [], "query": "", "ids": []})
    );
}

#[test]
fn hook_rewrite_timeout_never_passes_the_hook_budget() {
    // test_hook_rewrite_timeout_never_passes_the_hook_budget
    let h = py_home();
    set_config(&h, "[prompt_hook]\ntimeout = 30\n");
    let fake = PFake::out("NONE");
    assert_eq!(
        hook_ok(
            &h,
            &fake,
            json!({"prompt": "does R6 revert still", "cwd": "/nonexistent-sm-r2"})
        ),
        None
    );
    let t = fake.calls.borrow()[0].timeout;
    assert!(HOOK_BUDGET_S - 1.0 < t && t <= HOOK_BUDGET_S, "{t}");
}

#[test]
fn a_nan_timeout_in_the_config_reaches_the_runner_and_raises_with_no_log_row_or_backoff() {
    // Python `min(cfg, deadline - time.monotonic())` keeps a NaN `cfg` (nan < x is False), and
    // `subprocess.run(timeout=nan)` then raises
    // ValueError: cannot convert float NaN to integer (probed with Python 3.14.6: `min(nan, 11.0)`
    // is nan, `int(float("nan"))` gives that text). The hook catches no ValueError, so it logs
    // no row and writes no backoff. The real runner plays `true` as the model binary.
    let h = py_home();
    set_config(&h, "[prompt_hook]\ntimeout = nan\nclaude_bin = \"true\"\n");
    let payload = pl(json!({"prompt": "does R6 revert still", "cwd": "/nonexistent-sm-r2"}));
    let err = run_prompt_hook(&payload, false, Some(&h.path), &subprocess_run).unwrap_err();
    assert_eq!(
        err.python_text(),
        "ValueError: cannot convert float NaN to integer"
    );
    assert!(!std::path::Path::new(&h.file("hook.log")).exists());
    assert!(!std::path::Path::new(&backoff_file(&h)).exists());
}

#[test]
fn hook_lets_a_store_error_that_is_no_lock_reach_the_cli() {
    // test_hook_lets_a_store_error_that_is_no_lock_reach_the_cli
    let h = py_home();
    break_store(&h);
    let e = run_hook(
        &h,
        &PFake::out("R6 revert\n"),
        &pl(json!({"prompt": "does R6 revert still"})),
        false,
    )
    .unwrap_err();
    assert_eq!(
        (e.class.as_ref(), e.msg.as_str()),
        ("OperationalError", "unable to open database file")
    );
}

#[test]
fn a_store_error_that_is_no_lock_in_the_local_path_writes_local_error() {
    // test_a_store_error_that_is_no_lock_in_the_local_path_writes_local_error
    let h = py_home();
    break_store(&h);
    set_config(&h, "[prompt_hook]\nmodel = \"none\"\n");
    assert_eq!(
        hook_ok(
            &h,
            &PFake::out("x"),
            json!({"prompt": "does R6 revert still", "cwd": "/nonexistent-sm-r2"})
        ),
        None
    );
    assert_eq!(
        local_row(&h)["local_error"],
        "OperationalError: unable to open database file"
    );
}

#[test]
fn an_exception_in_the_local_path_writes_local_error_and_injects_nothing() {
    // test_an_exception_in_the_local_path_writes_local_error_and_injects_nothing. Python patches
    // local_query to raise ValueError("boom"); Rust has no seam, so a real error stands in:
    // `projects = 5` makes local_candidates iterate an int. The expected text comes from running
    // Python's local_candidates(prompt, 5) (TypeError: 'int' object is not iterable).
    let h = py_home();
    set_config(&h, "[prompt_hook]\nmodel = \"none\"\nprojects = 5\n");
    assert_eq!(
        hook_ok(
            &h,
            &PFake::out("x"),
            json!({"prompt": "does R6 revert still", "cwd": "/nonexistent-sm-r2"})
        ),
        None
    );
    assert_eq!(
        local_row(&h),
        json!({"path": "local", "fallback": "off", "local_error": "TypeError: 'int' object is not iterable", "dropped": [], "query": "", "ids": []})
    );
}

/// `n` items that each hold the three words kestrelx, zephyrx and quillx: three rare words stand
/// in for Python's one word under `LOCAL_TAU = 0.0`. Each word has idf ln(N/n); with 120 items
/// in scope and n = 6 that is 3.05 per word, 9.1 in all, which clears 8.0; for n = 24 the
/// store holds 400 fillers (3 * ln(425/24) = 8.6).
fn kestrel_items(h: &PyHome, n: usize) {
    bulk(
        h,
        "global",
        (0..n).map(|i| format!("Kestrelx zephyrx quillx runbook part {i}")),
    );
}

const KESTREL_PROMPT: &str = "kestrelx zephyrx quillx now please";

#[test]
fn the_local_search_reads_20_plus_the_ids_injected_earlier_in_the_session() {
    // test_the_local_search_reads_20_plus_the_ids_injected_earlier_in_the_session. Python spies
    // that the search depth is 23 for 3 injected ids. Here 24 items match, the session holds the
    // top 20, and the hook returns ranks 21 to 24: a 20-deep search would find none of them.
    let h = py_home_with(400);
    kestrel_items(&h, 24);
    let ranked: Vec<String> = local_query(
        KESTREL_PROMPT,
        24,
        0,
        None,
        "",
        Some(&h.path),
        Some(&h.path),
        None,
    )
    .unwrap()
    .1
    .into_iter()
    .map(|it| it.id)
    .collect();
    assert_eq!(ranked.len(), 24);
    set_config(&h, "[prompt_hook]\nmodel = \"none\"\n");
    write_state(
        &session_file(&h, "s-deep"),
        &format!(
            "{{\"transcript_size\": 0, \"injected\": {}}}",
            json!(ranked[..20])
        ),
    );
    let found = hook_ok(
        &h,
        &PFake::out("x"),
        json!({"prompt": KESTREL_PROMPT, "session_id": "s-deep", "cwd": h.path}),
    )
    .unwrap();
    assert_eq!(found.fallback, "off");
    assert_eq!(
        found
            .items
            .iter()
            .map(|it| it.id.clone())
            .collect::<Vec<_>>(),
        ranked[20..].to_vec()
    );
}

#[test]
fn the_local_path_drops_injected_ids_after_the_floor_and_still_fills_the_limit() {
    // test_the_local_path_drops_injected_ids_after_the_floor_and_still_fills_the_limit
    // (three rare words instead of one, as in kestrel_items)
    let h = py_home_with(120);
    kestrel_items(&h, 6);
    let ranked: Vec<String> = local_query(
        KESTREL_PROMPT,
        6,
        0,
        None,
        "",
        Some(&h.path),
        Some(&h.path),
        None,
    )
    .unwrap()
    .1
    .into_iter()
    .map(|it| it.id)
    .collect();
    assert_eq!(ranked.len(), 6);
    write_state(
        &session_file(&h, "s-fill"),
        &format!(
            "{{\"transcript_size\": 0, \"injected\": {}}}",
            json!(ranked[..4])
        ),
    );
    set_config(&h, "[prompt_hook]\nmodel = \"none\"\n");
    let found = hook_ok(
        &h,
        &PFake::out("x"),
        json!({"prompt": KESTREL_PROMPT, "session_id": "s-fill", "cwd": h.path}),
    )
    .unwrap();
    assert_eq!(
        found
            .items
            .iter()
            .map(|it| it.id.clone())
            .collect::<Vec<_>>(),
        ranked[4..].to_vec()
    );
    assert_eq!(local_row(&h)["dropped"], json!(ranked[..4]));
}

// ------------------------------------------------------------------ recall state

/// The state-file fixture: the Python `home`, with the hook folder made.
fn state_home() -> PyHome {
    let h = py_home();
    set_config(&h, "[catch_up]\nenabled = false\n[prompt_hook]\n");
    std::fs::create_dir_all(h.file("hook")).unwrap();
    h
}

fn ask_n(h: &PyHome, session: &str, f: &PFake, n: usize) -> (String, f64, f64) {
    let before = clock::now_epoch();
    let p =
        pl(json!({"prompt": format!("R6 revert check {n}"), "session_id": session, "cwd": h.path}));
    let found = run_hook(h, f, &p, false).unwrap();
    (
        found.map(|r| r.notice).unwrap_or_default(),
        before,
        clock::now_epoch(),
    )
}

fn state_ask(h: &PyHome, session: &str, f: &PFake) -> String {
    ask_n(h, session, f, 0).0
}

fn file_json(path: &str) -> Json {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn a_020_backoff_file_inside_its_window_gives_the_error_text_with_no_parenthetical() {
    // test_a_020_backoff_file_inside_its_window_gives_the_error_text_with_no_parenthetical
    let h = state_home();
    let until = clock::now_epoch() + 300.0;
    write_state(
        &backoff_file(&h),
        &format!("{{\"failures\": 2, \"until\": {until}}}"),
    );
    let runner = PFake::out("R6 weakening");
    assert_eq!(state_ask(&h, "s1", &runner), error_notice("", &hm(until)));
    assert_eq!(ask_n(&h, "s1", &runner, 1).0, "");
    assert_eq!(runner.n(), 0);
}

#[test]
fn a_020_file_after_one_failure_then_a_timeout_opens_an_error_window() {
    // test_a_020_file_after_one_failure_then_a_timeout_opens_an_error_window
    let h = state_home();
    write_state(&backoff_file(&h), "{\"failures\": 1, \"until\": 0}");
    let (got, b, a) = ask_n(&h, "s1", &PFake::timeout(), 0);
    one_of_until(
        &got,
        |end| error_notice(" (rewrite timed out after 8.0 s)", end),
        b,
        a,
    );
}

#[test]
fn a_030_file_written_inside_a_window_keeps_who_was_told() {
    // test_a_030_file_written_inside_a_window_keeps_who_was_told
    let h = state_home();
    let until = clock::now_epoch() + 120.0;
    write_state(
        &backoff_file(&h),
        &format!(
            "{{\"failures\": 2, \"until\": {until}, \"error\": \"rewrite timed out after 8.0 s\", \"cause\": \"timeout\", \"notified\": [\"s1\"]}}"
        ),
    );
    let runner = PFake::out("R6 weakening");
    assert_eq!(state_ask(&h, "s1", &runner), "");
    assert_eq!(state_ask(&h, "s2", &runner), timeout_notice(&hm(until)));
    assert_eq!(runner.n(), 0);
}

#[test]
fn a_window_that_ends_now_is_closed_and_the_model_runs() {
    // test_a_window_that_ends_now_is_closed_and_the_model_runs. Python freezes the clock at
    // `until`; Rust's clock runs, so `until` is a second in the past (the boundary itself
    // is `until <= now` in backoff_active and take_backoff_notice).
    let h = state_home();
    let until = clock::now_epoch() - 1.0;
    write_state(
        &backoff_file(&h),
        &format!(
            "{{\"failures\": 2, \"until\": {until}, \"error\": \"claude exited 1: boom\", \"cause\": \"error\"}}"
        ),
    );
    let runner = PFake::out("R6 weakening");
    assert_eq!(state_ask(&h, "s1", &runner), "");
    assert_eq!(runner.n(), 1);
    assert!(!std::path::Path::new(&backoff_file(&h)).exists());
}

#[test]
fn a_030_window_that_told_the_empty_id_stays_quiet_for_agy_and_a_payload_with_no_id() {
    // Spec change (2026-11-19): notified=[""] keeps no-id Claude and no-conversationId agy quiet,
    // while agy with conversationId="c1" gets notified once on its own id.
    let h = state_home();
    let until = clock::now_epoch() + 120.0;
    write_state(
        &backoff_file(&h),
        &format!(
            "{{\"failures\": 2, \"until\": {until}, \"error\": \"claude exited 1: boom\", \"cause\": \"error\", \"notified\": [\"\"]}}"
        ),
    );
    let runner = PFake::out("R6 weakening");
    let no_id = run_hook(
        &h,
        &runner,
        &pl(json!({"prompt": "R6 revert check", "cwd": h.path})),
        false,
    )
    .unwrap()
    .unwrap();
    let agy_no_id = run_hook(
        &h,
        &runner,
        &pl(json!({"lastUserInput": "R6 revert check agy noid", "cwd": h.path})),
        true,
    )
    .unwrap()
    .unwrap();
    assert_eq!((no_id.notice.as_str(), agy_no_id.notice.as_str()), ("", ""));
    let agy_c1 = run_hook(
        &h,
        &runner,
        &pl(json!({"lastUserInput": "R6 revert check agy", "conversationId": "c1", "cwd": h.path})),
        true,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        agy_c1.notice,
        error_notice(" (claude exited 1: boom)", &hm(until))
    );
    let agy_c1_again = run_hook(
        &h,
        &runner,
        &pl(json!({"lastUserInput": "R6 revert check agy again", "conversationId": "c1", "cwd": h.path})),
        true,
    )
    .unwrap();
    assert!(!agy_c1_again.is_some_and(|r| !r.notice.is_empty()));
    assert_eq!(
        state_ask(&h, "s1", &runner),
        error_notice(" (claude exited 1: boom)", &hm(until))
    );
    assert_eq!(runner.n(), 0);
}

#[test]
fn junk_backoff_values_read_as_defaults() {
    // test_junk_backoff_values_read_as_defaults (6 params)
    let big = format!("1{}", "0".repeat(400));
    for junk in [
        "{\"failures\": \"x\", \"until\": \"soon\", \"error\": 5, \"cause\": [\"timeout\"], \"notified\": \"s1\"}".to_string(),
        "{\"failures\": true, \"until\": null, \"error\": null, \"cause\": null, \"notified\": null}".into(),
        "{\"failures\": 1.5, \"until\": NaN, \"error\": [\"e\"], \"cause\": 1, \"notified\": [1, null]}".into(),
        "{\"until\": Infinity}".into(),
        format!("{{\"until\": {big}}}"),
        "{\"until\": 1e300}".into(),
    ] {
        let h = state_home();
        write_state(&backoff_file(&h), &junk);
        assert_eq!(Backoff::read(&PyPath::new(&backoff_file(&h))).unwrap(), Backoff::default(), "{junk}");
        let runner = PFake::exit(1);
        assert_eq!(state_ask(&h, "s1", &runner), "");
        assert_eq!(runner.n(), 1);
        assert_eq!(
            file_json(&backoff_file(&h)),
            json!({"failures": 1, "until": 0.0, "error": "claude exited 1: boom", "cause": "error", "notified": []}),
            "{junk}"
        );
    }
}

#[test]
fn junk_values_inside_a_window_never_print_none_or_a_type() {
    // test_junk_values_inside_a_window_never_print_none_or_a_type
    let h = state_home();
    let until = clock::now_epoch() + 60.0;
    write_state(
        &backoff_file(&h),
        &format!(
            "{{\"failures\": 2, \"until\": {until}, \"error\": null, \"cause\": 7, \"notified\": [3, \"s2\"]}}"
        ),
    );
    let want = Backoff {
        failures: 2,
        until,
        notified: vec!["s2".into()],
        ..Default::default()
    };
    assert_eq!(
        Backoff::read(&PyPath::new(&backoff_file(&h))).unwrap(),
        want
    );
    assert_eq!(
        state_ask(&h, "s1", &PFake::out("R6 weakening")),
        error_notice("", &hm(until))
    );
    assert_eq!(state_ask(&h, "s2", &PFake::out("R6 weakening")), "");
}

#[test]
fn a_backoff_file_that_is_no_object_reads_as_no_state() {
    // test_a_backoff_file_that_is_no_object_reads_as_no_state (4 params)
    for raw in ["[1, 2]", "null", "{torn", "\"text\""] {
        let h = state_home();
        write_state(&backoff_file(&h), raw);
        assert_eq!(
            Backoff::read(&PyPath::new(&backoff_file(&h))).unwrap(),
            Backoff::default(),
            "{raw}"
        );
    }
}

#[test]
fn backoff_round_trip_keeps_the_030_keys() {
    // test_backoff_round_trip_keeps_the_030_keys
    let h = state_home();
    let runner = PFake::timeout();
    ask_n(&h, "s1", &runner, 0);
    let (got, b, a) = ask_n(&h, "s1", &runner, 1);
    one_of_until(&got, timeout_notice, b, a);
    let written = file_json(&backoff_file(&h));
    let until = written["until"].as_f64().unwrap();
    assert!(b + 600.0 <= until && until <= a + 600.0, "{until}");
    assert_eq!(
        written,
        json!({"failures": 2, "until": until, "error": "rewrite timed out after 8.0 s", "cause": "timeout", "notified": ["s1"]})
    );
    let p = PyPath::new(&backoff_file(&h));
    Backoff::read(&p).unwrap().write(&p).unwrap();
    assert_eq!(file_json(&backoff_file(&h)), written);
}

#[test]
fn a_failed_replace_keeps_the_temp_file_and_names_both_paths() {
    // `os.replace(tmp, path)` onto a folder, probed with Python 3.14.6 on macOS (mkstemp in the same folder):
    // IsADirectoryError: [Errno 21] Is a directory: '<dir>/.backoff.json.<8 chars>' -> '<dir>/backoff.json'
    // and the temp file is still there afterwards.
    let h = py_home();
    let dst = backoff_file(&h);
    std::fs::create_dir_all(&dst).unwrap();
    let err = Backoff::default().write(&PyPath::new(&dst)).unwrap_err();
    let temps: Vec<String> = std::fs::read_dir(h.file("hook"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n != "backoff.json")
        .collect();
    assert_eq!(temps.len(), 1, "{temps:?}");
    assert!(
        temps[0].starts_with(".backoff.json.") && temps[0].len() == ".backoff.json.".len() + 8,
        "{temps:?}"
    );
    let tmp = h.file(&format!("hook/{}", temps[0]));
    assert_eq!(
        err.python_text(),
        format!("IsADirectoryError: [Errno 21] Is a directory: '{tmp}' -> '{dst}'")
    );
    assert!(std::path::Path::new(&tmp).is_file());
}

#[test]
fn junk_session_values_read_as_defaults() {
    // test_junk_session_values_read_as_defaults (4 params)
    for junk in [
        "{\"transcript_size\": \"big\", \"injected\": 5}",
        "{\"transcript_size\": null, \"injected\": null}",
        "{\"transcript_size\": true, \"injected\": [1, null]}",
        "{\"transcript_size\": 2.5, \"injected\": \"abc\"}",
    ] {
        let h = state_home();
        write_state(&session_file(&h, "s1"), junk);
        assert_eq!(
            SessionState::read(&PyPath::new(&session_file(&h, "s1"))).unwrap(),
            Some(SessionState::default()),
            "{junk}"
        );
        let found = hook_ok(
            &h,
            &PFake::out("R6 weakening"),
            json!({"prompt": "R6 revert check", "session_id": "s1", "cwd": h.path}),
        )
        .unwrap();
        assert_eq!(found.items.len(), 1);
    }
}

#[test]
fn session_round_trip_keeps_the_030_keys() {
    // test_session_round_trip_keeps_the_030_keys
    let h = state_home();
    let runner = PFake::out("R6 weakening");
    let found = hook_ok(
        &h,
        &runner,
        json!({"prompt": "R6 revert check", "session_id": "s1", "cwd": h.path}),
    )
    .unwrap();
    let written = file_json(&session_file(&h, "s1"));
    assert_eq!(
        written,
        json!({"transcript_size": 0, "injected": [found.items[0].id]})
    );
    let p = PyPath::new(&session_file(&h, "s1"));
    SessionState::read(&p).unwrap().unwrap().write(&p).unwrap();
    assert_eq!(file_json(&session_file(&h, "s1")), written);
    let again = hook_ok(
        &h,
        &runner,
        json!({"prompt": "R6 revert check two", "session_id": "s1", "cwd": h.path}),
    )
    .unwrap();
    assert_eq!(again.items, vec![]);
}

// ------------------------------------------------------------------ recall sequence

const SESSIONS: [&str; 4] = ["s1", "s2", "agy", "noid"];
const OUTCOMES: [&str; 3] = ["ok", "timeout", "error"];

#[derive(Clone, Copy, Debug)]
enum Ev {
    Prompt(&'static str, &'static str),
    Pass,
}

fn all_events() -> Vec<Ev> {
    let mut v: Vec<Ev> = SESSIONS
        .iter()
        .flat_map(|s| OUTCOMES.iter().map(move |o| Ev::Prompt(s, o)))
        .collect();
    v.push(Ev::Pass);
    v
}

fn error_text(outcome: &str) -> &'static str {
    match outcome {
        "timeout" => "rewrite timed out after 8.0 s",
        "error" => "claude exited 1: boom",
        _ => "",
    }
}

struct Notice {
    all_timeouts: bool,
    error: String,
    until: (f64, f64),
}

impl Notice {
    fn check(&self, got: &str) {
        if self.all_timeouts {
            return one_of_until(got, timeout_notice, self.until.0, self.until.1);
        }
        let paren = if self.error.is_empty() {
            String::new()
        } else {
            format!(" ({})", self.error)
        };
        one_of_until(
            got,
            |end| error_notice(&paren, end),
            self.until.0,
            self.until.1,
        );
    }
}

/// The spec model of the docstring: window open or closed, one streak, who was told.
struct Spec {
    failures: i64,
    open: bool,
    all_timeouts: bool,
    error: String,
    notified: std::collections::HashSet<String>,
    until: (f64, f64),
}

impl Spec {
    fn new() -> Spec {
        Spec {
            failures: 0,
            open: false,
            all_timeouts: true,
            error: String::new(),
            notified: Default::default(),
            until: (0.0, 0.0),
        }
    }

    fn take(&mut self, sid: &str) -> Option<Notice> {
        let key = match sid {
            "noid" => "",
            // Spec change (2026-11-19): agy Harness::prompt sends conversationId "conv-1".
            "agy" => "conv-1",
            _ => sid,
        };
        if !self.notified.insert(key.to_string()) {
            return None;
        }
        Some(Notice {
            all_timeouts: self.all_timeouts,
            error: self.error.clone(),
            until: self.until,
        })
    }

    /// (model called, notice, fallback) for one prompt sent between `before` and `after`.
    fn step(
        &mut self,
        sid: &str,
        outcome: &str,
        before: f64,
        after: f64,
    ) -> (bool, Option<Notice>, &'static str) {
        if self.open {
            return (false, self.take(sid), "backoff");
        }
        if outcome == "ok" {
            *self = Spec::new();
            return (true, None, "");
        }
        self.all_timeouts = (self.failures == 0 || self.all_timeouts) && outcome == "timeout";
        self.failures += 1;
        self.error = error_text(outcome).to_string();
        let fallback = if outcome == "timeout" {
            "timeout"
        } else {
            "error"
        };
        if self.failures >= 2 {
            self.open = true;
            self.until = (before, after);
            self.notified.clear();
            return (true, self.take(sid), fallback);
        }
        (true, None, fallback)
    }
}

struct Harness {
    home: PyHome,
    cwd: String,
    log_at: usize,
}

impl Harness {
    fn new(model: &str) -> Harness {
        let h = py_home();
        let code = h.file("code");
        std::fs::create_dir_all(format!("{code}/alpha")).unwrap();
        let cwd = h.file(&format!(
            "-{}-alpha",
            crate::core::refs::encode_project_dir(&code).trim_matches('-')
        ));
        let hn = Harness {
            home: h,
            cwd,
            log_at: 0,
        };
        hn.config(model);
        hn
    }

    fn config(&self, model: &str) {
        let roots = self.home.file("code");
        set_config(
            &self.home,
            &format!(
                "[catch_up]\nenabled = false\n[repos]\nroots = [\"{roots}\"]\n[prompt_hook]\n{model}"
            ),
        );
    }

    fn reset(&mut self) {
        let _ = std::fs::remove_dir_all(self.home.file("hook"));
        let _ = std::fs::remove_file(self.home.file("hook.log"));
        self.log_at = 0;
    }

    /// (model called, notice, hook.log path, fallback, clock before, clock after) for one prompt.
    fn prompt(
        &mut self,
        sid: &str,
        outcome: &str,
        n: usize,
    ) -> (bool, String, String, String, f64, f64) {
        let fake = match outcome {
            "ok" => PFake::out("R6 weakening"),
            "timeout" => PFake::timeout(),
            "blank" => PFake::fail("OSError", ""),
            _ => PFake::exit(1),
        };
        let text = format!("R6 revert check {sid} {n}");
        let (payload, agy) = match sid {
            "agy" => (
                json!({"lastUserInput": text, "conversationId": "conv-1", "cwd": self.cwd}),
                true,
            ),
            "noid" => (json!({"prompt": text, "cwd": self.cwd}), false),
            _ => (
                json!({"prompt": text, "session_id": sid, "cwd": self.cwd}),
                false,
            ),
        };
        let before = clock::now_epoch();
        let found = run_hook(&self.home, &fake, &pl(payload), agy).unwrap();
        let after = clock::now_epoch();
        let log = std::fs::read_to_string(self.home.file("hook.log")).unwrap();
        let new: Vec<Json> = log[self.log_at..]
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        self.log_at = log.len();
        assert_eq!(new.len(), 1, "{new:?}");
        let fallback = new[0]
            .get("fallback")
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_string();
        if let Some(f) = &found {
            assert_eq!(f.fallback, fallback);
        }
        (
            fake.n() > 0,
            found.map(|f| f.notice).unwrap_or_default(),
            new[0]["path"].as_str().unwrap().into(),
            fallback,
            before,
            after,
        )
    }

    fn run(&mut self, events: &[Ev]) {
        self.reset();
        let mut spec = Spec::new();
        for (n, ev) in events.iter().enumerate() {
            let Ev::Prompt(sid, outcome) = *ev else {
                if spec.open {
                    shift_window(&self.home, 1e9);
                    spec.open = false;
                }
                continue;
            };
            let (got_called, got_notice, got_path, got_fb, before, after) =
                self.prompt(sid, outcome, n);
            let (called, notice, fallback) = spec.step(sid, outcome, before, after);
            let path = if outcome == "ok" && called {
                "haiku"
            } else {
                "local"
            };
            let seen = format!("sequence {events:?}");
            assert_eq!(
                (got_called, got_path.as_str(), got_fb.as_str()),
                (called, path, fallback),
                "{seen}"
            );
            match notice {
                None => assert_eq!(got_notice, "", "{seen}"),
                Some(n) => n.check(&got_notice),
            }
        }
    }
}

#[test]
fn every_sequence_up_to_three_events_matches_the_spec() {
    // test_every_sequence_up_to_three_events_matches_the_spec: all 13^3 = 2197 sequences
    let mut h = Harness::new("");
    let ev = all_events();
    for a in &ev {
        for b in &ev {
            for c in &ev {
                h.run(&[*a, *b, *c]);
            }
        }
    }
}

#[test]
fn seeded_random_sequences_up_to_eight_events_match_the_spec() {
    // test_seeded_random_sequences_up_to_eight_events_match_the_spec (300 seeds). Python draws
    // from random.Random(seed); Rust has no Mersenne Twister, so a 64-bit LCG per seed draws
    // other sequences of the same shape, with the blank-message failure added.
    let mut h = Harness::new("");
    let mut ev = all_events();
    ev.extend(SESSIONS.iter().map(|s| Ev::Prompt(s, "blank")));
    for seed in 0..300u64 {
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
        let mut next = |n: usize| {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (x >> 33) as usize % n
        };
        let len = 1 + next(8);
        let events: Vec<Ev> = (0..len).map(|_| ev[next(ev.len())]).collect();
        h.run(&events);
    }
}

#[test]
fn window_from_blank_errors_prints_no_parenthetical() {
    // test_window_from_blank_errors_prints_no_parenthetical
    Harness::new("").run(&[
        Ev::Prompt("s1", "blank"),
        Ev::Prompt("s1", "blank"),
        Ev::Prompt("s2", "ok"),
    ]);
}

#[test]
fn a_failure_after_a_window_opens_a_new_one_that_tells_every_session_again() {
    // test_a_failure_after_a_window_opens_a_new_one_that_tells_every_session_again (3 params)
    use Ev::{Pass, Prompt};
    let mut h = Harness::new("");
    h.run(&[
        Prompt("s1", "error"),
        Prompt("s1", "error"),
        Pass,
        Prompt("s1", "error"),
        Prompt("s1", "ok"),
    ]);
    h.run(&[
        Prompt("s1", "timeout"),
        Prompt("agy", "timeout"),
        Pass,
        Prompt("agy", "timeout"),
        Prompt("s2", "ok"),
        Prompt("agy", "ok"),
    ]);
    h.run(&[
        Prompt("s1", "error"),
        Prompt("s2", "timeout"),
        Pass,
        Prompt("s2", "error"),
        Pass,
        Prompt("s1", "ok"),
        Prompt("s1", "error"),
    ]);
}

#[test]
fn model_none_never_calls_the_model_or_keeps_backoff_state() {
    // test_model_none_never_calls_the_model_or_keeps_backoff_state (12 draws, Python seed 7)
    let mut h = Harness::new("model = \"none\"\n");
    h.reset();
    let ev = all_events();
    let mut x = 7u64;
    for n in 0..12 {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let Ev::Prompt(sid, outcome) = ev[(x >> 33) as usize % ev.len()] else {
            continue;
        };
        let (called, notice, path, fallback, ..) = h.prompt(sid, outcome, n);
        assert_eq!(
            (called, notice.as_str(), path.as_str(), fallback.as_str()),
            (false, "", "local", "off")
        );
    }
    assert!(!std::path::Path::new(&backoff_file(&h.home)).exists());
}

#[test]
fn claude_output_that_is_not_utf8_raises_unicode_decode_error() {
    // `subprocess.run(text=True)` decodes strictly. Python 3.15.1 on `sh -c 'printf "\377"'`:
    // UnicodeDecodeError: 'utf-8' codec can't decode byte 0xff in position 0: invalid start byte.
    // The hook does not catch it (it is no OSError, RuntimeError or timeout), so the CLI wrapper
    // prints `sm hook: ...` and injects nothing. Every model and git call goes through
    // capture.rs `run_process`.
    let run = |script: &str| {
        run_process(&Run {
            argv: ["sh", "-c", script].iter().map(|s| s.to_string()).collect(),
            input: Some(String::new()),
            timeout: Some(5.0),
            text: true,
            ..Default::default()
        })
    };
    let err = run("printf '\\377'").unwrap_err();
    assert_eq!(
        (err.class.as_ref(), err.msg.as_str()),
        (
            "UnicodeDecodeError",
            "'utf-8' codec can't decode byte 0xff in position 0: invalid start byte"
        )
    );
    // valid UTF-8 and CRLF are read as Python reads them
    assert_eq!(run("printf 'a\\r\\nb\\rc'").unwrap().stdout, "a\nb\nc");
}

#[test]
fn query_limit_for_dedupe_is_capped() {
    // test_query_limit_for_dedupe_is_capped. Python lowers `_MAX_QUERY_LIMIT` to 3; Rust's is 50
    // A union of 55 items proves the 50-item cap vs 51: 60 "cafe" items (folded index) and 5
    // "café" items (exact accent index) union to 55 before the cap.
    let h = py_home();
    set_config(
        &h,
        "[prompt_hook]\nlimit = 2\nmin_idf = 0.0\n[catch_up]\nenabled = false\n",
    );
    let filler: String = (0..80)
        .map(|n| format!("filler{n}"))
        .collect::<Vec<_>>()
        .join(" ");
    bulk(&h, "global", (0..60).map(|i| format!("Cafe cafe cafe {i}")));
    bulk(&h, "global", (0..5).map(|i| format!("café {i} {filler}")));
    let q = "café";
    let wide = |limit| {
        let a = QueryArgs {
            limit,
            home: Some(&h.path),
            cwd: Some(&h.path),
            ..Default::default()
        };
        super::run_query(q, &a, None)
            .unwrap()
            .into_iter()
            .map(|it| it.id)
            .collect::<Vec<_>>()
    };
    let ranked = wide(51);
    assert_eq!(ranked.len(), 51);
    write_state(
        &session_file(&h, "s-cap"),
        &format!(
            "{{\"transcript_size\": 0, \"injected\": {}}}",
            json!(ranked[..49])
        ),
    );
    let found = hook_ok(
        &h,
        &PFake::out(q),
        json!({"prompt": "café still failing please", "session_id": "s-cap", "cwd": h.path}),
    )
    .unwrap();
    assert_eq!(
        found
            .items
            .iter()
            .map(|it| it.id.clone())
            .collect::<Vec<_>>(),
        ranked[49..50].to_vec()
    );
}

#[test]
fn torn_session_state_starts_the_session_over() {
    // test_torn_session_state_starts_the_session_over
    let h = session_home();
    let t = transcript(&h, &["{\"type\":\"user\"}"]);
    let first = ask1(&h, &t);
    let state = std::fs::read_dir(h.file("hook"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("claude-")
        })
        .unwrap();
    std::fs::write(&state, "{\"transcript_size\": 1").unwrap();
    assert_eq!(ask1(&h, &t), first);
    assert_eq!(poss(&h), [json!("first"), json!("first")]);
}

#[test]
fn scan_stops_when_the_file_ends_before_the_expected_size() {
    // test_scan_stops_when_the_file_ends_before_the_expected_size
    let h = session_home();
    let t = transcript(&h, &["{\"type\":\"user\"}"]);
    let size = std::fs::metadata(&t).unwrap().len() as i128;
    assert!(!super::compacted_since(&t, 0, size + 100).unwrap());
    std::fs::write(&t, format!("{BOUNDARY}\n")).unwrap();
    let size = std::fs::metadata(&t).unwrap().len() as i128;
    assert!(super::compacted_since(&t, 0, size).unwrap());
}
