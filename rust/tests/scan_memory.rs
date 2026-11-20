//! Peak memory of the doctor imports scan. This file holds one test, so the counting allocator
//! below sees no other test's allocations.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

use simple_memory::service::doctor::run_doctor;
use simple_memory::service::import_::run_import;

/// The system allocator, counting the bytes held now and the most held since the last reset.
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(n: usize) {
    let now = LIVE.fetch_add(n, SeqCst) + n;
    PEAK.fetch_max(now, SeqCst);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        unsafe { System.dealloc(p, layout) };
        LIVE.fetch_sub(layout.size(), SeqCst);
    }

    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, layout, new_size) };
        if !q.is_null() {
            LIVE.fetch_sub(layout.size(), SeqCst);
            grew(new_size);
        }
        q
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

const FILES: usize = 300;
const RESULT_LINES: usize = 410; // 410 lines of 50 bytes: about 20 KB a file, 6 MB in all

/// 300 stored task logs of about 20 KB each: the scan reads them one at a time, so the most it
/// holds at once stays under a quarter of the 6 MB of bodies on disk. A scan that keeps every parsed file
/// until it picks one file per source holds more than all the bodies.
#[test]
fn a_doctor_imports_scan_does_not_hold_every_task_log_body() {
    let d = tempfile::tempdir().unwrap();
    let tmp = std::fs::canonicalize(d.path()).unwrap();
    let (home, root, none) = (tmp.join("home"), tmp.join("tl"), tmp.join("none"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&none).unwrap();
    std::fs::write(
        home.join("config.toml"),
        format!(
            "[upkeep]\ntasklog_roots = [\"{}\"]\nhandoff_dirs = [\"{}\"]\n[automemory]\nroot = \"{}\"\n",
            root.display(),
            none.display(),
            none.display()
        ),
    )
    .unwrap();
    let day = root.join("rep/docs/tasklog/2026-11-01");
    std::fs::create_dir_all(&day).unwrap();
    let mut on_disk = 0;
    for i in 0..FILES {
        let results: String = (0..RESULT_LINES)
            .map(|j| {
                format!(
                    "  - result line {j:04} of task {i:04} in area {:02}.....\n",
                    j % 17
                )
            })
            .collect();
        let text = format!(
            "id: 20261101-090000-t{i:04}\ntitle: T {i}\nas_of: 2026-11-01T09:00:00+00:00\n\
             results:\n{results}"
        );
        on_disk += text.len();
        std::fs::write(day.join(format!("t{i:04}.yaml")), text).unwrap();
    }
    let home_s = home.to_str();
    let res = run_import("tasklog", root.to_str().unwrap(), false, false, home_s).unwrap();
    assert_eq!((res.inserted, res.failed_rows.len()), (FILES as i64, 0));

    let before = LIVE.load(SeqCst);
    PEAK.store(before, SeqCst);
    let lag = run_doctor(false, false, false, true, home_s)
        .unwrap()
        .lag
        .unwrap()
        .0;
    let rise = PEAK.load(SeqCst) - before;
    assert_eq!((lag.tasklog_files, lag.not_stored.len()), (FILES as i64, 0));
    eprintln!("bodies on disk {on_disk} bytes, peak rise over the scan {rise} bytes");
    assert!(on_disk > 6_000_000, "{on_disk}");
    assert!(rise < on_disk / 4, "the scan held {rise} bytes at once");
}
