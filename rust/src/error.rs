//! Python exceptions mapped to one Rust error type.
//!
//! `base` is the class an `except` clause tests, `class` the exact name `type(exc).__name__`
//! gives, and `msg` CPython's `str(exc)`. Exception text reaches logs and the prompt notice, so
//! each constructor builds the message CPython builds.

use std::borrow::Cow;
use std::fmt;
use std::path::Path;

use crate::py::str::repr;

/// The Python base class an `except` clause catches. `Other` holds classes outside the six
/// builtins sm catches (OverflowError, TimeoutExpired, CalledProcessError, YAML errors); the CLI
/// treats it like `Runtime` and `Index`: the uncaught path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Base {
    Value,
    Key,
    Type,
    Os,
    Sqlite,
    Runtime,
    Index,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SmError {
    pub base: Base,
    pub class: Cow<'static, str>,
    pub msg: String,
}

impl fmt::Display for SmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for SmError {}

/// `os.strerror`: the platform libc's text, as CPython gives it.
fn strerror(errno: i32) -> String {
    // SAFETY: strerror returns a pointer to a static or thread-local NUL-terminated string.
    unsafe {
        let p = libc::strerror(errno);
        std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

/// CPython's OSError subclass for an errno (Objects/exceptions.c `errnomap`).
fn os_class(errno: i32) -> &'static str {
    match errno {
        libc::EAGAIN | libc::EALREADY | libc::EINPROGRESS => "BlockingIOError",
        libc::ECHILD => "ChildProcessError",
        libc::EPIPE | libc::ESHUTDOWN => "BrokenPipeError",
        libc::ECONNABORTED => "ConnectionAbortedError",
        libc::ECONNREFUSED => "ConnectionRefusedError",
        libc::ECONNRESET => "ConnectionResetError",
        libc::EEXIST => "FileExistsError",
        libc::ENOENT => "FileNotFoundError",
        libc::EISDIR => "IsADirectoryError",
        libc::ENOTDIR => "NotADirectoryError",
        libc::EINTR => "InterruptedError",
        libc::EACCES | libc::EPERM => "PermissionError",
        libc::ESRCH => "ProcessLookupError",
        libc::ETIMEDOUT => "TimeoutError",
        _ => "OSError",
    }
}

/// `repr()` of a path as Python holds it: a str decoded with surrogateescape, so a byte that is
/// not UTF-8 shows as `\udcXX`.
pub fn repr_path(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    match std::str::from_utf8(bytes) {
        Ok(s) => repr(s),
        Err(_) => crate::py::str::repr_surrogateescape(bytes),
    }
}

impl SmError {
    pub fn new(base: Base, class: impl Into<Cow<'static, str>>, msg: impl Into<String>) -> Self {
        SmError {
            base,
            class: class.into(),
            msg: msg.into(),
        }
    }

    /// An error named by its Python class; the base is the one Python's `except` sees.
    pub fn from_python(class: impl Into<Cow<'static, str>>, msg: impl Into<String>) -> Self {
        let class = class.into();
        let base = base_of(&class);
        SmError {
            base,
            class,
            msg: msg.into(),
        }
    }

    pub fn value(msg: impl Into<String>) -> Self {
        Self::new(Base::Value, "ValueError", msg)
    }

    /// `KeyError(key)`: `str()` of a KeyError is the repr of its key.
    pub fn key(key: &str) -> Self {
        Self::new(Base::Key, "KeyError", repr(key))
    }

    /// A KeyError for an item id the store does not hold: `item <id> not found`, plus a hint when
    /// the id has lower case after `m_` or is shorter than a full id.
    pub fn not_found(id: &str) -> Self {
        let partial = id.chars().count() < 28
            || id
                .strip_prefix("m_")
                .unwrap_or(id)
                .chars()
                .any(|c| c.is_ascii_lowercase());
        let hint = if partial {
            " (ids are case-sensitive and must be given in full)"
        } else {
            ""
        };
        Self::new(Base::Key, "KeyError", format!("item {id} not found{hint}"))
    }

    pub fn type_(msg: impl Into<String>) -> Self {
        Self::new(Base::Type, "TypeError", msg)
    }

    pub fn runtime(msg: impl Into<String>) -> Self {
        Self::new(Base::Runtime, "RuntimeError", msg)
    }

    pub fn index(msg: impl Into<String>) -> Self {
        Self::new(Base::Index, "IndexError", msg)
    }

    pub fn overflow(msg: impl Into<String>) -> Self {
        Self::new(Base::Other, "OverflowError", msg)
    }

    /// UnicodeDecodeError of strict UTF-8, as `bytes.decode()` words it:
    /// `'utf-8' codec can't decode byte 0xff in position 3: invalid start byte`.
    pub fn unicode_decode(bytes: &[u8], err: std::str::Utf8Error) -> Self {
        let pos = err.valid_up_to();
        let msg = match err.error_len() {
            Some(n) => {
                let first = bytes[pos];
                let reason = utf8_reason(bytes, pos, n);
                if n == 1 {
                    format!(
                        "'utf-8' codec can't decode byte 0x{first:02x} in position {pos}: {reason}"
                    )
                } else {
                    format!(
                        "'utf-8' codec can't decode bytes in position {pos}-{}: {reason}",
                        pos + n - 1
                    )
                }
            }
            None => {
                let end = bytes.len() - 1;
                if end == pos {
                    format!(
                        "'utf-8' codec can't decode byte 0x{:02x} in position {pos}: unexpected end of data",
                        bytes[pos]
                    )
                } else {
                    format!(
                        "'utf-8' codec can't decode bytes in position {pos}-{end}: unexpected end of data"
                    )
                }
            }
        };
        Self::new(Base::Value, "UnicodeDecodeError", msg)
    }

    /// JSONDecodeError: `<msg>: line L column C (char P)`, with P a code point offset.
    pub fn json_decode(msg: &str, doc: &str, pos: usize) -> Self {
        let before: Vec<char> = doc.chars().take(pos).collect();
        let lineno = before.iter().filter(|&&c| c == '\n').count() + 1;
        let colno = match before.iter().rposition(|&c| c == '\n') {
            Some(i) => pos - i,
            None => pos + 1,
        };
        Self::new(
            Base::Value,
            "JSONDecodeError",
            format!("{msg}: line {lineno} column {colno} (char {pos})"),
        )
    }

    /// OSError from errno, in CPython's `[Errno N] strerror: 'file'` form (and `'a' -> 'b'`).
    pub fn os(errno: i32, filename: Option<&Path>, filename2: Option<&Path>) -> Self {
        let text = strerror(errno);
        let msg = match (filename, filename2) {
            (Some(a), Some(b)) => format!(
                "[Errno {errno}] {text}: {} -> {}",
                repr_path(a),
                repr_path(b)
            ),
            (Some(a), None) => format!("[Errno {errno}] {text}: {}", repr_path(a)),
            _ => format!("[Errno {errno}] {text}"),
        };
        Self::new(Base::Os, os_class(errno), msg)
    }

    #[cfg(test)]
    pub fn file_not_found(path: &Path) -> Self {
        Self::os(libc::ENOENT, Some(path), None)
    }

    pub fn is_a_directory(path: &Path) -> Self {
        Self::os(libc::EISDIR, Some(path), None)
    }

    #[cfg(test)]
    pub fn permission(path: &Path) -> Self {
        Self::os(libc::EACCES, Some(path), None)
    }

    /// An io::Error as the OSError Python raises for the same call; an error with no errno
    /// (a Rust-side `InvalidData`, for example) keeps its text under plain OSError.
    pub fn from_io(err: &std::io::Error, path: Option<&Path>) -> Self {
        match err.raw_os_error() {
            Some(errno) => Self::os(errno, path, None),
            None => Self::new(Base::Os, "OSError", err.to_string()),
        }
    }

    /// The sqlite3 exception Python raises for the same failure (Modules/_sqlite/util.c
    /// `_pysqlite_seterror`): the class from the primary result code, the message from
    /// `sqlite3_errmsg`. For an error from `Connection::open*`, use `sqlite_open`.
    pub fn sqlite(err: &rusqlite::Error) -> Self {
        match err {
            rusqlite::Error::SqliteFailure(e, msg) => {
                let text = match msg {
                    Some(m) => m.clone(),
                    None => rusqlite::ffi::code_to_str(e.extended_code).to_string(),
                };
                sqlite_error(e.extended_code, text)
            }
            // rusqlite adds the SQL and the offset; Python shows sqlite3_errmsg alone.
            rusqlite::Error::SqlInputError { error, msg, .. } => {
                sqlite_error(error.extended_code, msg.clone())
            }
            rusqlite::Error::InvalidParameterCount(given, want) => Self::new(
                Base::Sqlite,
                "ProgrammingError",
                format!(
                    "Incorrect number of bindings supplied. The current statement uses {want}, and there are {given} supplied."
                ),
            ),
            other => Self::new(Base::Sqlite, "DatabaseError", other.to_string()),
        }
    }

    /// `sqlite(err)` for a failed open of `path`: rusqlite appends `: <path>` to a CANTOPEN
    /// message, and Python's `sqlite3.connect` does not.
    pub fn sqlite_open(err: &rusqlite::Error, path: &Path) -> Self {
        let mut e = Self::sqlite(err);
        let suffix = format!(": {}", path.to_string_lossy());
        if let rusqlite::Error::SqliteFailure(f, _) = err
            && f.code == rusqlite::ErrorCode::CannotOpen
            && let Some(head) = e.msg.strip_suffix(&suffix)
        {
            e.msg = head.to_string();
        }
        e
    }

    /// Does an `except <base>` clause catch this error?
    pub fn is(&self, base: Base) -> bool {
        self.base == base
    }

    pub fn python_name(&self) -> &str {
        &self.class
    }

    /// `f"{type(exc).__name__}: {exc}"`, the form the parity protocol carries.
    #[cfg(any(test, feature = "parity"))]
    pub fn python_text(&self) -> String {
        format!("{}: {}", self.class, self.msg)
    }
}

impl From<rusqlite::Error> for SmError {
    fn from(err: rusqlite::Error) -> Self {
        SmError::sqlite(&err)
    }
}

/// The error `_pysqlite_seterror` raises for a result code. SQLITE_NOMEM raises the builtin
/// `MemoryError()` with no text (`PyErr_NoMemory`), which no `except sqlite3.Error` catches, so
/// the CLI does not.
fn sqlite_error(extended_code: i32, msg: String) -> SmError {
    match sqlite_class(extended_code) {
        "MemoryError" => SmError::new(Base::Other, "MemoryError", ""),
        class => SmError::new(Base::Sqlite, class, msg),
    }
}

/// The sqlite3 class for a result code (Modules/_sqlite/util.c).
fn sqlite_class(extended_code: i32) -> &'static str {
    use rusqlite::ffi;
    match extended_code & 0xff {
        ffi::SQLITE_INTERNAL | ffi::SQLITE_NOTFOUND => "InternalError",
        ffi::SQLITE_NOMEM => "MemoryError",
        ffi::SQLITE_ERROR
        | ffi::SQLITE_PERM
        | ffi::SQLITE_ABORT
        | ffi::SQLITE_BUSY
        | ffi::SQLITE_LOCKED
        | ffi::SQLITE_READONLY
        | ffi::SQLITE_INTERRUPT
        | ffi::SQLITE_IOERR
        | ffi::SQLITE_FULL
        | ffi::SQLITE_CANTOPEN
        | ffi::SQLITE_PROTOCOL
        | ffi::SQLITE_EMPTY
        | ffi::SQLITE_SCHEMA => "OperationalError",
        ffi::SQLITE_CORRUPT => "DatabaseError",
        ffi::SQLITE_TOOBIG => "DataError",
        ffi::SQLITE_CONSTRAINT | ffi::SQLITE_MISMATCH => "IntegrityError",
        ffi::SQLITE_MISUSE | ffi::SQLITE_RANGE => "InterfaceError",
        _ => "DatabaseError",
    }
}

