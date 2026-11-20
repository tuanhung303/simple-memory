//! `sm mcp` as its clients run it: a subprocess that speaks JSON-RPC 2.0 over its pipes.
//! Port of the Python MCP tests (Python main 555b8f2), one Rust test per Python test; a
//! parametrized Python test is one Rust test that loops its cases and names the case in each
//! assertion message.
//!
//! The handshake mirrors what Claude Code 2.1.317 sent a probe server on 2026-11-04. Every sm
//! process runs with only PATH, HOME and SIMPLE_MEMORY_HOME (a temp folder). Where the Python
//! test reads the installed package version, this one reads the crate version: the Rust server
//! reports its own (an accepted difference, service/mcp.rs `server_info`).

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const VERSIONS: [&str; 5] = [
    "2026-07-28",
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];

fn init() -> Value {
    json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}})
}

fn initialized() -> Value {
    json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
}

/// Per-request metadata of revision 2026-07-28, shaped as Claude Code 2.1.317 sent it on 2026-11-04.
fn meta() -> Value {
    json!({"io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name": "t", "version": "1"},
        "io.modelcontextprotocol/clientCapabilities": {"roots": {"listChanged": true}}})
}

fn server_meta() -> Value {
    json!({"io.modelcontextprotocol/serverInfo": {"name": "simple-memory", "title": "Simple Memory", "version": VERSION}})
}

fn modern(rid: Value, method: &str, params: Value, meta_override: Option<Value>) -> Value {
    let mut p = params.as_object().cloned().unwrap_or_default();
    p.insert("_meta".into(), meta_override.unwrap_or_else(meta));
    json!({"jsonrpc": "2.0", "id": rid, "method": method, "params": p})
}

fn call(rid: i64, name: &str, arguments: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": rid, "method": "tools/call", "params": {"name": name, "arguments": arguments}})
}

fn ping(rid: i64) -> Value {
    json!({"jsonrpc": "2.0", "id": rid, "method": "ping"})
}

/// A line as written: a JSON message, or raw bytes.
enum Line {
    Msg(Value),
    Raw(&'static [u8]),
}

impl From<Value> for Line {
    fn from(v: Value) -> Line {
        Line::Msg(v)
    }
}

struct Fx {
    tmp: tempfile::TempDir,
    home_override: Option<PathBuf>,
}

impl Fx {
    fn new() -> Fx {
        Fx {
            tmp: tempfile::tempdir().unwrap(),
            home_override: None,
        }
    }

    fn root(&self) -> &Path {
        self.tmp.path()
    }

    fn home(&self) -> PathBuf {
        self.home_override
            .clone()
            .unwrap_or_else(|| self.root().join("home"))
    }

    /// Only what the server needs, so a failing test prints no copy of the environment.
    fn command(&self, args: &[&str], cwd: &Path) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_sm"));
        c.args(args)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap())
            .env("HOME", self.root())
            .env("SIMPLE_MEMORY_HOME", self.home())
            .current_dir(cwd);
        for key in ["TZ", "SM_UTC_OFFSET"] {
            if let Some(val) = std::env::var_os(key) {
                c.env(key, val);
            }
        }
        c
    }

    /// A git checkout named alpha: the server's cwd, so its repo key is `alpha`.
    fn repo_dir(&self) -> PathBuf {
        let d = self.root().join("alpha");
        std::fs::create_dir(&d).unwrap();
        git_init(&d);
        d
    }

    /// Send each line, close stdin; (responses, stderr). Every stdout line must be a JSON-RPC 2.0
    /// message: stdout carries the protocol only.
    fn converse(&self, lines: Vec<Line>, cwd: &Path) -> (Vec<Value>, String) {
        let mut data: Vec<u8> = Vec::new();
        for l in lines {
            match l {
                Line::Msg(v) => {
                    data.extend(serde_json::to_vec(&v).unwrap());
                    data.push(b'\n');
                }
                Line::Raw(b) => data.extend(b),
            }
        }
        let mut child = self
            .command(&["mcp"], cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let writer = std::thread::spawn(move || stdin.write_all(&data));
        let out = child.wait_with_output().unwrap();
        let _ = writer.join();
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(out.status.success(), "{stderr}");
        let responses: Vec<Value> = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(|l| {
                serde_json::from_str(l)
                    .unwrap_or_else(|e| panic!("stdout line is no JSON ({e}): {l}"))
            })
            .collect();
        assert!(responses.iter().all(|m| m["jsonrpc"] == "2.0"));
        (responses, stderr)
    }

    fn conv(&self, msgs: Vec<Value>, cwd: &Path) -> (Vec<Value>, String) {
        self.converse(msgs.into_iter().map(Line::Msg).collect(), cwd)
    }

    /// The sm CLI with the same environment.
    fn cli(&self, args: &[&str], cwd: &Path) -> std::process::Output {
        self.command(args, cwd)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn cli_json(&self, args: &[&str], cwd: &Path) -> Value {
        let o = self.cli(args, cwd);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        serde_json::from_slice(&o.stdout).unwrap()
    }
}

fn git_init(dir: &Path) {
    let s = Command::new("git")
        .args(["init", "-q"])
        .arg(dir)
        .status()
        .unwrap();
    assert!(s.success());
}

/// The JSON a tool answer carries in its one text content block.
fn payload(response: &Value) -> Value {
    let content = response["result"]["content"].as_array().unwrap();
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["type"], "text");
    serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap()
}

