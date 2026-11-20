//! `sm hook prompt` as Claude Code and agy run it: a subprocess with a prompt payload on stdin.
//! Ports of the CLI and catch-up-start tests of the Python recall tests (Python main
//! 555b8f2): test_cli_hook_prints_valid_output_for_both_harnesses, the SM_PROMPT_HOOK=0 half of
//! test_run_prompt_hook_off_switches, and four of the five test_catch_up_* tests (the fifth,
//! test_catch_up_start_error_never_breaks_the_hook_output, is a unit test in src/cli/hook.rs).
//!
//! The last tests are Rust's own: a `git` first on PATH that hangs, for the hook's budget.
//!
//! Every sm process runs with HOME and SIMPLE_MEMORY_HOME in a temp folder. The catch-up child
//! is the real `sm import catch-up --quiet` on that folder. A test waits for its catchup.log
//! row before it ends, so no child writes into a folder that was deleted.

mod common;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use common::busy_reference;

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new(config: &str) -> Env {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        std::fs::write(dir.path().join("home/config.toml"), config).unwrap();
        Env { dir }
    }

    fn home(&self) -> String {
        format!("{}/home", self.dir.path().display())
    }

    fn trace(&self) -> String {
        format!("{}/trace.jsonl", self.dir.path().display())
    }

    /// A claude stand-in that answers with `query` and records that it ran.
    fn fake_claude(&self, query: &str) -> String {
        let path = format!("{}/fake-claude.sh", self.dir.path().display());
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\ncat >/dev/null\ntouch '{}/claude-ran'\necho '{query}'\n",
                self.dir.path().display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn run(&self, args: &[&str], stdin: &str, extra: &[(&str, &str)]) -> (i32, String, String) {
        let mut c = Command::new(env!("CARGO_BIN_EXE_sm"));
        c.args(args)
            .env("HOME", self.dir.path())
            .env("SIMPLE_MEMORY_HOME", self.home())
            .env("SM_PARITY_TRACE", self.trace())
            .env_remove("SM_PROMPT_HOOK")
            .env_remove("SM_PARITY_NOW")
            .env_remove("SM_PARITY_SEED")
            .current_dir(self.dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in extra {
            c.env(k, v);
        }
        let mut child = c.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        (
            out.status.code().unwrap(),
            String::from_utf8(out.stdout).unwrap(),
            String::from_utf8(out.stderr).unwrap(),
        )
    }

    fn put(&self, text: &str) {
        let (code, _, err) = self.run(
            &[
                "put", text, "--tag", "decided", "--kind", "decision", "--repo", "global",
            ],
            "",
            &[],
        );
        assert_eq!(code, 0, "{err}");
    }

    /// The `import catch-up` spawn rows of the trace file.
    fn catch_up_spawns(&self) -> Vec<serde_json::Value> {
        let text = std::fs::read_to_string(self.trace()).unwrap_or_default();
        text.lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .filter(|r| {
                r["spawn"]["argv"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|x| x == "catch-up"))
            })
            .collect()
    }

    /// Wait until every started catch-up wrote its catchup.log row.
    fn wait_for_children(&self, n: usize) {
        let log = format!("{}/catchup.log", self.home());
        for _ in 0..300 {
            if std::fs::read_to_string(&log)
                .map(|t| t.lines().count() >= n)
                .unwrap_or(false)
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("the catch-up child wrote no catchup.log row");
    }

    fn set_stamp_age(&self, minutes: u64) {
        let stamp = format!("{}/hook/catchup.stamp", self.home());
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&stamp)
            .unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(minutes * 60))
            .unwrap();
    }

    fn stamp_mtime(&self) -> SystemTime {
        std::fs::metadata(format!("{}/hook/catchup.stamp", self.home()))
            .unwrap()
            .modified()
            .unwrap()
    }
}

const OFF: &str = "[catch_up]\nenabled = false\n";

fn payload(prompt: &str) -> String {
    serde_json::json!({"prompt": prompt, "cwd": "/nonexistent-sm-r2"}).to_string()
}

