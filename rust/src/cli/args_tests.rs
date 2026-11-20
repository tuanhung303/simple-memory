// Generated from the original Python argparse parity cases; exercised directly in Rust.

use super::*;
use crate::py::argparse::Exit;
use crate::py::io;
use std::collections::BTreeMap;
use std::os::unix::ffi::OsStringExt;

#[allow(non_snake_case)]
fn S(s: &str) -> String {
    s.to_string()
}

enum Arg {
    Text(String),
    Bytes(Vec<u8>),
}

impl Arg {
    fn into_string(self) -> String {
        match self {
            Self::Text(s) => s,
            Self::Bytes(bs) => io::decode_argv(&std::ffi::OsString::from_vec(bs)),
        }
    }
}

#[derive(Clone)]
enum V {
    None,
    Bool(bool),
    Int(i64),
    BigInt(String),
    Float(f64),
    Str(String),
    BytesStr(Vec<u8>),
    List(Vec<V>),
}

impl V {
    fn default_bias() -> Self {
        Self::List(
            crate::core::rank::DEFAULT_BIAS
                .iter()
                .map(|s| Self::Str((*s).to_string()))
                .collect(),
        )
    }
}

fn bytes_to_py_str(bs: &[u8]) -> String {
    let os = std::ffi::OsString::from_vec(bs.to_vec());
    io::decode_argv(&os)
}

fn val_eq(actual: &Value, expected: &V) -> Result<(), String> {
    match (actual, expected) {
        (Value::None, V::None) => Ok(()),
        (Value::Bool(a), V::Bool(b)) if a == b => Ok(()),
        (Value::Int(a), V::Int(b)) if a == b => Ok(()),
        (Value::BigInt(a), V::BigInt(b)) if a == b => Ok(()),
        (Value::Float(a), V::Float(b)) if a.is_nan() && b.is_nan() => Ok(()),
        (Value::Float(a), V::Float(b)) if a == b => Ok(()),
        (Value::Str(a), V::Str(b)) if a == b => Ok(()),
        (Value::Str(a), V::BytesStr(bs)) => {
            let exp = bytes_to_py_str(bs);
            if a == &exp {
                Ok(())
            } else {
                Err(format!("bytes string mismatch: {a:?} != {exp:?}"))
            }
        }
        (Value::List(a), V::List(b)) => {
            if a.len() != b.len() {
                return Err(format!("list len {} != {}", a.len(), b.len()));
            }
            for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
                val_eq(x, y).map_err(|e| format!("[{i}]: {e}"))?;
            }
            Ok(())
        }
        _ => Err(format!("value mismatch: got {actual:?}")),
    }
}

fn last_stderr(out: &Out) -> String {
    let stderr = out.captured().1;
    stderr
        .trim_end_matches('\n')
        .rsplit('\n')
        .next()
        .unwrap_or_default()
        .to_string()
}

fn assert_err(argv: Vec<Arg>, expected_code: i32, expected_stderr: &str) {
    let argv_strs: Vec<String> = argv.into_iter().map(Arg::into_string).collect();
    let mut out = Out::capture();
    let res = build_parser().parse_args(&argv_strs, &mut out);
    let code = match res {
        Ok(ns) => panic!("expected exit {expected_code} for {argv_strs:?}, got Ok({ns:?})"),
        Err(Exit(c)) => c,
    };
    out.finish(code);
    assert_eq!(code, expected_code, "code mismatch for {argv_strs:?}");
    assert_eq!(
        last_stderr(&out),
        expected_stderr,
        "stderr mismatch for {argv_strs:?}"
    );
}

