//! Port of the Python capture tests (simple-memory main 555b8f2): session cards on fixture
//! transcripts. Inputs and expected values follow the Python test. The Python FakeClaude runner
//! is a stub `claude` shell script set as `[prompt_hook] claude_bin`; it records argv, env, cwd
//! and stdin per call (the Python test reads the same fields from `fake.calls`).
mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, UNIX_EPOCH};

use serde_json::{Value, json};
use simple_memory::config::load_config;
use simple_memory::core::item::{Item, epoch_to_as_of};
use simple_memory::py::path::PyPath;
use simple_memory::py::value::PyValue;
use simple_memory::service::capture::{CaptureResult, capture_sessions, run};
use simple_memory::sources::transcript::{SessionScan, scan_transcript};
use simple_memory::store::items::put_items_batch;

const NOW: f64 = 1_800_000_000.0;
const HOUR: f64 = 3600.0;
const SID_A: &str = "aaaaaaaa-1111-4111-8111-000000000001";
const SID_B: &str = "bbbbbbbb-2222-4222-8222-000000000002";
const SID_C: &str = "cccccccc-3333-4333-8333-000000000003";
const CARD_ANSWER: &str = "Alpha parser fixed and pushed\n\
- Done: Fixed the parser in src/alpha.py and pushed abc1234.\n\
- Decided: Keep the old flag.\n\
- Open: Docs still name the old flag.\n";

// ---------------------------------------------------------------- transcript rows

fn user(content: Value, extra: Value) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("type".into(), json!("user"));
    m.insert("isSidechain".into(), json!(false));
    m.insert(
        "message".into(),
        json!({"role": "user", "content": content}),
    );
    if let Value::Object(e) = extra {
        m.extend(e);
    }
    Value::Object(m)
}

fn ask(text: &str, extra: Value) -> Value {
    user(json!(text), extra)
}

fn assistant(blocks: Value, extra: Value) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("type".into(), json!("assistant"));
    m.insert("isSidechain".into(), json!(false));
    m.insert(
        "message".into(),
        json!({"role": "assistant", "content": blocks}),
    );
    if let Value::Object(e) = extra {
        m.extend(e);
    }
    Value::Object(m)
}

fn commit_pair(tool_id: &str, result: &str, is_error: bool) -> Vec<Value> {
    vec![
        assistant(
            json!([{"type": "tool_use", "id": tool_id, "name": "Bash", "input": {"command": "git commit -m 'fix parser'"}}]),
            json!({}),
        ),
        user(
            json!([{"type": "tool_result", "tool_use_id": tool_id, "content": result, "is_error": is_error}]),
            json!({}),
        ),
    ]
}

fn set_mtime(path: &Path, t: f64) {
    let f = std::fs::File::options().write(true).open(path).unwrap();
    f.set_modified(UNIX_EPOCH + Duration::from_secs_f64(t))
        .unwrap();
}

fn write_rows(path: &Path, rows: &[Value], mtime: f64) -> PathBuf {
    let text: String = rows
        .iter()
        .map(|r| format!("{}\n", serde_json::to_string(r).unwrap()))
        .collect();
    common::write(path, &text);
    set_mtime(path, mtime);
    path.to_path_buf()
}

fn work_rows(cwd: &Path, prompts: usize) -> Vec<Value> {
    work_rows_at(cwd, prompts, "2027-01-15T08:00:00.000Z", "alpha parser")
}

/// `prompts` owner prompts "step N of the <work> work" and replies, every row at `ts`.
fn work_rows_at(cwd: &Path, prompts: usize, ts: &str, work: &str) -> Vec<Value> {
    let mut rows = vec![json!({"type": "permission-mode", "permissionMode": "default"})];
    for n in 0..prompts {
        rows.push(ask(
            &format!("step {n} of the {work} work"),
            json!({"cwd": cwd.to_str().unwrap(), "timestamp": ts, "turnOrigin": "human"}),
        ));
        rows.push(assistant(
            json!([{"type": "text", "text": format!("Done with step {n}.")}]),
            json!({"cwd": cwd.to_str().unwrap(), "timestamp": ts}),
        ));
    }
    rows
}

// ---------------------------------------------------------------- the env fixture

struct Env {
    _d: tempfile::TempDir,
    tmp: PathBuf,
    repo: PathBuf,
    root: PathBuf,
    home: PathBuf,
    project: PathBuf,
    stub: PathBuf,
}

/// One `claude` call as the stub saw it.
struct Call {
    /// argv after the binary.
    args: Vec<String>,
    hook: String,
    traffic: String,
    cwd: String,
    stdin: String,
}

impl Env {
    fn new() -> Env {
        let d = tempfile::tempdir().unwrap();
        let tmp = std::fs::canonicalize(d.path()).unwrap();
        let repo = tmp.join("alpha");
        std::fs::create_dir_all(&repo).unwrap();
        common::git(&["init", "-q", repo.to_str().unwrap()]);
        let root = tmp.join("projects");
        let home = tmp.join("home");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let stub = tmp.join("stub");
        std::fs::create_dir_all(stub.join("calls")).unwrap();
        let script = format!(
            "#!/bin/sh\nd='{d}'\nn=$$\n\
             printf '%s\\0' \"$@\" > \"$d/calls/$n.args\"\n\
             printf '%s' \"${{SM_PROMPT_HOOK-<unset>}}\" > \"$d/calls/$n.hook\"\n\
             printf '%s' \"${{CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC-<unset>}}\" > \"$d/calls/$n.traffic\"\n\
             pwd -P > \"$d/calls/$n.cwd\"\n\
             cat > \"$d/calls/$n.stdin\"\n\
             cat \"$d/answer\"\n\
             rc=$(cat \"$d/rc\")\n\
             if [ \"$rc\" != 0 ]; then echo boom >&2; fi\n\
             exit \"$rc\"\n",
            d = stub.display()
        );
        let bin = stub.join("claude");
        std::fs::write(&bin, script).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env = Env {
            _d: d,
            tmp,
            repo,
            root: root.clone(),
            home,
            project: root.join("-tmp-alpha"),
            stub,
        };
        env.answer(CARD_ANSWER, 0);
        env
    }

    /// FakeClaude(stdout, returncode).
    fn answer(&self, stdout: &str, rc: i32) {
        std::fs::write(self.stub.join("answer"), stdout).unwrap();
        std::fs::write(self.stub.join("rc"), rc.to_string()).unwrap();
    }

    fn home_str(&self) -> &str {
        self.home.to_str().unwrap()
    }

