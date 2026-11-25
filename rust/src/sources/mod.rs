pub mod automemory;
pub mod handoff;
pub mod proposal;
pub mod tasklog;
pub mod transcript;

use std::borrow::Cow;
use std::io::BufRead;
use std::os::unix::fs::MetadataExt;

#[cfg(test)]
use serde_json::Value;

use crate::core::item::Item;
use crate::error::{Base, SmError};
use crate::py::path::PyPath;
use crate::py::value::{PyValue, type_name};

/// An error raised while reading, parsing or converting the source file `path`, for an import of
/// a path the user named: one line that names the file, in a class the CLI prints as
/// `error: <line>` (a class it would print as a traceback becomes a ValueError).
pub(crate) fn file_error(path: &str, e: SmError) -> SmError {
    let printed = matches!(
        e.base,
        Base::Value | Base::Key | Base::Type | Base::Os | Base::Sqlite
    );
    if !printed {
        return SmError::value(format!("{path}: {}: {}", e.class, e.msg));
    }
    if e.msg.contains(path) {
        return e;
    }
    SmError {
        msg: format!("{path}: {}", e.msg),
        ..e
    }
}

/// The files of a folder scan with their items, and `<path>: <reason>` for each file the parser
/// refused.
pub type Scan = (Vec<SourceFile>, Vec<String>);

/// `parse` on each file of a folder: an error raised while reading, parsing or converting one
/// file skips that file, never the rest of the folder.
pub(crate) fn parse_each(
    files: Vec<PyPath>,
    parse: impl Fn(&PyPath) -> Result<Item, SmError>,
) -> Scan {
    let (mut parsed, mut skipped) = (Vec::new(), Vec::new());
    for f in files {
        match parse(&f) {
            Ok(item) => parsed.push((f, item)),
            Err(e) => {
                let msg = crate::py::str::split(&file_error(f.as_str(), e).msg).join(" ");
                skipped.push(if msg.starts_with(f.as_str()) {
                    msg
                } else {
                    format!("{f}: {msg}")
                });
            }
        }
    }
    (parsed, skipped)
}

/// `path.stat().st_mtime` (follows links): CPython's `sec + nsec * 1e-9`.
pub(crate) fn mtime(p: &PyPath) -> Result<f64, SmError> {
    let os = p.to_os();
    let md = std::fs::metadata(&os).map_err(|e| SmError::from_io(&e, Some(&os)))?;
    Ok(md.mtime() as f64 + md.mtime_nsec() as f64 * 1e-9)
}

/// A task log or handoff file of a scan and the item it gives.
pub type SourceFile = (PyPath, Item);

/// `sorted(paths)`: Python orders paths by `str(path).split("/")`.
pub(crate) fn sorted_paths(mut paths: Vec<PyPath>) -> Vec<PyPath> {
    paths.sort_by(|a, b| a.as_str().split('/').cmp(b.as_str().split('/')));
    paths
}

/// A file read line by line, streamed, never read whole (transcripts reach 580 MB).
pub(crate) struct TextLines {
    reader: std::io::BufReader<std::fs::File>,
}

impl TextLines {
    /// `open()`: an OSError names the path; a folder is IsADirectoryError, as Python checks at
    /// open time.
    pub(crate) fn open(p: &PyPath) -> Result<TextLines, SmError> {
        let os = p.to_os();
        let file = std::fs::File::open(&os).map_err(|e| SmError::from_io(&e, Some(&os)))?;
        if file.metadata().is_ok_and(|m| m.is_dir()) {
            return Err(SmError::is_a_directory(&os));
        }
        Ok(TextLines {
            reader: std::io::BufReader::new(file),
        })
    }

    /// The next line of `open(path, encoding="utf-8", errors="replace")`, `\n` included when it
    /// had one; None at the end. Lines end at `\n`, `\r\n` or `\r`, each read as `\n`
    /// (universal newlines), and bad bytes become U+FFFD.
    pub(crate) fn next_line(&mut self) -> Result<Option<String>, SmError> {
        let io = |e: std::io::Error| SmError::from_io(&e, None);
        let mut line: Vec<u8> = Vec::new();
        loop {
            let buf = self.reader.fill_buf().map_err(io)?;
            if buf.is_empty() {
                break;
            }
            match buf.iter().position(|&b| b == b'\n' || b == b'\r') {
                None => {
                    line.extend_from_slice(buf);
                    let n = buf.len();
                    self.reader.consume(n);
                }
                Some(i) => {
                    let cr = buf[i] == b'\r';
                    line.extend_from_slice(&buf[..i]);
                    line.push(b'\n');
                    self.reader.consume(i + 1);
                    if cr && self.reader.fill_buf().map_err(io)?.first() == Some(&b'\n') {
                        self.reader.consume(1);
                    }
                    return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
                }
            }
        }
        Ok(if line.is_empty() {
            None
        } else {
            Some(String::from_utf8_lossy(&line).into_owned())
        })
    }