fn assert_ok_fields(
    argv: Vec<Arg>,
    base: Vec<(&'static str, V)>,
    overrides: Vec<(&'static str, V)>,
) {
    let argv_strs: Vec<String> = argv.into_iter().map(Arg::into_string).collect();
    let mut expected: BTreeMap<&'static str, V> = base.into_iter().collect();
    for (k, v) in overrides {
        expected.insert(k, v);
    }
    let mut out = Out::capture();
    let ns = build_parser()
        .parse_args(&argv_strs, &mut out)
        .unwrap_or_else(|Exit(c)| {
            panic!(
                "unexpected exit {c} for {argv_strs:?}: {}",
                last_stderr(&out)
            )
        });
    out.finish(0);
    assert_eq!(last_stderr(&out), "", "unexpected stderr for {argv_strs:?}");
    assert_eq!(
        ns.iter().count(),
        expected.len(),
        "field count mismatch for {argv_strs:?}"
    );
    for (k, exp_v) in &expected {
        let got = ns
            .iter()
            .find(|(nk, _)| nk == k)
            .map(|(_, v)| v)
            .unwrap_or_else(|| panic!("missing key {k} for {argv_strs:?}"));
        if let Err(msg) = val_eq(got, exp_v) {
            panic!("{argv_strs:?} key {k}: {msg}");
        }
    }
}

fn base_backup() -> Vec<(&'static str, V)> {
    vec![
        ("version", V::None),
        ("command", V::Str("backup".to_string())),
        ("keep", V::None),
    ]
}

fn base_config() -> Vec<(&'static str, V)> {
    vec![
        ("version", V::None),
        ("command", V::Str("config".to_string())),
    ]
}

fn base_doctor() -> Vec<(&'static str, V)> {
    vec![
        ("version", V::None),
        ("command", V::Str("doctor".to_string())),
        ("rebuild_fts", V::Bool(false)),
        ("backfill_refs", V::Bool(false)),
        ("dry_run", V::Bool(false)),
        ("imports", V::Bool(false)),
    ]
}

fn base_export() -> Vec<(&'static str, V)> {
    vec![
        ("version", V::None),
        ("command", V::Str("export".to_string())),
        ("include_history", V::Bool(false)),
        ("repo", V::Str("".to_string())),
        ("all_repos", V::Bool(false)),
    ]
}

fn base_hook() -> Vec<(&'static str, V)> {
    vec![
        ("version", V::None),
        ("command", V::Str("hook".to_string())),
        ("event", V::Str("prompt".to_string())),
        ("agy", V::Bool(true)),
    ]
}

fn base_hooks() -> Vec<(&'static str, V)> {
    vec![
        ("version", V::None),
        ("command", V::Str("hooks".to_string())),
        ("hooks_action", V::Str("print".to_string())),
        ("harness", V::Str("agy".to_string())),
    ]
}

fn base_init() -> Vec<(&'static str, V)> {
    vec![
        ("version", V::None),
        ("command", V::Str("init".to_string())),
    ]
}

fn base_mcp() -> Vec<(&'static str, V)> {
    vec![("version", V::None), ("command", V::Str("mcp".to_string()))]
}

fn base_put() -> Vec<(&'static str, V)> {
    vec![
        ("version", V::None),
        ("command", V::Str("put".to_string())),
        ("body", V::List(vec![])),
        ("kind", V::Str("fact".to_string())),
        ("tag", V::Str("".to_string())),
        ("evidence", V::Str("".to_string())),
        ("refs", V::Str("".to_string())),
        ("tags", V::Str("".to_string())),
        ("title", V::Str("".to_string())),
        ("source", V::Str("".to_string())),
        ("repo", V::Str("".to_string())),
        ("as_of", V::Str("".to_string())),
        ("supersedes", V::Str("".to_string())),
    ]
}

fn base_query() -> Vec<(&'static str, V)> {
    vec![
        ("version", V::None),
        ("command", V::Str("query".to_string())),
        ("query", V::Str("x".to_string())),
        ("limit", V::Int(4)),
        ("recent", V::Bool(false)),
        ("bias", V::None),
        ("tag", V::Str("".to_string())),
        ("kind", V::Str("".to_string())),
        ("source", V::Str("".to_string())),
        ("since", V::Str("".to_string())),
        ("until", V::Str("".to_string())),
        ("in", V::Str("".to_string())),
        ("all_repos", V::Bool(false)),
        ("repo", V::Str("".to_string())),
        ("include_history", V::Bool(false)),
        ("format", V::Str("text".to_string())),
    ]
}

fn base_related() -> Vec<(&'static str, V)> {
    vec![
        ("version", V::None),
        ("command", V::Str("related".to_string())),
        ("id", V::Str("id".to_string())),
        ("limit", V::Int(10)),
        ("format", V::Str("text".to_string())),
    ]
}

fn base_show() -> Vec<(&'static str, V)> {
    vec![
        ("version", V::None),
        ("command", V::Str("show".to_string())),
        (
            "ids",
            V::List(vec![V::Str("a".to_string()), V::Str("b".to_string())]),
        ),
    ]
}

fn base_stats() -> Vec<(&'static str, V)> {
    vec![
        ("version", V::None),
        ("command", V::Str("stats".to_string())),
        ("days", V::Float(7.0_f64)),
        ("format", V::Str("text".to_string())),
    ]
}

fn base_supersede() -> Vec<(&'static str, V)> {
    vec![
        ("version", V::None),
        ("command", V::Str("supersede".to_string())),
        ("old_id", V::Str("a".to_string())),
        ("new_id", V::Str("b".to_string())),
    ]
}

fn base_upkeep() -> Vec<(&'static str, V)> {
    vec![
        ("version", V::None),
        ("command", V::Str("upkeep".to_string())),
        ("dry_run", V::Bool(false)),
        ("report", V::Bool(true)),
        ("limit", V::Int(50)),
    ]
}

#[test]
fn argparse_root_cases() {
    // case 0
    assert_err(
        vec![],
        2,
        "sm: error: the following arguments are required: command",
    );
    // case 1
    assert_err(vec![Arg::Text("-h".to_string())], 0, "");
    // case 2
    assert_err(vec![Arg::Text("--help".to_string())], 0, "");
    // case 3
    assert_err(vec![Arg::Text("--version".to_string())], 0, "");
    // case 4
    assert_err(vec![Arg::Text("--vers".to_string())], 0, "");
    // case 5
    assert_err(vec![Arg::Text("--v".to_string())], 0, "");
    // case 6
    assert_err(
        vec![Arg::Text("--version=1".to_string())],
        2,
        "sm: error: argument --version: ignored explicit argument '1'",
    );
    // case 7
    assert_err(vec![Arg::Text("--he".to_string())], 0, "");
    // case 8
    assert_err(vec![Arg::Text("-hx".to_string())], 0, "");
    // case 9
    assert_err(
        vec![Arg::Text("-x".to_string())],
        2,
        "sm: error: the following arguments are required: command",
    );
    // case 10
    assert_err(
        vec![Arg::Text("--zz".to_string())],
        2,
        "sm: error: the following arguments are required: command",
    );
    // case 11
    assert_err(
        vec![Arg::Text("--zz".to_string()), Arg::Text("put".to_string())],
        2,
        "sm: error: unrecognized arguments: --zz",
    );
    // case 12
    assert_err(
        vec![
            Arg::Text("--zz".to_string()),
            Arg::Text("put".to_string()),
            Arg::Text("--yy".to_string()),
        ],
        2,
        "sm: error: unrecognized arguments: --zz --yy",
    );
    // case 14
    assert_err(
        vec![
            Arg::Text("--version".to_string()),
            Arg::Text("put".to_string()),
        ],
        0,
        "",
    );
    // case 15
    assert_err(
        vec![
            Arg::Text("--version".to_string()),
            Arg::Text("x".to_string()),
        ],
        0,
        "",
    );
    // case 16
    assert_err(
        vec![Arg::Text("-h".to_string()), Arg::Text("put".to_string())],
        0,
        "",
    );
    // case 17
    assert_err(
        vec![Arg::Text("qury".to_string())],
        2,
        "sm: error: argument command: invalid choice: 'qury', maybe you meant 'query'? (choose from 'put', 'query', 'show', 'related', 'supersede', 'retire', 'amend', 'list', 'import', 'export', 'backup', 'hook', 'doctor', 'upkeep', 'stats', 'init', 'config', 'hooks', 'mcp')",
    );
    // case 18
    assert_err(
        vec![Arg::Text("quer".to_string())],
        2,
        "sm: error: argument command: invalid choice: 'quer', maybe you meant 'query'? (choose from 'put', 'query', 'show', 'related', 'supersede', 'retire', 'amend', 'list', 'import', 'export', 'backup', 'hook', 'doctor', 'upkeep', 'stats', 'init', 'config', 'hooks', 'mcp')",
    );
    // case 19
    assert_err(
        vec![Arg::Text("Put".to_string())],
        2,
        "sm: error: argument command: invalid choice: 'Put', maybe you meant 'put'? (choose from 'put', 'query', 'show', 'related', 'supersede', 'retire', 'amend', 'list', 'import', 'export', 'backup', 'hook', 'doctor', 'upkeep', 'stats', 'init', 'config', 'hooks', 'mcp')",
    );
    // case 20
    assert_err(
        vec![Arg::Text("impor".to_string())],
        2,
        "sm: error: argument command: invalid choice: 'impor', maybe you meant 'import'? (choose from 'put', 'query', 'show', 'related', 'supersede', 'retire', 'amend', 'list', 'import', 'export', 'backup', 'hook', 'doctor', 'upkeep', 'stats', 'init', 'config', 'hooks', 'mcp')",
    );
    // case 21
    assert_err(
        vec![Arg::Text("hook2".to_string())],
        2,
        "sm: error: argument command: invalid choice: 'hook2', maybe you meant 'hook'? (choose from 'put', 'query', 'show', 'related', 'supersede', 'retire', 'amend', 'list', 'import', 'export', 'backup', 'hook', 'doctor', 'upkeep', 'stats', 'init', 'config', 'hooks', 'mcp')",
    );
    // case 22
    assert_err(
        vec![Arg::Text("-".to_string())],
        2,
        "sm: error: argument command: invalid choice: '-' (choose from 'put', 'query', 'show', 'related', 'supersede', 'retire', 'amend', 'list', 'import', 'export', 'backup', 'hook', 'doctor', 'upkeep', 'stats', 'init', 'config', 'hooks', 'mcp')",
    );
    // case 23
    assert_err(
        vec![Arg::Text("--=x".to_string())],
        2,
        "sm: error: ambiguous option: --=x could match --help, --version",
    );
    // case 25
    assert_err(
        vec![
            Arg::Text("--".to_string()),
            Arg::Text("--".to_string()),
            Arg::Text("put".to_string()),
        ],
        2,
        "sm: error: argument command: invalid choice: '--' (choose from 'put', 'query', 'show', 'related', 'supersede', 'retire', 'amend', 'list', 'import', 'export', 'backup', 'hook', 'doctor', 'upkeep', 'stats', 'init', 'config', 'hooks', 'mcp')",
    );
    // case 26
    assert_err(
        vec![Arg::Text("".to_string())],
        2,
        "sm: error: argument command: invalid choice: '' (choose from 'put', 'query', 'show', 'related', 'supersede', 'retire', 'amend', 'list', 'import', 'export', 'backup', 'hook', 'doctor', 'upkeep', 'stats', 'init', 'config', 'hooks', 'mcp')",
    );
    // case 27
    assert_err(
        vec![Arg::Text("".to_string()), Arg::Text("x".to_string())],
        2,
        "sm: error: argument command: invalid choice: '' (choose from 'put', 'query', 'show', 'related', 'supersede', 'retire', 'amend', 'list', 'import', 'export', 'backup', 'hook', 'doctor', 'upkeep', 'stats', 'init', 'config', 'hooks', 'mcp')",
    );
    // case 28
    assert_err(
        vec![Arg::Text(S("x").repeat(250))],
        2,
        "sm: error: argument command: invalid choice: 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx' (choose from 'put', 'query', 'show', 'related', 'supersede', 'retire', 'amend', 'list', 'import', 'export', 'backup', 'hook', 'doctor', 'upkeep', 'stats', 'init', 'config', 'hooks', 'mcp')",
    );
    // case 29
    assert_err(
        vec![Arg::Text(format!("qu{}", S("e").repeat(300)))],
        2,
        "sm: error: argument command: invalid choice: 'queeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee' (choose from 'put', 'query', 'show', 'related', 'supersede', 'retire', 'amend', 'list', 'import', 'export', 'backup', 'hook', 'doctor', 'upkeep', 'stats', 'init', 'config', 'hooks', 'mcp')",
    );
    // case 30
    assert_err(
        vec![Arg::Text(S("querx").repeat(50))],
        2,
        "sm: error: argument command: invalid choice: 'querxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerxquerx' (choose from 'put', 'query', 'show', 'related', 'supersede', 'retire', 'amend', 'list', 'import', 'export', 'backup', 'hook', 'doctor', 'upkeep', 'stats', 'init', 'config', 'hooks', 'mcp')",
    );
    // case 31
    assert_err(
        vec![Arg::Text("\u{65e5}\u{672c}".to_string())],
        2,
        "sm: error: argument command: invalid choice: '\u{65e5}\u{672c}' (choose from 'put', 'query', 'show', 'related', 'supersede', 'retire', 'amend', 'list', 'import', 'export', 'backup', 'hook', 'doctor', 'upkeep', 'stats', 'init', 'config', 'hooks', 'mcp')",
    );
    // case 32
    assert_err(
        vec![Arg::Bytes(vec![0xff])],
        2,
        "sm: error: argument command: invalid choice: '\\udcff' (choose from 'put', 'query', 'show', 'related', 'supersede', 'retire', 'amend', 'list', 'import', 'export', 'backup', 'hook', 'doctor', 'upkeep', 'stats', 'init', 'config', 'hooks', 'mcp')",
    );
    // case 33
    assert_err(
        vec![Arg::Bytes(vec![0x71, 0x75, 0xff, 0x65, 0x72, 0x79])],
        2,
        "sm: error: argument command: invalid choice: 'qu\\udcffery', maybe you meant 'query'? (choose from 'put', 'query', 'show', 'related', 'supersede', 'retire', 'amend', 'list', 'import', 'export', 'backup', 'hook', 'doctor', 'upkeep', 'stats', 'init', 'config', 'hooks', 'mcp')",
    );
    // case 34
    assert_err(
        vec![Arg::Text("acp".to_string())],
        2,
        "sm: error: argument command: invalid choice: 'acp', maybe you meant 'mcp'? (choose from 'put', 'query', 'show', 'related', 'supersede', 'retire', 'amend', 'list', 'import', 'export', 'backup', 'hook', 'doctor', 'upkeep', 'stats', 'init', 'config', 'hooks', 'mcp')",
    );
    // case 35
    assert_err(
        vec![Arg::Text("bcp".to_string())],
        2,
        "sm: error: argument command: invalid choice: 'bcp', maybe you meant 'mcp'? (choose from 'put', 'query', 'show', 'related', 'supersede', 'retire', 'amend', 'list', 'import', 'export', 'backup', 'hook', 'doctor', 'upkeep', 'stats', 'init', 'config', 'hooks', 'mcp')",
    );
    // case 36
    assert_err(
        vec![Arg::Text("-h=x".to_string())],
        2,
        "sm: error: argument -h/--help: ignored explicit argument 'x'",
    );
    // case 37
    assert_err(
        vec![Arg::Text("-h-x".to_string())],
        2,
        "sm: error: argument -h/--help: ignored explicit argument '-x'",
    );
    // case 38
    assert_err(vec![Arg::Text("-hh".to_string())], 0, "");
    // case 39
    assert_err(vec![Arg::Text("-hv".to_string())], 0, "");
    // case 268
    assert_err(
        vec![
            Arg::Text("--zz".to_string()),
            Arg::Text("import".to_string()),
            Arg::Text("--yy".to_string()),
            Arg::Text("tasklog".to_string()),
            Arg::Text("p".to_string()),
            Arg::Text("--ww".to_string()),
        ],
        2,
        "sm: error: unrecognized arguments: --zz --yy --ww",
    );
}

#[test]
fn argparse_put_cases() {
    // case 13
    assert_err(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--version".to_string()),
        ],
        2,
        "sm: error: unrecognized arguments: --version",
    );
    // case 24
    assert_ok_fields(
        vec![
            Arg::Text("--".to_string()),
            Arg::Text("put".to_string()),
            Arg::Text("a".to_string()),
        ],
        base_put(),
        vec![("body", V::List(vec![V::Str("a".to_string())]))],
    );
    // case 40
    assert_err(
        vec![Arg::Text("put".to_string()), Arg::Text("-h=1".to_string())],
        2,
        "sm put: error: argument -h/--help: ignored explicit argument '1'",
    );
    // case 41
    assert_err(
        vec![Arg::Text("put".to_string()), Arg::Text("-hq".to_string())],
        0,
        "",
    );
    // case 42
    assert_ok_fields(vec![Arg::Text("put".to_string())], base_put(), vec![]);
    // case 43
    assert_ok_fields(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("a".to_string()),
            Arg::Text("b".to_string()),
        ],
        base_put(),
        vec![(
            "body",
            V::List(vec![V::Str("a".to_string()), V::Str("b".to_string())]),
        )],
    );
    // case 44
    assert_err(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("a".to_string()),
            Arg::Text("--tag".to_string()),
            Arg::Text("decided".to_string()),
            Arg::Text("b".to_string()),
        ],
        2,
        "sm: error: unrecognized arguments: b",
    );
    // case 45
    assert_ok_fields(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--tag".to_string()),
            Arg::Text("decided".to_string()),
            Arg::Text("a".to_string()),
            Arg::Text("b".to_string()),
        ],
        base_put(),
        vec![
            (
                "body",
                V::List(vec![V::Str("a".to_string()), V::Str("b".to_string())]),
            ),
            ("tag", V::Str("decided".to_string())),
        ],
    );
    // case 46
    assert_ok_fields(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--tag=decided".to_string()),
            Arg::Text("a".to_string()),
        ],
        base_put(),
        vec![
            ("body", V::List(vec![V::Str("a".to_string())])),
            ("tag", V::Str("decided".to_string())),
        ],
    );
    // case 47
    assert_err(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--ta".to_string()),
            Arg::Text("x".to_string()),
        ],
        2,
        "sm put: error: ambiguous option: --ta could match --tag, --tags",
    );
    // case 48
    assert_err(
        vec![Arg::Text("put".to_string()), Arg::Text("--tag".to_string())],
        2,
        "sm put: error: argument --tag: expected one argument",
    );
    // case 49
    assert_ok_fields(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--refs".to_string()),
            Arg::Text("A".to_string()),
            Arg::Text("--refs".to_string()),
            Arg::Text("B".to_string()),
        ],
        base_put(),
        vec![("refs", V::Str("A,B".to_string()))],
    );
    // case 50
    assert_ok_fields(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--refs".to_string()),
            Arg::Text("".to_string()),
            Arg::Text("--refs".to_string()),
            Arg::Text("B".to_string()),
        ],
        base_put(),
        vec![("refs", V::Str("B".to_string()))],
    );
    // case 51
    assert_ok_fields(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--refs".to_string()),
            Arg::Text("A".to_string()),
            Arg::Text("--refs".to_string()),
            Arg::Text("".to_string()),
        ],
        base_put(),
        vec![("refs", V::Str("A,".to_string()))],
    );
    // case 52
    assert_ok_fields(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--ref".to_string()),
            Arg::Text("A".to_string()),
        ],
        base_put(),
        vec![("refs", V::Str("A".to_string()))],
    );
    // case 53
    assert_ok_fields(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--ev".to_string()),
            Arg::Text("e".to_string()),
        ],
        base_put(),
        vec![("evidence", V::Str("e".to_string()))],
    );
    // case 54
    assert_ok_fields(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--as-of".to_string()),
            Arg::Text("2026-01-01".to_string()),
            Arg::Text("x".to_string()),
        ],
        base_put(),
        vec![
            ("body", V::List(vec![V::Str("x".to_string())])),
            ("as_of", V::Str("2026-01-01".to_string())),
        ],
    );
    // case 55
    assert_ok_fields(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--as".to_string()),
            Arg::Text("x".to_string()),
        ],
        base_put(),
        vec![("as_of", V::Str("x".to_string()))],
    );
    // case 56
    assert_ok_fields(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--supersedes".to_string()),
            Arg::Text("a,b".to_string()),
            Arg::Text("--supersedes".to_string()),
            Arg::Text("c".to_string()),
        ],
        base_put(),
        vec![("supersedes", V::Str("a,b,c".to_string()))],
    );
    // case 57
    assert_ok_fields(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--sup".to_string()),
            Arg::Text("x".to_string()),
        ],
        base_put(),
        vec![("supersedes", V::Str("x".to_string()))],
    );
    // case 58
    assert_ok_fields(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--kind".to_string()),
            Arg::Text("decision".to_string()),
            Arg::Text("--title".to_string()),
            Arg::Text("T".to_string()),
            Arg::Text("--source".to_string()),
            Arg::Text("s".to_string()),
            Arg::Text("--repo".to_string()),
            Arg::Text("r".to_string()),
            Arg::Text("x".to_string()),
        ],
        base_put(),
        vec![
            ("body", V::List(vec![V::Str("x".to_string())])),
            ("kind", V::Str("decision".to_string())),
            ("title", V::Str("T".to_string())),
            ("source", V::Str("s".to_string())),
            ("repo", V::Str("r".to_string())),
        ],
    );
    // case 59
    assert_ok_fields(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--tags".to_string()),
            Arg::Text("a".to_string()),
            Arg::Text("--tags".to_string()),
            Arg::Text("b".to_string()),
        ],
        base_put(),
        vec![("tags", V::Str("a,b".to_string()))],
    );
    // case 60
    assert_err(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--tag".to_string()),
            Arg::Text("-x".to_string()),
        ],
        2,
        "sm put: error: argument --tag: expected one argument",
    );
    // case 61
    assert_ok_fields(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--tag".to_string()),
            Arg::Text("-5".to_string()),
        ],
        base_put(),
        vec![("tag", V::Str("-5".to_string()))],
    );
    // case 62
    assert_ok_fields(
        vec![Arg::Text("put".to_string()), Arg::Text("-5".to_string())],
        base_put(),
        vec![("body", V::List(vec![V::Str("-5".to_string())]))],
    );
    // case 63
    assert_ok_fields(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--".to_string()),
            Arg::Text("--tag".to_string()),
        ],
        base_put(),
        vec![("body", V::List(vec![V::Str("--tag".to_string())]))],
    );
    // case 64
    assert_ok_fields(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("a".to_string()),
            Arg::Text("--".to_string()),
            Arg::Text("b".to_string()),
            Arg::Text("--".to_string()),
            Arg::Text("c".to_string()),
        ],
        base_put(),
        vec![(
            "body",
            V::List(vec![
                V::Str("a".to_string()),
                V::Str("b".to_string()),
                V::Str("--".to_string()),
                V::Str("c".to_string()),
            ]),
        )],
    );
    // case 65
    assert_err(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--title".to_string()),
            Arg::Text("--".to_string()),
        ],
        2,
        "sm put: error: argument --title: expected one argument",
    );
    // case 66
    assert_ok_fields(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--title=".to_string()),
            Arg::Text("x".to_string()),
        ],
        base_put(),
        vec![("body", V::List(vec![V::Str("x".to_string())]))],
    );
    // case 67
    assert_ok_fields(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--title=a=b".to_string()),
        ],
        base_put(),
        vec![("title", V::Str("a=b".to_string()))],
    );
    // case 68
    assert_err(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--nope".to_string()),
            Arg::Text("x".to_string()),
        ],
        2,
        "sm: error: unrecognized arguments: --nope",
    );
    // case 69
    assert_ok_fields(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--kind".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--kind".to_string()),
            Arg::Text("y".to_string()),
        ],
        base_put(),
        vec![("kind", V::Str("y".to_string()))],
    );
    // case 70
    assert_err(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("-k".to_string()),
            Arg::Text("x".to_string()),
        ],
        2,
        "sm: error: unrecognized arguments: -k",
    );
    // case 71
    assert_err(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--help".to_string()),
        ],
        0,
        "",
    );
    // case 72
    assert_err(
        vec![Arg::Text("put".to_string()), Arg::Text("--he".to_string())],
        0,
        "",
    );
    // case 73
    assert_err(
        vec![Arg::Text("put".to_string()), Arg::Text("--h".to_string())],
        0,
        "",
    );
    // case 74
    assert_err(
        vec![Arg::Text("put".to_string()), Arg::Text("-h".to_string())],
        0,
        "",
    );
    // case 75
    assert_ok_fields(
        vec![Arg::Text("put".to_string()), Arg::Text("".to_string())],
        base_put(),
        vec![("body", V::List(vec![V::Str("".to_string())]))],
    );
    // case 76
    assert_ok_fields(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--tag".to_string()),
            Arg::Text("".to_string()),
        ],
        base_put(),
        vec![],
    );
    // case 77
    assert_ok_fields(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("h\u{e9}llo".to_string()),
            Arg::Text("--tag".to_string()),
            Arg::Text("prior".to_string()),
        ],
        base_put(),
        vec![
            ("body", V::List(vec![V::Str("h\u{e9}llo".to_string())])),
            ("tag", V::Str("prior".to_string())),
        ],
    );
    // case 78
    assert_ok_fields(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("a b".to_string()),
            Arg::Text("-c d".to_string()),
        ],
        base_put(),
        vec![(
            "body",
            V::List(vec![V::Str("a b".to_string()), V::Str("-c d".to_string())]),
        )],
    );
    // case 79
    assert_err(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--t".to_string()),
            Arg::Text("x".to_string()),
        ],
        2,
        "sm put: error: ambiguous option: --t could match --tag, --tags, --title",
    );
    // case 80
    assert_err(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--r".to_string()),
            Arg::Text("x".to_string()),
        ],
        2,
        "sm put: error: ambiguous option: --r could match --refs, --repo",
    );
    // case 81
    assert_err(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--s".to_string()),
            Arg::Text("x".to_string()),
        ],
        2,
        "sm put: error: ambiguous option: --s could match --source, --supersedes",
    );
    // case 82
    assert_ok_fields(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--k".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("y".to_string()),
        ],
        base_put(),
        vec![
            ("body", V::List(vec![V::Str("y".to_string())])),
            ("kind", V::Str("x".to_string())),
        ],
    );
    // case 83
    assert_err(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--tag".to_string()),
            Arg::Text("a".to_string()),
            Arg::Text("b".to_string()),
            Arg::Text("--kind".to_string()),
            Arg::Text("c".to_string()),
            Arg::Text("d".to_string()),
        ],
        2,
        "sm: error: unrecognized arguments: d",
    );
    // case 84
    assert_ok_fields(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("-".to_string()),
            Arg::Text("a".to_string()),
        ],
        base_put(),
        vec![(
            "body",
            V::List(vec![V::Str("-".to_string()), V::Str("a".to_string())]),
        )],
    );
    // case 85
    assert_err(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--dry-run".to_string()),
        ],
        2,
        "sm: error: unrecognized arguments: --dry-run",
    );
    // case 86
    assert_ok_fields(
        vec![
            Arg::Text("put".to_string()),
            Arg::Bytes(vec![0x61, 0xff, 0x62, 0xfe, 0xfd]),
        ],
        base_put(),
        vec![(
            "body",
            V::List(vec![V::BytesStr(vec![0x61, 0xff, 0x62, 0xfe, 0xfd])]),
        )],
    );
    // case 87
    assert_ok_fields(
        vec![
            Arg::Text("put".to_string()),
            Arg::Text("--title".to_string()),
            Arg::Bytes(vec![0xc3]),
        ],
        base_put(),
        vec![("title", V::BytesStr(vec![0xc3]))],
    );
}