    /// config.toml with the capture root and the stub, plus `capture_extra` lines under
    /// `[capture]` and `other` sections; the cfg `load_config` gives.
    fn cfg_with(&self, capture_extra: &str, other: &str) -> PyValue {
        self.write_config(capture_extra, other, Some(&self.stub.join("claude")));
        load_config(Some(self.home_str())).unwrap()
    }

    fn cfg(&self) -> PyValue {
        self.cfg_with("", "")
    }

    fn write_config(&self, capture_extra: &str, other: &str, claude_bin: Option<&Path>) {
        let bin = claude_bin
            .map(|b| format!("[prompt_hook]\nclaude_bin = \"{}\"\n", b.display()))
            .unwrap_or_default();
        let text = format!(
            "[capture]\nroot = \"{}\"\n{capture_extra}\n{bin}{other}",
            self.root.display()
        );
        common::write(&self.home.join("config.toml"), &text);
    }

    fn capture(&self, cfg: &PyValue, now: f64) -> CaptureResult {
        capture_sessions(Some(self.home_str()), cfg, false, None, None, Some(now)).unwrap()
    }

    fn calls(&self) -> Vec<Call> {
        let dir = self.stub.join("calls");
        let mut out = Vec::new();
        for e in std::fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            if p.extension().is_some_and(|x| x == "args") {
                let read = |ext: &str| std::fs::read_to_string(p.with_extension(ext)).unwrap();
                let raw = std::fs::read(&p).unwrap();
                let body = raw.strip_suffix(&[0u8]).unwrap_or(&raw);
                let args = body
                    .split(|b| *b == 0)
                    .map(|a| String::from_utf8(a.to_vec()).unwrap())
                    .collect();
                out.push(Call {
                    args,
                    hook: read("hook"),
                    traffic: read("traffic"),
                    cwd: read("cwd").trim_end().to_string(),
                    stdin: read("stdin"),
                });
            }
        }
        out
    }

    fn transcript(&self, sid: &str, rows: &[Value], mtime: f64) -> PathBuf {
        write_rows(&self.project.join(format!("{sid}.jsonl")), rows, mtime)
    }
}

#[derive(Debug, PartialEq)]
struct Card {
    repo: String,
    kind: String,
    title: String,
    body: String,
    refs: String,
    tags: String,
    source: String,
    tag: String,
    evidence: String,
    as_of: String,
}

fn cards(home: &Path) -> Vec<Card> {
    let conn = common::open(home);
    let mut stmt = conn
        .prepare(
            "SELECT repo, kind, title, body, refs, tags, source, tag, evidence, as_of FROM items \
             WHERE source LIKE 'session:%' ORDER BY source",
        )
        .unwrap();
    stmt.query_map([], |r| {
        let s = |i: usize| r.get::<_, Option<String>>(i).map(Option::unwrap_or_default);
        Ok(Card {
            repo: s(0)?,
            kind: s(1)?,
            title: s(2)?,
            body: s(3)?,
            refs: s(4)?,
            tags: s(5)?,
            source: s(6)?,
            tag: s(7)?,
            evidence: s(8)?,
            as_of: s(9)?,
        })
    })
    .unwrap()
    .collect::<rusqlite::Result<Vec<_>>>()
    .unwrap()
}

/// One superseded task item that covers a session up to `NOW`.
fn store(home: &Path, source: &str, evidence: &str) {
    let ts = epoch_to_as_of(NOW).unwrap();
    store_at(home, source, evidence, &ts);
}

fn store_at(home: &Path, source: &str, evidence: &str, as_of: &str) {
    let conn = common::open(home);
    let item = Item {
        source: source.into(),
        evidence: evidence.into(),
        as_of: as_of.into(),
        ..Item::bare("m_X", "alpha", "task", "t", "b", "derived")
    }
    .post_init();
    let res = put_items_batch(&conn, &[item], false).unwrap();
    assert!(res.5.is_empty(), "store failed: {:?}", res.5);
    conn.execute("UPDATE items SET status = 'superseded'", [])
        .unwrap();
}

fn n(res: &CaptureResult, key: &str) -> i64 {
    res.counts[key]
}

fn sources(home: &Path) -> Vec<String> {
    cards(home).into_iter().map(|c| c.source).collect()
}

fn scan(dir: &Path, rows: &[Value]) -> SessionScan {
    let path = write_rows(&dir.join("p").join(format!("{SID_A}.jsonl")), rows, NOW);
    scan_transcript(&PyPath::new(path.to_str().unwrap()), 30000, None).unwrap()
}

// ---------------------------------------------------------------- scan_transcript