    /// The next line of `for raw in path.open("rb")`: lines end at `\n` only, which they keep.
    pub(crate) fn next_bytes(&mut self) -> Result<Option<Vec<u8>>, SmError> {
        let mut line = Vec::new();
        let n = self
            .reader
            .read_until(b'\n', &mut line)
            .map_err(|e| SmError::from_io(&e, None))?;
        Ok((n > 0).then_some(line))
    }
}

/// Python `bool(value)`.
pub(crate) fn truthy(v: &PyValue) -> bool {
    match v {
        PyValue::None => false,
        PyValue::Bool(b) => *b,
        PyValue::Int(s) => s != "0",
        PyValue::Float(x) => *x != 0.0,
        PyValue::Str(s) => !s.is_empty(),
        PyValue::Bytes(b) => !b.is_empty(),
        PyValue::List(v) | PyValue::Tuple(v) | PyValue::Set(v) => !v.is_empty(),
        PyValue::Dict(d) => !d.is_empty(),
        PyValue::Date(_) | PyValue::DateTime(_) | PyValue::Time(_) | PyValue::Cycle(_) => true,
    }
}

/// `for x in value`: the items Python's iteration gives, or its TypeError.
/// Items are borrowed, never cloned: a deep clone of a list nested thousands deep would
/// overflow the stack.
pub(crate) fn iterate(v: &PyValue) -> Result<Vec<Cow<'_, PyValue>>, SmError> {
    let not_iterable = |name: &str| Err(SmError::type_(format!("'{name}' object is not iterable")));
    match v {
        PyValue::List(v) | PyValue::Tuple(v) | PyValue::Set(v) => {
            Ok(v.iter().map(Cow::Borrowed).collect())
        }
        PyValue::Dict(d) => Ok(d.iter().map(|(k, _)| Cow::Borrowed(k)).collect()),
        PyValue::Str(s) => Ok(s
            .chars()
            .map(|c| Cow::Owned(PyValue::Str(c.to_string())))
            .collect()),
        PyValue::Bytes(b) => Ok(b
            .iter()
            .map(|&x| Cow::Owned(PyValue::int(i64::from(x))))
            .collect()),
        PyValue::None => not_iterable("NoneType"),
        PyValue::Bool(_) => not_iterable("bool"),
        PyValue::Int(_) => not_iterable("int"),
        PyValue::Float(_) => not_iterable("float"),
        PyValue::Date(_) => not_iterable("datetime.date"),
        PyValue::DateTime(_) => not_iterable("datetime.datetime"),
        PyValue::Time(_) => not_iterable("datetime.time"),
        PyValue::Cycle(_) => not_iterable("list"),
    }
}

/// `data.get(key)` on a str key.
pub(crate) fn dict_get<'a>(data: &'a [(PyValue, PyValue)], key: &str) -> Option<&'a PyValue> {
    data.iter()
        .find(|(k, _)| matches!(k, PyValue::Str(s) if s == key))
        .map(|(_, v)| v)
}

/// A Python str.
pub(crate) fn s(v: &str) -> PyValue {
    PyValue::Str(v.into())
}

/// A str-keyed dict.
pub(crate) fn dict(pairs: Vec<(&str, PyValue)>) -> PyValue {
    PyValue::Dict(pairs.into_iter().map(|(k, v)| (s(k), v)).collect())
}

/// `value.get(key)` on a str key; None when `value` is not a dict.
pub(crate) fn value_get<'a>(v: &'a PyValue, key: &str) -> Option<&'a PyValue> {
    match v {
        PyValue::Dict(d) => dict_get(d, key),
        _ => None,
    }
}

/// `value.get(key)` where `value` must be a dict (`(row.get("message") or {}).get(...)`):
/// any other type raises the AttributeError Python gives.
pub(crate) fn get_attr<'a>(v: &'a PyValue, key: &str) -> Result<Option<&'a PyValue>, SmError> {
    match v {
        PyValue::Dict(d) => Ok(dict_get(d, key)),
        other => {
            let tp = match other {
                PyValue::Date(_) => "datetime.date",
                PyValue::DateTime(_) => "datetime.datetime",
                _ => type_name(other),
            };
            Err(SmError::from_python(
                "AttributeError",
                format!("'{tp}' object has no attribute 'get'"),
            ))
        }
    }
}

