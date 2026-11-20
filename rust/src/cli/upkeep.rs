//! The `upkeep` command, its report and the lock a manual run takes.

use indexmap::IndexMap;

use crate::config::get_home;
use crate::error::SmError;
use crate::py::argparse::Namespace;
use crate::py::float::format_fixed;
use crate::py::io::Out;
use crate::py::str::{slice, split};
use crate::service::catchup::{LOCK_HELD_MESSAGE, catchup_lock};
use crate::service::upkeep::{Report, STALE_DAYS, run_report, run_upkeep};

/// A manual run that writes takes the catch-up lock,
/// so it never races a catch-up's same part. `Ok(None)`: another process holds it, and the
/// message is printed; `Ok(Some(guard))`: run while the guard lives.
pub(crate) fn manual_run_lock(
    writes: bool,
    out: &mut Out,
) -> Result<Option<Option<std::fs::File>>, SmError> {
    if !writes {
        return Ok(Some(None));
    }
    match catchup_lock(&get_home(None)?)? {
        Some(f) => Ok(Some(Some(f))),
        None => {
            out.eprint(LOCK_HELD_MESSAGE)?;
            Ok(None)
        }
    }
}

pub fn print_upkeep_report(rep: &Report, limit: i64, out: &mut Out) -> Result<(), SmError> {
    let items: usize = rep.groups.iter().map(|g| g.ids.len()).sum();
    out.print(&format!(
        "title_groups={} items={items} near_duplicate_pairs={}",
        rep.groups.len(),
        rep.pairs.len()
    ))?;
    for g in &rep.groups {
        let mut kinds: Vec<(&String, &i64)> = g.kinds.iter().collect();
        kinds.sort();
        let kinds: Vec<String> = kinds.iter().map(|(k, v)| format!("{k}:{v}")).collect();
        out.print(&format!(
            "group repo={} n={} kinds={}: {}",
            g.repo,
            g.ids.len(),
            kinds.join(","),
            g.title
        ))?;
        out.print(&format!("  {}", g.ids.join(" ")))?;
    }
    let take = usize::try_from(limit.max(0)).unwrap_or(usize::MAX);
    for pr in rep.pairs.iter().take(take) {
        out.print(&format!(
            "pair {} {} {}",
            format_fixed(pr.score, 3),
            pr.a,
            pr.b
        ))?;
        out.print(&format!("  a: {}", split(&pr.title_a).join(" ")))?;
        out.print(&format!("  b: {}", split(&pr.title_b).join(" ")))?;
    }
    let mut by_repo: IndexMap<&str, i64> = IndexMap::new();
    for r in &rep.stale {
        *by_repo.entry(r.repo.as_str()).or_default() += 1;
    }
    out.print(&format!(
        "stale_planned_assumed={} older_than_days={STALE_DAYS}",
        rep.stale.len()
    ))?;
    let mut repos: Vec<(&str, i64)> = by_repo.into_iter().collect();
    repos.sort();
    for (repo, n) in repos {
        out.print(&format!("stale repo={repo} n={n}"))?;
        for r in rep.stale.iter().filter(|r| r.repo == repo) {
            out.print(&format!(
                "  {} {} {} {}",
                r.id,
                r.tag,
                slice(&r.as_of, None, Some(10)),
                split(&r.title).join(" ")
            ))?;
        }
    }
    Ok(())
}

pub fn run(ns: &Namespace, out: &mut Out) -> Result<i32, SmError> {
    let dry_run = ns.get_bool("dry_run");
    if ns.get_bool("report") {
        if dry_run {
            return Err(SmError::value("--report writes nothing; drop --dry-run"));
        }
        print_upkeep_report(&run_report(None, None)?, ns.get_int("limit"), out)?;
        return Ok(0);
    }
    let Some(lock) = manual_run_lock(!dry_run, out)? else {
        return Ok(1);
    };
    let res = run_upkeep(dry_run, None)?;
    drop(lock);
    let mode = if dry_run { "dry-run" } else { "applied" };
    out.print(&format!(
        "upkeep ({mode}): sessions={} superseded={} handoffs_without_session={}",
        res.sessions, res.superseded, res.handoffs_without_session
    ))?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use crate::py::io::Out;
    use crate::py::path::PyPath;
    use crate::service::catchup::{LOCK_HELD_MESSAGE, catchup_lock};

    const CHILD: &str = "SM_I_LOCK_CHILD";

    /// (exit code, last stderr line, stdout) of `sm <args>` in this process.
    fn sm(args: &[&str]) -> (i32, String, String) {
        let mut out = Out::capture();
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let code = super::super::run(&args, &mut out);
        out.finish(code);
        let (o, e) = out.captured();
        (code, e.lines().last().unwrap_or_default().to_string(), o)
    }

    /// While another process holds hook/catchup.lock, a manual run that writes prints
    /// LOCK_HELD_MESSAGE and exits 1 before it opens memory.db; a catch-up exits 0 silently and
    /// writes no catchup.log row. The home comes from SIMPLE_MEMORY_HOME, so the runs happen in a
    /// child test process that gets a scratch one.
    #[test]
    fn a_held_catchup_lock_stops_every_manual_run_that_writes() {
        if let Some(h) = std::env::var_os(CHILD) {
            let h = h.into_string().unwrap();
            assert_eq!(
                sm(&["upkeep"]),
                (1, LOCK_HELD_MESSAGE.to_string(), String::new())
            );
            assert_eq!(
                sm(&["import", "automemory"]),
                (1, LOCK_HELD_MESSAGE.to_string(), String::new())
            );
            assert_eq!(
                sm(&["import", "sessions"]),
                (1, LOCK_HELD_MESSAGE.to_string(), String::new())
            );
            assert_eq!(
                sm(&["import", "catch-up"]),
                (0, String::new(), String::new())
            );
            assert!(!std::path::Path::new(&format!("{h}/memory.db")).exists());
            assert!(!std::path::Path::new(&format!("{h}/catchup.log")).exists());
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let h = d.path().join("sm");
        let held = catchup_lock(&PyPath::new(h.to_str().unwrap())).unwrap();
        assert!(held.is_some());
        let name = "cli::upkeep::tests::a_held_catchup_lock_stops_every_manual_run_that_writes";
        let st = std::process::Command::new(std::env::current_exe().unwrap())
            .args([name, "--exact", "--quiet"])
            .env("HOME", d.path())
            .env("SIMPLE_MEMORY_HOME", &h)
            .env(CHILD, &h)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&st.stdout);
        assert!(st.status.success() && text.contains("1 passed"), "{text}");
        drop(held);
    }
}
