//! One test per binary: it counts this process's open file descriptors, which a parallel test
//! would change. Python's `subprocess.run` closes every pipe end on a timeout (`Popen.__exit__`).

use simple_memory::service::capture::{Run, run_process};

fn open_fds() -> usize {
    std::fs::read_dir("/dev/fd").unwrap().count()
}

#[test]
fn a_timeout_with_a_grandchild_leaves_no_pipe_open() {
    let gc = format!("{}/tests/fx/gc.sh", env!("CARGO_MANIFEST_DIR"));
    let before = open_fds();
    let e = run_process(&Run {
        argv: vec![gc],
        input: Some("x".into()),
        timeout: Some(0.5),
        text: true,
        ..Default::default()
    })
    .unwrap_err();
    assert_eq!(e.class, "TimeoutExpired");
    // The grandchild (`sleep 8`) still holds its copies; ours must be closed now.
    assert_eq!(open_fds(), before);
}
