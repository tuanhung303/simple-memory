//! The `sm __parity` internal test protocol.
//!
//! Stdin carries one JSON object per line, `{"fn": name, "args": [...], "kwargs": {...}}`; stdout
//! carries one line per input, in order: `{"ok": value}` or `{"err": "<Class>: <str(exc)>"}`.
//! Values encode special types structurally: a dataclass is `{"__type": name, fields in order}`,
//! a set is `{"__set": sorted}`, nan and inf are `{"__float": ...}`, and bytes are `{"__bytes": hex}`.

use serde_json::{Map, Value};

use crate::error::SmError;

pub struct Call {
    pub args: Vec<Value>,
    pub kwargs: Map<String, Value>,
}

pub type Reply = Result<Value, SmError>;

type Entry = fn(&str, &Call) -> Option<Reply>;

/// Modules registered for internal structural test calls.
const ENTRIES: &[Entry] = &[
    crate::clock::parity,
    crate::core::item::parity,
    crate::py::str::parity,
];

/// The reply of the first module that owns `f`, or None when no module does.
pub fn dispatch(f: &str, call: &Call) -> Option<Reply> {
    ENTRIES.iter().find_map(|entry| entry(f, call))
}

/// Replace surrogate-escape escapes `\udc80`-`\udcff` with internal private-use stand-in chars.
/// Other lone surrogates remain escapes and raise `ProtocolError`.
fn stand_ins_in(line: &str) -> String {
    let hex4 = |s: &str| {
        (s.len() >= 4 && s.is_char_boundary(4))
            .then(|| u32::from_str_radix(&s[..4], 16).ok())
            .flatten()
    };
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(i) = rest.find('\\') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let esc = rest.get(1..2).unwrap_or("");
        if esc != "u" {
            let n = 1 + esc.len();
            out.push_str(&rest[..n]);
            rest = &rest[n..];
            continue;
        }
        let unit = hex4(&rest[2..]);
        let next_low = rest.get(6..8) == Some("\\u")
            && hex4(&rest[8..]).is_some_and(|u| (0xDC00..0xE000).contains(&u));
        match unit {
            Some(0xD800..0xDC00) if next_low => {
                out.push_str(&rest[..12]);
                rest = &rest[12..];
            }
            Some(u @ 0xDC80..0xDD00) => {
                out.push(
                    char::from_u32(crate::py::str::STAND_IN_BASE + (u - 0xDC00))
                        .expect("a plane 16 code point"),
                );
                rest = &rest[6..];
            }
            _ => {
                out.push_str(&rest[..2]);
                rest = &rest[2..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The reply line with each stand-in char written as the `\udcXX` escape Python's json gives
/// the lone surrogate.
fn stand_ins_out(line: String) -> String {
    if !line
        .chars()
        .any(|c| crate::py::str::surrogate_byte(c).is_some())
    {
        return line;
    }
    let mut out = String::with_capacity(line.len());
    for c in line.chars() {
        match crate::py::str::surrogate_byte(c) {
            Some(b) => out.push_str(&format!("\\udc{b:02x}")),
            None => out.push(c),
        }
    }
    out
}

/// One protocol line in, one reply line out.
pub fn reply_line(line: &str) -> String {
    let reply = match serde_json::from_str::<Value>(&stand_ins_in(line)) {
        Ok(Value::Object(mut obj)) => {
            let f = obj
                .get("fn")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let args = match obj.remove("args") {
                Some(Value::Array(a)) => a,
                _ => Vec::new(),
            };
            let kwargs = match obj.remove("kwargs") {
                Some(Value::Object(m)) => m,
                _ => Map::new(),
            };
            let call = Call { args, kwargs };
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dispatch(&f, &call))) {
                Ok(Some(r)) => r,
                Ok(None) => Err(SmError::new(
                    crate::error::Base::Other,
                    "LookupError",
                    format!("no parity entry for {f}"),
                )),
                Err(p) => {
                    let text = p
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_default();
                    Err(SmError::new(crate::error::Base::Other, "RustPanic", text))
                }
            }
        }
        _ => Err(SmError::new(
            crate::error::Base::Other,
            "ProtocolError",
            "a case line is not a JSON object",
        )),
    };
    let mut out = Map::new();
    match reply {
        Ok(v) => out.insert("ok".into(), v),
        Err(e) => out.insert("err".into(), Value::from(e.python_text())),
    };
    stand_ins_out(Value::Object(out).to_string())
}

/// The JSON-lines loop; returns the exit code.
#[cfg(feature = "parity")]
pub fn serve() -> i32 {
    use std::io::{BufRead, Write};
    std::panic::set_hook(Box::new(|_| {}));
    let stdin = std::io::stdin();
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    for line in stdin.lock().lines() {
        let Ok(line) = line else { return 2 };
        if line.trim().is_empty() {
            continue;
        }
        if writeln!(out, "{}", reply_line(&line)).is_err() {
            return 2;
        }
    }
    if out.flush().is_err() { 2 } else { 0 }
}

// ---------------------------------------------------------------- value encoding

#[cfg(test)]
impl Call {
    /// Argument `i`, or keyword `name` when fewer positional arguments came.
    pub fn arg(&self, i: usize, name: &str) -> Option<&Value> {
        self.args.get(i).or_else(|| self.kwargs.get(name))
    }

    /// A str argument; None and a missing argument are both None.
    pub fn opt_str(&self, i: usize, name: &str) -> Result<Option<&str>, SmError> {
        match self.arg(i, name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s)),
            Some(v) => Err(SmError::type_(format!(
                "parity: {name} must be a str or None, got {v}"
            ))),
        }
    }

    pub fn str(&self, i: usize, name: &str) -> Result<&str, SmError> {
        self.opt_str(i, name)?
            .ok_or_else(|| SmError::type_(format!("parity: {name} is required")))
    }

    pub fn opt_bool(&self, i: usize, name: &str) -> Result<Option<bool>, SmError> {
        match self.arg(i, name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(v) => Err(SmError::type_(format!(
                "parity: {name} must be a bool, got {v}"
            ))),
        }
    }
}