#[test]
fn argparse_query_cases() {
    // case 88
    assert_err(
        vec![Arg::Text("query".to_string())],
        2,
        "sm query: error: the following arguments are required: query",
    );
    // case 89
    assert_ok_fields(
        vec![Arg::Text("query".to_string()), Arg::Text("x".to_string())],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 90
    assert_ok_fields(
        vec![Arg::Text("query".to_string()), Arg::Text("-5".to_string())],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("-5".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 91
    assert_err(
        vec![Arg::Text("query".to_string()), Arg::Text("-R6".to_string())],
        2,
        "sm query: error: the following arguments are required: query",
    );
    // case 92
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("--".to_string()),
            Arg::Text("-R6".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("-R6".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 93
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("y".to_string()),
        ],
        2,
        "sm: error: unrecognized arguments: y",
    );
    // case 94
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("3".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(3)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 95
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit=3".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(3)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 96
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--lim".to_string()),
            Arg::Text("3".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(3)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 97
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--l".to_string()),
            Arg::Text("3".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(3)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 98
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("\u{663}".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(3)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 99
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("1_0".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(10)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 100
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text(" 7 ".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(7)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 101
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("+5".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(5)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 102
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("-5".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(-5)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 103
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("-0".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(0)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 104
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("3.0".to_string()),
        ],
        2,
        "sm query: error: argument --limit: invalid int value: '3.0'",
    );
    // case 105
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("x".to_string()),
        ],
        2,
        "sm query: error: argument --limit: invalid int value: 'x'",
    );
    // case 106
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("".to_string()),
        ],
        2,
        "sm query: error: argument --limit: invalid int value: ''",
    );
    // case 107
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("1__0".to_string()),
        ],
        2,
        "sm query: error: argument --limit: invalid int value: '1__0'",
    );
    // case 108
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("_1".to_string()),
        ],
        2,
        "sm query: error: argument --limit: invalid int value: '_1'",
    );
    // case 109
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("1_".to_string()),
        ],
        2,
        "sm query: error: argument --limit: invalid int value: '1_'",
    );
    // case 110
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("0x10".to_string()),
        ],
        2,
        "sm query: error: argument --limit: invalid int value: '0x10'",
    );
    // case 111
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text(S("9").repeat(23)),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::BigInt(S("9").repeat(23))),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 112
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text((i128::from(i64::MIN) - 1).to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::BigInt((i128::from(i64::MIN) - 1).to_string())),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 113
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text(i64::MAX.to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(i64::MAX)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 114
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text(S("9").repeat(4300)),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::BigInt(S("9").repeat(4300))),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 115
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text(S("9").repeat(4301)),
        ],
        2,
        "sm query: error: argument --limit: invalid int value: '99999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999'",
    );
    // case 116
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("007".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(7)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 117
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("\u{663}_\u{663}".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(33)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 118
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("\u{2003}5".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(5)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 119
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("\u{96b}".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(5)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 120
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("\u{b2}".to_string()),
        ],
        2,
        "sm query: error: argument --limit: invalid int value: '\u{b2}'",
    );
    // case 121
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("\u{ff15}".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(5)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 122
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("-".to_string()),
        ],
        2,
        "sm query: error: argument --limit: invalid int value: '-'",
    );
    // case 123
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("+".to_string()),
        ],
        2,
        "sm query: error: argument --limit: invalid int value: '+'",
    );
    // case 124
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("5\n".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(5)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 125
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("\u{1c}5\u{1f}".to_string()),
        ],
        2,
        "sm query: error: argument --limit: invalid int value: '\\x1c5\\x1f'",
    );
    // case 126
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("\u{1c}5".to_string()),
        ],
        2,
        "sm query: error: argument --limit: invalid int value: '\\x1c5'",
    );
    // case 127
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("\u{85}5".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(5)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 128
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("5\u{0}".to_string()),
        ],
        2,
        "sm query: error: argument --limit: invalid int value: '5\\x00'",
    );
    // case 129
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("1 0".to_string()),
        ],
        2,
        "sm query: error: argument --limit: invalid int value: '1 0'",
    );
    // case 130
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--recent".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(true)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 131
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--recent".to_string()),
            Arg::Text("--bias".to_string()),
            Arg::Text("fact".to_string()),
        ],
        2,
        "sm query: error: argument --bias: not allowed with argument --recent",
    );
    // case 132
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--bias".to_string()),
            Arg::Text("fact".to_string()),
            Arg::Text("--recent".to_string()),
        ],
        2,
        "sm query: error: argument --recent: not allowed with argument --bias",
    );
    // case 133
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--bias".to_string()),
            Arg::Text("Fact, decided".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            (
                "bias",
                V::List(vec![
                    V::Str("fact".to_string()),
                    V::Str("decided".to_string()),
                ]),
            ),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
            ("bias_text", V::Str("Fact, decided".to_string())),
        ],
    );
    // case 134
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--bias".to_string()),
            Arg::Text("fact".to_string()),
            Arg::Text("--bias".to_string()),
            Arg::Text("decided".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            (
                "bias",
                V::List(vec![
                    V::Str("fact".to_string()),
                    V::Str("decided".to_string()),
                ]),
            ),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
            ("bias_text", V::Str("fact,decided".to_string())),
        ],
    );
    // case 135
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--bias".to_string()),
            Arg::Text("".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            ("bias", V::default_bias()),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
            ("bias_text", V::Str("".to_string())),
        ],
    );
    // case 136
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--bias".to_string()),
            Arg::Text("".to_string()),
            Arg::Text("--bias".to_string()),
            Arg::Text("x".to_string()),
        ],
        2,
        "sm query: error: argument --bias: Invalid bias 'x'; expected one of recency, relevance, executed, verified, decided, client-stated, derived, assumed, prior, planned, fact, decision, learning, episode, task, handoff",
    );
    // case 137
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--bias".to_string()),
            Arg::Text("zz".to_string()),
        ],
        2,
        "sm query: error: argument --bias: Invalid bias 'zz'; expected one of recency, relevance, executed, verified, decided, client-stated, derived, assumed, prior, planned, fact, decision, learning, episode, task, handoff",
    );
    // case 138
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--bias".to_string()),
            Arg::Text("recency,recency".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            ("bias", V::List(vec![V::Str("recency".to_string())])),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
            ("bias_text", V::Str("recency,recency".to_string())),
        ],
    );
    // case 139
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--bias".to_string()),
            Arg::Text("task".to_string()),
            Arg::Text("--bias".to_string()),
            Arg::Text("zz".to_string()),
        ],
        2,
        "sm query: error: argument --bias: Invalid bias 'zz'; expected one of recency, relevance, executed, verified, decided, client-stated, derived, assumed, prior, planned, fact, decision, learning, episode, task, handoff",
    );
    // case 140
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--bias".to_string()),
            Arg::Text(",,".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            ("bias", V::default_bias()),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
            ("bias_text", V::Str(",,".to_string())),
        ],
    );
    // case 141
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--bias".to_string()),
            Arg::Text(" RELEVANCE ".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            ("bias", V::List(vec![V::Str("relevance".to_string())])),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
            ("bias_text", V::Str(" RELEVANCE ".to_string())),
        ],
    );
    // case 142
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--bi".to_string()),
            Arg::Text("task".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            ("bias", V::List(vec![V::Str("task".to_string())])),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
            ("bias_text", V::Str("task".to_string())),
        ],
    );
    // case 143
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--b".to_string()),
            Arg::Text("task".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            ("bias", V::List(vec![V::Str("task".to_string())])),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
            ("bias_text", V::Str("task".to_string())),
        ],
    );
    // case 144
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--re".to_string()),
        ],
        2,
        "sm query: error: ambiguous option: --re could match --recent, --repo",
    );
    // case 145
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--rec".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(true)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 146
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--rep".to_string()),
            Arg::Text("r".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("r".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 147
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--tag".to_string()),
            Arg::Text("a".to_string()),
            Arg::Text("--tag".to_string()),
            Arg::Text("b".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("a,b".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 148
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--kind".to_string()),
            Arg::Text("fact".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("fact".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 149
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--all".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(true)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 150
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--al".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(true)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 151
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--a".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(true)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 152
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--include-history".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(true)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 153
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--inc".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(true)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 154
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--i".to_string()),
        ],
        2,
        "sm query: error: ambiguous option: --i could match --in, --include-history",
    );
    // case 155
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--format".to_string()),
            Arg::Text("json".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("json".to_string())),
        ],
    );
    // case 156
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--format".to_string()),
            Arg::Text("jsno".to_string()),
        ],
        2,
        "sm query: error: argument --format: invalid choice: 'jsno', maybe you meant 'json'? (choose from 'text', 'brief', 'json')",
    );
    // case 157
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--format".to_string()),
            Arg::Text("JSON".to_string()),
        ],
        2,
        "sm query: error: argument --format: invalid choice: 'JSON' (choose from 'text', 'brief', 'json')",
    );
    // case 158
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--format".to_string()),
            Arg::Text("x".to_string()),
        ],
        2,
        "sm query: error: argument --format: invalid choice: 'x' (choose from 'text', 'brief', 'json')",
    );
    // case 159
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--f".to_string()),
            Arg::Text("brief".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("brief".to_string())),
        ],
    );
    // case 160
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--format=brief".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("brief".to_string())),
        ],
    );
    // case 161
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--format".to_string()),
            Arg::Text("brie".to_string()),
        ],
        2,
        "sm query: error: argument --format: invalid choice: 'brie', maybe you meant 'brief'? (choose from 'text', 'brief', 'json')",
    );
    // case 162
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--format".to_string()),
            Arg::Text("texts".to_string()),
        ],
        2,
        "sm query: error: argument --format: invalid choice: 'texts', maybe you meant 'text'? (choose from 'text', 'brief', 'json')",
    );
    // case 163
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--format".to_string()),
            Arg::Text(S("j").repeat(250)),
        ],
        2,
        "sm query: error: argument --format: invalid choice: 'jjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjj' (choose from 'text', 'brief', 'json')",
    );
    // case 164
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--format".to_string()),
            Arg::Text(S("json").repeat(60)),
        ],
        2,
        "sm query: error: argument --format: invalid choice: 'jsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjsonjson' (choose from 'text', 'brief', 'json')",
    );
    // case 165
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--format".to_string()),
            Arg::Text("js\u{f6}n".to_string()),
        ],
        2,
        "sm query: error: argument --format: invalid choice: 'js\u{f6}n', maybe you meant 'json'? (choose from 'text', 'brief', 'json')",
    );
    // case 166
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--format".to_string()),
            Arg::Text("".to_string()),
        ],
        2,
        "sm query: error: argument --format: invalid choice: '' (choose from 'text', 'brief', 'json')",
    );
    // case 167
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--recent=1".to_string()),
        ],
        2,
        "sm query: error: argument --recent: ignored explicit argument '1'",
    );
    // case 168
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--all=".to_string()),
        ],
        2,
        "sm query: error: argument --all: ignored explicit argument ''",
    );
    // case 169
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--all=x".to_string()),
        ],
        2,
        "sm query: error: argument --all: ignored explicit argument 'x'",
    );
    // case 170
    assert_ok_fields(
        vec![Arg::Text("query".to_string()), Arg::Text("a b".to_string())],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("a b".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 171
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("-a b".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("-a b".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 172
    assert_ok_fields(
        vec![Arg::Text("query".to_string()), Arg::Text("-.5".to_string())],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("-.5".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 173
    assert_err(
        vec![Arg::Text("query".to_string()), Arg::Text("-.x".to_string())],
        2,
        "sm query: error: the following arguments are required: query",
    );
    // case 174
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("-\u{663}".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("-\u{663}".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 175
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("--".to_string()),
            Arg::Text("--".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("--".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 176
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("3".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("5".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(5)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 177
    assert_err(
        vec![Arg::Text("query".to_string()), Arg::Text("-h".to_string())],
        0,
        "",
    );
    // case 178
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("-h".to_string()),
        ],
        0,
        "",
    );
    // case 179
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("3".to_string()),
        ],
        2,
        "sm query: error: the following arguments are required: query",
    );
    // case 180
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("3".to_string()),
            Arg::Text("x".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(3)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 181
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
        ],
        2,
        "sm query: error: argument --limit: expected one argument",
    );
    // case 182
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("--recent".to_string()),
        ],
        2,
        "sm query: error: argument --limit: expected one argument",
    );
    // case 183
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("-.5".to_string()),
        ],
        2,
        "sm query: error: argument --limit: invalid int value: '-.5'",
    );
    // case 184
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--tag".to_string()),
        ],
        2,
        "sm query: error: argument --tag: expected one argument",
    );
    // case 185
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("--".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("y".to_string()),
        ],
        2,
        "sm: error: unrecognized arguments: y",
    );
    // case 186
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--".to_string()),
            Arg::Text("y".to_string()),
        ],
        2,
        "sm: error: unrecognized arguments: y",
    );
    // case 187
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("--bias=".to_string()),
            Arg::Text("x".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            ("bias", V::default_bias()),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
            ("bias_text", V::Str("".to_string())),
        ],
    );
    // case 188
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("--tag=a,b".to_string()),
            Arg::Text("--tag=c".to_string()),
            Arg::Text("x".to_string()),
        ],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::Str("x".to_string())),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("a,b,c".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 189
    assert_ok_fields(
        vec![Arg::Text("query".to_string()), Arg::Bytes(vec![0xff])],
        base_query(),
        vec![
            ("version", V::None),
            ("command", V::Str("query".to_string())),
            ("query", V::BytesStr(vec![0xff])),
            ("limit", V::Int(4)),
            ("recent", V::Bool(false)),
            ("bias", V::None),
            ("tag", V::Str("".to_string())),
            ("kind", V::Str("".to_string())),
            ("in", V::Str("".to_string())),
            ("all_repos", V::Bool(false)),
            ("repo", V::Str("".to_string())),
            ("include_history", V::Bool(false)),
            ("format", V::Str("text".to_string())),
        ],
    );
    // case 190
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--format".to_string()),
            Arg::Bytes(vec![0xff]),
        ],
        2,
        "sm query: error: argument --format: invalid choice: '\\udcff' (choose from 'text', 'brief', 'json')",
    );
    // case 191
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--format".to_string()),
            Arg::Bytes(vec![0x6a, 0x73, 0x6f, 0xff]),
        ],
        2,
        "sm query: error: argument --format: invalid choice: 'jso\\udcff', maybe you meant 'json'? (choose from 'text', 'brief', 'json')",
    );
    // case 192
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Bytes(vec![0xff]),
        ],
        2,
        "sm query: error: argument --limit: invalid int value: '\\udcff'",
    );
    // case 193
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--bias".to_string()),
            Arg::Bytes(vec![0xff]),
        ],
        2,
        "sm query: error: argument --bias: Invalid bias '\\udcff'; expected one of recency, relevance, executed, verified, decided, client-stated, derived, assumed, prior, planned, fact, decision, learning, episode, task, handoff",
    );
    // case 194
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--zz=1".to_string()),
        ],
        2,
        "sm: error: unrecognized arguments: --zz=1",
    );
    // case 195
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("-5".to_string()),
        ],
        2,
        "sm: error: unrecognized arguments: -5",
    );
    // case 196
    assert_err(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("-5".to_string()),
            Arg::Text("-6".to_string()),
        ],
        2,
        "sm: error: unrecognized arguments: -6",
    );
    // wave2: --source (repeatable comma list), --since, --until
    assert_ok_fields(
        vec![
            Arg::Text("query".to_string()),
            Arg::Text("x".to_string()),
            Arg::Text("--source".to_string()),
            Arg::Text("task:".to_string()),
            Arg::Text("--source".to_string()),
            Arg::Text("agent:".to_string()),
            Arg::Text("--since".to_string()),
            Arg::Text("2026-05-01".to_string()),
            Arg::Text("--until".to_string()),
            Arg::Text("2026-05-31".to_string()),
        ],
        base_query(),
        vec![
            ("source", V::Str("task:,agent:".to_string())),
            ("since", V::Str("2026-05-01".to_string())),
            ("until", V::Str("2026-05-31".to_string())),
        ],
    );
}

