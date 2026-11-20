//! `pathlib.Path` as sm uses it (Python 3.15.1 on macOS): the pure parts (normalization, parts,
//! name, stem, suffix, parents, joins, relative_to), `expanduser` with `~user`, `resolve` as a
//! port of `posixpath.realpath(strict=False)` (an lstat and readlink walk; never `canonicalize`,
//! which returns the case on disk), `glob`/`rglob` in Python's yield order,
//! `exists`/`is_file`/`is_dir`, `iterdir` and `read_text` with Python's decode and newline rules.
//!
//! A path is a Python str: a byte that is not UTF-8 is a stand-in char, and
//! `to_os` gives the bytes back.

use std::fmt;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

use crate::error::SmError;
use crate::py::str::decode_surrogateescape;

/// A `PosixPath`: its normalized str (`str(path)`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PyPath {
    s: String,
}

impl fmt::Display for PyPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.s)
    }
}

/// `splitroot` of a posix str: one or two leading slashes are the root, three or more are one.
fn split_root(s: &str) -> (&'static str, &str) {
    if s.starts_with("//") && !s.starts_with("///") {
        ("//", &s[2..])
    } else if s.starts_with('/') {
        ("/", s.trim_start_matches('/'))
    } else {
        ("", s)
    }
}

impl PyPath {
    /// `Path(s)`: empty and `.` parts dropped, `..` kept, no parts is `.`.
    pub fn new(s: &str) -> PyPath {
        let (root, rest) = split_root(s);
        let tail: Vec<&str> = rest
            .split('/')
            .filter(|p| !p.is_empty() && *p != ".")
            .collect();
        PyPath::from_parts(root, &tail)
    }

    fn from_parts<S: AsRef<str>>(root: &str, tail: &[S]) -> PyPath {
        let mut s = root.to_string();
        for (i, p) in tail.iter().enumerate() {
            if i > 0 {
                s.push('/');
            }
            s.push_str(p.as_ref());
        }
        if s.is_empty() {
            s.push('.');
        }
        PyPath { s }
    }

    /// A path the OS gave (a dir entry, a link target), decoded with surrogateescape.
    #[cfg(test)]
    pub fn from_os(p: &std::path::Path) -> PyPath {
        PyPath::new(&decode_surrogateescape(p.as_os_str().as_bytes()))
    }

    pub fn as_str(&self) -> &str {
        &self.s
    }

    /// The OS path (`os.fsencode`): stand-ins become their bytes again.
    pub fn to_os(&self) -> PathBuf {
        crate::py::io::fs_path(&self.s)
    }