/// A float as encoded in the parity protocol: nan and inf as `{"__float": ...}`.
#[cfg(test)]
pub fn float_value(x: f64) -> Value {
    if x.is_nan() {
        tagged("__float", Value::from("nan"))
    } else if x.is_infinite() {
        tagged("__float", Value::from(if x > 0.0 { "inf" } else { "-inf" }))
    } else {
        Value::from(x)
    }
}

/// A float argument: a JSON number (int or float) or `{"__float": ...}`.
#[cfg(test)]
pub fn value_float(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Object(m) if m.len() == 1 => match m.get("__float").and_then(|s| s.as_str()) {
            Some("nan") => Some(f64::NAN),
            Some("inf") => Some(f64::INFINITY),
            Some("-inf") => Some(f64::NEG_INFINITY),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
pub fn bytes_value(b: &[u8]) -> Value {
    tagged(
        "__bytes",
        Value::from(b.iter().map(|x| format!("{x:02x}")).collect::<String>()),
    )
}

#[cfg(test)]
pub fn date_value(iso: String) -> Value {
    tagged("__date", Value::from(iso))
}

#[cfg(test)]
pub fn datetime_value(iso: String) -> Value {
    tagged("__datetime", Value::from(iso))
}

/// A set of strs, sorted in Python's (code point) order.
#[cfg(test)]
pub fn str_set_value<I: IntoIterator<Item = String>>(items: I) -> Value {
    let mut v: Vec<String> = items.into_iter().collect();
    v.sort();
    v.dedup();
    tagged("__set", Value::from(v))
}

#[cfg(test)]
fn tagged(key: &str, v: Value) -> Value {
    let mut m = Map::new();
    m.insert(key.into(), v);
    Value::Object(m)
}

/// A dataclass value: `__type` first, then the fields in field order.
#[cfg(test)]
pub fn dataclass_value(name: &str, fields: Vec<(&str, Value)>) -> Value {
    let mut m = Map::new();
    m.insert("__type".into(), Value::from(name));
    for (k, v) in fields {
        m.insert(k.into(), v);
    }
    Value::Object(m)
}

#[cfg(test)]
pub fn item_value(it: &crate::core::item::Item) -> Value {
    dataclass_value(
        "Item",
        vec![
            ("id", Value::from(it.id.clone())),
            ("repo", Value::from(it.repo.clone())),
            ("kind", Value::from(it.kind.clone())),
            ("title", Value::from(it.title.clone())),
            ("body", Value::from(it.body.clone())),
            ("tag", Value::from(it.tag.clone())),
            ("refs", Value::from(it.refs.clone())),
            ("tags", Value::from(it.tags.clone())),
            ("source", Value::from(it.source.clone())),
            ("evidence", Value::from(it.evidence.clone())),
            ("as_of", Value::from(it.as_of.clone())),
            ("status", Value::from(it.status.clone())),
            (
                "superseded_by",
                it.superseded_by.clone().map_or(Value::Null, Value::from),
            ),
            ("content_hash", Value::from(it.content_hash.clone())),
            ("supersedes", Value::from(it.supersedes.clone())),
        ],
    )
}

/// `{"__type": "Item", ...}` back to an Item, through `Item(**fields)` so `__post_init__` runs as
/// the bridge's decode does.
#[cfg(test)]
pub fn value_item(v: &Value) -> Result<crate::core::item::Item, SmError> {
    let Value::Object(m) = v else {
        return Err(SmError::type_("parity: an Item is a JSON object"));
    };
    if m.get("__type").and_then(|t| t.as_str()) != Some("Item") {
        return Err(SmError::type_("parity: the object is not an Item"));
    }
    let mut kwargs = m.clone();
    kwargs.remove("__type");
    crate::core::item::Item::from_call(&Call {
        args: Vec::new(),
        kwargs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_line_shapes() {
        assert_eq!(
            reply_line(r#"{"fn": "py.str.lower", "args": ["ĐORĐ"]}"#),
            r#"{"ok":"đorđ"}"#
        );
        assert_eq!(
            reply_line(r#"{"fn": "core.item.validate_as_of", "args": ["0000-01-01"]}"#),
            r#"{"err":"ValueError: Invalid ISO 8601 timestamp '0000-01-01', for example 2026-11-21T09:30:00Z"}"#
        );
        assert_eq!(
            reply_line(r#"{"fn": "nope.x", "args": []}"#),
            r#"{"err":"LookupError: no parity entry for nope.x"}"#
        );
        assert_eq!(
            reply_line("[1]"),
            r#"{"err":"ProtocolError: a case line is not a JSON object"}"#
        );
    }

    #[test]
    fn surrogateescape_strs_cross_the_protocol() {
        // The bridge sends Python's lone '\udc80' as the JSON escape; Python's reply escapes it.
        assert_eq!(
            reply_line(r#"{"fn": "py.str.repr", "args": ["R5\udc80"]}"#),
            r#"{"ok":"'R5\\udc80'"}"#
        );
        assert_eq!(
            reply_line(r#"{"fn": "py.str.lower", "args": ["A\uDCFF"]}"#),
            r#"{"ok":"a\udcff"}"#
        );
        assert_eq!(
            reply_line(r#"{"fn": "py.str.len", "args": ["\udc80\udcff"]}"#),
            r#"{"ok":2}"#
        );
        // A pair, an escaped backslash and other lone surrogates are left as they are.
        assert_eq!(
            reply_line(r#"{"fn": "py.str.len", "args": ["\ud83d\ude00"]}"#),
            r#"{"ok":1}"#
        );
        // A pair whose low half is in the stand-in range is U+10080, not a lone high plus a stand-in.
        assert_eq!(
            reply_line(r#"{"fn": "py.str.len", "args": ["\ud800\udc80"]}"#),
            r#"{"ok":1}"#
        );
        assert_eq!(
            reply_line(r#"{"fn": "py.str.len", "args": ["\\udc80"]}"#),
            r#"{"ok":6}"#
        );
        for lone in [r"\udc7f", r"\udd00", r"\ud800", r"\udc80\ud800"] {
            let line = format!(r#"{{"fn": "py.str.len", "args": ["{lone}"]}}"#);
            assert!(
                reply_line(&line).starts_with(r#"{"err":"ProtocolError"#),
                "{lone}"
            );
        }
    }

    #[test]
    fn float_encoding() {
        assert_eq!(float_value(f64::NAN).to_string(), r#"{"__float":"nan"}"#);
        assert_eq!(
            float_value(f64::NEG_INFINITY).to_string(),
            r#"{"__float":"-inf"}"#
        );
        assert_eq!(float_value(1.0).to_string(), "1.0");
        assert_eq!(
            value_float(&serde_json::json!({"__float": "inf"})),
            Some(f64::INFINITY)
        );
        assert_eq!(
            str_set_value(["b".to_string(), "a".to_string()]).to_string(),
            r#"{"__set":["a","b"]}"#
        );
    }

    #[test]
    fn item_round_trip_keeps_field_order() {
        let v = serde_json::json!({"__type": "Item", "id": "m_1", "repo": "r", "kind": "fact", "title": "T",
            "body": " b ", "tag": "decided", "as_of": "2026-01-01T00:00:00+00:00", "supersedes": ["m_0"]});
        let it = value_item(&v).unwrap();
        let back = item_value(&it);
        let keys: Vec<&str> = back
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(
            keys,
            [
                "__type",
                "id",
                "repo",
                "kind",
                "title",
                "body",
                "tag",
                "refs",
                "tags",
                "source",
                "evidence",
                "as_of",
                "status",
                "superseded_by",
                "content_hash",
                "supersedes"
            ]
        );
        assert_eq!(
            back["content_hash"],
            "3e23e8160039594a33894f6564e1b1348bbd7a0088d42c4acb73eeaed59c009d"
        );
        assert_eq!(back["supersedes"], serde_json::json!(["m_0"]));
    }
}