#[test]
fn cli_hook_prints_valid_output_for_both_harnesses() {
    // test_cli_hook_prints_valid_output_for_both_harnesses
    let env = Env::new("");
    let claude = env.fake_claude("R6 pruning");
    std::fs::write(
        env.home() + "/config.toml",
        format!("{OFF}[prompt_hook]\nclaude_bin = \"{claude}\"\n"),
    )
    .unwrap();
    env.put("R6 stays reverted: narrowing prune_stale() loses old catches");
    // one item: every word has idf 0, so the floor does not matter on the model path
    let p = payload("is R6 reverted yet");
    let (code, out, _) = env.run(&["hook", "prompt"], &p, &[]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let ctx = v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
    assert!(
        ctx.contains("query \"R6 pruning\"") && ctx.contains("R6 stays reverted"),
        "{ctx}"
    );

    let (_, out, _) = env.run(&["hook", "prompt", "--agy"], &p, &[]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let steps = v["injectSteps"].as_array().unwrap();
    assert_eq!(steps.len(), 1);
    assert!(
        steps[0]["ephemeralMessage"]
            .as_str()
            .unwrap()
            .contains("R6 stays reverted")
    );

    assert_eq!(env.run(&["hook", "prompt"], &payload("ok"), &[]).1, "");
    let (code, out, err) = env.run(&["hook", "prompt"], "not json", &[]);
    assert_eq!((code, out.as_str()), (0, ""));
    assert!(err.starts_with("sm hook: "), "{err}");
    let (_, out, _) = env.run(&["hook", "prompt", "--agy"], "not json", &[]);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&out).unwrap(),
        serde_json::json!({"injectSteps": []})
    );
}

#[test]
fn run_prompt_hook_sm_prompt_hook_zero_calls_no_model() {
    // test_run_prompt_hook_off_switches, the SM_PROMPT_HOOK=0 half
    let env = Env::new("");
    let claude = env.fake_claude("R6");
    std::fs::write(
        env.home() + "/config.toml",
        format!("{OFF}[prompt_hook]\nclaude_bin = \"{claude}\"\n"),
    )
    .unwrap();
    env.put("R6 stays reverted: narrowing prune_stale() loses old catches");
    let (code, out, _) = env.run(
        &["hook", "prompt"],
        &payload("is R6 reverted yet"),
        &[("SM_PROMPT_HOOK", "0")],
    );
    assert_eq!((code, out.as_str()), (0, ""));
    assert!(!Path::new(&format!("{}/claude-ran", env.dir.path().display())).exists());
}

#[test]
#[cfg_attr(
    not(feature = "parity"),
    ignore = "counts spawns through SM_PARITY_TRACE, which only the parity build writes"
)]
fn catch_up_starts_detached_only_when_the_stamp_is_missing_or_old() {
    // test_catch_up_starts_detached_only_when_the_stamp_is_missing_or_old, with the
    // test_catch_up_child_runs_on_the_home_the_hook_was_given check: the child's catchup.log lands
    // in the hook's home.
    let env = Env::new("[prompt_hook]\n");
    env.run(&["hook", "prompt"], &payload("ok"), &[]);
    let starts = env.catch_up_spawns();
    assert_eq!(starts.len(), 1);
    // argv[0] is `<sm>` where Python has sys.executable -m simple_memory (an accepted difference)
    assert_eq!(
        starts[0],
        serde_json::json!({"spawn": {"argv": ["<sm>", "import", "catch-up", "--quiet"], "cwd": null, "start_new_session": true,
            "stdin": "DEVNULL", "stdout": "DEVNULL", "stderr": "DEVNULL"}})
    );
    env.wait_for_children(1);
    assert!(Path::new(&format!("{}/hook/catchup.stamp", env.home())).is_file());
    env.run(&["hook", "prompt"], &payload("ok"), &[]);
    assert_eq!(env.catch_up_spawns().len(), 1);
    env.set_stamp_age(11);
    let old = SystemTime::now() - Duration::from_secs(11 * 60);
    env.run(
        &["hook", "prompt", "--agy"],
        "{\"lastUserInput\": \"ok\"}",
        &[],
    );
    assert_eq!(env.catch_up_spawns().len(), 2);
    assert!(env.stamp_mtime() > old + Duration::from_secs(60));
    env.wait_for_children(2);
}

#[test]
fn catch_up_interval_comes_from_config() {
    // test_catch_up_interval_comes_from_config. The hook touches the stamp before every start,
    // so the stamp mtime shows a start in both builds; the spawn rows need the parity trace.
    let env = Env::new("[catch_up]\ninterval_minutes = 30\n");
    std::fs::create_dir_all(env.home() + "/hook").unwrap();
    std::fs::write(env.home() + "/hook/catchup.stamp", "").unwrap();
    env.set_stamp_age(11);
    let ten_ago = SystemTime::now() - Duration::from_secs(10 * 60);
    env.run(&["hook", "prompt"], &payload("ok"), &[]);
    assert!(env.stamp_mtime() < ten_ago);
    if cfg!(feature = "parity") {
        assert_eq!(env.catch_up_spawns().len(), 0);
    }
    env.set_stamp_age(31);
    env.run(&["hook", "prompt"], &payload("ok"), &[]);
    assert!(env.stamp_mtime() > ten_ago);
    if cfg!(feature = "parity") {
        assert_eq!(env.catch_up_spawns().len(), 1);
    }
    env.wait_for_children(1);
}

