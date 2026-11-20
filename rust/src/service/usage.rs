//! usage.log: one JSON row per `sm show`, `sm query` and
//! `sm related` call, so `sm stats` can tell which recalled items agents open.
//!
//! Row: {"ts": UTC ISO seconds, "cmd", "ids": in the order shown, "repo": the query scope or null,
//! "flags": {}}, written with `ensure_ascii=False`. No query text: hook.log holds the hook's queries.

use std::io::Write;

use crate::error::SmError;
use crate::py::path::{Errors, PyPath, encode_text};
use crate::py::value::PyValue;

pub const USAGE_LOG: &str = "usage.log";
pub const LOG_MAX_BYTES: u64 = 5_000_000;
pub const LOG_KEEP_ROWS: usize = 5_000;

/// The row `log_usage` appends, without its newline.
pub fn usage_row(
    cmd: &str,
    ids: &[String],
    repo: Option<&str>,
    flags: Option<&PyValue>,
) -> Result<String, SmError> {
    let ts = crate::clock::now_utc()
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string();
    let flags = match flags {
        None | Some(PyValue::None) => PyValue::Dict(Vec::new()),
        Some(f) => f.clone(),
    };
    let row = PyValue::Dict(vec![
        (PyValue::Str("ts".into()), PyValue::Str(ts)),
        (PyValue::Str("cmd".into()), PyValue::Str(cmd.into())),
        (
            PyValue::Str("ids".into()),
            PyValue::List(ids.iter().map(|i| PyValue::Str(i.clone())).collect()),
        ),
        (
            PyValue::Str("repo".into()),
            repo.map_or(PyValue::None, |r| PyValue::Str(r.into())),
        ),
        (PyValue::Str("flags".into()), flags),
    ]);
    crate::py::json::dumps(&row, &crate::py::json::Opts::unicode())
}

/// Append one row. An OSError is dropped, because a usage row must never fail the command; a
/// UnicodeEncodeError (an id holding a surrogateescape byte) is not an OSError and propagates
/// after the log is opened, as Python's does.
pub fn log_usage(
    cmd: &str,
    ids: &[String],
    repo: Option<&str>,
    flags: Option<&PyValue>,
    home: Option<&str>,
) -> Result<(), SmError> {
    let line = usage_row(cmd, ids, repo, flags)? + "\n";
    let log = crate::config::get_home(home)?.join(USAGE_LOG);
    let os = log.to_os();
    let mut fh = match std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&os)
    {
        Ok(f) => f,
        Err(_) => return Ok(()),
    };
    let bytes = encode_text(&line)?;
    if fh.write_all(&bytes).is_err() {
        return Ok(());
    }
    drop(fh);
    match std::fs::metadata(&os) {
        Ok(m) if m.len() > LOG_MAX_BYTES => match keep_newest_rows(&log, LOG_KEEP_ROWS) {
            Err(e) if !e.is(crate::error::Base::Os) => Err(e),
            _ => Ok(()),
        },
        _ => Ok(()),
    }
}

/// Rewrite the log with its newest `keep` rows (Python's `[-keep:]`, so 0 keeps them all) through
/// a 0600 temp file `<name>.XXXXXXXX.tmp` beside it and a rename, so a reader never sees it half
/// written. The read is universal-newline text with errors="replace", as `read_text` gives it.
pub fn keep_newest_rows(log: &PyPath, keep: usize) -> Result<(), SmError> {
    let text = log.read_text(Errors::Replace)?;
    let rows = crate::py::str::splitlines(&text, true);
    let start = if keep == 0 {
        0
    } else {
        rows.len().saturating_sub(keep)
    };
    let body = rows[start..].concat();
    let dir = log.parent().to_os();
    let tmp = tempfile::Builder::new()
        .prefix(&format!("{}.", log.name()))
        .suffix(".tmp")
        .rand_bytes(8)
        .tempfile_in(&dir)
        .map_err(|e| SmError::from_io(&e, Some(&dir)))?;
    // On any error the NamedTempFile drop unlinks the temp file (Python's `except BaseException`).
    tmp.as_file()
        .write_all(body.as_bytes())
        .map_err(|e| SmError::from_io(&e, Some(tmp.path())))?;
    let os = log.to_os();
    tmp.persist(&os)
        .map_err(|e| SmError::from_io(&e.error, Some(&os)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home() -> tempfile::TempDir {
        tempfile::tempdir().expect("a temp dir")
    }

    fn read(home: &tempfile::TempDir) -> String {
        std::fs::read_to_string(home.path().join(USAGE_LOG)).expect("usage.log")
    }

    #[test]
    fn appends_one_unicode_row_per_call() {
        let h = tmp_home();
        let home = h.path().to_str().expect("utf-8 temp path");
        let flags = PyValue::Dict(vec![(PyValue::Str("limit".into()), PyValue::int(5))]);
        log_usage(
            "show",
            &["m_1".into(), "m_2".into()],
            Some("r"),
            Some(&flags),
            Some(home),
        )
        .unwrap();
        log_usage("query", &["ë".into()], None, None, Some(home)).unwrap();
        let text = read(&h);
        let rows: Vec<&str> = text.lines().collect();
        assert_eq!(rows.len(), 2);
        assert!(
            rows[0].ends_with(
                r#", "cmd": "show", "ids": ["m_1", "m_2"], "repo": "r", "flags": {"limit": 5}}"#
            ),
            "{}",
            rows[0]
        );
        assert!(
            rows[1].ends_with(r#", "cmd": "query", "ids": ["ë"], "repo": null, "flags": {}}"#),
            "{}",
            rows[1]
        );
        assert!(
            rows[0].starts_with(r#"{"ts": ""#) && rows[0][8..33].ends_with("+00:00"),
            "{}",
            rows[0]
        );
    }

    #[test]
    fn os_error_is_dropped_but_encode_error_is_not() {
        let h = tmp_home();
        let missing = h.path().join("nope/deeper");
        assert!(log_usage("show", &[], None, None, missing.to_str()).is_ok());
        let home = h.path().to_str().expect("utf-8 temp path");
        let err = log_usage("show", &["a\u{10FDFF}".into()], None, None, Some(home)).unwrap_err();
        // Python probe, same calls: positions 61 and 67-68 of the row text.
        assert_eq!(
            err.python_text(),
            "UnicodeEncodeError: 'utf-8' codec can't encode character '\\udcff' in position 61: surrogates not allowed"
        );
        let run = log_usage(
            "show",
            &["x".into(), "ab\u{10FD80}\u{10FD81}c".into()],
            None,
            None,
            Some(home),
        )
        .unwrap_err();
        assert_eq!(
            run.msg,
            "'utf-8' codec can't encode characters in position 67-68: surrogates not allowed"
        );
        // The log was opened, so it exists, empty.
        assert_eq!(read(&h), "");
    }

    #[test]
    fn keep_newest_rows_keeps_the_tail_and_zero_keeps_all() {
        let h = tmp_home();
        let log = PyPath::new(h.path().join(USAGE_LOG).to_str().expect("utf-8"));
        let mut bytes = "a\nb\r\nc\rd\u{2028}e\n".as_bytes().to_vec();
        bytes.push(0xff);
        std::fs::write(log.to_os(), bytes).unwrap();
        keep_newest_rows(&log, 3).unwrap();
        assert_eq!(read(&h), "d\u{2028}e\n\u{FFFD}");
        keep_newest_rows(&log, 0).unwrap();
        assert_eq!(read(&h), "d\u{2028}e\n\u{FFFD}");
        let names: Vec<_> = std::fs::read_dir(h.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "no temp file left: {names:?}");
    }
}