#[test]
fn argparse_show_cases() {
    // case 197
    assert_err(
        vec![Arg::Text("show".to_string())],
        2,
        "sm show: error: the following arguments are required: ID",
    );
    // case 198
    assert_ok_fields(
        vec![Arg::Text("show".to_string()), Arg::Text("a".to_string())],
        base_show(),
        vec![("ids", V::List(vec![V::Str("a".to_string())]))],
    );
    // case 199
    assert_ok_fields(
        vec![
            Arg::Text("show".to_string()),
            Arg::Text("a".to_string()),
            Arg::Text("b".to_string()),
        ],
        base_show(),
        vec![],
    );
    // case 200
    assert_ok_fields(
        vec![
            Arg::Text("show".to_string()),
            Arg::Text("--".to_string()),
            Arg::Text("-a".to_string()),
        ],
        base_show(),
        vec![("ids", V::List(vec![V::Str("-a".to_string())]))],
    );
    // case 201
    assert_err(
        vec![
            Arg::Text("show".to_string()),
            Arg::Text("a".to_string()),
            Arg::Text("--x".to_string()),
        ],
        2,
        "sm: error: unrecognized arguments: --x",
    );
    // case 202
    assert_ok_fields(
        vec![
            Arg::Text("show".to_string()),
            Arg::Text("a".to_string()),
            Arg::Text("--".to_string()),
            Arg::Text("b".to_string()),
        ],
        base_show(),
        vec![],
    );
    // case 203
    assert_err(
        vec![Arg::Text("show".to_string()), Arg::Text("-x".to_string())],
        2,
        "sm show: error: the following arguments are required: ID",
    );
    // case 204
    assert_ok_fields(
        vec![
            Arg::Text("show".to_string()),
            Arg::Text("a".to_string()),
            Arg::Text("-5".to_string()),
        ],
        base_show(),
        vec![(
            "ids",
            V::List(vec![V::Str("a".to_string()), V::Str("-5".to_string())]),
        )],
    );
    // case 205
    assert_err(
        vec![Arg::Text("show".to_string()), Arg::Text("-h".to_string())],
        0,
        "",
    );
}