#[test]
fn catch_up_off_switches_start_nothing() {
    // test_catch_up_off_switches_start_nothing
    let env = Env::new(OFF);
    env.run(&["hook", "prompt"], &payload("ok"), &[]);
    std::fs::write(env.home() + "/config.toml", "[prompt_hook]\n").unwrap();
    env.run(
        &["hook", "prompt"],
        &payload("ok"),
        &[("SM_PROMPT_HOOK", "0")],
    );
    // The stamp is touched before every start, so its absence shows no start in both builds.
    if cfg!(feature = "parity") {
        assert_eq!(env.catch_up_spawns().len(), 0);
    }
    assert!(!Path::new(&format!("{}/hook/catchup.stamp", env.home())).exists());
}

/// A `git` first on PATH that hangs: the PATH for `Env::run`.
fn hanging_git(env: &Env) -> String {
    let bin = env.dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    write_script(&bin.join("git"), "exec sleep 30");
    format!("{}:{}", bin.display(), std::env::var("PATH").unwrap())
}

fn write_script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A home whose model ends its answer (`answer`, a shell line) 10.5 s into the hook's 12 s budget,
/// 1 s before its 11.5 s rewrite timeout, and a hanging git: the repo lookup after the model call
/// then has 1.5 s left, less than its own 2 s timeout.
fn late_model_and_hanging_git(answer: &str) -> (Env, String) {
    let env = Env::new("");
    let model = env.dir.path().join("late-claude.sh");
    write_script(&model, &format!("cat >/dev/null\nsleep 10.5\n{answer}"));
    std::fs::write(
        env.home() + "/config.toml",
        format!(
            "{OFF}[prompt_hook]\nclaude_bin = \"{}\"\ntimeout = 11.5\n",
            model.display()
        ),
    )
    .unwrap();
    let path = hanging_git(&env);
    (env, path)
}

/// The newest hook.log row, after one `sm hook prompt` run that printed nothing.
fn quiet_hook_row(env: &Env, path: &str) -> serde_json::Value {
    let (out, row) = hook_row(env, "does R6 revert still", path);
    assert_eq!(out, "");
    row
}

/// What one `sm hook prompt` run printed, and its hook.log row; the run exits 0 with no stderr.
fn hook_row(env: &Env, prompt: &str, path: &str) -> (String, serde_json::Value) {
    let (code, out, err) = env.run(&["hook", "prompt"], &payload(prompt), &[("PATH", path)]);
    assert_eq!((code, err.as_str()), (0, ""));
    let log = std::fs::read_to_string(env.home() + "/hook.log").unwrap();
    (
        out,
        serde_json::from_str(log.lines().last().unwrap()).unwrap(),
    )
}

#[test]
fn a_hanging_git_after_the_model_gets_only_the_time_left() {
    let (env, path) = late_model_and_hanging_git("echo 'R6 revert'");
    let row = quiet_hook_row(&env, &path);
    assert_eq!(
        (&row["path"], &row["query"], &row["error"]),
        (
            &serde_json::json!("error"),
            &serde_json::json!("R6 revert"),
            &serde_json::json!("budget")
        )
    );
    // With its own 2 s the git call ends near 12.5 s.
    let ms = row["ms"].as_i64().unwrap();
    assert!(ms < 12_250, "{ms}");
}

#[test]
fn a_hanging_git_in_the_local_fallback_gets_only_the_time_left() {
    let (env, path) = late_model_and_hanging_git("exit 1");
    let row = quiet_hook_row(&env, &path);
    assert_eq!(
        (&row["path"], &row["fallback"], &row["local_error"]),
        (
            &serde_json::json!("local"),
            &serde_json::json!("error"),
            &serde_json::json!("budget")
        )
    );
    let ms = row["ms"].as_i64().unwrap();
    assert!(ms < 12_250, "{ms}");
}