fn tool_names() -> Vec<&'static str> {
    vec![
        "memory_query",
        "memory_show",
        "memory_related",
        "memory_put",
        "memory_supersede",
    ]
}

#[test]
fn handshake_as_claude_code_runs_it() {
    // A legacy client's probe carries no `_meta`, so it is malformed (-32602) and the client falls back.
    let fx = Fx::new();
    let repo = fx.repo_dir();
    let (out, err) = fx.conv(
        vec![
            json!({"jsonrpc": "2.0", "id": "server-discover-probe-1", "method": "server/discover", "params": {}}),
            init(),
            initialized(),
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
            ping(2),
        ],
        &repo,
    );
    assert_eq!(
        out[0],
        json!({"jsonrpc": "2.0", "id": "server-discover-probe-1", "error": {
            "code": -32602, "message": "_meta needs io.modelcontextprotocol/protocolVersion and clientCapabilities"}})
    );
    let init = &out[1]["result"];
    assert_eq!(out[1]["id"], 0);
    assert_eq!(init["protocolVersion"], "2025-11-25");
    assert_eq!(
        init["capabilities"],
        json!({"tools": {"listChanged": false}})
    );
    assert_eq!(init["serverInfo"]["name"], "simple-memory");
    assert_eq!(init["serverInfo"]["version"], VERSION);
    // the notification got no response: the next line answers id 1
    assert_eq!(out[2]["id"], 1);
    let tools = out[2]["result"]["tools"].as_array().unwrap();
    assert_eq!(
        tools
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        tool_names()
    );
    assert_eq!(tools[0]["inputSchema"]["required"], json!(["query"]));
    assert_eq!(out[3], json!({"jsonrpc": "2.0", "id": 2, "result": {}}));
    assert_eq!(out.len(), 4);
    assert!(
        err.starts_with("sm mcp: serving MCP 2026-07-28 and 2025-11-25 on stdio"),
        "{err}"
    );
}