#[test]
fn argparse_related_cases() {
    // case 206
    assert_err(
        vec![Arg::Text("related".to_string())],
        2,
        "sm related: error: the following arguments are required: id",
    );
    // case 207
    assert_ok_fields(
        vec![
            Arg::Text("related".to_string()),
            Arg::Text("id".to_string()),
        ],
        base_related(),
        vec![],
    );
    // case 208
    assert_ok_fields(
        vec![
            Arg::Text("related".to_string()),
            Arg::Text("id".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("2".to_string()),
            Arg::Text("--format".to_string()),
            Arg::Text("json".to_string()),
        ],
        base_related(),
        vec![("limit", V::Int(2)), ("format", V::Str("json".to_string()))],
    );
    // case 209
    assert_err(
        vec![
            Arg::Text("related".to_string()),
            Arg::Text("id".to_string()),
            Arg::Text("--format".to_string()),
            Arg::Text("xml".to_string()),
        ],
        2,
        "sm related: error: argument --format: invalid choice: 'xml' (choose from 'text', 'json')",
    );
    // case 210
    assert_err(
        vec![
            Arg::Text("related".to_string()),
            Arg::Text("id".to_string()),
            Arg::Text("--format".to_string()),
            Arg::Text("tex".to_string()),
        ],
        2,
        "sm related: error: argument --format: invalid choice: 'tex', maybe you meant 'text'? (choose from 'text', 'json')",
    );
    // case 211
    assert_err(
        vec![
            Arg::Text("related".to_string()),
            Arg::Text("a".to_string()),
            Arg::Text("b".to_string()),
        ],
        2,
        "sm: error: unrecognized arguments: b",
    );
    // case 212
    assert_err(
        vec![
            Arg::Text("related".to_string()),
            Arg::Text("id".to_string()),
            Arg::Text("--l".to_string()),
            Arg::Text("x".to_string()),
        ],
        2,
        "sm related: error: argument --limit: invalid int value: 'x'",
    );
    // case 213
    assert_ok_fields(
        vec![
            Arg::Text("related".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("1".to_string()),
            Arg::Text("id".to_string()),
        ],
        base_related(),
        vec![("limit", V::Int(1))],
    );
    // case 214
    assert_ok_fields(
        vec![
            Arg::Text("related".to_string()),
            Arg::Text("id".to_string()),
            Arg::Text("--f".to_string()),
            Arg::Text("json".to_string()),
        ],
        base_related(),
        vec![("format", V::Str("json".to_string()))],
    );
}

#[test]
fn argparse_supersede_cases() {
    // case 215
    assert_err(
        vec![Arg::Text("supersede".to_string())],
        2,
        "sm supersede: error: the following arguments are required: old_id, new_id",
    );
    // case 216
    assert_err(
        vec![
            Arg::Text("supersede".to_string()),
            Arg::Text("a".to_string()),
        ],
        2,
        "sm supersede: error: the following arguments are required: new_id",
    );
    // case 217
    assert_ok_fields(
        vec![
            Arg::Text("supersede".to_string()),
            Arg::Text("a".to_string()),
            Arg::Text("b".to_string()),
        ],
        base_supersede(),
        vec![],
    );
    // case 218
    assert_err(
        vec![
            Arg::Text("supersede".to_string()),
            Arg::Text("a".to_string()),
            Arg::Text("b".to_string()),
            Arg::Text("c".to_string()),
        ],
        2,
        "sm: error: unrecognized arguments: c",
    );
    // case 219
    assert_ok_fields(
        vec![
            Arg::Text("supersede".to_string()),
            Arg::Text("--".to_string()),
            Arg::Text("-a".to_string()),
            Arg::Text("-b".to_string()),
        ],
        base_supersede(),
        vec![
            ("old_id", V::Str("-a".to_string())),
            ("new_id", V::Str("-b".to_string())),
        ],
    );
    // case 220
    assert_err(
        vec![
            Arg::Text("supersede".to_string()),
            Arg::Text("a".to_string()),
            Arg::Text("--x".to_string()),
            Arg::Text("b".to_string()),
        ],
        2,
        "sm: error: unrecognized arguments: --x",
    );
}

#[test]
fn argparse_import_cases() {
    // case 221
    assert_err(
        vec![Arg::Text("import".to_string())],
        2,
        "sm import: error: the following arguments are required: source_kind",
    );
    // case 222
    assert_err(
        vec![Arg::Text("import".to_string()), Arg::Text("x".to_string())],
        2,
        "sm import: error: argument source_kind: invalid choice: 'x' (choose from 'tasklog', 'handoff', 'legacy', 'proposal', 'automemory', 'sessions', 'jsonl', 'catch-up')",
    );
    // case 223
    assert_err(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("tasklo".to_string()),
            Arg::Text("p".to_string()),
        ],
        2,
        "sm import: error: argument source_kind: invalid choice: 'tasklo', maybe you meant 'tasklog'? (choose from 'tasklog', 'handoff', 'legacy', 'proposal', 'automemory', 'sessions', 'jsonl', 'catch-up')",
    );
    // case 224
    assert_err(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("tasklog".to_string()),
        ],
        2,
        "sm import tasklog: error: the following arguments are required: path",
    );
    // case 225
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("tasklog".to_string()),
            Arg::Text("p".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("tasklog".to_string())),
            ("path", V::Str("p".to_string())),
            ("dry_run", V::Bool(false)),
            ("apply", V::Bool(false)),
        ],
    );
    // case 226
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("tasklog".to_string()),
            Arg::Text("p".to_string()),
            Arg::Text("--dry-run".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("tasklog".to_string())),
            ("path", V::Str("p".to_string())),
            ("dry_run", V::Bool(true)),
            ("apply", V::Bool(false)),
        ],
    );
    // case 227
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("handoff".to_string()),
            Arg::Text("--dry-run".to_string()),
            Arg::Text("p".to_string()),
            Arg::Text("--apply".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("handoff".to_string())),
            ("path", V::Str("p".to_string())),
            ("dry_run", V::Bool(true)),
            ("apply", V::Bool(true)),
        ],
    );
    // case 228
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("legacy".to_string()),
            Arg::Text("p".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("legacy".to_string())),
            ("path", V::Str("p".to_string())),
            ("dry_run", V::Bool(false)),
            ("apply", V::Bool(false)),
        ],
    );
    // case 229
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("proposal".to_string()),
            Arg::Text("p".to_string()),
            Arg::Text("--apply".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("proposal".to_string())),
            ("path", V::Str("p".to_string())),
            ("dry_run", V::Bool(false)),
            ("apply", V::Bool(true)),
        ],
    );
    // case 230
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("proposal".to_string()),
            Arg::Text("p".to_string()),
            Arg::Text("--ap".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("proposal".to_string())),
            ("path", V::Str("p".to_string())),
            ("dry_run", V::Bool(false)),
            ("apply", V::Bool(true)),
        ],
    );
    // case 231
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("proposal".to_string()),
            Arg::Text("p".to_string()),
            Arg::Text("--d".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("proposal".to_string())),
            ("path", V::Str("p".to_string())),
            ("dry_run", V::Bool(true)),
            ("apply", V::Bool(false)),
        ],
    );
    // case 232
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("automemory".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("automemory".to_string())),
            ("dry_run", V::Bool(false)),
            ("root", V::None),
        ],
    );
    // case 233
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("automemory".to_string()),
            Arg::Text("--root".to_string()),
            Arg::Text("r".to_string()),
            Arg::Text("--dry-run".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("automemory".to_string())),
            ("dry_run", V::Bool(true)),
            ("root", V::Str("r".to_string())),
        ],
    );
    // case 234
    assert_err(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("automemory".to_string()),
            Arg::Text("x".to_string()),
        ],
        2,
        "sm: error: unrecognized arguments: x",
    );
    // case 235
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("automemory".to_string()),
            Arg::Text("--r".to_string()),
            Arg::Text("x".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("automemory".to_string())),
            ("dry_run", V::Bool(false)),
            ("root", V::Str("x".to_string())),
        ],
    );
    // case 236
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("sessions".to_string()),
            Arg::Text("--since-days".to_string()),
            Arg::Text("1.5".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("3".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("sessions".to_string())),
            ("dry_run", V::Bool(false)),
            ("since_days", V::Float(1.5_f64)),
            ("limit", V::Int(3)),
        ],
    );
    // case 237
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("sessions".to_string()),
            Arg::Text("--since-days".to_string()),
            Arg::Text("inf".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("sessions".to_string())),
            ("dry_run", V::Bool(false)),
            ("since_days", V::Float(f64::INFINITY)),
            ("limit", V::None),
        ],
    );
    // case 238
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("sessions".to_string()),
            Arg::Text("--since-days".to_string()),
            Arg::Text("nan".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("sessions".to_string())),
            ("dry_run", V::Bool(false)),
            ("since_days", V::Float(f64::NAN)),
            ("limit", V::None),
        ],
    );
    // case 239
    assert_err(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("sessions".to_string()),
            Arg::Text("--since-days".to_string()),
            Arg::Text("-Infinity".to_string()),
        ],
        2,
        "sm import sessions: error: argument --since-days: expected one argument",
    );
    // case 240
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("sessions".to_string()),
            Arg::Text("--since-days".to_string()),
            Arg::Text("1_0.5".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("sessions".to_string())),
            ("dry_run", V::Bool(false)),
            ("since_days", V::Float(10.5_f64)),
            ("limit", V::None),
        ],
    );
    // case 241
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("sessions".to_string()),
            Arg::Text("--since-days".to_string()),
            Arg::Text("\u{663}".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("sessions".to_string())),
            ("dry_run", V::Bool(false)),
            ("since_days", V::Float(3.0_f64)),
            ("limit", V::None),
        ],
    );
    // case 242
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("sessions".to_string()),
            Arg::Text("--since-days".to_string()),
            Arg::Text("\u{663}.\u{665}".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("sessions".to_string())),
            ("dry_run", V::Bool(false)),
            ("since_days", V::Float(3.5_f64)),
            ("limit", V::None),
        ],
    );
    // case 243
    assert_err(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("sessions".to_string()),
            Arg::Text("--since-days".to_string()),
            Arg::Text("x".to_string()),
        ],
        2,
        "sm import sessions: error: argument --since-days: invalid float value: 'x'",
    );
    // case 244
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("sessions".to_string()),
            Arg::Text("--since-days".to_string()),
            Arg::Text("1e3".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("sessions".to_string())),
            ("dry_run", V::Bool(false)),
            ("since_days", V::Float(1000.0_f64)),
            ("limit", V::None),
        ],
    );
    // case 245
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("sessions".to_string()),
            Arg::Text("--since-days".to_string()),
            Arg::Text("-1".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("sessions".to_string())),
            ("dry_run", V::Bool(false)),
            ("since_days", V::Float(-1.0_f64)),
            ("limit", V::None),
        ],
    );
    // case 246
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("sessions".to_string()),
            Arg::Text("--since-days".to_string()),
            Arg::Text(" .5 ".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("sessions".to_string())),
            ("dry_run", V::Bool(false)),
            ("since_days", V::Float(0.5_f64)),
            ("limit", V::None),
        ],
    );
    // case 247
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("sessions".to_string()),
            Arg::Text("--since-days".to_string()),
            Arg::Text("5.".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("sessions".to_string())),
            ("dry_run", V::Bool(false)),
            ("since_days", V::Float(5.0_f64)),
            ("limit", V::None),
        ],
    );
    // case 248
    assert_err(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("sessions".to_string()),
            Arg::Text("--since-days".to_string()),
            Arg::Text("1e".to_string()),
        ],
        2,
        "sm import sessions: error: argument --since-days: invalid float value: '1e'",
    );
    // case 249
    assert_err(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("sessions".to_string()),
            Arg::Text("--since-days".to_string()),
            Arg::Text("1_e5".to_string()),
        ],
        2,
        "sm import sessions: error: argument --since-days: invalid float value: '1_e5'",
    );
    // case 250
    assert_err(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("sessions".to_string()),
            Arg::Text("--since-days".to_string()),
            Arg::Text("0x1p3".to_string()),
        ],
        2,
        "sm import sessions: error: argument --since-days: invalid float value: '0x1p3'",
    );
    // case 251
    assert_err(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("sessions".to_string()),
            Arg::Text("--since-days".to_string()),
            Arg::Text("".to_string()),
        ],
        2,
        "sm import sessions: error: argument --since-days: invalid float value: ''",
    );
    // case 252
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("sessions".to_string()),
            Arg::Text("--since-days".to_string()),
            Arg::Text("1e400".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("sessions".to_string())),
            ("dry_run", V::Bool(false)),
            ("since_days", V::Float(f64::INFINITY)),
            ("limit", V::None),
        ],
    );
    // case 253
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("sessions".to_string()),
            Arg::Text("--since-days".to_string()),
            Arg::Text("\u{2005}1.5".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("sessions".to_string())),
            ("dry_run", V::Bool(false)),
            ("since_days", V::Float(1.5_f64)),
            ("limit", V::None),
        ],
    );
    // case 254
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("sessions".to_string()),
            Arg::Text("--s".to_string()),
            Arg::Text("2".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("sessions".to_string())),
            ("dry_run", V::Bool(false)),
            ("since_days", V::Float(2.0_f64)),
            ("limit", V::None),
        ],
    );
    // case 255
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("sessions".to_string()),
            Arg::Text("--l".to_string()),
            Arg::Text("2".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("sessions".to_string())),
            ("dry_run", V::Bool(false)),
            ("since_days", V::None),
            ("limit", V::Int(2)),
        ],
    );
    // case 256
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("sessions".to_string()),
            Arg::Text("--d".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("sessions".to_string())),
            ("dry_run", V::Bool(true)),
            ("since_days", V::None),
            ("limit", V::None),
        ],
    );
    // case 257
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("jsonl".to_string()),
            Arg::Text("f".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("jsonl".to_string())),
            ("path", V::Str("f".to_string())),
            ("dry_run", V::Bool(false)),
        ],
    );
    // case 258
    assert_err(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("jsonl".to_string()),
        ],
        2,
        "sm import jsonl: error: the following arguments are required: path",
    );
    // case 259
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("jsonl".to_string()),
            Arg::Text("f".to_string()),
            Arg::Text("--dry-run".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("jsonl".to_string())),
            ("path", V::Str("f".to_string())),
            ("dry_run", V::Bool(true)),
        ],
    );
    // case 260
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("catch-up".to_string()),
            Arg::Text("--quiet".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("catch-up".to_string())),
            ("quiet", V::Bool(true)),
        ],
    );
    // case 261
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("catch-up".to_string()),
            Arg::Text("--q".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("catch-up".to_string())),
            ("quiet", V::Bool(true)),
        ],
    );
    // case 262
    assert_err(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("catch-up".to_string()),
            Arg::Text("x".to_string()),
        ],
        2,
        "sm: error: unrecognized arguments: x",
    );
    // case 263
    assert_err(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("catchup".to_string()),
        ],
        2,
        "sm import: error: argument source_kind: invalid choice: 'catchup', maybe you meant 'catch-up'? (choose from 'tasklog', 'handoff', 'legacy', 'proposal', 'automemory', 'sessions', 'jsonl', 'catch-up')",
    );
    // case 264
    assert_err(
        vec![Arg::Text("import".to_string()), Arg::Text("-h".to_string())],
        0,
        "",
    );
    // case 265
    assert_err(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("tasklog".to_string()),
            Arg::Text("-h".to_string()),
        ],
        0,
        "",
    );
    // case 266
    assert_err(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("--dry-run".to_string()),
            Arg::Text("tasklog".to_string()),
            Arg::Text("p".to_string()),
        ],
        2,
        "sm: error: unrecognized arguments: --dry-run",
    );
    // case 267
    assert_err(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("tasklog".to_string()),
            Arg::Text("p".to_string()),
            Arg::Text("--zz".to_string()),
        ],
        2,
        "sm: error: unrecognized arguments: --zz",
    );
    // case 269
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("--".to_string()),
            Arg::Text("tasklog".to_string()),
            Arg::Text("p".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("tasklog".to_string())),
            ("path", V::Str("p".to_string())),
            ("dry_run", V::Bool(false)),
            ("apply", V::Bool(false)),
        ],
    );
    // case 270
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("tasklog".to_string()),
            Arg::Text("--".to_string()),
            Arg::Text("-p".to_string()),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("tasklog".to_string())),
            ("path", V::Str("-p".to_string())),
            ("dry_run", V::Bool(false)),
            ("apply", V::Bool(false)),
        ],
    );
    // case 271
    assert_ok_fields(
        vec![
            Arg::Text("import".to_string()),
            Arg::Text("tasklog".to_string()),
            Arg::Bytes(vec![0x2f, 0x74, 0x6d, 0x70, 0x2f, 0x61, 0xff]),
        ],
        vec![],
        vec![
            ("version", V::None),
            ("command", V::Str("import".to_string())),
            ("source_kind", V::Str("tasklog".to_string())),
            (
                "path",
                V::BytesStr(vec![0x2f, 0x74, 0x6d, 0x70, 0x2f, 0x61, 0xff]),
            ),
            ("dry_run", V::Bool(false)),
            ("apply", V::Bool(false)),
        ],
    );
}