#[test]
fn a_hanging_git_with_time_left_gets_its_own_2_s_and_the_hook_goes_on() {
    let env = Env::new(&format!("{OFF}[prompt_hook]\nmodel = \"none\"\n"));
    let path = hanging_git(&env);
    let row = quiet_hook_row(&env, &path);
    assert_eq!(
        (&row["path"], &row["fallback"], &row["local_error"]),
        (
            &serde_json::json!("local"),
            &serde_json::json!("off"),
            &serde_json::Value::Null
        )
    );
    let git_ms = row["git_ms"].as_i64().unwrap();
    assert!((1900..2600).contains(&git_ms), "{git_ms}");
    // the local search's store time holds its repo lookup
    let db_ms = row["db_ms"].as_i64().unwrap();
    assert!(db_ms >= git_ms, "{db_ms} {git_ms}");
}

#[test]
fn a_git_that_answers_after_the_deadline_is_used_and_the_fallback_injects() {
    // The model outlives the budget, so its rewrite times out at the deadline and the local
    // fallback's git call starts after it. Like the store's 1 ms try, git gets one 100 ms try.
    let env = Env::new("");
    let model = env.dir.path().join("hung-claude.sh");
    write_script(&model, "exec sleep 30");
    std::fs::write(
        env.home() + "/config.toml",
        format!(
            "{OFF}[prompt_hook]\nclaude_bin = \"{}\"\ntimeout = 20\n",
            model.display()
        ),
    )
    .unwrap();
    // nine items: four words held by one of them clear the local floor, 4 * ln(9) = 8.8 >= 8.0
    env.put("the loader retry fails on flaky disks");
    for n in 1..=8 {
        env.put(&format!(
            "filler note number {n} about kettles and teapots {n}"
        ));
    }
    let held = env.dir.path().join("reference.db");
    let holder = rusqlite::Connection::open(&held).unwrap();
    holder
        .execute_batch("CREATE TABLE t (x); BEGIN IMMEDIATE")
        .unwrap();
    let reference = busy_reference(&held, 12_000);
    let (out, row) = hook_row(
        &env,
        "why does the loader retry fail on flaky disks today",
        &std::env::var("PATH").unwrap(),
    );
    let reference_ms = (reference.join().unwrap() * 1000.0) as i64;
    drop(holder);
    assert!(
        out.contains("the loader retry fails on flaky disks"),
        "{out}"
    );
    assert_eq!(
        (&row["path"], &row["fallback"], &row["local_error"]),
        (
            &serde_json::json!("local"),
            &serde_json::json!("timeout"),
            &serde_json::Value::Null
        )
    );
    assert_eq!(row["ids"].as_array().map(Vec::len), Some(1));
    // The hanging-git bound plus the 100 ms try, plus a quarter of the busy handler's stretch
    // in a 12 s wait beside the hook, as in the lib's exclusive-lock test (12 364 to 12 469 ms
    // failed a bare 12 350 at load 31). On this host that stretch is about the same idle and
    // under owner load (12 s took 12 694 to 12 886 ms), so the bound sits near 12 520 to
    // 12 570 ms either way: a fixed allowance of about 0.2 s that does not grow with load. The
    // smallest extra wait it catches is about 0.5 s; a full 2 s wait for the late git fails it
    // by about 1.5 s.
    let ms = row["ms"].as_i64().unwrap();
    let bound = 12_350 + (reference_ms - 12_000) / 4;
    assert!(ms < bound, "{ms} {bound} {reference_ms}");
}

#[test]
fn a_git_that_cannot_start_near_the_deadline_is_no_budget() {
    // With no git on PATH the call fails at once, inside its 1.5 s: the repo stays unassigned and
    // the model's query runs. The model script names its programs, as PATH holds none.
    let env = Env::new("");
    let empty = env.dir.path().join("empty-bin");
    std::fs::create_dir(&empty).unwrap();
    let model = env.dir.path().join("late-claude.sh");
    write_script(
        &model,
        "/bin/cat >/dev/null\n/bin/sleep 10.5\necho 'R6 revert'",
    );
    std::fs::write(
        env.home() + "/config.toml",
        format!(
            "{OFF}[prompt_hook]\nclaude_bin = \"{}\"\ntimeout = 11.5\n",
            model.display()
        ),
    )
    .unwrap();
    let (_, row) = hook_row(&env, "does R6 revert still", &empty.to_string_lossy());
    assert_eq!(
        (&row["path"], &row["query"], &row["error"]),
        (
            &serde_json::json!("haiku"),
            &serde_json::json!("R6 revert"),
            &serde_json::Value::Null
        )
    );
}