    /// `path.root`: "", "/" or "//".
    pub fn root(&self) -> &'static str {
        split_root(&self.s).0
    }

    fn tail(&self) -> Vec<&str> {
        let rest = split_root(&self.s).1;
        if rest.is_empty() || rest == "." {
            Vec::new()
        } else {
            rest.split('/').collect()
        }
    }

    /// `path.parts`: the root (if any), then each part.
    pub fn parts(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.root().is_empty() {
            out.push(self.root().to_string());
        }
        out.extend(self.tail().into_iter().map(String::from));
        out
    }

    /// `path.name`: the last part, "" for a root or `.`.
    pub fn name(&self) -> &str {
        self.tail().last().copied().unwrap_or("")
    }

    /// `path.suffix`: the last `.x` of the name once leading dots are stripped.
    #[cfg(test)]
    pub fn suffix(&self) -> &str {
        let name = self.name().trim_start_matches('.');
        match name.rfind('.') {
            Some(i) => &name[i..],
            None => "",
        }
    }

    /// `path.stem`: the name minus its suffix, if what is left holds a non-dot char.
    pub fn stem(&self) -> &str {
        let name = self.name();
        match name.rfind('.') {
            Some(i) if !name[..i].trim_start_matches('.').is_empty() => &name[..i],
            _ => name,
        }
    }

    /// `path.parent`: the path minus its last part (a root or `.` is its own parent).
    pub fn parent(&self) -> PyPath {
        let tail = self.tail();
        if tail.is_empty() {
            return self.clone();
        }
        PyPath::from_parts(self.root(), &tail[..tail.len() - 1])
    }

    /// `list(path.parents)`: nearest first, the root or `.` last.
    pub fn parents(&self) -> Vec<PyPath> {
        let tail = self.tail();
        (0..tail.len())
            .rev()
            .map(|n| PyPath::from_parts(self.root(), &tail[..n]))
            .collect()
    }

    /// `path / other` (`joinpath`): an absolute `other` replaces the path.
    pub fn join(&self, other: &str) -> PyPath {
        if other.starts_with('/') {
            return PyPath::new(other);
        }
        if self.s == "." {
            return PyPath::new(other);
        }
        if self.s.ends_with('/') {
            return PyPath::new(&format!("{}{}", self.s, other));
        }
        PyPath::new(&format!("{}/{}", self.s, other))
    }

    /// `path.with_name(name)`.
    pub fn with_name(&self, name: &str) -> Result<PyPath, SmError> {
        if name.is_empty() || name.contains('/') || name == "." {
            return Err(SmError::value(format!(
                "Invalid name {}",
                crate::py::str::repr(name)
            )));
        }
        let mut tail = self.tail();
        if tail.is_empty() {
            return Err(SmError::value(format!("{} has an empty name", self.repr())));
        }
        *tail.last_mut().expect("tail is not empty") = name;
        Ok(PyPath::from_parts(self.root(), &tail))
    }

    /// `repr(path)` of a concrete path: `PosixPath('...')`.
    pub fn repr(&self) -> String {
        format!("PosixPath({})", crate::py::str::repr(&self.s))
    }

    /// `path.is_relative_to(other)`.
    pub fn is_relative_to(&self, other: &PyPath) -> bool {
        other == self || self.parents().contains(other)
    }

    /// `path.relative_to(other)` (walk_up=False).
    pub fn relative_to(&self, other: &PyPath) -> Result<PyPath, SmError> {
        if !self.is_relative_to(other) {
            return Err(SmError::value(format!(
                "{} is not in the subpath of {}",
                crate::py::str::repr(&self.s),
                crate::py::str::repr(&other.s)
            )));
        }
        let tail = self.tail();
        Ok(PyPath::from_parts("", &tail[other.tail().len()..]))
    }

    /// `path.expanduser()`: a leading `~` or `~user` part becomes that home folder.
    pub fn expanduser(&self) -> Result<PyPath, SmError> {
        let tail = self.tail();
        if !self.root().is_empty() || tail.is_empty() || !tail[0].starts_with('~') {
            return Ok(self.clone());
        }
        let home = expanduser_str(tail[0])?;
        if home.starts_with('~') {
            return Err(SmError::runtime("Could not determine home directory."));
        }
        let base = PyPath::new(&home);
        let mut parts = base
            .tail()
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();
        parts.extend(tail[1..].iter().map(|p| p.to_string()));
        Ok(PyPath::from_parts(base.root(), &parts))
    }

    /// `Path.home()`.
    pub fn home() -> Result<PyPath, SmError> {
        let h = expanduser_str("~")?;
        if h == "~" {
            return Err(SmError::runtime("Could not determine home directory."));
        }
        Ok(PyPath::new(&h))
    }

    /// `Path.cwd()`.
    pub fn cwd() -> Result<PyPath, SmError> {
        getcwd().map(|s| PyPath::new(&s))
    }

    /// `path.resolve()`: `posixpath.realpath(path, strict=False)`.
    pub fn resolve(&self) -> Result<PyPath, SmError> {
        realpath(&self.s).map(|s| PyPath::new(&s))
    }

    /// `path.exists()` (follows links; any error is False).
    pub fn exists(&self) -> bool {
        std::fs::metadata(self.to_os()).is_ok()
    }

    pub fn is_file(&self) -> bool {
        std::fs::metadata(self.to_os()).is_ok_and(|m| m.is_file())
    }

    pub fn is_dir(&self) -> bool {
        std::fs::metadata(self.to_os()).is_ok_and(|m| m.is_dir())
    }

    /// `list(path.iterdir())` in directory order; an OSError names the path.
    pub fn iterdir(&self) -> Result<Vec<PyPath>, SmError> {
        let entries = scandir(&self.s).map_err(|e| SmError::from_io(&e, Some(&self.to_os())))?;
        Ok(entries
            .into_iter()
            .map(|(_, name)| {
                if self.s == "." {
                    PyPath::new(&name)
                } else {
                    self.join(&name)
                }
            })
            .collect())
    }

    /// `path.read_text(encoding="utf-8", errors=...)`: universal newlines (`\r\n` and `\r`
    /// become `\n`).
    pub fn read_text(&self, errors: Errors) -> Result<String, SmError> {
        let os = self.to_os();
        let bytes = std::fs::read(&os).map_err(|e| SmError::from_io(&e, Some(&os)))?;
        decode_text(&bytes, errors)
    }

    /// `path.mkdir(parents=..., exist_ok=...)`: an OSError names the folder that failed.
    pub fn mkdir(&self, parents: bool, exist_ok: bool) -> Result<(), SmError> {
        let os = self.to_os();
        match std::fs::create_dir(&os) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if !parents || self.parent() == *self {
                    return Err(SmError::from_io(&e, Some(&os)));
                }
                self.parent().mkdir(true, true)?;
                self.mkdir(false, exist_ok)
            }
            Err(e) => {
                if !exist_ok || !self.is_dir() {
                    return Err(SmError::from_io(&e, Some(&os)));
                }
                Ok(())
            }
        }
    }

    /// `list(path.glob(pattern))`, in the order Python yields.
    pub fn glob(&self, pattern: &str) -> Result<Vec<PyPath>, SmError> {
        let parts = parse_pattern(pattern)?;
        let root = &self.s;
        let start = if root.ends_with('/') {
            root.clone()
        } else {
            format!("{root}/")
        };
        let mut out = Vec::new();
        let sel = Selector::build(&parts);
        sel.select(&start, false, &mut out);
        Ok(out
            .into_iter()
            .map(|mut p| {
                if root == "." {
                    p = p.strip_prefix("./").map(String::from).unwrap_or(p);
                }
                PyPath::new(&p)
            })
            .collect())
    }

    /// `list(path.rglob(pattern))`: `glob("**/" + pattern)`.
    pub fn rglob(&self, pattern: &str) -> Result<Vec<PyPath>, SmError> {
        self.glob(&format!("**/{pattern}"))
    }
}