#[test]
fn argparse_export_cases() {
    // case 272
    assert_ok_fields(vec![Arg::Text("export".to_string())], base_export(), vec![]);
    // case 273
    assert_ok_fields(
        vec![
            Arg::Text("export".to_string()),
            Arg::Text("--all".to_string()),
        ],
        base_export(),
        vec![("all_repos", V::Bool(true))],
    );
    // case 274
    assert_ok_fields(
        vec![
            Arg::Text("export".to_string()),
            Arg::Text("--repo".to_string()),
            Arg::Text("r".to_string()),
        ],
        base_export(),
        vec![("repo", V::Str("r".to_string()))],
    );
    // case 275
    assert_err(
        vec![
            Arg::Text("export".to_string()),
            Arg::Text("--repo".to_string()),
            Arg::Text("r".to_string()),
            Arg::Text("--all".to_string()),
        ],
        2,
        "sm export: error: argument --all: not allowed with argument --repo",
    );
    // case 276
    assert_err(
        vec![
            Arg::Text("export".to_string()),
            Arg::Text("--all".to_string()),
            Arg::Text("--repo".to_string()),
            Arg::Text("r".to_string()),
        ],
        2,
        "sm export: error: argument --repo: not allowed with argument --all",
    );
    // case 277
    assert_ok_fields(
        vec![
            Arg::Text("export".to_string()),
            Arg::Text("--include-history".to_string()),
        ],
        base_export(),
        vec![("include_history", V::Bool(true))],
    );
    // case 278
    assert_err(
        vec![
            Arg::Text("export".to_string()),
            Arg::Text("--repo=".to_string()),
            Arg::Text("--all".to_string()),
        ],
        2,
        "sm export: error: argument --all: not allowed with argument --repo",
    );
    // case 279
    assert_err(
        vec![Arg::Text("export".to_string()), Arg::Text("x".to_string())],
        2,
        "sm: error: unrecognized arguments: x",
    );
    // case 280
    assert_ok_fields(
        vec![
            Arg::Text("export".to_string()),
            Arg::Text("--a".to_string()),
        ],
        base_export(),
        vec![("all_repos", V::Bool(true))],
    );
    // case 281
    assert_ok_fields(
        vec![
            Arg::Text("export".to_string()),
            Arg::Text("--r".to_string()),
            Arg::Text("x".to_string()),
        ],
        base_export(),
        vec![("repo", V::Str("x".to_string()))],
    );
    // case 282
    assert_ok_fields(
        vec![
            Arg::Text("export".to_string()),
            Arg::Text("--i".to_string()),
        ],
        base_export(),
        vec![("include_history", V::Bool(true))],
    );
}