#[test]
fn owner_prompt_count_ignores_reminders_tool_results_command_output_and_meta() {
    let d = tempfile::tempdir().unwrap();
    let rows = vec![
        ask(
            "First real ask\nsecond line",
            json!({"turnOrigin": "human"}),
        ),
        user(
            json!([{"type": "text", "text": "<system-reminder>ctx</system-reminder>\nSecond ask"}]),
            json!({}),
        ),
        ask(
            "<system-reminder>only a reminder</system-reminder>",
            json!({}),
        ),
        ask(
            "<task-notification>lane done</task-notification>",
            json!({"turnOrigin": "task_notification"}),
        ),
        user(
            json!([{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]),
            json!({}),
        ),
        ask(
            "<local-command-stdout>output</local-command-stdout>",
            json!({}),
        ),
        ask("<command-name>/clear</command-name>", json!({})),
        ask("<bash-stdout>ls</bash-stdout>", json!({})),
        ask(
            "Caveat: The messages below were generated by the user while running local commands.",
            json!({}),
        ),
        ask("Skill text loaded", json!({"isMeta": true})),
        ask(
            "This session is being continued from a previous conversation.",
            json!({"isCompactSummary": true}),
        ),
        ask("a lane prompt", json!({"isSidechain": true})),
        ask("peer message", json!({"turnOrigin": "peer"})),
        ask("Third ask, plain", json!({})),
    ];
    let s = scan(d.path(), &rows);
    assert_eq!(s.prompts, 3);
    assert_eq!(s.first_prompt, "First real ask\nsecond line");
    assert_eq!(
        s.dialogue,
        [
            "Owner: First real ask\nsecond line",
            "Owner: Second ask",
            "Owner: Third ask, plain"
        ]
    );
}

#[test]
fn commit_counts_only_successful_results_and_reads_sha_and_subject() {
    let d = tempfile::tempdir().unwrap();
    let mut rows = Vec::new();
    rows.extend(commit_pair(
        "c1",
        "[main abc1234] fix parser\n 1 file changed",
        false,
    ));
    rows.extend(commit_pair(
        "c2",
        "On branch main\nnothing to commit, working tree clean",
        false,
    ));
    rows.extend(commit_pair("c3", "error: hook failed", true));
    rows.extend(commit_pair(
        "c4",
        "[feat (root-commit) 0123abcd] first commit",
        false,
    ));
    rows.push(assistant(json!([{"type": "tool_use", "id": "l1", "name": "Bash", "input": {"command": "git log -1 --oneline"}}]), json!({})));
    rows.push(user(
        json!([{"type": "tool_result", "tool_use_id": "l1", "content": "[main 9876fed] older"}]),
        json!({}),
    ));
    let s = scan(d.path(), &rows);
    assert_eq!(s.commits, 2);
    assert_eq!(s.shas, ["abc1234", "0123abcd"]);
    assert_eq!(s.commit_subjects, ["fix parser", "first commit"]);
}

#[test]
fn dialogue_keeps_start_and_end_within_the_cap() {
    let d = tempfile::tempdir().unwrap();
    let rows: Vec<Value> = (0..10)
        .map(|k| {
            ask(
                &format!("ask {k} {}", "x".repeat(90)),
                json!({"turnOrigin": "human"}),
            )
        })
        .collect();
    // Each line "Owner: ask N " + 90 x is 103 characters; half the cap is 250, so the start
    // keeps 2 lines (206) and the end keeps the last 2.
    let path = write_rows(
        &d.path().join("p").join(format!("{SID_A}.jsonl")),
        &rows,
        NOW,
    );
    let s = scan_transcript(&PyPath::new(path.to_str().unwrap()), 500, None).unwrap();
    let heads: Vec<String> = s
        .dialogue
        .iter()
        .map(|l| l.chars().take(12).collect())
        .collect();
    assert_eq!(
        heads,
        [
            "Owner: ask 0",
            "Owner: ask 1",
            "[...]",
            "Owner: ask 8",
            "Owner: ask 9"
        ]
    );
}

#[test]
fn commit_dry_run_and_no_changes_result_are_not_commits() {
    let d = tempfile::tempdir().unwrap();
    let mut rows = vec![
        assistant(
            json!([{"type": "tool_use", "id": "d1", "name": "Bash", "input": {"command": "git commit --dry-run -m x"}}]),
            json!({}),
        ),
        user(
            json!([{"type": "tool_result", "tool_use_id": "d1", "content": "[main abc1234] x"}]),
            json!({}),
        ),
    ];
    rows.extend(commit_pair(
        "c1",
        "no changes added to commit (use \"git add\")",
        false,
    ));
    assert_eq!(scan(d.path(), &rows).commits, 0);
}

// ---------------------------------------------------------------- capture_sessions

#[test]
fn card_from_haiku_has_the_frozen_shape() {
    let env = Env::new();
    let mut rows = work_rows(&env.repo, 5);
    rows.extend(commit_pair("c1", "[main abc1234] fix parser", false));
    rows.push(assistant(json!([{"type": "tool_use", "id": "w1", "name": "Write", "input": {"file_path": "/w/src/alpha.py"}}]), json!({})));
    rows.push(assistant(
        json!([{"type": "text", "text": "Logged 20270115-080000-aaaaaaaa-alpha-fix-1a2b. \
                 Resumed from 20270114-070000-dddddddd-4444-4444-8444-000000000004.md."}]),
        json!({}),
    ));
    let path = env.transcript(SID_A, &rows, NOW - 2.0 * HOUR);
    let res = env.capture(&env.cfg(), NOW);
    assert_eq!((n(&res, "carded"), n(&res, "plain")), (1, 0));
    assert_eq!(
        res.by_repo
            .iter()
            .map(|(k, v)| (k.as_str(), *v))
            .collect::<Vec<_>>(),
        [("alpha", 1)]
    );
    let got = cards(&env.home);
    assert_eq!(got.len(), 1);
    let card = &got[0];
    assert_eq!(card.repo, "alpha");
    assert_eq!(card.kind, "episode");
    assert_eq!(card.tag, "derived");
    assert_eq!(card.tags, "session-card");
    assert_eq!(card.source, format!("session:{SID_A}"));
    assert_eq!(card.evidence, path.to_str().unwrap());
    let now_iso = epoch_to_as_of(NOW).unwrap();
    assert_eq!(card.as_of, now_iso);
    assert_eq!(card.title, "Alpha parser fixed and pushed");
    assert_eq!(
        card.body,
        format!(
            "- Done: Fixed the parser in src/alpha.py and pushed abc1234.\n\
             - Decided: Keep the old flag.\n\
             - Open: Docs still name the old flag.\n\n\
             Session {SID_A}: owner prompts 5, commits 1, last active {now_iso}."
        )
    );
    let refs: Vec<&str> = card.refs.split_whitespace().collect();
    assert_eq!(
        refs[..3],
        [
            "abc1234",
            "20270115-080000-aaaaaaaa-alpha-fix-1a2b",
            "/w/src/alpha.py"
        ]
    );
    assert!(
        !refs
            .iter()
            .any(|r| r.starts_with("20270114-070000-dddddddd"))
    );
    let calls = env.calls();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(
        call.args[..call.args.len() - 1],
        [
            "-p",
            "--model",
            "haiku",
            "--setting-sources",
            "",
            "--strict-mcp-config",
            "--settings",
            "{\"disableAllHooks\":true}",
            "--no-session-persistence",
            "--tools",
            "",
            "--system-prompt"
        ]
    );
    assert_eq!(call.hook, "0");
    assert_eq!(call.cwd, env.home.to_str().unwrap());
    assert!(
        call.stdin
            .starts_with("Owner: step 0 of the alpha parser work\n\nAgent: Done with step 0."),
        "{}",
        call.stdin
    );
}

/// `sm import sessions` as a user runs it, with the inherited variable set or removed.
fn sessions_with_traffic_var(env: &Env, inherited: Option<&str>) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sm"));
    cmd.args(["import", "sessions"])
        .env("HOME", &env.tmp)
        .env("SIMPLE_MEMORY_HOME", &env.home)
        .env_remove("SM_PARITY_NOW")
        .env_remove("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC");
    if let Some(v) = inherited {
        cmd.env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", v);
    }
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn card_call_turns_off_nonessential_claude_traffic() {
    for inherited in [None, Some("0")] {
        let env = Env::new();
        env.write_config("", "", Some(&env.stub.join("claude")));
        let now = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        env.transcript(SID_A, &work_rows(&env.repo, 5), now - 2.0 * HOUR);
        sessions_with_traffic_var(&env, inherited);
        let calls = env.calls();
        assert_eq!(calls.len(), 1, "inherited {inherited:?}");
        assert_eq!(calls[0].traffic, "1", "inherited {inherited:?}");
    }
}

#[test]
fn haiku_failure_writes_a_plain_card() {
    // (id, FakeClaude stdout, returncode)
    for (id, stdout, rc) in [
        ("claude-error", CARD_ANSWER, 1),
        ("no-bullets", "I cannot summarize this.\n", 0),
    ] {
        let env = Env::new();
        env.answer(stdout, rc);
        let mut rows = vec![ask(
            "Fix the alpha parser\nthen push",
            json!({"cwd": env.repo.to_str().unwrap(), "turnOrigin": "human", "timestamp": "2027-01-15T08:00:00Z"}),
        )];
        rows.extend(commit_pair("c1", "[main abc1234] fix parser", false));
        rows.push(assistant(json!([{"type": "tool_use", "id": "w1", "name": "Edit", "input": {"file_path": "/w/src/alpha.py"}}]), json!({})));
        env.transcript(SID_A, &rows, NOW - 2.0 * HOUR);
        let res = env.capture(&env.cfg(), NOW);
        assert_eq!((n(&res, "carded"), n(&res, "plain")), (1, 1), "{id}");
        let got = cards(&env.home);
        assert_eq!(got.len(), 1, "{id}");
        assert_eq!(got[0].title, "Fix the alpha parser", "{id}");
        let lines: Vec<&str> = got[0].body.lines().take(2).collect();
        assert_eq!(
            lines,
            ["- Commit: fix parser", "- Wrote: /w/src/alpha.py"],
            "{id}"
        );
    }
}

#[test]
fn headless_session_is_skipped() {
    let env = Env::new();
    let mut rows = vec![ask(
        "Summarize the repo",
        json!({"turnOrigin": "sdk", "cwd": env.repo.to_str().unwrap()}),
    )];
    rows.extend(commit_pair("c1", "[main abc1234] fix parser", false));
    env.transcript(SID_A, &rows, NOW - 2.0 * HOUR);
    let res = env.capture(&env.cfg(), NOW);
    assert_eq!((n(&res, "headless"), n(&res, "carded")), (1, 0));
    assert!(cards(&env.home).is_empty());
}

#[test]
fn light_session_without_commit_is_skipped_and_one_commit_is_enough() {
    let env = Env::new();
    env.transcript(SID_A, &work_rows(&env.repo, 4), NOW - 2.0 * HOUR);
    let mut one_commit = work_rows(&env.repo, 1);
    one_commit.extend(commit_pair("c1", "[main abc1234] fix parser", false));
    env.transcript(SID_B, &one_commit, NOW - 3.0 * HOUR);
    let res = env.capture(&env.cfg(), NOW);
    assert_eq!((n(&res, "light"), n(&res, "carded")), (1, 1));
    assert_eq!(sources(&env.home), [format!("session:{SID_B}")]);
}

#[test]
fn files_under_subagents_are_skipped() {
    let env = Env::new();
    write_rows(
        &env.project
            .join(SID_A)
            .join("subagents")
            .join("agent-a1.jsonl"),
        &work_rows(&env.repo, 5),
        NOW - 2.0 * HOUR,
    );
    let res = env.capture(&env.cfg(), NOW);
    assert_eq!(n(&res, "scanned"), 0);
    assert!(cards(&env.home).is_empty());
}

#[test]
fn covered_session_is_skipped() {
    let cases = [
        (
            "task-sid8",
            format!("task:20270115-080000-{}-alpha-fix-1a2b", &SID_A[..8]),
            String::new(),
        ),
        (
            "handoff-id",
            format!("handoff:20270115-080000-{SID_A}.md"),
            String::new(),
        ),
        (
            "evidence-id",
            String::new(),
            format!("transcript {SID_A} line 40"),
        ),
        ("session-source", format!("session:{SID_A}"), String::new()),
    ];
    for (id, source, evidence) in cases {
        let env = Env::new();
        store(&env.home, &source, &evidence);
        env.transcript(SID_A, &work_rows(&env.repo, 5), NOW - 2.0 * HOUR);
        let res = env.capture(&env.cfg(), NOW);
        assert_eq!((n(&res, "covered"), n(&res, "carded")), (1, 0), "{id}");
    }
}

#[test]
fn sid8_inside_a_longer_hex_run_does_not_cover() {
    let env = Env::new();
    store(
        &env.home,
        &format!("task:20270115-080000-{}0123-alpha-1a2b", &SID_A[..8]),
        "",
    );
    env.transcript(SID_A, &work_rows(&env.repo, 5), NOW - 2.0 * HOUR);
    let res = env.capture(&env.cfg(), NOW);
    assert_eq!((n(&res, "covered"), n(&res, "carded")), (0, 1));
}

#[test]
fn session_that_ran_sm_put_is_covered() {
    for place in ["main", "lane"] {
        let env = Env::new();
        let put = assistant(
            json!([{"type": "tool_use", "id": "p1", "name": "Bash", "input": {"command": "sm put --kind fact --tag decided 'x'"}}]),
            json!({}),
        );
        let mut rows = work_rows(&env.repo, 5);
        if place == "main" {
            rows.push(put);
        } else {
            write_rows(
                &env.project
                    .join(SID_A)
                    .join("subagents")
                    .join("agent-a1.jsonl"),
                &[put],
                NOW - 3.0 * HOUR,
            );
        }
        env.transcript(SID_A, &rows, NOW - 2.0 * HOUR);
        let res = env.capture(&env.cfg(), NOW);
        assert_eq!((n(&res, "covered"), n(&res, "carded")), (1, 0), "{place}");
    }
}

/// D1 (freshness audit, 2026-11-06): a session whose only put printed help, wrote to a scratch
/// home or failed stored nothing in the store, so it still gets a card. A put whose result is
/// not an error still covers.
#[test]
fn sm_put_that_stores_nothing_does_not_cover() {
    let cases = [
        ("help", "sm put --help", None),
        ("short-help", "cd /w && sm put -h", None),
        (
            "scratch-home",
            "HOME=/s SIMPLE_MEMORY_HOME=/s/.simple-memory sm put --kind fact 'x'",
            None,
        ),
        ("scratch-sm-home", "SIMPLE_MEMORY_HOME=/s sm put 'x'", None),
        ("error", "sm put --kind fact 'x'", Some(true)),
    ];
    for (id, command, is_error) in cases {
        for place in ["main", "lane"] {
            let env = Env::new();
            let mut put = vec![assistant(
                json!([{"type": "tool_use", "id": "p1", "name": "Bash", "input": {"command": command}}]),
                json!({}),
            )];
            if let Some(e) = is_error {
                put.push(user(
                    json!([{"type": "tool_result", "tool_use_id": "p1", "content": "error: no body", "is_error": e}]),
                    json!({}),
                ));
            }
            let mut rows = work_rows(&env.repo, 5);
            if place == "main" {
                rows.extend(put);
            } else {
                write_rows(
                    &env.project
                        .join(SID_A)
                        .join("subagents")
                        .join("agent-a1.jsonl"),
                    &put,
                    NOW - 3.0 * HOUR,
                );
            }
            env.transcript(SID_A, &rows, NOW - 2.0 * HOUR);
            let res = env.capture(&env.cfg(), NOW);
            assert_eq!(
                (n(&res, "covered"), n(&res, "carded")),
                (0, 1),
                "{id} {place}"
            );
        }
    }
}

#[test]
fn sm_put_with_a_result_that_is_not_an_error_covers() {
    for place in ["main", "lane"] {
        let env = Env::new();
        let put = vec![
            assistant(
                json!([{"type": "tool_use", "id": "p1", "name": "Bash", "input": {"command": "sm put --kind fact 'x'"}}]),
                json!({}),
            ),
            user(
                json!([{"type": "tool_result", "tool_use_id": "p1", "content": "stored m_X", "is_error": false}]),
                json!({}),
            ),
        ];
        let mut rows = work_rows(&env.repo, 5);
        if place == "main" {
            rows.extend(put);
        } else {
            write_rows(
                &env.project
                    .join(SID_A)
                    .join("subagents")
                    .join("agent-a1.jsonl"),
                &put,
                NOW - 3.0 * HOUR,
            );
        }
        env.transcript(SID_A, &rows, NOW - 2.0 * HOUR);
        let res = env.capture(&env.cfg(), NOW);
        assert_eq!((n(&res, "covered"), n(&res, "carded")), (1, 0), "{place}");
    }
}

#[test]
fn session_idle_under_idle_minutes_waits_for_a_later_run() {
    let env = Env::new();
    env.transcript(SID_A, &work_rows(&env.repo, 5), NOW - 59.0 * 60.0);
    let cfg = env.cfg();
    let first = env.capture(&cfg, NOW);
    assert_eq!((n(&first, "active"), n(&first, "carded")), (1, 0));
    let later = env.capture(&cfg, NOW + 2.0 * 60.0);
    assert_eq!((n(&later, "active"), n(&later, "carded")), (0, 1));
}

#[test]
fn transcript_older_than_since_days_is_not_counted() {
    let env = Env::new();
    env.transcript(SID_A, &work_rows(&env.repo, 5), NOW - 15.0 * 86400.0);
    let cfg = env.cfg();
    let res = env.capture(&cfg, NOW);
    assert_eq!(res.counts.values().sum::<i64>(), 0);
    let wider = capture_sessions(
        Some(env.home_str()),
        &cfg,
        false,
        Some(16.0),
        None,
        Some(NOW),
    )
    .unwrap();
    assert_eq!(n(&wider, "carded"), 1);
}

#[test]
fn unmapped_session_is_counted_not_carded() {
    let env = Env::new();
    let loose = env.tmp.join("loose");
    std::fs::create_dir_all(&loose).unwrap();
    env.transcript(SID_A, &work_rows(&loose, 5), NOW - 2.0 * HOUR);
    let res = env.capture(&env.cfg(), NOW);
    assert_eq!((n(&res, "unmapped"), n(&res, "carded")), (1, 0));
}

#[test]
fn second_run_writes_nothing_and_reads_no_transcript_bytes() {
    let env = Env::new();
    let carded = env.transcript(SID_A, &work_rows(&env.repo, 5), NOW - 2.0 * HOUR);
    let light = env.transcript(SID_B, &work_rows(&env.repo, 2), NOW - 2.0 * HOUR);
    let cfg = env.cfg();
    let first = env.capture(&cfg, NOW);
    assert_eq!((n(&first, "carded"), n(&first, "light")), (1, 1));
    let mode =
        |p: &Path, m: u32| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
    mode(&carded, 0);
    mode(&light, 0);
    let second = capture_sessions(Some(env.home_str()), &cfg, false, None, None, Some(NOW));
    mode(&carded, 0o644);
    mode(&light, 0o644);
    let second = second.unwrap();
    for (k, v) in &second.counts {
        assert_eq!(*v, if *k == "unchanged" { 2 } else { 0 }, "count {k}");
    }
    assert_eq!(env.calls().len(), 1);
    assert_eq!(cards(&env.home).len(), 1);
}

/// Not in the Python suite: a run of the Python `capture_sessions` (2026-11-05) on this setup gave
/// first `scanned=2 light=2`, second `scanned=1 unchanged=1 light=1`.
#[test]
fn a_transcript_with_a_new_mtime_and_the_same_size_is_scanned_again() {
    let env = Env::new();
    let touched = env.transcript(SID_B, &work_rows(&env.repo, 2), NOW - 2.0 * HOUR);
    env.transcript(SID_A, &work_rows(&env.repo, 2), NOW - 2.0 * HOUR);
    let cfg = env.cfg();
    let first = env.capture(&cfg, NOW);
    assert_eq!(
        (
            n(&first, "scanned"),
            n(&first, "unchanged"),
            n(&first, "light")
        ),
        (2, 0, 2)
    );
    set_mtime(&touched, NOW - 90.0 * 60.0);
    let second = env.capture(&cfg, NOW);
    assert_eq!(
        (
            n(&second, "scanned"),
            n(&second, "unchanged"),
            n(&second, "light")
        ),
        (1, 1, 1)
    );
}

#[test]
fn grown_transcript_of_a_carded_session_is_covered_not_carded_again() {
    let env = Env::new();
    let cfg = env.cfg();
    env.transcript(SID_A, &work_rows(&env.repo, 5), NOW - 2.0 * HOUR);
    env.capture(&cfg, NOW);
    env.transcript(SID_A, &work_rows(&env.repo, 7), NOW - 90.0 * 60.0);
    let res = env.capture(&cfg, NOW);
    assert_eq!(
        (n(&res, "scanned"), n(&res, "covered"), n(&res, "carded")),
        (1, 1, 0)
    );
}

// ---------------------------------------------------------------- cover ends at as_of (D2)

/// NOW + 1 h, after the time of `work_rows`.
const LATER: &str = "2027-01-15T09:00:00.000Z";

/// `rows` with each row's timestamp set to `ts`.
fn at(rows: Vec<Value>, ts: &str) -> Vec<Value> {
    rows.into_iter()
        .map(|mut r| {
            r["timestamp"] = json!(ts);
            r
        })
        .collect()
}

/// (status, last body line) of each session card.
fn card_rows(home: &Path) -> Vec<(String, String)> {
    let conn = common::open(home);
    let mut stmt = conn
        .prepare("SELECT status, body FROM items WHERE source LIKE 'session:%' ORDER BY rowid")
        .unwrap();
    stmt.query_map([], |r| {
        let body: String = r.get(1)?;
        Ok((r.get(0)?, body.lines().last().unwrap_or("").to_string()))
    })
    .unwrap()
    .collect::<rusqlite::Result<Vec<_>>>()
    .unwrap()
}

/// The stdin of each card call that holds `needle`.
fn stdins_with(env: &Env, needle: &str) -> Vec<String> {
    env.calls()
        .into_iter()
        .map(|c| c.stdin)
        .filter(|s| s.contains(needle))
        .collect()
}

#[test]
fn session_that_grows_past_its_card_gets_a_card_of_the_later_work_only() {
    let env = Env::new();
    let cfg = env.cfg();
    let write = |path: &str| {
        assistant(
            json!([{"type": "tool_use", "id": "w1", "name": "Write", "input": {"file_path": path}}]),
            json!({}),
        )
    };
    let mut rows = work_rows(&env.repo, 5);
    rows.push(write("/w/src/old.py"));
    env.transcript(SID_A, &rows, NOW - 2.0 * HOUR);
    assert_eq!(n(&env.capture(&cfg, NOW), "carded"), 1);
    rows.extend(work_rows_at(&env.repo, 5, LATER, "beta docs"));
    rows.extend(at(vec![write("/w/src/new.py")], LATER));
    env.transcript(SID_A, &rows, NOW + HOUR);
    let res = env.capture(&cfg, NOW + 3.0 * HOUR);
    assert_eq!(
        (n(&res, "scanned"), n(&res, "covered"), n(&res, "carded")),
        (1, 0, 1)
    );
    let later = stdins_with(&env, "beta docs");
    assert_eq!(later.len(), 1);
    assert!(later[0].starts_with("Owner: step 0 of the beta docs work"));
    assert!(!later[0].contains("alpha parser"), "{}", later[0]);
    let now_iso = epoch_to_as_of(NOW).unwrap();
    let later_iso = epoch_to_as_of(NOW + HOUR).unwrap();
    assert_eq!(
        card_rows(&env.home),
        [
            (
                "active".to_string(),
                format!(
                    "Session {SID_A}: owner prompts 5, commits 0, \
                     last active {now_iso}."
                )
            ),
            (
                "active".to_string(),
                format!(
                    "Session {SID_A}, work after {now_iso}: owner prompts 5, \
                     commits 0, last active {later_iso}."
                )
            ),
        ]
    );
    let refs: Vec<String> = cards(&env.home).into_iter().map(|c| c.refs).collect();
    let new: Vec<bool> = refs.iter().map(|r| r.contains("/w/src/new.py")).collect();
    let old: Vec<bool> = refs.iter().map(|r| r.contains("/w/src/old.py")).collect();
    assert_eq!(
        (new, old),
        (vec![false, true], vec![true, false]),
        "{refs:?}"
    );
}

/// A prompt and a commit after the card give a new card. With a commit before the card too,
/// the later card counts and names only the later commit (review U1 round 2: mutant
/// `later-commit`).
#[test]
fn a_commit_after_the_card_is_enough_for_a_new_card() {
    for early_commit in [false, true] {
        let env = Env::new();
        let cfg = env.cfg();
        let mut rows = work_rows(&env.repo, 5);
        if early_commit {
            rows.extend(commit_pair("c0", "[main 1111111] early fix", false));
        }
        env.transcript(SID_A, &rows, NOW - 2.0 * HOUR);
        assert_eq!(n(&env.capture(&cfg, NOW), "carded"), 1, "{early_commit}");
        rows.extend(work_rows_at(&env.repo, 1, LATER, "beta docs"));
        rows.extend(at(
            commit_pair("c1", "[main abc1234] fix parser", false),
            LATER,
        ));
        env.transcript(SID_A, &rows, NOW + HOUR);
        let res = env.capture(&cfg, NOW + 3.0 * HOUR);
        assert_eq!(
            (n(&res, "covered"), n(&res, "carded")),
            (0, 1),
            "{early_commit}"
        );
        let now_iso = epoch_to_as_of(NOW).unwrap();
        let later_iso = epoch_to_as_of(NOW + HOUR).unwrap();
        assert_eq!(
            card_rows(&env.home)[1].1,
            format!(
                "Session {SID_A}, work after {now_iso}: owner prompts 1, commits 1, \
                 last active {later_iso}."
            ),
            "{early_commit}"
        );
        // the card answer names abc1234 too, so both cards hold it; only the old one may hold
        // the early sha
        let later: String = common::open(&env.home)
            .query_row(
                "SELECT refs FROM items WHERE source LIKE 'session:%' ORDER BY rowid DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(later, "abc1234 src/alpha.py", "{early_commit}");
    }
}

/// Growth under the thresholds stays covered: 2 later prompts and no commit, or a commit with
/// no later owner prompt (the tasklog commit after a task log).
#[test]
fn small_growth_after_the_card_stays_covered() {
    let two = || work_rows_at(Path::new("/w"), 2, LATER, "beta docs");
    let commit = |id: &str| commit_pair(id, "[main abc1234] tasklog", false);
    // (case, a commit before the card, rows after it)
    for (id, base_commit, extra) in [
        ("two-prompts", false, two()),
        ("two-prompts-after-a-commit", true, two()),
        ("commit-only", false, at(commit("c2"), LATER)),
    ] {
        let env = Env::new();
        let cfg = env.cfg();
        let mut rows = work_rows(&env.repo, 5);
        if base_commit {
            rows.extend(commit("c1"));
        }
        env.transcript(SID_A, &rows, NOW - 2.0 * HOUR);
        assert_eq!(n(&env.capture(&cfg, NOW), "carded"), 1, "{id}");
        rows.extend(extra);
        env.transcript(SID_A, &rows, NOW + HOUR);
        let res = env.capture(&cfg, NOW + 3.0 * HOUR);
        assert_eq!(
            (n(&res, "scanned"), n(&res, "covered"), n(&res, "carded")),
            (1, 1, 0),
            "{id}"
        );
        assert_eq!(card_rows(&env.home).len(), 1, "{id}");
    }
}

/// A task item from before the work no longer covers it. A date-only as_of is midnight of that
/// day like any other value (lead decision 2026-11-06, review U1 row 3: sm stores every as_of
/// with a time), and only an unparsed as_of covers the whole transcript.
#[test]
fn an_item_covers_a_session_only_up_to_its_as_of() {
    let task = format!("task:20270115-080000-{}-alpha-fix-1a2b", &SID_A[..8]);
    let handoff = format!("handoff:20270115-080000-{SID_A}.md");
    let before_hour = epoch_to_as_of(NOW - HOUR).unwrap();
    let before_sec = epoch_to_as_of(NOW - 1.0).unwrap();
    let at_now = epoch_to_as_of(NOW).unwrap();
    let cases = [
        ("task-before", task.as_str(), before_hour.as_str(), (0, 1)),
        (
            "handoff-before",
            handoff.as_str(),
            before_sec.as_str(),
            (0, 1),
        ),
        ("task-at", task.as_str(), at_now.as_str(), (1, 0)),
        ("date-only-before", task.as_str(), "2027-01-14", (0, 1)),
        ("date-only-after", task.as_str(), "2027-01-16", (1, 0)),
        ("unparsed", task.as_str(), "last week", (1, 0)),
    ];
    for (id, source, as_of, want) in cases {
        let env = Env::new();
        // a put checks as_of, so the case's value goes in after it
        store(&env.home, source, "");
        common::open(&env.home)
            .execute("UPDATE items SET as_of = ?", [as_of])
            .unwrap();
        env.transcript(SID_A, &work_rows(&env.repo, 5), NOW - 2.0 * HOUR);
        let res = env.capture(&env.cfg(), NOW);
        assert_eq!((n(&res, "covered"), n(&res, "carded")), want, "{id}");
    }
}

/// The newest cover time wins, across items of one kind and across sid8 and full-id items.
#[test]
fn the_newest_cover_time_of_several_items_wins() {
    let env = Env::new();
    let item = |id: &str, source: &str, evidence: &str, as_of: &str| {
        Item {
            source: source.into(),
            evidence: evidence.into(),
            as_of: as_of.into(),
            ..Item::bare(id, "alpha", "task", "t", id, "derived")
        }
        .post_init()
    };
    let sid8 = &SID_A[..8];
    let at_now = epoch_to_as_of(NOW).unwrap();
    let before_hour = epoch_to_as_of(NOW - HOUR).unwrap();
    let items = [
        item(
            "m_A",
            &format!("task:20270115-080000-{sid8}-a-1a2b"),
            "",
            &at_now,
        ),
        item(
            "m_B",
            &format!("task:20270115-070000-{sid8}-b-1a2b"),
            "",
            &before_hour,
        ),
        item("m_C", "", &format!("transcript {SID_A}"), &before_hour),
    ];
    let res = put_items_batch(&common::open(&env.home), &items, false).unwrap();
    assert!(res.5.is_empty(), "store failed: {:?}", res.5);
    env.transcript(SID_A, &work_rows(&env.repo, 5), NOW - 2.0 * HOUR);
    let res = env.capture(&env.cfg(), NOW);
    assert_eq!((n(&res, "covered"), n(&res, "carded")), (1, 0));
}

/// A put covers up to the time of its tool_use row, in the main transcript or a lane.
#[test]
fn a_put_covers_the_session_up_to_its_own_time() {
    for place in ["main", "lane"] {
        let env = Env::new();
        let put = at(
            vec![assistant(
                json!([{"type": "tool_use", "id": "p1", "name": "Bash", "input": {"command": "sm put --kind fact 'x'"}}]),
                json!({}),
            )],
            "2027-01-15T08:00:00.000Z",
        );
        let mut rows = work_rows(&env.repo, 5);
        if place == "main" {
            rows.extend(put);
        } else {
            write_rows(
                &env.project
                    .join(SID_A)
                    .join("subagents")
                    .join("agent-a1.jsonl"),
                &put,
                NOW - 3.0 * HOUR,
            );
        }
        rows.extend(work_rows_at(&env.repo, 5, LATER, "beta docs"));
        env.transcript(SID_A, &rows, NOW + HOUR);
        let res = env.capture(&env.cfg(), NOW + 3.0 * HOUR);
        assert_eq!((n(&res, "covered"), n(&res, "carded")), (0, 1), "{place}");
        let later = stdins_with(&env, "beta docs");
        assert_eq!(later.len(), 1, "{place}");
        assert!(!later[0].contains("alpha parser"), "{place}: {}", later[0]);
    }
}

#[test]
fn max_per_run_caps_cards_and_the_next_run_takes_the_rest() {
    let env = Env::new();
    for (k, sid) in [SID_A, SID_B, SID_C].iter().enumerate() {
        env.transcript(sid, &work_rows(&env.repo, 5), NOW - (2.0 + k as f64) * HOUR);
    }
    let cfg = env.cfg_with("max_per_run = 2\n", "");
    let res1 = env.capture(&cfg, NOW);
    let res2 = env.capture(&cfg, NOW);
    assert_eq!((n(&res1, "carded"), n(&res2, "carded")), (2, 1));
    assert_eq!(
        sources(&env.home),
        [SID_A, SID_B, SID_C].map(|s| format!("session:{s}"))
    );
}

#[test]
fn catch_up_part_runs_claude_from_config_and_honors_max_per_run() {
    let env = Env::new();
    let log = env.tmp.join("claude-calls.log");
    let own = env.tmp.join("claude");
    common::write(
        &own,
        &format!(
            "#!/bin/sh\necho \"$1 $2 $3\" >> {}\ncat > /dev/null\nprintf \"%s\" \"{CARD_ANSWER}\"\n",
            log.display()
        ),
    );
    std::fs::set_permissions(&own, std::fs::Permissions::from_mode(0o755)).unwrap();
    env.write_config("max_per_run = 1\n", "", Some(&own));
    let cfg = load_config(Some(env.home_str())).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    for (k, sid) in [SID_A, SID_B].iter().enumerate() {
        env.transcript(sid, &work_rows(&env.repo, 5), now - (2.0 + k as f64) * HOUR);
    }
    let counts = run(&PyPath::new(env.home_str()), &cfg, false).unwrap();
    assert_eq!(
        (counts["candidates"], counts["carded"], counts["plain"]),
        (1, 1, 0)
    );
    assert_eq!(
        std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        ["-p --model haiku"]
    );
    assert_eq!(
        cards(&env.home)
            .iter()
            .map(|c| c.title.as_str())
            .collect::<Vec<_>>(),
        ["Alpha parser fixed and pushed"]
    );
}

#[test]
fn dry_run_counts_by_repo_and_writes_nothing() {
    let env = Env::new();
    for (k, sid) in [SID_A, SID_B, SID_C].iter().enumerate() {
        env.transcript(sid, &work_rows(&env.repo, 5), NOW - (2.0 + k as f64) * HOUR);
    }
    let cfg = env.cfg_with("max_per_run = 1\n", "");
    let res = capture_sessions(Some(env.home_str()), &cfg, true, None, None, Some(NOW)).unwrap();
    assert_eq!((n(&res, "candidates"), n(&res, "carded")), (3, 0));
    assert_eq!(
        res.by_repo
            .iter()
            .map(|(k, v)| (k.as_str(), *v))
            .collect::<Vec<_>>(),
        [("alpha", 3)]
    );
    assert!(env.calls().is_empty());
    assert!(cards(&env.home).is_empty());
    assert!(!env.home.join("capture-state.json").exists());
}

#[test]
fn missing_transcript_root_raises() {
    let env = Env::new();
    env.cfg();
    let text = std::fs::read_to_string(env.home.join("config.toml")).unwrap();
    let text = text.replace(
        env.root.to_str().unwrap(),
        env.tmp.join("nowhere").to_str().unwrap(),
    );
    common::write(&env.home.join("config.toml"), &text);
    let cfg = load_config(Some(env.home_str())).unwrap();
    let err =
        capture_sessions(Some(env.home_str()), &cfg, false, None, None, Some(NOW)).unwrap_err();
    assert_eq!(err.class, "FileNotFoundError");
    assert!(err.msg.contains("nowhere"), "{}", err.msg);
}

#[test]
fn cli_import_sessions_dry_run_prints_counts() {
    let env = Env::new();
    env.write_config("", "", None);
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    env.transcript(SID_A, &work_rows(&env.repo, 5), now - 2.0 * HOUR);
    let (code, out, _) = common::sm(
        &env.home,
        &["import", "sessions", "--dry-run", "--since-days", "1"],
    );
    assert_eq!(code, 0);
    assert_eq!(
        out.lines().collect::<Vec<_>>(),
        [
            "import sessions (dry-run): scanned=1 unchanged=0 active=0 headless=0 light=0 covered=0 \
             unmapped=0 candidates=1 carded=0 plain=0",
            "  repo alpha: 1",
        ]
    );
}

#[test]
fn cli_import_sessions_reports_a_missing_root() {
    let env = Env::new();
    common::write(
        &env.home.join("config.toml"),
        &format!(
            "[capture]\nroot = \"{}\"\n",
            env.tmp.join("nowhere").display()
        ),
    );
    let (code, _, err) = common::sm(&env.home, &["import", "sessions", "--dry-run"]);
    assert_eq!(code, 1);
    assert!(err.contains("is not a directory"), "{err}");
}

#[test]
fn card_without_timestamps_takes_as_of_from_the_file_time() {
    let env = Env::new();
    let rows: Vec<Value> = (0..5)
        .map(|k| {
            ask(
                &format!("ask {k}"),
                json!({"cwd": env.repo.to_str().unwrap(), "turnOrigin": "human"}),
            )
        })
        .collect();
    env.transcript(SID_A, &rows, NOW - 2.0 * HOUR);
    env.capture(&env.cfg(), NOW);
    let expected = epoch_to_as_of(NOW - 2.0 * HOUR).unwrap();
    assert_eq!(
        cards(&env.home)
            .iter()
            .map(|c| c.as_of.as_str())
            .collect::<Vec<_>>(),
        [expected.as_str()]
    );
}

#[test]
fn repo_falls_back_to_the_project_folder_name() {
    let env = Env::new();
    let code = env.tmp.join("code");
    std::fs::create_dir_all(code.join("beta")).unwrap();
    let cfg = env.cfg_with(
        "",
        &format!(
            "[repos]\nroots = [\"{}\"]\n[repo_aliases]\nbeta = \"beta-main\"\n",
            code.display()
        ),
    );
    let rows: Vec<Value> = (0..5)
        .map(|k| ask(&format!("ask {k}"), json!({"turnOrigin": "human"})))
        .collect();
    let key: String = code
        .join("beta")
        .to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    write_rows(
        &env.root.join(&key).join(format!("{SID_A}.jsonl")),
        &rows,
        NOW - 2.0 * HOUR,
    );
    // a folder name under no root maps to no repo: the session is counted unmapped
    write_rows(
        &env.root
            .join("-u-x-elsewhere-gamma")
            .join(format!("{SID_B}.jsonl")),
        &rows,
        NOW - 2.0 * HOUR,
    );
    let res = env.capture(&cfg, NOW);
    assert_eq!(
        res.by_repo
            .iter()
            .map(|(k, v)| (k.as_str(), *v))
            .collect::<Vec<_>>(),
        [("beta-main", 1)]
    );
    assert_eq!(n(&res, "unmapped"), 1);
}

#[test]
fn a_card_the_store_refuses_raises_and_keeps_the_session_open() {
    let env = Env::new();
    let conn = common::open(&env.home);
    conn.execute_batch(
        "CREATE TRIGGER refuse_cards BEFORE INSERT ON items WHEN NEW.source LIKE 'session:%' \
         BEGIN SELECT RAISE(ABORT, 'cards refused'); END",
    )
    .unwrap();
    drop(conn);
    env.transcript(SID_A, &work_rows(&env.repo, 5), NOW - 2.0 * HOUR);
    let err = capture_sessions(
        Some(env.home_str()),
        &env.cfg(),
        false,
        None,
        None,
        Some(NOW),
    )
    .unwrap_err();
    assert_eq!(err.class, "RuntimeError");
    assert!(err.msg.contains("cards refused"), "{}", err.msg);
    let state: Value = serde_json::from_str(
        &std::fs::read_to_string(env.home.join("capture-state.json")).unwrap(),
    )
    .unwrap();
    assert!(state.get(SID_A).is_none(), "{state}");
}

#[test]
fn state_file_that_is_not_an_object_raises() {
    let env = Env::new();
    common::write(&env.home.join("capture-state.json"), "[]");
    let err = capture_sessions(
        Some(env.home_str()),
        &env.cfg(),
        false,
        None,
        None,
        Some(NOW),
    )
    .unwrap_err();
    assert_eq!(err.class, "TypeError");
    assert!(err.msg.contains("expected an object"), "{}", err.msg);
}