// ---------------------------------------------------------------- expanduser, getcwd, realpath

/// What `pwd.getpwuid` / `pwd.getpwnam` give for `pw_dir`: no such user (KeyError), a user whose
/// `pw_dir` is NULL (Python's `None`), or the directory.
#[derive(Debug, PartialEq)]
enum Passwd {
    Missing,
    NoDir,
    Dir(String),
}

fn pw_dir_of(pw: &libc::passwd) -> Passwd {
    if pw.pw_dir.is_null() {
        return Passwd::NoDir;
    }
    // SAFETY: a non-null pw_dir points at a NUL-terminated string in the caller's buffer.
    Passwd::Dir(decode_surrogateescape(
        unsafe { std::ffi::CStr::from_ptr(pw.pw_dir) }.to_bytes(),
    ))
}

/// `getpwuid_r` / `getpwnam_r` with the buffer doubled on ERANGE, as CPython's pwd module does;
/// the reentrant calls are safe on any thread. Any other error is an OSError.
fn passwd_dir(name: Option<&str>) -> Result<Passwd, SmError> {
    // SAFETY: sysconf only reads its argument.
    let size = usize::try_from(unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) })
        .ok()
        .filter(|&n| n > 0)
        .unwrap_or(1024);
    passwd_dir_sized(name, size)
}

fn passwd_dir_sized(name: Option<&str>, mut size: usize) -> Result<Passwd, SmError> {
    let c = match name {
        Some(n) => Some(
            std::ffi::CString::new(crate::py::io::fs_path(n).into_os_string().into_vec())
                .map_err(|_| SmError::value("embedded null byte"))?,
        ),
        None => None,
    };
    loop {
        // SAFETY: `pwd` is a zeroed passwd that getpw*_r fills; its strings point into `buf`.
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buf = vec![0u8; size];
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: `buf` holds `size` bytes and `found` is a live out pointer; `n` is NUL-terminated.
        let rc = unsafe {
            match &c {
                None => libc::getpwuid_r(
                    libc::getuid(),
                    &mut pwd,
                    buf.as_mut_ptr().cast(),
                    size,
                    &mut found,
                ),
                Some(n) => libc::getpwnam_r(
                    n.as_ptr(),
                    &mut pwd,
                    buf.as_mut_ptr().cast(),
                    size,
                    &mut found,
                ),
            }
        };
        match rc {
            0 if found.is_null() => return Ok(Passwd::Missing),
            0 => return Ok(pw_dir_of(&pwd)),
            libc::ERANGE if size < (1 << 20) => size *= 2,
            errno => return Err(SmError::os(errno, None, None)),
        }
    }
}

/// The home `expanduser` takes from a passwd lookup: None when there is no such user (the path
/// stays as it is); a NULL `pw_dir` is `None.rstrip` in posixpath.expanduser.
fn home_of(pw: Passwd) -> Result<Option<String>, SmError> {
    match pw {
        Passwd::Missing => Ok(None),
        Passwd::NoDir => Err(SmError::from_python(
            "AttributeError",
            "'NoneType' object has no attribute 'rstrip'",
        )),
        Passwd::Dir(d) => Ok(Some(d)),
    }
}

/// `os.path.expanduser(s)` of a str.
pub fn expanduser_str(path: &str) -> Result<String, SmError> {
    if !path.starts_with('~') {
        return Ok(path.to_string());
    }
    let i = path[1..].find('/').map_or(path.len(), |j| j + 1);
    let home = if i == 1 {
        match std::env::var_os("HOME") {
            Some(h) => decode_surrogateescape(h.as_bytes()),
            None => match home_of(passwd_dir(None)?)? {
                Some(h) => h,
                None => return Ok(path.to_string()),
            },
        }
    } else {
        match home_of(passwd_dir(Some(&path[1..i]))?)? {
            Some(h) => h,
            None => return Ok(path.to_string()),
        }
    };
    let out = format!("{}{}", home.trim_end_matches('/'), &path[i..]);
    Ok(if out.is_empty() { "/".into() } else { out })
}

/// `os.getcwd()`.
pub fn getcwd() -> Result<String, SmError> {
    std::env::current_dir()
        .map(|p| decode_surrogateescape(p.as_os_str().as_bytes()))
        .map_err(|e| SmError::from_io(&e, None))
}

enum Part {
    Name(String),
    /// A link whose target is pushed: record the resolved path for it once its parts are done.
    Done(String),
}