#[test]
fn argparse_backup_cases() {
    // case 283
    assert_ok_fields(vec![Arg::Text("backup".to_string())], base_backup(), vec![]);
    // case 284
    assert_ok_fields(
        vec![
            Arg::Text("backup".to_string()),
            Arg::Text("--keep".to_string()),
            Arg::Text("3".to_string()),
        ],
        base_backup(),
        vec![("keep", V::Int(3))],
    );
    // case 285
    assert_err(
        vec![
            Arg::Text("backup".to_string()),
            Arg::Text("--keep".to_string()),
            Arg::Text("x".to_string()),
        ],
        2,
        "sm backup: error: argument --keep: invalid int value: 'x'",
    );
    // case 286
    assert_ok_fields(
        vec![
            Arg::Text("backup".to_string()),
            Arg::Text("--k".to_string()),
            Arg::Text("2".to_string()),
        ],
        base_backup(),
        vec![("keep", V::Int(2))],
    );
    // case 287
    assert_ok_fields(
        vec![
            Arg::Text("backup".to_string()),
            Arg::Text("--keep".to_string()),
            Arg::Text("-1".to_string()),
        ],
        base_backup(),
        vec![("keep", V::Int(-1))],
    );
    // case 288
    assert_err(
        vec![
            Arg::Text("backup".to_string()),
            Arg::Text("--keep".to_string()),
        ],
        2,
        "sm backup: error: argument --keep: expected one argument",
    );
}

#[test]
fn argparse_hook_cases() {
    // case 289
    assert_err(
        vec![Arg::Text("hook".to_string())],
        2,
        "sm hook: error: the following arguments are required: event",
    );
    // case 290
    assert_ok_fields(
        vec![
            Arg::Text("hook".to_string()),
            Arg::Text("prompt".to_string()),
        ],
        base_hook(),
        vec![("agy", V::Bool(false))],
    );
    // case 291
    assert_ok_fields(
        vec![
            Arg::Text("hook".to_string()),
            Arg::Text("prompt".to_string()),
            Arg::Text("--agy".to_string()),
        ],
        base_hook(),
        vec![],
    );
    // case 292
    assert_err(
        vec![Arg::Text("hook".to_string()), Arg::Text("prom".to_string())],
        2,
        "sm hook: error: argument event: invalid choice: 'prom', maybe you meant 'prompt'? (choose from 'prompt')",
    );
    // case 293
    assert_err(
        vec![Arg::Text("hook".to_string()), Arg::Text("x".to_string())],
        2,
        "sm hook: error: argument event: invalid choice: 'x' (choose from 'prompt')",
    );
    // case 294
    assert_ok_fields(
        vec![
            Arg::Text("hook".to_string()),
            Arg::Text("prompt".to_string()),
            Arg::Text("--ag".to_string()),
        ],
        base_hook(),
        vec![],
    );
    // case 295
    assert_ok_fields(
        vec![
            Arg::Text("hook".to_string()),
            Arg::Text("--agy".to_string()),
            Arg::Text("prompt".to_string()),
        ],
        base_hook(),
        vec![],
    );
    // case 296
    assert_err(
        vec![
            Arg::Text("hook".to_string()),
            Arg::Text("prompt".to_string()),
            Arg::Text("prompt".to_string()),
        ],
        2,
        "sm: error: unrecognized arguments: prompt",
    );
}

#[test]
fn argparse_doctor_cases() {
    // case 297
    assert_ok_fields(vec![Arg::Text("doctor".to_string())], base_doctor(), vec![]);
    // case 298
    assert_ok_fields(
        vec![
            Arg::Text("doctor".to_string()),
            Arg::Text("--rebuild-fts".to_string()),
            Arg::Text("--backfill-refs".to_string()),
            Arg::Text("--dry-run".to_string()),
            Arg::Text("--imports".to_string()),
        ],
        base_doctor(),
        vec![
            ("rebuild_fts", V::Bool(true)),
            ("backfill_refs", V::Bool(true)),
            ("dry_run", V::Bool(true)),
            ("imports", V::Bool(true)),
        ],
    );
    // case 299
    assert_ok_fields(
        vec![
            Arg::Text("doctor".to_string()),
            Arg::Text("--re".to_string()),
        ],
        base_doctor(),
        vec![("rebuild_fts", V::Bool(true))],
    );
    // case 300
    assert_ok_fields(
        vec![
            Arg::Text("doctor".to_string()),
            Arg::Text("--d".to_string()),
        ],
        base_doctor(),
        vec![("dry_run", V::Bool(true))],
    );
    // case 301
    assert_ok_fields(
        vec![
            Arg::Text("doctor".to_string()),
            Arg::Text("--i".to_string()),
        ],
        base_doctor(),
        vec![("imports", V::Bool(true))],
    );
    // case 302
    assert_ok_fields(
        vec![
            Arg::Text("doctor".to_string()),
            Arg::Text("--b".to_string()),
        ],
        base_doctor(),
        vec![("backfill_refs", V::Bool(true))],
    );
    // case 303
    assert_err(
        vec![
            Arg::Text("doctor".to_string()),
            Arg::Text("--imports=1".to_string()),
        ],
        2,
        "sm doctor: error: argument --imports: ignored explicit argument '1'",
    );
    // case 304
    assert_err(
        vec![Arg::Text("doctor".to_string()), Arg::Text("x".to_string())],
        2,
        "sm: error: unrecognized arguments: x",
    );
}