/// The base Python's `except` sees for a class name.
fn base_of(class: &str) -> Base {
    match class {
        "ValueError" | "UnicodeDecodeError" | "UnicodeEncodeError" | "UnicodeError"
        | "JSONDecodeError" => Base::Value,
        "KeyError" => Base::Key,
        "TypeError" => Base::Type,
        "RuntimeError" | "NotImplementedError" | "RecursionError" => Base::Runtime,
        "IndexError" => Base::Index,
        "OSError"
        | "BlockingIOError"
        | "ChildProcessError"
        | "BrokenPipeError"
        | "ConnectionAbortedError"
        | "ConnectionRefusedError"
        | "ConnectionResetError"
        | "ConnectionError"
        | "FileExistsError"
        | "FileNotFoundError"
        | "IsADirectoryError"
        | "NotADirectoryError"
        | "InterruptedError"
        | "PermissionError"
        | "ProcessLookupError"
        | "TimeoutError" => Base::Os,
        "Error" | "Warning" | "InterfaceError" | "DatabaseError" | "DataError"
        | "OperationalError" | "IntegrityError" | "InternalError" | "ProgrammingError"
        | "NotSupportedError" => Base::Sqlite,
        _ => Base::Other,
    }
}

/// The reason CPython's UTF-8 decoder gives for an invalid sequence of `n` bytes at `pos`.
fn utf8_reason(bytes: &[u8], pos: usize, n: usize) -> &'static str {
    if n == 1 && !(0xC2..=0xF4).contains(&bytes[pos]) {
        "invalid start byte"
    } else {
        "invalid continuation byte"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_errors_match_cpython_text() {
        let e = SmError::file_not_found(Path::new("/nonexistent/x"));
        assert_eq!(
            e.python_text(),
            "FileNotFoundError: [Errno 2] No such file or directory: '/nonexistent/x'"
        );
        assert!(e.is(Base::Os));
        let e = SmError::is_a_directory(Path::new("/tmp"));
        assert_eq!(
            e.python_text(),
            "IsADirectoryError: [Errno 21] Is a directory: '/tmp'"
        );
        let e = SmError::permission(Path::new("/etc/sudoers"));
        assert_eq!(
            e.python_text(),
            "PermissionError: [Errno 13] Permission denied: '/etc/sudoers'"
        );
        let e = SmError::os(
            libc::ENOENT,
            Some(Path::new("/nonexistent/a")),
            Some(Path::new("/nonexistent/b")),
        );
        assert_eq!(
            e.msg,
            "[Errno 2] No such file or directory: '/nonexistent/a' -> '/nonexistent/b'"
        );
        let e = SmError::file_not_found(Path::new("/tmp/x'y\"z/q"));
        assert_eq!(
            e.msg,
            "[Errno 2] No such file or directory: '/tmp/x\\'y\"z/q'"
        );
        assert_eq!(
            SmError::os(libc::EEXIST, Some(Path::new("/tmp")), None).class,
            "FileExistsError"
        );
        assert_eq!(
            SmError::os(libc::EAGAIN, None, None).python_text(),
            // EAGAIN is 35 on macOS and 11 on Linux; both libcs give the same text
            if cfg!(target_os = "macos") {
                "BlockingIOError: [Errno 35] Resource temporarily unavailable"
            } else {
                "BlockingIOError: [Errno 11] Resource temporarily unavailable"
            }
        );
    }

    #[test]
    fn non_utf8_path_reprs_with_surrogateescape() {
        use std::os::unix::ffi::OsStrExt;
        let p = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/\xff/q"));
        assert_eq!(repr_path(p), "'/tmp/\\udcff/q'");
    }

    #[test]
    fn from_io_reads_errno() {
        let err = std::fs::read("/nonexistent/sm-test").unwrap_err();
        let e = SmError::from_io(&err, Some(Path::new("/nonexistent/sm-test")));
        assert_eq!(
            e.python_text(),
            "FileNotFoundError: [Errno 2] No such file or directory: '/nonexistent/sm-test'"
        );
    }

    #[test]
    fn key_value_and_from_python_bases() {
        assert_eq!(SmError::key("x").to_string(), "'x'");
        assert!(SmError::from_python("JSONDecodeError", "m").is(Base::Value));
        assert!(SmError::from_python("TimeoutError", "m").is(Base::Os));
        assert!(SmError::from_python("AttributeError", "m").is(Base::Other));
        assert!(SmError::from_python("TimeoutExpired", "m").is(Base::Other));
        assert!(SmError::from_python("OperationalError", "m").is(Base::Sqlite));
        assert!(!SmError::overflow("date value out of range").is(Base::Value));
    }

    #[test]
    fn unicode_decode_messages() {
        let cases: &[(&[u8], &str)] = &[
            (
                b"ab\xffc",
                "'utf-8' codec can't decode byte 0xff in position 2: invalid start byte",
            ),
            (
                b"a\xc3(",
                "'utf-8' codec can't decode byte 0xc3 in position 1: invalid continuation byte",
            ),
            (
                b"a\xe2\x82",
                "'utf-8' codec can't decode bytes in position 1-2: unexpected end of data",
            ),
            (
                b"a\xe2",
                "'utf-8' codec can't decode byte 0xe2 in position 1: unexpected end of data",
            ),
            (
                b"\xf0\x90(",
                "'utf-8' codec can't decode bytes in position 0-1: invalid continuation byte",
            ),
            (
                b"\xed\xa0\x80",
                "'utf-8' codec can't decode byte 0xed in position 0: invalid continuation byte",
            ),
            (
                b"\xc0\x80",
                "'utf-8' codec can't decode byte 0xc0 in position 0: invalid start byte",
            ),
            (
                b"a\xf0\x90\x80",
                "'utf-8' codec can't decode bytes in position 1-3: unexpected end of data",
            ),
        ];
        for (b, want) in cases {
            let err = std::str::from_utf8(b).unwrap_err();
            assert_eq!(SmError::unicode_decode(b, err).msg, *want);
        }
    }

    #[test]
    fn json_decode_position() {
        let e = SmError::json_decode("Expecting property name enclosed in double quotes", "{x", 1);
        assert_eq!(
            e.python_text(),
            "JSONDecodeError: Expecting property name enclosed in double quotes: line 1 column 2 (char 1)"
        );
        let e = SmError::json_decode("Expecting value", "[\n  ,", 4);
        assert_eq!(e.msg, "Expecting value: line 2 column 3 (char 4)");
    }

    fn sq(f: impl FnOnce(&rusqlite::Connection) -> rusqlite::Result<()>) -> String {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch("create table t(x unique not null); create virtual table f using fts5(a);")
            .unwrap();
        SmError::sqlite(&f(&c).unwrap_err()).python_text()
    }

    #[test]
    fn sqlite_nomem_is_an_uncaught_memory_error() {
        let e = SmError::sqlite(&rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_NOMEM),
            Some("out of memory".into()),
        ));
        assert_eq!(
            (e.base, &*e.class, e.msg.as_str()),
            (Base::Other, "MemoryError", "")
        );
        assert!(!e.is(Base::Sqlite));
    }

    #[test]
    fn sqlite_errors_match_python() {
        assert_eq!(
            sq(|c| c.execute_batch("select * from nope")),
            "OperationalError: no such table: nope"
        );
        assert_eq!(
            sq(|c| c.execute_batch("insert into t values (1); insert into t values (1);")),
            "IntegrityError: UNIQUE constraint failed: t.x"
        );
        assert_eq!(
            sq(|c| c.execute_batch("insert into t values (null)")),
            "IntegrityError: NOT NULL constraint failed: t.x"
        );
        assert_eq!(
            sq(|c| c
                .query_row("select count(*) from f where f match ?", ["\"abc"], |_| Ok(
                    ()
                ))
                .map(|_| ())),
            "OperationalError: unterminated string"
        );
        assert_eq!(
            sq(|c| c
                .query_row("select count(*) from f where f match ?", ["a AND"], |_| Ok(
                    ()
                ))
                .map(|_| ())),
            "OperationalError: fts5: syntax error near \"\""
        );
        assert_eq!(
            sq(|c| c.execute_batch("selec 1")),
            "OperationalError: near \"selec\": syntax error"
        );
    }

    #[test]
    fn sqlite_locked_and_not_a_db() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("p.db");
        let a = rusqlite::Connection::open(&db).unwrap();
        a.execute_batch("create table t(x); begin exclusive; insert into t values (5);")
            .unwrap();
        let b = rusqlite::Connection::open(&db).unwrap();
        b.busy_timeout(std::time::Duration::ZERO).unwrap();
        let err = b
            .query_row("select count(*) from t", [], |_| Ok(()))
            .unwrap_err();
        assert_eq!(
            SmError::sqlite(&err).python_text(),
            "OperationalError: database is locked"
        );

        let bad = dir.path().join("bad.db");
        std::fs::write(&bad, "this is not a sqlite database file at all, padding padding padding padding padding padding padding").unwrap();
        let c = rusqlite::Connection::open(&bad).unwrap();
        let err = c
            .query_row("select 1 from sqlite_master", [], |_| Ok(()))
            .unwrap_err();
        assert_eq!(
            SmError::sqlite(&err).python_text(),
            "DatabaseError: file is not a database"
        );

        let missing = Path::new("/nonexistent/dir/x.db");
        let err = rusqlite::Connection::open(missing).unwrap_err();
        assert_eq!(
            SmError::sqlite_open(&err, missing).python_text(),
            "OperationalError: unable to open database file"
        );
    }
}