/// `posixpath.realpath(filename, strict=False)`: every OSError of lstat or readlink is ignored.
pub fn realpath(filename: &str) -> Result<String, SmError> {
    let trimmed = filename.trim_end_matches('/');
    let mut rest: Vec<Part> = trimmed
        .split('/')
        .rev()
        .map(|s| Part::Name(s.to_string()))
        .collect();
    let mut path = if filename.starts_with('/') {
        "/".to_string()
    } else {
        getcwd()?
    };
    let mut seen: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();
    while let Some(part) = rest.pop() {
        let name = match part {
            Part::Done(link) => {
                seen.insert(link, Some(path.clone()));
                continue;
            }
            Part::Name(n) => n,
        };
        if name.is_empty() || name == "." {
            continue;
        }
        if name == ".." {
            let cut = path.rfind('/').expect("an absolute path holds a slash");
            path.truncate(cut);
            if path.is_empty() {
                path.push('/');
            }
            continue;
        }
        let newpath = if path == "/" {
            format!("/{name}")
        } else {
            format!("{path}/{name}")
        };
        if newpath.contains('\0') {
            return Err(SmError::value("lstat: embedded null character in path"));
        }
        let os = crate::py::io::fs_path(&newpath);
        let Ok(meta) = std::fs::symlink_metadata(&os) else {
            path = newpath;
            continue;
        };
        if !meta.file_type().is_symlink() {
            path = newpath;
            continue;
        }
        if let Some(cached) = seen.get(&newpath) {
            match cached {
                Some(p) => path = p.clone(),
                None => path = newpath,
            }
            continue;
        }
        let Ok(target) = std::fs::read_link(&os) else {
            path = newpath;
            continue;
        };
        let target = decode_surrogateescape(target.as_os_str().as_bytes());
        if target.starts_with('/') {
            path = "/".into();
        }
        seen.insert(newpath.clone(), None);
        rest.push(Part::Done(newpath));
        rest.extend(target.split('/').rev().map(|s| Part::Name(s.to_string())));
    }
    Ok(path)
}

// ---------------------------------------------------------------- read_text

/// The `errors=` of a text read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Errors {
    Strict,
    Replace,
}

/// UTF-8 text as `TextIOWrapper(encoding="utf-8", errors=..., newline=None).read()` gives it.
pub fn decode_text(bytes: &[u8], errors: Errors) -> Result<String, SmError> {
    let text = match errors {
        Errors::Strict => std::str::from_utf8(bytes)
            .map_err(|e| SmError::unicode_decode(bytes, e))?
            .to_string(),
        Errors::Replace => String::from_utf8_lossy(bytes).into_owned(),
    };
    if !text.contains('\r') {
        return Ok(text);
    }
    Ok(text.replace("\r\n", "\n").replace('\r', "\n"))
}

/// `text.encode("utf-8")` as a text file's write does it: a stand-in (a surrogateescape byte) is
/// a lone surrogate to Python, so a run of them raises one UnicodeEncodeError naming the run's
/// code-point positions in `text`.
pub fn encode_text(text: &str) -> Result<Vec<u8>, SmError> {
    let stand_in = |c: char| crate::py::io::surrogate_byte(c).is_some();
    let Some(start) = text.chars().position(stand_in) else {
        return Ok(text.as_bytes().to_vec());
    };
    let run: Vec<char> = text
        .chars()
        .skip(start)
        .take_while(|&c| stand_in(c))
        .collect();
    let msg = if run.len() == 1 {
        format!(
            "'utf-8' codec can't encode character '\\udc{:02x}' in position {start}: surrogates not allowed",
            crate::py::io::surrogate_byte(run[0]).unwrap_or(0)
        )
    } else {
        format!(
            "'utf-8' codec can't encode characters in position {start}-{}: surrogates not allowed",
            start + run.len() - 1
        )
    };
    Err(SmError::new(
        crate::error::Base::Value,
        "UnicodeEncodeError",
        msg,
    ))
}

// ---------------------------------------------------------------- glob

/// `_parse_pattern`: relative only; `.` and empty parts dropped; a trailing `/` adds "".
fn parse_pattern(pattern: &str) -> Result<Vec<String>, SmError> {
    if pattern.starts_with('/') {
        return Err(SmError::new(
            crate::error::Base::Other,
            "NotImplementedError",
            "Non-relative patterns are unsupported",
        ));
    }
    let mut parts: Vec<String> = pattern
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .map(String::from)
        .collect();
    if parts.is_empty() {
        return Err(SmError::value(format!(
            "Unacceptable pattern: {}",
            crate::py::str::repr(pattern)
        )));
    }
    if pattern.ends_with('/') {
        parts.push(String::new());
    }
    Ok(parts)
}

fn is_magic(part: &str) -> bool {
    part.contains(['*', '?', '['])
}

/// `os.scandir(path)` as (entry, name) pairs; the whole listing fails on any error.
fn scandir(path: &str) -> std::io::Result<Vec<(std::fs::DirEntry, String)>> {
    std::fs::read_dir(crate::py::io::fs_path(path))?
        .map(|e| {
            e.map(|e| {
                let name = decode_surrogateescape(e.file_name().as_bytes());
                (e, name)
            })
        })
        .collect()
}