/// A Python value in structured parity encoding; a dict key that is not a str has no form.
#[cfg(test)]
pub(crate) fn protocol(v: &PyValue) -> Result<Value, SmError> {
    use crate::parity::{bytes_value, date_value, datetime_value, float_value};
    Ok(match v {
        PyValue::None => Value::Null,
        PyValue::Bool(b) => Value::Bool(*b),
        PyValue::Int(s) => serde_json::from_str(s)
            .map_err(|_| SmError::type_("parity: an int beyond the protocol"))?,
        PyValue::Float(x) => float_value(*x),
        PyValue::Str(s) => Value::from(s.clone()),
        PyValue::Bytes(b) => bytes_value(b),
        PyValue::Date(d) => date_value(d.isoformat()),
        PyValue::DateTime(dt) => datetime_value(dt.isoformat('T')),
        PyValue::Time(_) => return Err(SmError::type_("no protocol form for time")),
        PyValue::List(x) | PyValue::Tuple(x) => {
            Value::Array(x.iter().map(protocol).collect::<Result<_, _>>()?)
        }
        PyValue::Dict(d) => {
            let mut m = serde_json::Map::new();
            for (k, x) in d {
                let PyValue::Str(k) = k else {
                    return Err(SmError::type_(format!(
                        "dict key {} is not a str; the protocol has no form for it",
                        k.py_repr()
                    )));
                };
                m.insert(k.clone(), protocol(x)?);
            }
            Value::Object(m)
        }
        PyValue::Set(_) | PyValue::Cycle(_) => {
            return Err(SmError::type_("parity: no protocol form for this value"));
        }
    })
}

/// A temp root with no symlink in its path, so a test's resolved paths are its literal ones:
/// on macOS `/tmp` links to `/private/tmp`; on Linux `/tmp` is a plain folder.
#[cfg(test)]
pub(crate) const TEST_TMP: &str = if cfg!(target_os = "macos") {
    "/private/tmp"
} else {
    "/tmp"
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_sort_by_parts() {
        let got = sorted_paths(
            ["/b/a-b/x", "/b/a/x", "/b/a", "/a"]
                .map(PyPath::new)
                .to_vec(),
        );
        let got: Vec<&str> = got.iter().map(PyPath::as_str).collect();
        assert_eq!(got, ["/a", "/b/a", "/b/a/x", "/b/a-b/x"]);
    }

    #[test]
    fn get_on_a_non_dict_is_attribute_error() {
        let e = get_attr(&PyValue::Str("x".into()), "content").unwrap_err();
        assert_eq!(
            e.python_text(),
            "AttributeError: 'str' object has no attribute 'get'"
        );
        let d = PyValue::Dict(vec![(PyValue::Str("a".into()), PyValue::int(1))]);
        assert_eq!(get_attr(&d, "a").unwrap(), Some(&PyValue::int(1)));
        assert_eq!(get_attr(&d, "b").unwrap(), None);
    }

    #[test]
    fn time_values_act_as_python_datetime_time() {
        // Python 3.15.1: bool(time(1, 2)) is True, type name `time`, iter() raises
        let t = PyValue::Time(crate::py::value::Time {
            hour: 1,
            minute: 2,
            second: 0,
            microsecond: 0,
        });
        assert!(truthy(&t));
        assert_eq!(type_name(&t), "time");
        assert_eq!(
            iterate(&t).unwrap_err().python_text(),
            "TypeError: 'datetime.time' object is not iterable"
        );
        assert_eq!(
            protocol(&t).unwrap_err().python_text(),
            "TypeError: no protocol form for time"
        );
    }

    #[test]
    fn text_lines_read_crlf_and_cr_as_one_newline() {
        let d = tempfile::tempdir_in(crate::sources::TEST_TMP).unwrap();
        let p = d.path().join("t.txt");
        std::fs::write(&p, b"a\r\nb\rc\n\r\nd").unwrap();
        let mut lines = TextLines::open(&PyPath::new(p.to_str().unwrap())).unwrap();
        let mut got = Vec::new();
        while let Some(l) = lines.next_line().unwrap() {
            got.push(l);
        }
        // Python: open(p).readlines() == ['a\n', 'b\n', 'c\n', '\n', 'd']
        assert_eq!(got, ["a\n", "b\n", "c\n", "\n", "d"]);
    }

    #[test]
    fn text_lines_read_bytes_split_at_lf_only() {
        let d = tempfile::tempdir_in(crate::sources::TEST_TMP).unwrap();
        let p = d.path().join("t.bin");
        std::fs::write(&p, b"a\r\nb\rc\n\n\xffd").unwrap();
        let mut lines = TextLines::open(&PyPath::new(p.to_str().unwrap())).unwrap();
        let mut got: Vec<Vec<u8>> = Vec::new();
        while let Some(l) = lines.next_bytes().unwrap() {
            got.push(l);
        }
        // Python: list(open(p, "rb")) == [b'a\r\n', b'b\rc\n', b'\n', b'\xffd']
        assert_eq!(got, [&b"a\r\n"[..], b"b\rc\n", b"\n", b"\xffd"]);
        let dir = TextLines::open(&PyPath::new(d.path().to_str().unwrap()));
        assert_eq!(dir.err().unwrap().class, "IsADirectoryError");
    }
}