#[test]
fn each_answer_is_flushed_before_the_next_request() {
    // A client waits for each answer with stdin still open, as Claude Code does.
    let fx = Fx::new();
    let repo = fx.repo_dir();
    let mut child = fx
        .command(&["mcp"], &repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let (tx, rx) = mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if tx.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    for rid in 0..2 {
        let msg = if rid == 0 { init() } else { ping(rid) };
        stdin.write_all(&serde_json::to_vec(&msg).unwrap()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
        let line = rx
            .recv_timeout(Duration::from_secs(20))
            .unwrap_or_else(|_| panic!("no answer to request {rid} within 20 s"));
        assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["id"], rid);
    }
    drop(stdin);
    assert!(child.wait().unwrap().success());
}

#[test]
fn initialize_negotiates_the_version() {
    let fx = Fx::new();
    let repo = fx.repo_dir();
    for (asked, answered) in [
        ("2024-11-05", "2024-11-05"),
        ("2025-06-18", "2025-06-18"),
        ("2026-07-28", "2025-11-25"),
        ("1.0.0", "2025-11-25"),
    ] {
        let mut msg = init();
        msg["params"]["protocolVersion"] = json!(asked);
        let (out, _) = fx.conv(vec![msg], &repo);
        assert_eq!(
            out[0]["result"]["protocolVersion"], answered,
            "asked {asked}"
        );
    }
}

#[test]
fn protocol_errors_keep_the_server_running() {
    let fx = Fx::new();
    let repo = fx.repo_dir();
    let (out, err) = fx.converse(
        vec![
            Line::Raw(b"{not json\n"),
            Line::Raw(b"\xff\xfe\n"),
            Line::Raw(b"\n"),
            json!([init()]).into(),
            json!({"id": 5, "method": "ping"}).into(),
            json!({"jsonrpc": "2.0", "id": 6, "method": 7}).into(),
            json!({"jsonrpc": "2.0", "id": 7, "method": "resources/list"}).into(),
            json!({"jsonrpc": "2.0", "method": "notifications/unknown"}).into(),
            json!({"jsonrpc": "2.0", "id": 8, "method": "initialize", "params": {}}).into(),
            json!({"jsonrpc": "2.0", "id": 9, "method": "tools/list", "params": [1]}).into(),
            json!({"jsonrpc": "2.0", "id": 10, "method": "tools/call", "params": {"name": "memory_drop", "arguments": {}}}).into(),
            json!({"jsonrpc": "2.0", "id": 11, "method": "tools/call", "params": {"name": "memory_show", "arguments": [1]}}).into(),
            ping(12).into(),
        ],
        &repo,
    );
    let got: Vec<(Value, Value)> = out
        .iter()
        .map(|m| (m["id"].clone(), m["error"]["code"].clone()))
        .collect();
    let want: Vec<(Value, Value)> = [
        (None, Some(-32700)),
        (None, Some(-32700)),
        (None, Some(-32600)),
        (Some(5), Some(-32600)),
        (Some(6), Some(-32600)),
        (Some(7), Some(-32601)),
        (Some(8), Some(-32602)),
        (Some(9), Some(-32602)),
        (Some(10), Some(-32602)),
        (Some(11), Some(-32602)),
        (Some(12), None),
    ]
    .iter()
    .map(|(i, c)| {
        (
            i.map_or(Value::Null, |i| json!(i)),
            c.map_or(Value::Null, |c| json!(c)),
        )
    })
    .collect();
    assert_eq!(got, want);
    assert_eq!(out[8]["error"]["message"], "Unknown tool: memory_drop");
    assert_eq!(err.matches("sm mcp: parse error").count(), 2, "{err}");
}

#[test]
fn put_query_show_round_trip_scoped_to_the_cwd_repo() {
    let fx = Fx::new();
    let repo = fx.repo_dir();
    let (out, _) = fx.conv(
        vec![
            init(),
            initialized(),
            call(1, "memory_put", json!({"text": "Deploy window is Tuesday after the B review", "tag": "decided", "refs": "T42"})),
            call(2, "memory_query", json!({"query": "deploy window"})),
            call(3, "memory_query", json!({"query": "deploy window", "repo": "beta"})),
        ],
        &repo,
    );
    let put = payload(&out[1]);
    assert_eq!(
        (
            put["created"].clone(),
            put["overlaps"].clone(),
            out[1]["result"]["isError"].clone()
        ),
        (json!(true), json!([]), json!(false))
    );
    let found = payload(&out[2]);
    assert_eq!(found["scope"], json!({"repo": "alpha", "all_repos": false}));
    let rows: Vec<Value> = found["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| json!([r["id"], r["repo"], r["tag"], r["refs"]]))
        .collect();
    assert_eq!(rows, vec![json!([put["id"], "alpha", "decided", ["T42"]])]);
    assert_eq!(payload(&out[3])["results"], json!([]));

    let (out, _) = fx.conv(
        vec![
            init(),
            call(1, "memory_show", json!({"ids": [put["id"], "m_missing"]})),
        ],
        &repo,
    );
    let shown = payload(&out[1]);
    assert_eq!(out[1]["result"]["isError"], true);
    assert_eq!(shown["missing"], json!(["m_missing"]));
    let items = shown["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(
        json!([
            item["id"],
            item["repo"],
            item["kind"],
            item["body"],
            item["status"]
        ]),
        json!([
            put["id"],
            "alpha",
            "fact",
            "Deploy window is Tuesday after the B review",
            "active"
        ])
    );
}

#[test]
fn query_in_keeps_the_scope_across_repos() {
    let fx = Fx::new();
    let repo = fx.repo_dir();
    std::fs::create_dir_all(fx.home()).unwrap();
    std::fs::write(
        fx.home().join("config.toml"),
        "[scopes]\nacme = []\n\"acme/globex\" = [\"GLX\"]\ninitech = [\"repo:gamma\"]\n",
    )
    .unwrap();
    let (out, _) = fx.conv(
        vec![
            init(),
            initialized(),
            call(
                1,
                "memory_put",
                json!({"text": "GLX deploy window is Tuesday", "tag": "decided", "repo": "beta"}),
            ),
            call(
                2,
                "memory_put",
                json!({"text": "Deploy window is Friday", "tag": "decided", "repo": "gamma"}),
            ),
            call(
                3,
                "memory_query",
                json!({"query": "deploy window", "in": "acme"}),
            ),
            call(
                4,
                "memory_query",
                json!({"query": "deploy window", "in": "acme/globex,initech"}),
            ),
            call(
                5,
                "memory_query",
                json!({"query": "deploy window", "in": "globex"}),
            ),
        ],
        &repo,
    );
    let glx = payload(&out[1])["id"].clone();
    let fri = payload(&out[2])["id"].clone();
    let found = payload(&out[3]);
    let ids = |v: &Value| -> Vec<Value> {
        let mut ids: Vec<Value> = v["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].clone())
            .collect();
        ids.sort_by_key(|i| i.as_str().unwrap().to_string());
        ids
    };
    // the cwd repo is alpha; acme reaches beta through its child's code
    assert_eq!(ids(&found), vec![glx.clone()]);
    assert_eq!(
        found["scope"],
        json!({"repo": null, "all_repos": true, "in": ["acme"]})
    );
    let mut both = vec![glx, fri];
    both.sort_by_key(|i| i.as_str().unwrap().to_string());
    assert_eq!(ids(&payload(&out[4])), both);
    assert_eq!(out[5]["result"]["isError"], true);
    assert_eq!(
        out[5]["result"]["content"][0]["text"],
        "error: unknown scope: globex; known: acme, acme/globex, initech"
    );
}

#[test]
fn query_and_related_give_the_cli_json() {
    let fx = Fx::new();
    let repo = fx.repo_dir();
    let (out, _) = fx.conv(
        vec![
            init(),
            call(
                1,
                "memory_put",
                json!({"text": "Cache A holds 3 days", "tag": "assumed", "refs": "T7"}),
            ),
            call(
                2,
                "memory_put",
                json!({"text": "Cache B holds 5 days", "tag": "decided", "refs": "T7"}),
            ),
        ],
        &repo,
    );
    let first = payload(&out[1])["id"].as_str().unwrap().to_string();
    let (out, _) = fx.conv(
        vec![
            init(),
            call(
                1,
                "memory_query",
                json!({"query": "cache holds", "bias": "decided", "limit": 1}),
            ),
            call(2, "memory_related", json!({"id": first})),
        ],
        &repo,
    );
    let query = payload(&out[1]);
    assert_eq!(
        query,
        fx.cli_json(
            &[
                "query",
                "cache holds",
                "--bias",
                "decided",
                "--limit",
                "1",
                "--format",
                "json"
            ],
            &repo
        )
    );
    let titles: Vec<&Value> = query["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| &r["title"])
        .collect();
    assert_eq!(titles, vec!["Cache B holds 5 days"]);
    assert_eq!(query["more"]["beyond_limit"], 1);
    let related = payload(&out[2]);
    assert_eq!(
        related,
        fx.cli_json(&["related", &first, "--format", "json"], &repo)
    );
    let rows: Vec<Value> = related["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| json!([r["title"], r["relation"]]))
        .collect();
    assert_eq!(rows, vec![json!(["Cache B holds 5 days", ["refs"]])]);
}

#[test]
fn query_json_names_no_1_letter_or_operator_word_as_missing() {
    let fx = Fx::new();
    let repo = fx.repo_dir();
    fx.conv(
        vec![
            init(),
            call(
                1,
                "memory_put",
                json!({"text": "Kiwi base", "tag": "assumed"}),
            ),
        ],
        &repo,
    );
    let text = "kiwi a Q3 AND or Not NEAR";
    let (out, _) = fx.conv(
        vec![init(), call(1, "memory_query", json!({"query": text}))],
        &repo,
    );
    let query = payload(&out[1]);
    assert_eq!(
        query,
        fx.cli_json(&["query", text, "--format", "json"], &repo)
    );
    // the 2-character id Q3 stays; a, and, or, not and near (any case) are no hint
    assert_eq!(query["missing"], json!([{"term": "q3", "in_scope": 0}]));
    let hits = query["results"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(
        (
            hits[0]["why"]["terms_matched"].clone(),
            hits[0]["why"]["terms_missing"].clone()
        ),
        (json!(["kiwi"]), json!(["q3"]))
    );
}

#[test]
fn query_arguments_fail_as_the_repeated_cli_flags_fail() {
    let fx = Fx::new();
    let repo = fx.repo_dir();
    let cases: [(&[&str], Value, &str); 3] = [
        (
            &["--tag", "executed", "--tag", "guessed"],
            json!({"tag": "executed,guessed"}),
            "Invalid tag 'guessed'; expected one of executed, verified, decided, client-stated, derived, assumed, prior, planned, n/a",
        ),
        (
            &["--kind", "fact", "--kind", "note"],
            json!({"kind": "fact,note"}),
            "Invalid kind 'note'; expected one of fact, decision, learning, episode, task, handoff",
        ),
        (
            &["--bias", "executed", "--bias", "newest"],
            json!({"bias": "executed,newest"}),
            "Invalid bias 'newest'; expected one of recency, relevance, executed, verified, decided, client-stated, derived, assumed, prior, planned, fact, decision, learning, episode, task, handoff",
        ),
    ];
    for (flags, arguments, message) in cases {
        let mut args = arguments.as_object().unwrap().clone();
        args.insert("query".into(), json!("x"));
        let (out, _) = fx.conv(
            vec![init(), call(1, "memory_query", Value::Object(args))],
            &repo,
        );
        let content = out[1]["result"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(
            (
                out[1]["result"]["isError"].clone(),
                content[0]["text"].clone()
            ),
            (json!(true), json!(format!("error: {message}")))
        );
        let mut cli_args = vec!["query", "x"];
        cli_args.extend(flags);
        let o = fx.cli(&cli_args, &repo);
        let stderr = String::from_utf8_lossy(&o.stderr);
        // a bad --bias is an argparse usage error (exit 2); a bad --tag or --kind is an error line (exit 1)
        let want_code = if flags[0] == "--bias" { 2 } else { 1 };
        assert_eq!(
            (o.status.code(), stderr.contains(message)),
            (Some(want_code), true),
            "{flags:?}: {stderr}"
        );
    }
}

#[test]
fn query_takes_comma_lists_as_the_cli_takes_repeated_flags() {
    let fx = Fx::new();
    let repo = fx.repo_dir();
    fx.conv(
        vec![
            init(),
            call(
                1,
                "memory_put",
                json!({"text": "Gate A opens at nine", "tag": "decided", "kind": "decision"}),
            ),
            call(
                2,
                "memory_put",
                json!({"text": "Gate B opens at ten", "tag": "assumed"}),
            ),
            call(
                3,
                "memory_put",
                json!({"text": "Gate C opens at noon", "tag": "prior", "kind": "learning"}),
            ),
        ],
        &repo,
    );
    let (out, _) = fx.conv(
        vec![
            init(),
            call(
                1,
                "memory_query",
                json!({"query": "gate", "tag": "decided,assumed"}),
            ),
        ],
        &repo,
    );
    let query = payload(&out[1]);
    assert_eq!(
        query,
        fx.cli_json(
            &[
                "query", "gate", "--tag", "decided", "--tag", "assumed", "--format", "json"
            ],
            &repo
        )
    );
    let mut titles: Vec<&str> = query["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["title"].as_str().unwrap())
        .collect();
    titles.sort();
    assert_eq!(titles, vec!["Gate A opens at nine", "Gate B opens at ten"]);
}

#[test]
fn default_repo_maps_a_project_key_cwd_and_a_marker_folder_by_git() {
    let fx = Fx::new();
    let root = fx.root().to_path_buf();
    std::fs::create_dir(fx.home()).unwrap();
    std::fs::write(
        fx.home().join("config.toml"),
        "[repos]\nroots = [\"~/code\"]\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("code/alpha")).unwrap();
    let hooks = root.join("code/claude-code-hooks");
    std::fs::create_dir(&hooks).unwrap();
    git_init(&hooks);
    let pad = root.join("u-1000/-u-ann-code-alpha/sess/scratchpad");
    std::fs::create_dir_all(&pad).unwrap();
    let (out, _) = fx.conv(
        vec![
            init(),
            call(
                1,
                "memory_put",
                json!({"text": "Pad note", "tag": "assumed"}),
            ),
            call(2, "memory_query", json!({"query": "pad note"})),
        ],
        &pad,
    );
    // the query finds it, so its scope is alpha too: a repo scope leaves out other repos
    let hits = payload(&out[2])["results"].as_array().unwrap().clone();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["repo"], "alpha");
    // `-code-` inside a folder name is no project key: the repo is the git checkout's name
    let (out, _) = fx.conv(
        vec![
            init(),
            call(
                1,
                "memory_put",
                json!({"text": "Hook note", "tag": "assumed"}),
            ),
            call(2, "memory_query", json!({"query": "hook note"})),
        ],
        &hooks,
    );
    let hits = payload(&out[2])["results"].as_array().unwrap().clone();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["repo"], "claude-code-hooks");
}

#[test]
fn put_reports_overlap_with_a_recent_task() {
    let fx = Fx::new();
    let repo = fx.repo_dir();
    let (out, _) = fx.conv(
        vec![
            init(),
            call(1, "memory_put", json!({"text": "Rotated the B cache keys on node one", "kind": "task", "tag": "decided"})),
            call(2, "memory_put", json!({"text": "Rotated the B cache keys on node one today", "tag": "decided"})),
        ],
        &repo,
    );
    let (task, fact) = (payload(&out[1]), payload(&out[2]));
    assert_eq!(task["overlaps"], json!([]));
    let lines = fact["overlaps"].as_array().unwrap();
    assert_eq!(lines.len(), 1);
    let line = lines[0].as_str().unwrap();
    assert!(
        line.starts_with(&format!("sm put: {} shares ", fact["id"].as_str().unwrap())),
        "{line}"
    );
    assert!(
        line.ends_with(&format!(
            "; if it replaces that item, run sm supersede {} {}",
            task["id"].as_str().unwrap(),
            fact["id"].as_str().unwrap()
        )),
        "{line}"
    );
}

#[test]
fn supersede_hides_the_old_item() {
    let fx = Fx::new();
    let repo = fx.repo_dir();
    let (out, _) = fx.conv(
        vec![
            init(),
            call(
                1,
                "memory_put",
                json!({"text": "Port is 8080", "tag": "assumed"}),
            ),
            call(
                2,
                "memory_put",
                json!({"text": "Port is 9090", "tag": "decided", "evidence": "owner in chat"}),
            ),
        ],
        &repo,
    );
    let (old, new) = (
        payload(&out[1])["id"].clone(),
        payload(&out[2])["id"].clone(),
    );
    let (out, _) = fx.conv(
        vec![
            init(),
            call(1, "memory_supersede", json!({"old_id": old, "new_id": new})),
            call(2, "memory_query", json!({"query": "port"})),
            call(3, "memory_show", json!({"ids": [old]})),
        ],
        &repo,
    );
    assert_eq!(
        payload(&out[1]),
        json!({"old_id": old, "superseded_by": new})
    );
    let ids: Vec<Value> = payload(&out[2])["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].clone())
        .collect();
    assert_eq!(ids, vec![new.clone()]);
    let shown = payload(&out[3]);
    let items = shown["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(
        (
            items[0]["status"].clone(),
            items[0]["superseded_by"].clone()
        ),
        (json!("superseded"), new)
    );
}

#[test]
fn a_failing_tool_answers_is_error() {
    let fx = Fx::new();
    let repo = fx.repo_dir();
    let cases: Vec<(&str, Value, &str)> = vec![
        (
            "memory_put",
            json!({"text": "Ran the B suite", "tag": "executed"}),
            "error: Tag 'executed' requires non-empty --evidence",
        ),
        (
            "memory_put",
            json!({"text": "x", "tag": "guessed"}),
            "error: argument 'tag' must be one of executed, verified, decided, client-stated, derived, assumed, prior, planned, got 'guessed'",
        ),
        (
            "memory_put",
            json!({"tag": "decided"}),
            "error: missing required argument 'text'",
        ),
        (
            "memory_query",
            json!({"query": "x", "limit": 0}),
            "error: argument 'limit' must be at least 1, got 0",
        ),
        (
            "memory_query",
            json!({"query": "x", "limit": "3"}),
            "error: argument 'limit' must be integer, got \"3\"",
        ),
        (
            "memory_query",
            json!({"query": "x", "all_repos": 1}),
            "error: argument 'all_repos' must be boolean, got 1",
        ),
        (
            "memory_query",
            json!({"query": "x", "limit": true}),
            "error: argument 'limit' must be integer, got true",
        ),
        (
            "memory_query",
            json!({"query": "x", "bias": "loudest"}),
            "error: Invalid bias 'loudest'; expected one of recency, relevance, executed, verified, decided, client-stated, derived, assumed, prior, planned, fact, decision, learning, episode, task, handoff",
        ),
        (
            "memory_query",
            json!({"query": "x", "since": "1d"}),
            "error: unknown argument 'since'; expected one of query, bias, kind, tag, limit, all_repos, repo, in",
        ),
        (
            "memory_show",
            json!({"ids": []}),
            "error: argument 'ids' needs at least 1 item(s)",
        ),
        (
            "memory_show",
            json!({"ids": [3]}),
            "error: every item of 'ids' must be string",
        ),
        (
            "memory_related",
            json!({"id": "m_missing"}),
            "error: Item not found: m_missing",
        ),
        (
            "memory_supersede",
            json!({"old_id": "m_a", "new_id": "m_b"}),
            "error: Item not found: m_a",
        ),
    ];
    for (name, arguments, message) in cases {
        let (out, err) = fx.conv(
            vec![init(), call(1, name, arguments.clone()), ping(2)],
            &repo,
        );
        let content = out[1]["result"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1, "{name} {arguments}");
        assert_eq!(
            (
                out[1]["result"]["isError"].clone(),
                content[0]["text"].clone()
            ),
            (json!(true), json!(message)),
            "{name} {arguments}"
        );
        assert!(
            err.contains(&format!("sm mcp: {name}: ")),
            "{name} {arguments}: {err}"
        );
        assert_eq!(out[2]["result"], json!({}), "{name} {arguments}");
    }
}

#[test]
fn a_modern_client_never_sends_initialize() {
    let fx = Fx::new();
    let repo = fx.repo_dir();
    let (out, _) = fx.conv(
        vec![
            modern(json!("discover-1"), "server/discover", json!({}), None),
            modern(json!(1), "tools/list", json!({}), None),
            modern(json!(2), "tools/call", json!({"name": "memory_put", "arguments": {"text": "Deploy window is Tuesday", "tag": "decided"}}), None),
            modern(json!(3), "tools/call", json!({"name": "memory_query", "arguments": {"query": "deploy window"}}), None),
            modern(json!(4), "tools/call", json!({"name": "memory_show", "arguments": {"ids": ["m_missing"]}}), None),
        ],
        &repo,
    );
    let discover = &out[0]["result"];
    assert_eq!(
        *discover,
        json!({
            "resultType": "complete",
            "supportedVersions": VERSIONS,
            "capabilities": {"tools": {"listChanged": false}},
            "_meta": server_meta(),
            "instructions": discover["instructions"],
            "ttlMs": 300000,
            "cacheScope": "public",
        })
    );
    assert!(
        discover["instructions"]
            .as_str()
            .unwrap()
            .starts_with("Simple Memory holds decisions")
    );
    let tools = out[1]["result"].as_object().unwrap();
    let rest: Value = tools
        .iter()
        .filter(|(k, _)| *k != "tools")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect::<serde_json::Map<_, _>>()
        .into();
    assert_eq!(
        rest,
        json!({"resultType": "complete", "_meta": server_meta(), "ttlMs": 300000, "cacheScope": "public"})
    );
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, tool_names());
    for response in &out[2..] {
        assert_eq!(
            (
                response["result"]["resultType"].clone(),
                response["result"]["_meta"].clone()
            ),
            (json!("complete"), server_meta())
        );
    }
    let put = payload(&out[2]);
    let rows: Vec<Value> = payload(&out[3])["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| json!([r["id"], r["repo"]]))
        .collect();
    assert_eq!(rows, vec![json!([put["id"], "alpha"])]);
    assert_eq!(
        (
            out[4]["result"]["isError"].clone(),
            payload(&out[4])["missing"].clone()
        ),
        (json!(true), json!(["m_missing"]))
    );
}

#[test]
fn a_modern_request_with_an_unsupported_version_names_the_supported_ones() {
    let fx = Fx::new();
    let repo = fx.repo_dir();
    for asked in ["2025-11-25", "1900-01-01"] {
        let mut m = meta();
        m["io.modelcontextprotocol/protocolVersion"] = json!(asked);
        let (out, _) = fx.conv(
            vec![modern(json!(1), "tools/list", json!({}), Some(m))],
            &repo,
        );
        assert_eq!(
            out[0],
            json!({"jsonrpc": "2.0", "id": 1, "error": {
                "code": -32022, "message": "Unsupported protocol version", "data": {"supported": VERSIONS, "requested": asked}}}),
            "asked {asked}"
        );
    }
}

#[test]
fn a_modern_request_missing_a_required_meta_field_is_invalid_params() {
    let fx = Fx::new();
    let repo = fx.repo_dir();
    let metas = [
        json!({"io.modelcontextprotocol/protocolVersion": "2026-07-28"}),
        json!({"io.modelcontextprotocol/clientCapabilities": {}}),
        json!({"io.modelcontextprotocol/protocolVersion": 20260728, "io.modelcontextprotocol/clientCapabilities": {}}),
        json!({"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientCapabilities": []}),
        json!([]),
    ];
    for m in metas {
        let (out, _) = fx.conv(
            vec![
                modern(
                    json!(1),
                    "tools/call",
                    json!({"name": "memory_query", "arguments": {"query": "x"}}),
                    Some(m.clone()),
                ),
                modern(json!(2), "ping", json!({}), None),
            ],
            &repo,
        );
        assert_eq!(
            out[0],
            json!({"jsonrpc": "2.0", "id": 1, "error": {
                "code": -32602, "message": "_meta needs io.modelcontextprotocol/protocolVersion and clientCapabilities"}}),
            "meta {m}"
        );
        // ping and initialize are not methods of revision 2026-07-28
        assert_eq!(
            out[1]["error"],
            json!({"code": -32601, "message": "Method not found: ping"}),
            "meta {m}"
        );
    }
}

#[test]
fn initialize_selects_the_legacy_era_even_with_modern_meta() {
    let fx = Fx::new();
    let repo = fx.repo_dir();
    let mut msg = init();
    msg["params"]["_meta"] = meta();
    let (out, _) = fx.conv(vec![msg], &repo);
    assert_eq!(out[0]["result"]["protocolVersion"], "2025-11-25");
    assert!(out[0]["result"].get("resultType").is_none());
}

fn usage_rows(fx: &Fx) -> Vec<Value> {
    let text = std::fs::read_to_string(fx.home().join("usage.log")).unwrap();
    text.lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn mcp_reads_write_the_usage_rows_the_cli_writes() {
    let fx = Fx::new();
    let repo = fx.repo_dir();
    let (out, _) = fx.conv(
        vec![
            init(),
            call(
                1,
                "memory_put",
                json!({"text": "Cache A holds 3 days", "tag": "assumed", "refs": "T7"}),
            ),
            call(
                2,
                "memory_put",
                json!({"text": "Cache B holds 5 days", "tag": "decided", "refs": "T7"}),
            ),
        ],
        &repo,
    );
    let (a, b) = (
        payload(&out[1])["id"].clone(),
        payload(&out[2])["id"].clone(),
    );
    fx.conv(
        vec![
            init(),
            call(
                1,
                "memory_query",
                json!({"query": "cache holds", "bias": "decided", "limit": 3, "kind": "fact"}),
            ),
            call(2, "memory_show", json!({"ids": [b, "m_missing", a]})),
            call(3, "memory_related", json!({"id": a})),
            call(4, "memory_related", json!({"id": "m_missing"})),
        ],
        &repo,
    );
    let rows: Vec<Value> = usage_rows(&fx)
        .into_iter()
        .map(|mut r| {
            r.as_object_mut().unwrap().remove("ts");
            r
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            json!({"cmd": "query", "ids": [b, a], "repo": "alpha", "flags": {
                "bias": ["decided"], "kind": "fact", "tag": "", "format": "json", "limit": 3, "all": false, "via": "mcp"}}),
            json!({"cmd": "show", "ids": [b, a], "repo": null, "flags": {"via": "mcp"}}),
            json!({"cmd": "related", "ids": [a, b], "repo": null, "flags": {"via": "mcp"}}),
        ]
    );
}

/// `YYYY-MM-DDTHH:MM:SS+00:00` for the epoch second, as Python's `isoformat(timespec="seconds")`.
fn iso_utc(epoch: i64) -> String {
    let (days, secs) = (epoch.div_euclid(86400), epoch.rem_euclid(86400));
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}+00:00",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

#[test]
fn an_item_opened_with_memory_show_counts_as_opened_in_stats() {
    // Usage tracking began 2 hours ago and the hook injected the item 6 times; one memory_show opens it.
    let fx = Fx::new();
    let repo = fx.repo_dir();
    let (out, _) = fx.conv(
        vec![
            init(),
            call(
                1,
                "memory_put",
                json!({"text": "Quux deploy note", "tag": "assumed"}),
            ),
        ],
        &repo,
    );
    let item = payload(&out[1])["id"].clone();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let first =
        json!({"ts": iso_utc(now - 7200), "cmd": "query", "ids": [], "repo": null, "flags": {}});
    std::fs::write(fx.home().join("usage.log"), format!("{first}\n")).unwrap();
    let hook_rows: String = (0..6)
        .map(|k| {
            format!(
                "{}\n",
                json!({"ts": iso_utc(now - 3600 - 60 * k), "ids": [item]})
            )
        })
        .collect();
    std::fs::write(fx.home().join("hook.log"), hook_rows).unwrap();
    let stats = |fx: &Fx| fx.cli_json(&["stats", "--format", "json"], &repo);
    let before = stats(&fx);
    let never: Vec<Value> = before["never_opened"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].clone())
        .collect();
    assert_eq!(
        (never, before["recall"]["opened_ids"].clone()),
        (vec![item.clone()], json!(0))
    );

    fx.conv(
        vec![init(), call(1, "memory_show", json!({"ids": [item]}))],
        &repo,
    );

    let after = stats(&fx);
    assert_eq!(
        (
            after["never_opened"].clone(),
            after["recall"]["opened_ids"].clone(),
            after["recall"]["tracked_ids"].clone()
        ),
        (json!([]), json!(1), json!(1))
    );
}

#[test]
fn an_oserror_in_a_tool_answers_is_error_and_the_server_keeps_running() {
    // SIMPLE_MEMORY_HOME under a regular file: opening the store raises `[Errno 20] Not a directory`.
    // Python 3.15.1 on the same input answers the first call with isError true and that text,
    // logs `sm mcp: memory_query: ...` to stderr, and still answers the ping.
    let mut fx = Fx::new();
    let repo = fx.repo_dir();
    let afile = fx.root().join("afile");
    std::fs::write(&afile, "").unwrap();
    fx.home_override = Some(afile.join("sm"));
    let (out, err) = fx.conv(
        vec![call(1, "memory_query", json!({"query": "loader"})), ping(2)],
        &repo,
    );
    let content = out[0]["result"]["content"].as_array().unwrap();
    assert_eq!(content.len(), 1);
    assert_eq!(
        content[0]["text"],
        format!(
            "error: [Errno 20] Not a directory: '{}'",
            afile.join("sm").display()
        )
    );
    assert_eq!(out[0]["result"]["isError"], true);
    assert_eq!(out[1], json!({"jsonrpc": "2.0", "id": 2, "result": {}}));
    assert!(
        err.contains("sm mcp: memory_query: [Errno 20] Not a directory"),
        "{err}"
    );
}