/// `DirEntry.is_dir(follow_symlinks=...)`; an error is False.
fn entry_is_dir(e: &std::fs::DirEntry, follow: bool) -> bool {
    match e.file_type() {
        Ok(t) if t.is_symlink() && follow => std::fs::metadata(e.path()).is_ok_and(|m| m.is_dir()),
        Ok(t) => t.is_dir(),
        Err(_) => false,
    }
}

/// `os.path.lexists`.
fn lexists(path: &str) -> bool {
    std::fs::symlink_metadata(crate::py::io::fs_path(path)).is_ok()
}

/// The selector chain `_GlobberBase.selector` builds (case-sensitive, `**` does not follow links).
enum Selector {
    Exists,
    Special(String, Box<Selector>),
    Literal(String, Box<Selector>),
    Wildcard(Option<Pattern>, bool, Box<Selector>),
    Recursive(bool, Box<Selector>),
}

impl Selector {
    /// `parts` in pattern order.
    fn build(parts: &[String]) -> Selector {
        let Some((part, rest)) = parts.split_first() else {
            return Selector::Exists;
        };
        if part == "**" {
            let mut rest = rest;
            while rest.first().is_some_and(|p| p == "**") {
                rest = &rest[1..];
            }
            return Selector::Recursive(!rest.is_empty(), Box::new(Selector::build(rest)));
        }
        if part.is_empty() || part == ".." {
            let text = if rest.is_empty() {
                part.clone()
            } else {
                format!("{part}/")
            };
            return Selector::Special(text, Box::new(Selector::build(rest)));
        }
        if !is_magic(part) {
            let mut text = part.clone();
            let mut rest = rest;
            while let Some(next) = rest.first().filter(|p| !is_magic(p)) {
                text = format!("{text}/{next}");
                rest = &rest[1..];
            }
            if !rest.is_empty() {
                text.push('/');
            }
            return Selector::Literal(text, Box::new(Selector::build(rest)));
        }
        let pat = if part == "*" {
            None
        } else {
            Some(Pattern::compile(part))
        };
        Selector::Wildcard(pat, !rest.is_empty(), Box::new(Selector::build(rest)))
    }

    fn select(&self, path: &str, exists: bool, out: &mut Vec<String>) {
        match self {
            Selector::Exists => {
                if exists || lexists(path) {
                    out.push(path.to_string());
                }
            }
            Selector::Special(text, next) => next.select(&format!("{path}{text}"), exists, out),
            Selector::Literal(text, next) => next.select(&format!("{path}{text}"), false, out),
            Selector::Wildcard(pat, dir_only, next) => {
                let Ok(entries) = scandir(path) else { return };
                for (entry, name) in entries {
                    if pat.as_ref().is_some_and(|p| !p.matches(&name)) {
                        continue;
                    }
                    let entry_path = format!("{path}{name}");
                    if *dir_only {
                        if entry_is_dir(&entry, true) {
                            next.select(&format!("{entry_path}/"), true, out);
                        }
                    } else {
                        out.push(entry_path);
                    }
                }
            }
            Selector::Recursive(dir_only, next) => {
                next.select(path, exists, out);
                let mut stack = vec![path.to_string()];
                while let Some(dir) = stack.pop() {
                    let Ok(entries) = scandir(&dir) else { continue };
                    for (entry, name) in entries {
                        let is_dir = entry_is_dir(&entry, false);
                        if !is_dir && *dir_only {
                            continue;
                        }
                        let mut entry_path = format!("{dir}{name}");
                        if *dir_only {
                            entry_path.push('/');
                            next.select(&entry_path, true, out);
                        } else {
                            out.push(entry_path.clone());
                        }
                        if is_dir {
                            if !*dir_only {
                                entry_path.push('/');
                            }
                            stack.push(entry_path);
                        }
                    }
                }
            }
        }
    }
}

/// One glob segment as `fnmatch._translate` reads it, matched against a whole name.
#[derive(Debug)]
pub struct Pattern {
    toks: Vec<Tok>,
}

#[derive(Debug)]
enum Tok {
    Star,
    Any,
    Lit(char),
    Set {
        negate: bool,
        items: Vec<(char, char)>,
    },
    Never,
}

impl Pattern {
    /// `fnmatch._translate(pat)` with `*` as `[^/]*` and `?` as `[^/]`.
    pub fn compile(pat: &str) -> Pattern {
        let p: Vec<char> = pat.chars().collect();
        let n = p.len();
        let mut toks = Vec::new();
        let mut i = 0;
        while i < n {
            let c = p[i];
            i += 1;
            match c {
                '*' => {
                    if !matches!(toks.last(), Some(Tok::Star)) {
                        toks.push(Tok::Star);
                    }
                }
                '?' => toks.push(Tok::Any),
                '[' => {
                    let mut j = i;
                    if j < n && p[j] == '!' {
                        j += 1;
                    }
                    if j < n && p[j] == ']' {
                        j += 1;
                    }
                    while j < n && p[j] != ']' {
                        j += 1;
                    }
                    if j >= n {
                        toks.push(Tok::Lit('['));
                        continue;
                    }
                    toks.push(set_token(&p, i, j));
                    i = j + 1;
                }
                c => toks.push(Tok::Lit(c)),
            }
        }
        Pattern { toks }
    }

