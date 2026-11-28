//! `service::capture::run_process` against CPython 3.15.1 `subprocess.run(..., capture_output=True,
//! timeout=...)`. Each expected text is the uv interpreter's output for the same call (lane F6
//! probes). Runtime files live under /tmp/sm-parity-f6/.

use std::time::Instant;

use simple_memory::error::SmError;
use simple_memory::py::value::PyValue;
use simple_memory::service::capture::{Run, called_process_error, capture_sessions, run_process};

const SCRATCH: &str = "/tmp/sm-parity-f6";

fn argv(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

/// `f"{type(exc).__name__}: {exc}"` of the error, as `SmError::python_text` gives it.
fn python_text(e: &SmError) -> String {
    format!("{}: {}", e.class, e.msg)
}

fn err_text(r: &Run) -> String {
    python_text(&run_process(r).unwrap_err())
}

fn gc_sh() -> String {
    format!("{}/tests/fx/gc.sh", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn a_grandchild_holding_stdout_times_out_at_the_deadline() {
    // Python: TimeoutExpired after 2.00 s; the old reader joins waited for the grandchild (8.02 s).
    let started = Instant::now();
    let text = err_text(&Run {
        argv: vec![gc_sh()],
        input: Some("x".into()),
        timeout: Some(2.0),
        text: true,
        ..Default::default()
    });
    let took = started.elapsed().as_secs_f64();
    assert_eq!(
        text,
        format!(
            "TimeoutExpired: Command '['{}']' timed out after 2.0 seconds",
            gc_sh()
        )
    );
    assert!((2.0..3.0).contains(&took), "took {took:.2} s");
}

#[test]
fn a_child_that_closes_its_pipes_and_keeps_running_times_out_in_the_wait() {
    // Python: the poll loop ends at EOF, then wait(timeout=remaining) raises with the first timeout.
    let started = Instant::now();
    let text = err_text(&Run {
        argv: argv(&["sh", "-c", "exec >&- 2>&-; sleep 5"]),
        timeout: Some(1.0),
        text: true,
        ..Default::default()
    });
    let took = started.elapsed().as_secs_f64();
    assert_eq!(
        text,
        "TimeoutExpired: Command '['sh', '-c', 'exec >&- 2>&-; sleep 5']' timed out after 1.0 seconds"
    );
    assert!((1.0..2.0).contains(&took), "took {took:.2} s");
}

#[test]
fn a_timeout_kills_the_child() {
    std::fs::create_dir_all(SCRATCH).unwrap();
    let pid_file = format!("{SCRATCH}/kill-pid");
    let script = format!("echo $$ > {pid_file}; exec sleep 30");
    let text = err_text(&Run {
        argv: argv(&["sh", "-c", &script]),
        timeout: Some(0.5),
        text: true,
        ..Default::default()
    });
    assert!(text.starts_with("TimeoutExpired: "), "{text}");
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SAFETY: signal 0 only checks that the pid exists.
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    assert!(!alive, "child {pid} still runs");
    std::fs::remove_file(&pid_file).unwrap();
}

#[test]
fn check_on_a_signal_killed_child_names_the_signal() {
    let run = |sig: &str, check: bool| Run {
        argv: argv(&["sh", "-c", &format!("kill -{sig} $$")]),
        // guards a hang only, like the 30 s cases below
        timeout: Some(30.0),
        text: true,
        check,
        ..Default::default()
    };
    assert_eq!(
        err_text(&run("9", true)),
        "CalledProcessError: Command '['sh', '-c', 'kill -9 $$']' died with <Signals.SIGKILL: 9>."
    );
    assert_eq!(
        err_text(&run("15", true)),
        "CalledProcessError: Command '['sh', '-c', 'kill -15 $$']' died with <Signals.SIGTERM: 15>."
    );
    assert_eq!(run_process(&run("9", false)).unwrap().returncode, -9);
}

#[test]
fn check_on_a_non_zero_exit_prints_the_list_without_quotes() {
    // CPython 3.15.1 subprocess.py:154: f"Command {self.cmd!r} returned non-zero exit status ...".
    // The timeout only guards against a hang: 5 s once ran out in a full parity test run while
    // a review ran mutants and a 200,000-string probe in parallel (2026-11-06).
    let r = Run {
        argv: argv(&["sh", "-c", "exit 3"]),
        timeout: Some(30.0),
        text: true,
        check: true,
        ..Default::default()
    };
    assert_eq!(
        err_text(&r),
        "CalledProcessError: Command ['sh', '-c', 'exit 3'] returned non-zero exit status 3."
    );
}

#[test]
fn timeouts_poll_cannot_take_raise_as_python() {
    let t = |timeout: f64| {
        err_text(&Run {
            argv: argv(&["true"]),
            timeout: Some(timeout),
            text: true,
            ..Default::default()
        })
    };
    assert_eq!(
        t(f64::NAN),
        "ValueError: cannot convert float NaN to integer"
    );
    assert_eq!(
        t(f64::INFINITY),
        "OverflowError: cannot convert float infinity to integer"
    );
    assert_eq!(
        t(1e300),
        "OverflowError: timestamp out of range for C PyTime_t"
    );
    assert_eq!(
        t(9.3e9),
        "OverflowError: timestamp out of range for C PyTime_t"
    );
    assert_eq!(t(9.2e9), "OverflowError: timeout is too large");
    assert_eq!(t(2147484.0), "OverflowError: timeout is too large");
    assert_eq!(
        t(0.0),
        "TimeoutExpired: Command '['true']' timed out after 0.0 seconds"
    );
    assert_eq!(
        t(-1.0),
        "TimeoutExpired: Command '['true']' timed out after -1.0 seconds"
    );
    assert_eq!(
        t(f64::NEG_INFINITY),
        "TimeoutExpired: Command '['true']' timed out after -inf seconds"
    );
    let ok = run_process(&Run {
        argv: argv(&["true"]),
        timeout: Some(2147483.0),
        text: true,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(ok.returncode, 0);
}

#[test]
fn a_spent_timeout_raises_before_any_wait() {
    // Python: TimeoutExpired at once (0.0 s) for `sleep 3` with timeout -1.0.
    let started = Instant::now();
    let text = err_text(&Run {
        argv: argv(&["sleep", "3"]),
        timeout: Some(-1.0),
        text: true,
        ..Default::default()
    });
    assert_eq!(
        text,
        "TimeoutExpired: Command '['sleep', '3']' timed out after -1.0 seconds"
    );
    assert!(started.elapsed().as_secs_f64() < 1.0);
}

#[test]
fn a_signal_python_has_no_name_for() {
    // Python: str(CalledProcessError(-n, ['x'])) for every n in 1..=65 on this OS; signal.Signals
    // has no 32 on macOS or Linux.
    let text = |n: i32| python_text(&called_process_error(&argv(&["x"]), -n));
    assert_eq!(
        text(32),
        "CalledProcessError: Command '['x']' died with unknown signal 32."
    );
    #[cfg(target_os = "macos")]
    let table = MACOS_SIGNAL_TEXT;
    #[cfg(target_os = "linux")]
    let table = LINUX_SIGNAL_TEXT;
    // No table was probed on any other OS, so only the signal-32 case above runs there.
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let table: [(i32, &str); 0] = [];
    for (n, want) in table {
        assert_eq!(text(n), format!("CalledProcessError: {want}"), "signal {n}");
    }
}

/// `str(subprocess.CalledProcessError(-n, ['x']))` for n in 1..=65, from CPython 3.15.2 on
/// ubuntu-latest.
#[cfg(target_os = "linux")]
const LINUX_SIGNAL_TEXT: [(i32, &str); 65] = [
    (1, "Command '['x']' died with <Signals.SIGHUP: 1>."),
    (2, "Command '['x']' died with <Signals.SIGINT: 2>."),
    (3, "Command '['x']' died with <Signals.SIGQUIT: 3>."),
    (4, "Command '['x']' died with <Signals.SIGILL: 4>."),
    (5, "Command '['x']' died with <Signals.SIGTRAP: 5>."),
    (6, "Command '['x']' died with <Signals.SIGABRT: 6>."),
    (7, "Command '['x']' died with <Signals.SIGBUS: 7>."),
    (8, "Command '['x']' died with <Signals.SIGFPE: 8>."),
    (9, "Command '['x']' died with <Signals.SIGKILL: 9>."),
    (10, "Command '['x']' died with <Signals.SIGUSR1: 10>."),
    (11, "Command '['x']' died with <Signals.SIGSEGV: 11>."),
    (12, "Command '['x']' died with <Signals.SIGUSR2: 12>."),
    (13, "Command '['x']' died with <Signals.SIGPIPE: 13>."),
    (14, "Command '['x']' died with <Signals.SIGALRM: 14>."),
    (15, "Command '['x']' died with <Signals.SIGTERM: 15>."),
    (16, "Command '['x']' died with <Signals.SIGSTKFLT: 16>."),
    (17, "Command '['x']' died with <Signals.SIGCHLD: 17>."),
    (18, "Command '['x']' died with <Signals.SIGCONT: 18>."),
    (19, "Command '['x']' died with <Signals.SIGSTOP: 19>."),
    (20, "Command '['x']' died with <Signals.SIGTSTP: 20>."),
    (21, "Command '['x']' died with <Signals.SIGTTIN: 21>."),
    (22, "Command '['x']' died with <Signals.SIGTTOU: 22>."),
    (23, "Command '['x']' died with <Signals.SIGURG: 23>."),
    (24, "Command '['x']' died with <Signals.SIGXCPU: 24>."),
    (25, "Command '['x']' died with <Signals.SIGXFSZ: 25>."),
    (26, "Command '['x']' died with <Signals.SIGVTALRM: 26>."),
    (27, "Command '['x']' died with <Signals.SIGPROF: 27>."),
    (28, "Command '['x']' died with <Signals.SIGWINCH: 28>."),
    (29, "Command '['x']' died with <Signals.SIGIO: 29>."),
    (30, "Command '['x']' died with <Signals.SIGPWR: 30>."),
    (31, "Command '['x']' died with <Signals.SIGSYS: 31>."),
    (32, "Command '['x']' died with unknown signal 32."),
    (33, "Command '['x']' died with unknown signal 33."),
    (34, "Command '['x']' died with <Signals.SIGRTMIN: 34>."),
    (35, "Command '['x']' died with unknown signal 35."),
    (36, "Command '['x']' died with unknown signal 36."),
    (37, "Command '['x']' died with unknown signal 37."),
    (38, "Command '['x']' died with unknown signal 38."),
    (39, "Command '['x']' died with unknown signal 39."),
    (40, "Command '['x']' died with unknown signal 40."),
    (41, "Command '['x']' died with unknown signal 41."),
    (42, "Command '['x']' died with unknown signal 42."),
    (43, "Command '['x']' died with unknown signal 43."),
    (44, "Command '['x']' died with unknown signal 44."),
    (45, "Command '['x']' died with unknown signal 45."),
    (46, "Command '['x']' died with unknown signal 46."),
    (47, "Command '['x']' died with unknown signal 47."),
    (48, "Command '['x']' died with unknown signal 48."),
    (49, "Command '['x']' died with unknown signal 49."),
    (50, "Command '['x']' died with unknown signal 50."),
    (51, "Command '['x']' died with unknown signal 51."),
    (52, "Command '['x']' died with unknown signal 52."),
    (53, "Command '['x']' died with unknown signal 53."),
    (54, "Command '['x']' died with unknown signal 54."),
    (55, "Command '['x']' died with unknown signal 55."),
    (56, "Command '['x']' died with unknown signal 56."),
    (57, "Command '['x']' died with unknown signal 57."),
    (58, "Command '['x']' died with unknown signal 58."),
    (59, "Command '['x']' died with unknown signal 59."),
    (60, "Command '['x']' died with unknown signal 60."),
    (61, "Command '['x']' died with unknown signal 61."),
    (62, "Command '['x']' died with unknown signal 62."),
    (63, "Command '['x']' died with unknown signal 63."),
    (64, "Command '['x']' died with <Signals.SIGRTMAX: 64>."),
    (65, "Command '['x']' died with unknown signal 65."),
];

/// The same for macos-latest, CPython 3.15.2.
#[cfg(target_os = "macos")]
const MACOS_SIGNAL_TEXT: [(i32, &str); 65] = [
    (1, "Command '['x']' died with <Signals.SIGHUP: 1>."),
    (2, "Command '['x']' died with <Signals.SIGINT: 2>."),
    (3, "Command '['x']' died with <Signals.SIGQUIT: 3>."),
    (4, "Command '['x']' died with <Signals.SIGILL: 4>."),
    (5, "Command '['x']' died with <Signals.SIGTRAP: 5>."),
    (6, "Command '['x']' died with <Signals.SIGABRT: 6>."),
    (7, "Command '['x']' died with <Signals.SIGEMT: 7>."),
    (8, "Command '['x']' died with <Signals.SIGFPE: 8>."),
    (9, "Command '['x']' died with <Signals.SIGKILL: 9>."),
    (10, "Command '['x']' died with <Signals.SIGBUS: 10>."),
    (11, "Command '['x']' died with <Signals.SIGSEGV: 11>."),
    (12, "Command '['x']' died with <Signals.SIGSYS: 12>."),
    (13, "Command '['x']' died with <Signals.SIGPIPE: 13>."),
    (14, "Command '['x']' died with <Signals.SIGALRM: 14>."),
    (15, "Command '['x']' died with <Signals.SIGTERM: 15>."),
    (16, "Command '['x']' died with <Signals.SIGURG: 16>."),
    (17, "Command '['x']' died with <Signals.SIGSTOP: 17>."),
    (18, "Command '['x']' died with <Signals.SIGTSTP: 18>."),
    (19, "Command '['x']' died with <Signals.SIGCONT: 19>."),
    (20, "Command '['x']' died with <Signals.SIGCHLD: 20>."),
    (21, "Command '['x']' died with <Signals.SIGTTIN: 21>."),
    (22, "Command '['x']' died with <Signals.SIGTTOU: 22>."),
    (23, "Command '['x']' died with <Signals.SIGIO: 23>."),
    (24, "Command '['x']' died with <Signals.SIGXCPU: 24>."),
    (25, "Command '['x']' died with <Signals.SIGXFSZ: 25>."),
    (26, "Command '['x']' died with <Signals.SIGVTALRM: 26>."),
    (27, "Command '['x']' died with <Signals.SIGPROF: 27>."),
    (28, "Command '['x']' died with <Signals.SIGWINCH: 28>."),
    (29, "Command '['x']' died with <Signals.SIGINFO: 29>."),
    (30, "Command '['x']' died with <Signals.SIGUSR1: 30>."),
    (31, "Command '['x']' died with <Signals.SIGUSR2: 31>."),
    (32, "Command '['x']' died with unknown signal 32."),
    (33, "Command '['x']' died with unknown signal 33."),
    (34, "Command '['x']' died with unknown signal 34."),
    (35, "Command '['x']' died with unknown signal 35."),
    (36, "Command '['x']' died with unknown signal 36."),
    (37, "Command '['x']' died with unknown signal 37."),
    (38, "Command '['x']' died with unknown signal 38."),
    (39, "Command '['x']' died with unknown signal 39."),
    (40, "Command '['x']' died with unknown signal 40."),
    (41, "Command '['x']' died with unknown signal 41."),
    (42, "Command '['x']' died with unknown signal 42."),
    (43, "Command '['x']' died with unknown signal 43."),
    (44, "Command '['x']' died with unknown signal 44."),
    (45, "Command '['x']' died with unknown signal 45."),
    (46, "Command '['x']' died with unknown signal 46."),
    (47, "Command '['x']' died with unknown signal 47."),
    (48, "Command '['x']' died with unknown signal 48."),
    (49, "Command '['x']' died with unknown signal 49."),
    (50, "Command '['x']' died with unknown signal 50."),
    (51, "Command '['x']' died with unknown signal 51."),
    (52, "Command '['x']' died with unknown signal 52."),
    (53, "Command '['x']' died with unknown signal 53."),
    (54, "Command '['x']' died with unknown signal 54."),
    (55, "Command '['x']' died with unknown signal 55."),
    (56, "Command '['x']' died with unknown signal 56."),
    (57, "Command '['x']' died with unknown signal 57."),
    (58, "Command '['x']' died with unknown signal 58."),
    (59, "Command '['x']' died with unknown signal 59."),
    (60, "Command '['x']' died with unknown signal 60."),
    (61, "Command '['x']' died with unknown signal 61."),
    (62, "Command '['x']' died with unknown signal 62."),
    (63, "Command '['x']' died with unknown signal 63."),
    (64, "Command '['x']' died with unknown signal 64."),
    (65, "Command '['x']' died with unknown signal 65."),
];

#[test]
fn a_nul_in_argv_or_cwd_is_a_value_error() {
    let t = |a: &[&str], cwd: Option<&str>| {
        err_text(&Run {
            argv: argv(a),
            cwd: cwd.map(String::from),
            timeout: Some(5.0),
            text: true,
            ..Default::default()
        })
    };
    assert_eq!(
        t(&["sh", "-c", "echo a\0b"], None),
        "ValueError: embedded null byte"
    );
    assert_eq!(t(&["s\0h"], None), "ValueError: embedded null byte");
    assert_eq!(
        t(&["true"], Some("/tmp\0x")),
        "ValueError: embedded null byte"
    );
}

#[test]
fn a_cwd_that_chdir_refuses_is_named() {
    std::fs::create_dir_all(SCRATCH).unwrap();
    let file = format!("{SCRATCH}/cwd-file");
    std::fs::write(&file, "").unwrap();
    let t = |prog: &str, cwd: &str| {
        err_text(&Run {
            argv: argv(&[prog]),
            cwd: Some(cwd.into()),
            timeout: Some(5.0),
            text: true,
            ..Default::default()
        })
    };
    assert_eq!(
        t("true", &file),
        format!("NotADirectoryError: [Errno 20] Not a directory: '{file}'")
    );
    let missing = format!("{SCRATCH}/no-such-dir");
    assert_eq!(
        t("/nope/x", &missing),
        format!("FileNotFoundError: [Errno 2] No such file or directory: '{missing}'")
    );
    std::fs::remove_file(&file).unwrap();
    // A folder without search permission: chdir gives EACCES (Python: PermissionError).
    use std::os::unix::fs::PermissionsExt;
    let nox = format!("{SCRATCH}/cwd-nox");
    std::fs::create_dir_all(&nox).unwrap();
    std::fs::set_permissions(&nox, std::fs::Permissions::from_mode(0o600)).unwrap();
    let got = t("true", &nox);
    std::fs::set_permissions(&nox, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::remove_dir(&nox).unwrap();
    assert_eq!(
        got,
        format!("PermissionError: [Errno 13] Permission denied: '{nox}'")
    );
}

#[test]
fn input_that_utf8_cannot_encode_is_a_unicode_encode_error() {
    let r = Run {
        argv: argv(&["cat"]),
        input: Some("a\u{10FDFF}".into()),
        timeout: Some(5.0),
        text: true,
        ..Default::default()
    };
    assert_eq!(
        err_text(&r),
        "UnicodeEncodeError: 'utf-8' codec can't encode character '\\udcff' in position 1: surrogates not allowed"
    );
}

#[test]
fn input_past_the_pipe_size() {
    let big = "x".repeat(1_000_000);
    // A child that never reads: the write side ends on EPIPE. The timeout only guards against a
    // hang; the case once timed out after 5 s at load average 17.
    let r = run_process(&Run {
        argv: argv(&["sh", "-c", "exit 0"]),
        input: Some(big.clone()),
        timeout: Some(30.0),
        text: true,
        ..Default::default()
    });
    assert_eq!(r.unwrap().returncode, 0);
    // A child that sleeps without reading: the full pipe must not block past the timeout.
    let started = Instant::now();
    let text = err_text(&Run {
        argv: argv(&["sh", "-c", "sleep 3"]),
        input: Some(big.clone()),
        timeout: Some(1.0),
        text: true,
        ..Default::default()
    });
    assert_eq!(
        text,
        "TimeoutExpired: Command '['sh', '-c', 'sleep 3']' timed out after 1.0 seconds"
    );
    assert!(started.elapsed().as_secs_f64() < 2.0);
    // A child that reads it all and echoes it back.
    let r = run_process(&Run {
        argv: argv(&["cat"]),
        input: Some(big),
        timeout: Some(30.0),
        text: true,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(r.stdout.len(), 1_000_000);
}

#[test]
fn a_malformed_prompt_hook_does_not_raise_before_a_claude_call() {
    // Python reads `hook_cfg.get("claude_bin")` at the first claude call only; with
    // cfg["prompt_hook"] = 5 a dry run and a run with no candidate both return 0 candidates.
    let home = format!("{SCRATCH}/capture-home");
    let root = format!("{SCRATCH}/capture-root");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        format!("{home}/config.toml"),
        format!("[capture]\nroot = \"{root}\"\nagy_root = \"\"\n"),
    )
    .unwrap();
    let mut cfg = simple_memory::config::load_config(Some(&home)).unwrap();
    let PyValue::Dict(sections) = &mut cfg else {
        panic!("config is a dict")
    };
    let slot = sections
        .iter_mut()
        .find(|(k, _)| *k == PyValue::Str("prompt_hook".into()))
        .unwrap();
    slot.1 = PyValue::Int("5".into());
    for dry_run in [true, false] {
        let got = capture_sessions(Some(&home), &cfg, dry_run, None, None, None).unwrap();
        assert_eq!(got.counts["candidates"], 0, "dry_run={dry_run}");
    }
}

extern "C" fn ignore_signal(_: libc::c_int) {}

#[test]
fn a_signal_during_the_poll_is_retried() {
    // PEP 475: Python's poll retries on EINTR. SIGUSR1 has a handler without SA_RESTART here and
    // goes to this thread only, so the poll inside run_process sees EINTR at 0.2 s and 0.4 s.
    // SAFETY: installs a handler that does nothing for a signal no other test uses.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = ignore_signal as *const () as usize;
        sa.sa_flags = 0;
        assert_eq!(libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut()), 0);
    }
    // SAFETY: pthread_self has no preconditions; the id is used while this thread runs.
    let me = unsafe { libc::pthread_self() } as usize;
    let poker = std::thread::spawn(move || {
        for _ in 0..2 {
            std::thread::sleep(std::time::Duration::from_millis(200));
            // SAFETY: `me` is the test thread, which is inside run_process until the child exits.
            unsafe { libc::pthread_kill(me as libc::pthread_t, libc::SIGUSR1) };
        }
    });
    let r = run_process(&Run {
        argv: argv(&["sh", "-c", "sleep 1; echo done"]),
        // guards a hang only
        timeout: Some(30.0),
        text: true,
        ..Default::default()
    });
    poker.join().unwrap();
    assert_eq!(r.unwrap().stdout, "done\n");
}