#[test]
fn argparse_upkeep_cases() {
    // case 305
    assert_ok_fields(
        vec![Arg::Text("upkeep".to_string())],
        base_upkeep(),
        vec![("report", V::Bool(false))],
    );
    // case 306
    assert_ok_fields(
        vec![
            Arg::Text("upkeep".to_string()),
            Arg::Text("--report".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("5".to_string()),
        ],
        base_upkeep(),
        vec![("limit", V::Int(5))],
    );
    // case 307
    assert_ok_fields(
        vec![
            Arg::Text("upkeep".to_string()),
            Arg::Text("--dry-run".to_string()),
        ],
        base_upkeep(),
        vec![("dry_run", V::Bool(true)), ("report", V::Bool(false))],
    );
    // case 308
    assert_ok_fields(
        vec![
            Arg::Text("upkeep".to_string()),
            Arg::Text("--r".to_string()),
        ],
        base_upkeep(),
        vec![],
    );
    // case 309
    assert_err(
        vec![
            Arg::Text("upkeep".to_string()),
            Arg::Text("--l".to_string()),
            Arg::Text("x".to_string()),
        ],
        2,
        "sm upkeep: error: argument --limit: invalid int value: 'x'",
    );
    // case 310
    assert_ok_fields(
        vec![
            Arg::Text("upkeep".to_string()),
            Arg::Text("--report".to_string()),
            Arg::Text("--dry-run".to_string()),
        ],
        base_upkeep(),
        vec![("dry_run", V::Bool(true))],
    );
}

#[test]
fn argparse_stats_cases() {
    // case 311
    assert_ok_fields(vec![Arg::Text("stats".to_string())], base_stats(), vec![]);
    // case 312
    assert_ok_fields(
        vec![
            Arg::Text("stats".to_string()),
            Arg::Text("--days".to_string()),
            Arg::Text("1".to_string()),
        ],
        base_stats(),
        vec![("days", V::Float(1.0_f64))],
    );
    // case 313
    assert_ok_fields(
        vec![
            Arg::Text("stats".to_string()),
            Arg::Text("--days".to_string()),
            Arg::Text("0.5".to_string()),
            Arg::Text("--format".to_string()),
            Arg::Text("json".to_string()),
        ],
        base_stats(),
        vec![
            ("days", V::Float(0.5_f64)),
            ("format", V::Str("json".to_string())),
        ],
    );
    // case 314
    assert_err(
        vec![
            Arg::Text("stats".to_string()),
            Arg::Text("--days".to_string()),
            Arg::Text("x".to_string()),
        ],
        2,
        "sm stats: error: argument --days: invalid float value: 'x'",
    );
    // case 315
    assert_err(
        vec![
            Arg::Text("stats".to_string()),
            Arg::Text("--days".to_string()),
            Arg::Text("-inf".to_string()),
        ],
        2,
        "sm stats: error: argument --days: expected one argument",
    );
    // case 316
    assert_ok_fields(
        vec![
            Arg::Text("stats".to_string()),
            Arg::Text("--days".to_string()),
            Arg::Text("1_000".to_string()),
        ],
        base_stats(),
        vec![("days", V::Float(1000.0_f64))],
    );
    // case 317
    assert_err(
        vec![
            Arg::Text("stats".to_string()),
            Arg::Text("--format".to_string()),
            Arg::Text("jso".to_string()),
        ],
        2,
        "sm stats: error: argument --format: invalid choice: 'jso', maybe you meant 'json'? (choose from 'text', 'json')",
    );
    // case 318
    assert_ok_fields(
        vec![
            Arg::Text("stats".to_string()),
            Arg::Text("--d".to_string()),
            Arg::Text("3".to_string()),
        ],
        base_stats(),
        vec![("days", V::Float(3.0_f64))],
    );
    // case 319
    assert_ok_fields(
        vec![
            Arg::Text("stats".to_string()),
            Arg::Text("--f".to_string()),
            Arg::Text("text".to_string()),
        ],
        base_stats(),
        vec![],
    );
    // case 320
    assert_ok_fields(
        vec![
            Arg::Text("stats".to_string()),
            Arg::Text("--days".to_string()),
            Arg::Text("\u{663}".to_string()),
        ],
        base_stats(),
        vec![("days", V::Float(3.0_f64))],
    );
    // case 321
    assert_ok_fields(
        vec![
            Arg::Text("stats".to_string()),
            Arg::Text("--days".to_string()),
            Arg::Text("1.5e-3".to_string()),
        ],
        base_stats(),
        vec![("days", V::Float(0.0015_f64))],
    );
    // case 322
    assert_err(
        vec![
            Arg::Text("stats".to_string()),
            Arg::Text("--days".to_string()),
            Arg::Text("\u{1c}5".to_string()),
        ],
        2,
        "sm stats: error: argument --days: invalid float value: '\\x1c5'",
    );
    // case 323
    assert_err(
        vec![
            Arg::Text("stats".to_string()),
            Arg::Text("--days".to_string()),
            Arg::Text("5\u{1f}".to_string()),
        ],
        2,
        "sm stats: error: argument --days: invalid float value: '5\\x1f'",
    );
    // case 324
    assert_ok_fields(
        vec![
            Arg::Text("stats".to_string()),
            Arg::Text("--days".to_string()),
            Arg::Text("\u{3000}5".to_string()),
        ],
        base_stats(),
        vec![("days", V::Float(5.0_f64))],
    );
    // case 325
    assert_err(
        vec![
            Arg::Text("stats".to_string()),
            Arg::Text("--days".to_string()),
            Arg::Text("5?".to_string()),
        ],
        2,
        "sm stats: error: argument --days: invalid float value: '5?'",
    );
}

#[test]
fn argparse_init_cases() {
    // case 326
    assert_ok_fields(vec![Arg::Text("init".to_string())], base_init(), vec![]);
    // case 327
    assert_err(
        vec![Arg::Text("init".to_string()), Arg::Text("x".to_string())],
        2,
        "sm: error: unrecognized arguments: x",
    );
    // case 344
    assert_err(
        vec![Arg::Text("init".to_string()), Arg::Text("-h".to_string())],
        0,
        "",
    );
}

#[test]
fn argparse_config_cases() {
    // case 328
    assert_ok_fields(vec![Arg::Text("config".to_string())], base_config(), vec![]);
    // case 329
    assert_err(
        vec![
            Arg::Text("config".to_string()),
            Arg::Text("--x".to_string()),
        ],
        2,
        "sm: error: unrecognized arguments: --x",
    );
    // case 345
    assert_err(
        vec![
            Arg::Text("config".to_string()),
            Arg::Text("--h".to_string()),
        ],
        0,
        "",
    );
}

#[test]
fn argparse_hooks_cases() {
    // case 330
    assert_err(
        vec![Arg::Text("hooks".to_string())],
        2,
        "sm hooks: error: the following arguments are required: hooks_action",
    );
    // case 331
    assert_err(
        vec![
            Arg::Text("hooks".to_string()),
            Arg::Text("print".to_string()),
        ],
        2,
        "sm hooks print: error: the following arguments are required: harness",
    );
    // case 332
    assert_ok_fields(
        vec![
            Arg::Text("hooks".to_string()),
            Arg::Text("print".to_string()),
            Arg::Text("agy".to_string()),
        ],
        base_hooks(),
        vec![],
    );
    // case 333
    assert_ok_fields(
        vec![
            Arg::Text("hooks".to_string()),
            Arg::Text("print".to_string()),
            Arg::Text("claude-code".to_string()),
        ],
        base_hooks(),
        vec![("harness", V::Str("claude-code".to_string()))],
    );
    // case 334
    assert_ok_fields(
        vec![
            Arg::Text("hooks".to_string()),
            Arg::Text("print".to_string()),
            Arg::Text("mcp".to_string()),
        ],
        base_hooks(),
        vec![("harness", V::Str("mcp".to_string()))],
    );
    // case 335
    assert_err(
        vec![
            Arg::Text("hooks".to_string()),
            Arg::Text("print".to_string()),
            Arg::Text("claude".to_string()),
        ],
        2,
        "sm hooks print: error: argument harness: invalid choice: 'claude', maybe you meant 'claude-code'? (choose from 'claude-code', 'agy', 'mcp')",
    );
    // case 336
    assert_err(
        vec![
            Arg::Text("hooks".to_string()),
            Arg::Text("prnt".to_string()),
            Arg::Text("agy".to_string()),
        ],
        2,
        "sm hooks: error: argument hooks_action: invalid choice: 'prnt', maybe you meant 'print'? (choose from 'print')",
    );
    // case 337
    assert_err(
        vec![
            Arg::Text("hooks".to_string()),
            Arg::Text("print".to_string()),
            Arg::Text("agy".to_string()),
            Arg::Text("x".to_string()),
        ],
        2,
        "sm: error: unrecognized arguments: x",
    );
    // case 338
    assert_err(
        vec![
            Arg::Text("hooks".to_string()),
            Arg::Text("print".to_string()),
            Arg::Text("-h".to_string()),
        ],
        0,
        "",
    );
    // case 339
    assert_err(
        vec![Arg::Text("hooks".to_string()), Arg::Text("-h".to_string())],
        0,
        "",
    );
    // case 340
    assert_err(
        vec![
            Arg::Text("hooks".to_string()),
            Arg::Text("print".to_string()),
            Arg::Text("editor".to_string()),
        ],
        2,
        "sm hooks print: error: argument harness: invalid choice: 'editor' (choose from 'claude-code', 'agy', 'mcp')",
    );
}

#[test]
fn argparse_mcp_cases() {
    // case 341
    assert_ok_fields(vec![Arg::Text("mcp".to_string())], base_mcp(), vec![]);
    // case 342
    assert_err(
        vec![Arg::Text("mcp".to_string()), Arg::Text("--x".to_string())],
        2,
        "sm: error: unrecognized arguments: --x",
    );
    // case 343
    assert_err(
        vec![Arg::Text("mcp".to_string()), Arg::Text("x".to_string())],
        2,
        "sm: error: unrecognized arguments: x",
    );
}

fn base_list() -> Vec<(&'static str, V)> {
    vec![
        ("version", V::None),
        ("command", V::Str("list".to_string())),
        ("limit", V::Int(20)),
        ("offset", V::Int(0)),
        ("repo", V::Str("".to_string())),
        ("all_repos", V::Bool(false)),
        ("in", V::Str("".to_string())),
        ("kind", V::Str("".to_string())),
        ("tag", V::Str("".to_string())),
        ("source", V::Str("".to_string())),
        ("since", V::Str("".to_string())),
        ("until", V::Str("".to_string())),
        ("status", V::None),
        ("include_history", V::Bool(false)),
        ("format", V::Str("brief".to_string())),
    ]
}

#[test]
fn argparse_list_cases() {
    assert_ok_fields(vec![Arg::Text("list".to_string())], base_list(), vec![]);
    assert_ok_fields(
        vec![
            Arg::Text("list".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("5".to_string()),
            Arg::Text("--offset".to_string()),
            Arg::Text("10".to_string()),
            Arg::Text("--repo".to_string()),
            Arg::Text("anvil".to_string()),
            Arg::Text("--kind".to_string()),
            Arg::Text("fact".to_string()),
            Arg::Text("--kind".to_string()),
            Arg::Text("decision".to_string()),
            Arg::Text("--tag".to_string()),
            Arg::Text("decided".to_string()),
            Arg::Text("--source".to_string()),
            Arg::Text("task:".to_string()),
            Arg::Text("--source".to_string()),
            Arg::Text("agent:".to_string()),
            Arg::Text("--since".to_string()),
            Arg::Text("2026-01-01".to_string()),
            Arg::Text("--until".to_string()),
            Arg::Text("2026-12-31".to_string()),
            Arg::Text("--status".to_string()),
            Arg::Text("retired".to_string()),
            Arg::Text("--format".to_string()),
            Arg::Text("json".to_string()),
        ],
        base_list(),
        vec![
            ("limit", V::Int(5)),
            ("offset", V::Int(10)),
            ("repo", V::Str("anvil".to_string())),
            ("kind", V::Str("fact,decision".to_string())),
            ("tag", V::Str("decided".to_string())),
            ("source", V::Str("task:,agent:".to_string())),
            ("since", V::Str("2026-01-01".to_string())),
            ("until", V::Str("2026-12-31".to_string())),
            ("status", V::Str("retired".to_string())),
            ("format", V::Str("json".to_string())),
        ],
    );
    assert_ok_fields(
        vec![
            Arg::Text("list".to_string()),
            Arg::Text("--in".to_string()),
            Arg::Text("acme".to_string()),
            Arg::Text("--in".to_string()),
            Arg::Text("globex".to_string()),
            Arg::Text("--include-history".to_string()),
        ],
        base_list(),
        vec![
            ("in", V::Str("acme,globex".to_string())),
            ("include_history", V::Bool(true)),
        ],
    );
    // Scope mutex: --repo / --all / --in
    assert_err(
        vec![
            Arg::Text("list".to_string()),
            Arg::Text("--repo".to_string()),
            Arg::Text("anvil".to_string()),
            Arg::Text("--all".to_string()),
        ],
        2,
        "sm list: error: argument --all: not allowed with argument --repo",
    );
    assert_err(
        vec![
            Arg::Text("list".to_string()),
            Arg::Text("--repo".to_string()),
            Arg::Text("anvil".to_string()),
            Arg::Text("--in".to_string()),
            Arg::Text("acme".to_string()),
        ],
        2,
        "sm list: error: argument --in: not allowed with argument --repo",
    );
    assert_err(
        vec![
            Arg::Text("list".to_string()),
            Arg::Text("--all".to_string()),
            Arg::Text("--in".to_string()),
            Arg::Text("acme".to_string()),
        ],
        2,
        "sm list: error: argument --in: not allowed with argument --all",
    );
    // Status mutex: --status vs --include-history
    assert_err(
        vec![
            Arg::Text("list".to_string()),
            Arg::Text("--status".to_string()),
            Arg::Text("active".to_string()),
            Arg::Text("--include-history".to_string()),
        ],
        2,
        "sm list: error: argument --include-history: not allowed with argument --status",
    );
    // Invalid status choice
    assert_err(
        vec![
            Arg::Text("list".to_string()),
            Arg::Text("--status".to_string()),
            Arg::Text("bogus".to_string()),
        ],
        2,
        "sm list: error: argument --status: invalid choice: 'bogus' (choose from 'active', 'superseded', 'retired', 'all')",
    );
    // BigInt offset and limit parse as Value::BigInt
    assert_ok_fields(
        vec![
            Arg::Text("list".to_string()),
            Arg::Text("--limit".to_string()),
            Arg::Text("99999999999999999999".to_string()),
            Arg::Text("--offset".to_string()),
            Arg::Text("99999999999999999999".to_string()),
        ],
        base_list(),
        vec![
            ("limit", V::BigInt("99999999999999999999".to_string())),
            ("offset", V::BigInt("99999999999999999999".to_string())),
        ],
    );
}