    pub fn matches(&self, name: &str) -> bool {
        let s: Vec<char> = name.chars().collect();
        match_from(&self.toks, &s)
    }
}

/// The set `p[i..j]` (between `[` and `]`) as fnmatch builds it: chunks split on `-` ranges,
/// empty ranges dropped, `!` negates; an empty set never matches, `[!]` matches any char.
fn set_token(p: &[char], i0: usize, j: usize) -> Tok {
    let mut i = i0;
    let stuff: Vec<char> = p[i..j].to_vec();
    let mut chunks: Vec<Vec<char>> = Vec::new();
    if !stuff.contains(&'-') {
        chunks.push(stuff);
    } else {
        let mut k = if p[i] == '!' { i + 2 } else { i + 1 };
        loop {
            let found = (k..j).find(|&x| p[x] == '-');
            let Some(kk) = found else { break };
            chunks.push(p[i..kk].to_vec());
            i = kk + 1;
            k = kk + 3;
        }
        let chunk = p[i..j].to_vec();
        if !chunk.is_empty() {
            chunks.push(chunk);
        } else {
            chunks.last_mut().expect("a '-' made a chunk").push('-');
        }
        let mut k = chunks.len() - 1;
        while k > 0 {
            if chunks[k - 1].last() > chunks[k].first() {
                let tail: Vec<char> = chunks[k][1..].to_vec();
                chunks[k - 1].pop();
                chunks[k - 1].extend(tail);
                chunks.remove(k);
            }
            k -= 1;
        }
    }
    // `stuff` is the chunks joined by range hyphens; read it the way `re` reads the class.
    let joined: usize = chunks.iter().map(Vec::len).sum::<usize>() + chunks.len().saturating_sub(1);
    if joined == 0 {
        return Tok::Never;
    }
    let first = chunks[0].first().copied();
    if joined == 1 && first == Some('!') {
        return Tok::Any;
    }
    let negate = first == Some('!');
    // Items: each chunk's chars are literals; between chunk a and chunk b the last char of a and
    // the first of b form a range.
    let mut lits: Vec<(char, char)> = Vec::new();
    let mut prev_range_end: Option<char> = None;
    for (chunk_i, chunk) in chunks.iter().enumerate() {
        let mut chars: &[char] = chunk;
        if chunk_i == 0 && negate {
            chars = &chars[1..];
        }
        if let Some(lo) = prev_range_end.take() {
            // The range lo-hi uses this chunk's first char.
            match chars.split_first() {
                Some((&hi, rest)) => {
                    lits.push((lo, hi));
                    chars = rest;
                }
                None => lits.push((lo, lo)),
            }
        }
        let last_chunk = chunk_i + 1 == chunks.len();
        match chars.split_last() {
            Some((&last, body)) if !last_chunk => {
                lits.extend(body.iter().map(|&c| (c, c)));
                prev_range_end = Some(last);
            }
            _ => lits.extend(chars.iter().map(|&c| (c, c))),
        }
    }
    Tok::Set {
        negate,
        items: lits,
    }
}

fn tok_matches(t: &Tok, c: char) -> bool {
    match t {
        Tok::Any => c != '/',
        Tok::Lit(l) => *l == c,
        Tok::Set { negate, items } => items.iter().any(|&(lo, hi)| lo <= c && c <= hi) != *negate,
        Tok::Never | Tok::Star => false,
    }
}

fn match_from(toks: &[Tok], s: &[char]) -> bool {
    match toks.split_first() {
        None => s.is_empty(),
        Some((Tok::Star, rest)) => (0..=s.len())
            .take_while(|&k| k == 0 || s[k - 1] != '/')
            .any(|k| match_from(rest, &s[k..])),
        Some((t, rest)) => {
            s.first().is_some_and(|&c| tok_matches(t, c)) && match_from(rest, &s[1..])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pp(s: &str) -> PyPath {
        PyPath::new(s)
    }

    #[test]
    fn pure_parts_match_pathlib() {
        // Values from Python 3.15.1 PurePosixPath (probe in the lane report).
        assert_eq!(pp("///a//b/./c/").as_str(), "/a/b/c");
        assert_eq!(pp("//a/..").parts(), ["//", "a", ".."]);
        assert_eq!(pp("").as_str(), ".");
        // Moved from core/refs.rs (its private pathlib subset is gone).
        assert_eq!(pp("a//b/./c/").as_str(), "a/b/c");
        assert_eq!(pp("//x").as_str(), "//x");
        assert_eq!(pp("///x").as_str(), "/x");
        assert_eq!(pp("a/../b").as_str(), "a/../b");
        assert_eq!(pp("./a").as_str(), "a");
        assert_eq!(pp("a/../b").parent().as_str(), "a/..");
        for (s, name, stem, suffix) in [
            ("a.", "a.", "a", "."),
            ("a..", "a..", "a.", "."),
            (".a", ".a", ".a", ""),
            ("..", "..", "..", ""),
            ("...", "...", "...", ""),
            (".a.b", ".a.b", ".a", ".b"),
            ("a.tar.gz", "a.tar.gz", "a.tar", ".gz"),
            ("/", "", "", ""),
        ] {
            assert_eq!(
                (pp(s).name(), pp(s).stem(), pp(s).suffix()),
                (name, stem, suffix),
                "{s}"
            );
        }
        let parents: Vec<String> = pp("/a/b/c").parents().into_iter().map(|p| p.s).collect();
        assert_eq!(parents, ["/a/b", "/a", "/"]);
        assert!(pp(".").parents().is_empty());
        assert_eq!(pp("a").join("/x").as_str(), "/x");
        assert_eq!(pp(".").join("x").as_str(), "x");
        assert_eq!(pp("/").join("x").as_str(), "/x");
        assert_eq!(pp("//").join("x").as_str(), "//x");
    }

    #[test]
    fn relative_and_with_name() {
        assert!(pp("/h/u/x").is_relative_to(&pp("/h/u")));
        assert!(!pp("/h/ux").is_relative_to(&pp("/h/u")));
        assert_eq!(
            pp("/h/u/x/y").relative_to(&pp("/h/u")).unwrap().as_str(),
            "x/y"
        );
        assert_eq!(pp("/h/u").relative_to(&pp("/h/u")).unwrap().as_str(), ".");
        assert_eq!(
            pp("/a").relative_to(&pp("b")).unwrap_err().python_text(),
            "ValueError: '/a' is not in the subpath of 'b'"
        );
        assert_eq!(
            pp("/a/b.log").with_name("b.log.tmp").unwrap().as_str(),
            "/a/b.log.tmp"
        );
        assert_eq!(
            pp("/").with_name("x").unwrap_err().msg,
            "PosixPath('/') has an empty name"
        );
        assert_eq!(
            pp("a").with_name("x/y").unwrap_err().msg,
            "Invalid name 'x/y'"
        );
    }

    #[test]
    fn a_passwd_entry_with_a_null_pw_dir_is_python_none() {
        // Python 3.15.1, pwd.getpwuid/getpwnam patched to give pw_dir None, HOME unset:
        // posixpath.expanduser("~") and ("~someone/x") raise
        // AttributeError: 'NoneType' object has no attribute 'rstrip'.
        let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
        assert_eq!(pw_dir_of(&pw), Passwd::NoDir);
        let dir = std::ffi::CString::new("/h/\u{e9}").unwrap();
        pw.pw_dir = dir.as_ptr().cast_mut();
        assert_eq!(pw_dir_of(&pw), Passwd::Dir("/h/\u{e9}".into()));
        assert_eq!(
            home_of(Passwd::NoDir).unwrap_err().python_text(),
            "AttributeError: 'NoneType' object has no attribute 'rstrip'"
        );
        assert_eq!(home_of(Passwd::Missing).unwrap(), None);
        assert_eq!(
            home_of(Passwd::Dir("/h".into())).unwrap(),
            Some("/h".to_string())
        );
    }

    #[test]
    fn passwd_lookups_work_from_many_threads_at_once() {
        assert_eq!(
            passwd_dir(Some("nonexistent_user_zz")).unwrap(),
            Passwd::Missing
        );
        let one = passwd_dir(Some("root")).unwrap();
        let one_dir = |p: &Passwd| match p {
            Passwd::Dir(d) if !d.is_empty() => d.clone(),
            other => panic!("{other:?}"),
        };
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let want = one_dir(&one);
                std::thread::spawn(move || {
                    (0..300).all(|_| {
                        passwd_dir(Some("root")).is_ok_and(|p| p == Passwd::Dir(want.clone()))
                    })
                })
            })
            .collect();
        assert!(threads.into_iter().all(|t| t.join().unwrap()));
    }

    #[test]
    fn a_buffer_too_small_for_the_entry_is_doubled_until_it_fits() {
        // ERANGE from getpwnam_r on a 1-byte buffer.
        assert_eq!(
            passwd_dir_sized(Some("root"), 1).unwrap(),
            passwd_dir(Some("root")).unwrap()
        );
    }

    #[test]
    fn expanduser_forms() {
        let home = expanduser_str("~").unwrap();
        assert_eq!(
            pp("~/x").expanduser().unwrap().as_str(),
            format!("{}/x", home.trim_end_matches('/'))
        );
        assert_eq!(
            pp("~nonexistent_user_zz/x")
                .expanduser()
                .unwrap_err()
                .python_text(),
            "RuntimeError: Could not determine home directory."
        );
        // pwd.getpwnam("root").pw_dir: /var/root on macOS, /root on Linux
        let root_home = if cfg!(target_os = "macos") {
            "/var/root"
        } else {
            "/root"
        };
        assert_eq!(pp("~root").expanduser().unwrap().as_str(), root_home);
        assert_eq!(pp("/~/x").expanduser().unwrap().as_str(), "/~/x");
        assert_eq!(
            expanduser_str("~a\0b").unwrap_err().python_text(),
            "ValueError: embedded null byte"
        );
    }

    #[test]
    fn realpath_walks_links_without_changing_case() {
        let d = tempfile::tempdir().unwrap();
        let base = realpath(&d.path().to_string_lossy()).unwrap();
        std::fs::create_dir(d.path().join("Real")).unwrap();
        std::os::unix::fs::symlink("Real", d.path().join("ln")).unwrap();
        std::os::unix::fs::symlink("loop", d.path().join("loop")).unwrap();
        std::os::unix::fs::symlink("/nonexistent-sm/zz", d.path().join("abs")).unwrap();
        assert_eq!(
            realpath(&format!("{base}/ln/x/../y")).unwrap(),
            format!("{base}/Real/y")
        );
        // APFS is case-insensitive: lstat finds "real", and the case given stays (convention 8).
        assert_eq!(
            realpath(&format!("{base}/real")).unwrap(),
            format!("{base}/real")
        );
        assert_eq!(
            realpath(&format!("{base}/loop/a")).unwrap(),
            format!("{base}/loop/a")
        );
        assert_eq!(
            realpath(&format!("{base}/abs/q")).unwrap(),
            "/nonexistent-sm/zz/q"
        );
        assert_eq!(realpath("/..").unwrap(), "/");
        assert_eq!(realpath("//a").unwrap(), "/a");
    }

    #[test]
    fn glob_and_rglob_order() {
        let d = tempfile::tempdir().unwrap();
        let root = PyPath::from_os(d.path());
        for f in [
            "a/memory/x.md",
            "a/memory/MEMORY.md",
            "a/b.jsonl",
            "c/sub/d.yaml",
            "e.yaml",
            ".h/q.yaml",
        ] {
            let p = d.path().join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "x").unwrap();
        }
        let rel = |v: Vec<PyPath>| -> Vec<String> {
            let mut r: Vec<String> = v
                .into_iter()
                .map(|p| p.relative_to(&root).unwrap().s)
                .collect();
            r.sort();
            r
        };
        assert_eq!(
            rel(root.glob("*/memory/*.md").unwrap()),
            ["a/memory/MEMORY.md", "a/memory/x.md"]
        );
        assert_eq!(rel(root.glob("*/*.jsonl").unwrap()), ["a/b.jsonl"]);
        assert_eq!(
            rel(root.rglob("*.yaml").unwrap()),
            [".h/q.yaml", "c/sub/d.yaml", "e.yaml"]
        );
        assert_eq!(rel(root.glob("*/sub").unwrap()), ["c/sub"]);
        assert!(
            root.glob("")
                .unwrap_err()
                .python_text()
                .starts_with("ValueError: Unacceptable pattern: ''")
        );
        // Depth-first walk: the root's own matches come before any child's.
        assert_eq!(root.rglob("*.yaml").unwrap()[0].name(), "e.yaml");
    }

    #[test]
    fn fnmatch_sets() {
        let m = |p: &str, s: &str| Pattern::compile(p).matches(s);
        assert!(m("*.md", ".x.md"));
        assert!(!m("*.md", "a.mdx"));
        assert!(m("a?c", "abc"));
        assert!(m("[a-c]x", "bx"));
        assert!(!m("[!a-c]x", "bx"));
        // `[!]` has no closing bracket after the `!]`, so it is three literals (Python 3.15.1 probe).
        assert!(!m("[!]", "z"));
        assert!(m("[!]", "[!]"));
        assert!(m("[", "["));
        assert!(m("[]]", "]"));
        assert!(!m("[z-a]", "m"));
        assert!(m("[a-]", "-"));
    }

    #[test]
    fn read_text_newlines_and_errors() {
        assert_eq!(
            decode_text(b"a\r\nb\rc\n", Errors::Strict).unwrap(),
            "a\nb\nc\n"
        );
        assert_eq!(
            decode_text(b"\xef\xbb\xbfx", Errors::Strict).unwrap(),
            "\u{feff}x"
        );
        assert_eq!(
            decode_text(b"ab\xffc", Errors::Strict)
                .unwrap_err()
                .python_text(),
            "UnicodeDecodeError: 'utf-8' codec can't decode byte 0xff in position 2: invalid start byte"
        );
        assert_eq!(
            decode_text(b"a\xe2\x82\r\n", Errors::Replace).unwrap(),
            "a\u{fffd}\n"
        );
        let d = tempfile::tempdir().unwrap();
        let p = PyPath::from_os(d.path());
        assert_eq!(
            p.read_text(Errors::Strict).unwrap_err().python_text(),
            format!("IsADirectoryError: [Errno 21] Is a directory: '{p}'")
        );
        let missing = p.join("nope");
        assert_eq!(
            missing.read_text(Errors::Strict).unwrap_err().python_text(),
            format!("FileNotFoundError: [Errno 2] No such file or directory: '{missing}'")
        );
        assert!(!missing.exists() && !missing.is_file() && p.is_dir() && !p.is_file());
    }
}
