//! Ports of the Python lookup tests (8 tests, through the library's `run_lookup`) and
//! CLI lookup tests (27 tests, through the `sm` binary on a scratch SIMPLE_MEMORY_HOME)
//! of Python main 555b8f2. Expected values are the Python tests' own.
//!
//! The CLI half runs `sm` with only PATH, HOME and SIMPLE_MEMORY_HOME (a temp folder). The
//! session ids and refs in the fixtures are the Python tests' made-up ones.

use serde_json::{Value, json};
use simple_memory::core::item::Item;
use simple_memory::service::query::{QueryArgs, StrList, run_lookup};
use simple_memory::store::connect::connect;
use simple_memory::store::items::put_item;
use std::process::{Command, Stdio};

const TS: &str = "2026-10-25T10:00:00+00:00";

fn seed(home: &std::path::Path, rows: &[(&str, &str, &str, &str)]) {
    let conn = connect(&format!("{}/memory.db", home.display()), None).unwrap();
    for (id, repo, body, refs) in rows {
        let mut it = Item::bare(id, repo, "fact", body, body, "derived");
        it.refs = (*refs).to_string();
        it.as_of = TS.to_string();
        put_item(&conn, &it.post_init(), None).unwrap();
    }
}

fn args<'a>(home: &'a str, repo: &'a str) -> QueryArgs<'a> {
    QueryArgs {
        home: Some(home),
        repo,
        ..Default::default()
    }
}

fn owned(rows: &[(&str, i64)]) -> Vec<(String, i64)> {
    rows.iter().map(|(t, n)| (t.to_string(), *n)).collect()
}

#[test]
fn shared_refs_need_two_holders_and_skip_refs_the_query_names() {
    let d = tempfile::tempdir().unwrap();
    seed(
        d.path(),
        &[
            ("m_1", "a", "Kiwi one", "R6 x.md y.md solo1"),
            ("m_2", "a", "Kiwi two", "R6 x.md y.md"),
            ("m_3", "a", "Kiwi three", "y.md"),
        ],
    );
    let home = d.path().to_str().unwrap();
    let lk = run_lookup(
        "kiwi R6",
        &QueryArgs {
            limit: 3,
            ..args(home, "a")
        },
    )
    .unwrap();
    assert_eq!(lk.shared_refs, owned(&[("y.md", 3), ("x.md", 2)]));
}

#[test]
fn footer_lists_are_capped() {
    let d = tempfile::tempdir().unwrap();
    let mut rows = vec![
        ("m_hit", "a", "Kiwi base", "r1 r2 r3 r4"),
        ("m_hit2", "a", "Kiwi again", "r1 r2 r3 r4"),
    ];
    let ids: Vec<String> = ["b", "c", "d", "e"]
        .iter()
        .map(|r| format!("m_{r}"))
        .collect();
    for (id, repo) in ids.iter().zip(["b", "c", "d", "e"]) {
        rows.push((id, repo, "Kiwi elsewhere", ""));
    }
    seed(d.path(), &rows);
    let home = d.path().to_str().unwrap();
    let lk = run_lookup(
        "kiwi qa qb qc qd qe qf",
        &QueryArgs {
            limit: 2,
            ..args(home, "a")
        },
    )
    .unwrap();
    assert_eq!(
        lk.missing
            .iter()
            .map(|(t, _)| t.as_str())
            .collect::<Vec<_>>(),
        ["qa", "qb", "qc", "qd", "qe"]
    );
    assert_eq!(
        lk.other_repos
            .iter()
            .map(|(k, v)| (k.as_str(), *v))
            .collect::<Vec<_>>(),
        [("b", 1), ("c", 1), ("d", 1)]
    );
    assert_eq!(lk.shared_refs, owned(&[("r1", 2), ("r2", 2), ("r3", 2)]));
}

#[test]
fn all_repos_counts_no_other_repo() {
    let d = tempfile::tempdir().unwrap();
    seed(
        d.path(),
        &[("m_a", "a", "Kiwi a", ""), ("m_b", "b", "Kiwi b", "")],
    );
    let home = d.path().to_str().unwrap();
    let lk = run_lookup(
        "kiwi",
        &QueryArgs {
            all_repos: true,
            limit: 1,
            ..args(home, "")
        },
    )
    .unwrap();
    assert_eq!(
        (lk.repo.clone(), lk.other_repos.len(), lk.beyond_limit),
        (None, 0, 1)
    );
    // --repo with --all: the repo wins for the boost, every repo is in scope, so none is "other"
    let lk = run_lookup(
        "kiwi",
        &QueryArgs {
            all_repos: true,
            limit: 1,
            ..args(home, "a")
        },
    )
    .unwrap();
    assert_eq!(
        (lk.repo.as_deref(), lk.other_repos.len(), lk.beyond_limit),
        (Some("a"), 0, 1)
    );
}

#[test]
fn recent_reports_no_bias() {
    let d = tempfile::tempdir().unwrap();
    seed(d.path(), &[("m_a", "a", "Kiwi a", "")]);
    let home = d.path().to_str().unwrap();
    assert!(
        run_lookup(
            "kiwi",
            &QueryArgs {
                recent: true,
                ..args(home, "a")
            }
        )
        .unwrap()
        .bias
        .is_empty()
    );
    assert_eq!(
        run_lookup(
            "kiwi",
            &QueryArgs {
                bias: StrList::Str("executed"),
                ..args(home, "a")
            }
        )
        .unwrap()
        .bias,
        ["executed"]
    );
}

#[test]
fn a_ref_only_hit_never_gives_a_negative_beyond_count() {
    // "+++" has no word for FTS, so the in-scope count is 0 while the ref match finds the item
    let d = tempfile::tempdir().unwrap();
    seed(d.path(), &[("m_p", "a", "Plus signs", "+++")]);
    let home = d.path().to_str().unwrap();
    let lk = run_lookup("+++", &args(home, "a")).unwrap();
    assert_eq!(
        (
            lk.hits
                .iter()
                .map(|s| s.item.id.as_str())
                .collect::<Vec<_>>(),
            lk.beyond_limit,
            lk.missing.len()
        ),
        (vec!["m_p"], 0, 0)
    );
}

#[test]
fn other_repos_count_only_the_first_cap_of_a_wider_union() {
    // 60 strong "cafe" items in b fill the folded pool; 5 weak "café" items in c come in through
    // the accented index, so the union holds 55. The accent hit ranks c first and the pool keeps
    // the best 50: c 5, global 1, unassigned 1 (they take slots but are in scope, so never listed), b 43.
    let d = tempfile::tempdir().unwrap();
    let filler: String = (0..80)
        .map(|n| format!("filler{n}"))
        .collect::<Vec<_>>()
        .join(" ");
    let mut owned_rows: Vec<(String, String, String)> = Vec::new();
    for i in 0..60 {
        owned_rows.push((
            format!("m_b{i:02}"),
            "b".into(),
            format!("Cafe cafe cafe {i}"),
        ));
    }
    for i in 0..5 {
        owned_rows.push((format!("m_c{i}"), "c".into(), format!("café {i} {filler}")));
    }
    owned_rows.push(("m_g".into(), "global".into(), "Café café global".into()));
    owned_rows.push((
        "m_u".into(),
        "unassigned".into(),
        "Café café unassigned".into(),
    ));
    let rows: Vec<(&str, &str, &str, &str)> = owned_rows
        .iter()
        .map(|(i, r, b)| (i.as_str(), r.as_str(), b.as_str(), ""))
        .collect();
    seed(d.path(), &rows);
    let home = d.path().to_str().unwrap();
    let lk = run_lookup(
        "café",
        &QueryArgs {
            bias: StrList::Str("recency"),
            ..args(home, "a")
        },
    )
    .unwrap();
    let mut other: Vec<(&str, i64)> = lk
        .other_repos
        .iter()
        .map(|(k, v)| (k.as_str(), *v))
        .collect();
    other.sort();
    assert_eq!(
        (other, lk.other_repos_cap),
        (vec![("b", 43), ("c", 5)], Some(50))
    );
}

#[test]
fn missing_leaves_out_one_letter_terms_and_fts_operator_words() {
    let d = tempfile::tempdir().unwrap();
    seed(d.path(), &[("m_k", "a", "Kiwi base", "")]);
    let home = d.path().to_str().unwrap();
    let lk = run_lookup("kiwi NEAR(a b) Q3 OR And not Near", &args(home, "a")).unwrap();
    assert_eq!(
        lk.hits
            .iter()
            .map(|s| s.item.id.as_str())
            .collect::<Vec<_>>(),
        ["m_k"]
    );
    // a 2-letter id such as Q3 stays; one letter and the words and, or, not, near (any case) go
    assert_eq!(lk.missing, owned(&[("q3", 0)]));
}

#[test]
fn why_terms_missing_leaves_out_the_words_the_footer_leaves_out() {
    let d = tempfile::tempdir().unwrap();
    seed(d.path(), &[("m_k", "a", "Kiwi base", "")]);
    let home = d.path().to_str().unwrap();
    let lk = run_lookup("kiwi NEAR(a b) Q3 OR And not Near", &args(home, "a")).unwrap();
    assert_eq!(
        (
            lk.hits[0].why.terms_matched.clone(),
            lk.hits[0].why.terms_missing.clone()
        ),
        (vec!["kiwi".to_string()], vec!["q3".to_string()])
    );
}

// ---- CLI lookup tests ----

struct Cli {
    dir: tempfile::TempDir,
}

struct Out {
    code: i32,
    out: String,
    err: String,
}

impl Out {
    fn lines(&self) -> Vec<&str> {
        self.out.lines().collect()
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.out).unwrap()
    }

    fn more(&self) -> Vec<&str> {
        self.out
            .lines()
            .filter(|l| l.starts_with("more:"))
            .collect()
    }
}

impl Cli {
    fn new() -> Cli {
        Cli {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn run(&self, args: &[&str]) -> Out {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sm"));
        cmd.args(args)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap())
            .env("HOME", self.dir.path())
            .env("SIMPLE_MEMORY_HOME", self.dir.path())
            .current_dir(self.dir.path())
            .stdin(Stdio::null());
        for key in ["TZ", "SM_UTC_OFFSET"] {
            if let Some(val) = std::env::var_os(key) {
                cmd.env(key, val);
            }
        }
        let o = cmd.output().unwrap();
        Out {
            code: o.status.code().unwrap(),
            out: String::from_utf8(o.stdout).unwrap(),
            err: String::from_utf8(o.stderr).unwrap(),
        }
    }

    fn ok(&self, args: &[&str]) -> Out {
        let o = self.run(args);
        assert_eq!(o.code, 0, "{args:?}: {}", o.err);
        o
    }

    /// `_put`: a put into repo a, the id it prints.
    fn put(&self, body: &str, extra: &[&str]) -> String {
        let mut a = vec!["put", "--repo", "a"];
        a.extend(extra);
        a.push(body);
        self.ok(&a).out.trim().to_string()
    }

    /// `_seed_footer_case`: the R6 ref hits rank first; "yak" is held only by the third item of repo a.
    fn footer_case(&self) -> (String, String, String) {
        let first = self.put(
            "Kiwi plan for the rollout",
            &[
                "--tag",
                "decided",
                "--kind",
                "decision",
                "--refs",
                "R6 abc1234 db.pool",
            ],
        );
        let second = self.put(
            "Kiwi steps of the rollout",
            &["--tag", "derived", "--refs", "R6 abc1234 db.pool"],
        );
        let third = self.put("Yak shaving note", &["--tag", "derived"]);
        for body in ["Yak in repo b", "Another yak in repo b"] {
            self.ok(&["put", "--repo", "b", "--tag", "derived", body]);
        }
        self.ok(&["put", "--repo", "c", "--tag", "derived", "Yak in repo c"]);
        (first, second, third)
    }
}

fn id_lines(o: &Out, prefix: &str) -> Vec<String> {
    o.out
        .lines()
        .filter(|l| l.starts_with(prefix))
        .map(String::from)
        .collect()
}

#[test]
fn query_bias_with_an_unknown_value_exits_2_and_lists_the_values() {
    let c = Cli::new();
    let o = c.run(&["query", "kiwi", "--bias", "newest"]);
    assert_eq!(o.code, 2);
    assert!(
        o.err.contains(
            "argument --bias: Invalid bias 'newest'; expected one of recency, relevance, executed,"
        ),
        "{}",
        o.err
    );
}

#[test]
fn query_recent_and_bias_together_is_a_usage_error() {
    let c = Cli::new();
    let o = c.run(&["query", "kiwi", "--recent", "--bias", "executed"]);
    assert_eq!(o.code, 2);
    assert!(
        o.err
            .contains("argument --bias: not allowed with argument --recent"),
        "{}",
        o.err
    );
}

#[test]
fn query_bias_reorders_and_kind_filters() {
    // same words, so the same bm25: default bias puts decision+decided first, --bias recency
    // puts the newest fact first, and --bias task puts the old task first
    let c = Cli::new();
    let fact = c.put(
        "Kiwi rollout note A",
        &["--tag", "derived", "--as-of", "2026-11-01"],
    );
    let dec = c.put(
        "Kiwi rollout note B",
        &[
            "--tag",
            "decided",
            "--kind",
            "decision",
            "--as-of",
            "2026-01-01",
        ],
    );
    let task = c.put(
        "Kiwi rollout note C",
        &[
            "--tag",
            "planned",
            "--kind",
            "task",
            "--as-of",
            "2026-01-01",
        ],
    );
    let first_id = |args: &[&str]| {
        c.ok(args)
            .out
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .to_string()
    };
    assert_eq!(
        (
            first_id(&["query", "kiwi rollout", "--repo", "a"]),
            first_id(&["query", "kiwi rollout", "--repo", "a", "--bias", "recency"]),
            first_id(&["query", "kiwi rollout", "--repo", "a", "--bias", "task"]),
        ),
        (format!("[{dec}]"), format!("[{fact}]"), format!("[{task}]"))
    );
    let o = c.ok(&["query", "kiwi rollout", "--repo", "a", "--kind", "decision"]);
    let ids: Vec<&str> = o
        .out
        .lines()
        .filter(|l| l.starts_with('['))
        .map(|l| l.split_whitespace().next().unwrap())
        .collect();
    assert_eq!(ids, [format!("[{dec}]")]);
}

#[test]
fn query_kind_with_an_unknown_value_exits_1() {
    let c = Cli::new();
    let o = c.run(&["query", "kiwi", "--kind", "note"]);
    assert_eq!(o.code, 1);
    assert_eq!(
        o.err.trim(),
        "error: Invalid kind 'note'; expected one of fact, decision, learning, episode, task, handoff"
    );
}

#[test]
fn query_text_header_starts_with_the_id_then_rank_tokens_and_why() {
    let c = Cli::new();
    let (first, second, _) = c.footer_case();
    let o = c.ok(&[
        "query", "R6 yak", "--repo", "a", "--limit", "2", "--bias", "decided",
    ]);
    let lines = o.lines();
    // "Kiwi plan for the rollout": 25 title + 25 body chars = 50 / 4 -> 13 tokens (rounded up)
    let as_of = lines[0]
        .split("as_of=")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap();
    assert_eq!(
        lines[0],
        format!(
            "[{first}] #1 repo=a kind=decision tag=decided as_of={as_of} ~13 tok why=ref:R6,terms:1/2,bias:decided refs=R6 abc1234 db.pool"
        )
    );
    assert_eq!(lines[1], "  Kiwi plan for the rollout");
    assert!(
        lines[2].starts_with(&format!("[{second}] #2 repo=a kind=fact tag=derived ")),
        "{}",
        lines[2]
    );
    assert!(
        lines[2].contains(" why=ref:R6,terms:1/2 refs="),
        "{}",
        lines[2]
    );
}

#[test]
fn query_footer_names_missing_terms_more_matches_shared_refs_and_next() {
    let c = Cli::new();
    let (first, second, _) = c.footer_case();
    let o = c.ok(&[
        "query",
        "R6 yak zebra",
        "--repo",
        "a",
        "--limit",
        "2",
        "--bias",
        "decided",
    ]);
    let footer: Vec<&str> = o
        .out
        .lines()
        .filter(|l| !l.starts_with('[') && !l.starts_with("  "))
        .collect();
    assert_eq!(
        footer,
        [
            "missing: yak=1 -> sm query \"yak\" --repo a --bias decided; zebra=0".to_string(),
            "more: 1 beyond --limit 2 -> sm query \"R6 yak zebra\" --repo a --bias decided --limit 3; other repos b=2 c=1 -> sm query \"R6 yak zebra\" --all --bias decided".to_string(),
            "refs: abc1234=2 db.pool=2".to_string(),
            format!("next: sm show {first} {second}; sm related {first}"),
        ]
    );
}

#[test]
fn query_footer_command_puts_a_dash_query_after_the_double_dash() {
    let c = Cli::new();
    c.footer_case();
    let o = c.ok(&["query", "--repo", "b", "--limit", "1", "--", "-yak"]);
    assert_eq!(
        o.more(),
        [
            "more: 1 beyond --limit 1 -> sm query --repo b --limit 2 -- \"-yak\"; other repos a=1 c=1 -> sm query --all -- \"-yak\""
        ]
    );
}

#[test]
fn query_with_no_hit_in_scope_prints_only_where_the_matches_are() {
    let c = Cli::new();
    c.footer_case();
    let o = c.ok(&["query", "another", "--repo", "a"]);
    assert_eq!(
        o.lines(),
        [
            "missing: another=0",
            "more: other repos b=1 -> sm query \"another\" --all"
        ]
    );
    assert_eq!(
        c.ok(&["query", "nothingmatches", "--repo", "a"]).out,
        "missing: nothingmatches=0\n"
    );
}

#[test]
fn query_brief_prints_one_line_per_hit_then_the_footer() {
    let c = Cli::new();
    let (first, second, _) = c.footer_case();
    let o = c.ok(&[
        "query", "R6", "--repo", "a", "--format", "brief", "--bias", "decided",
    ]);
    let lines = o.lines();
    let day = lines[0].split_whitespace().nth(5).unwrap();
    assert_eq!(
        lines,
        [
            format!("[{first}] #1 a decision decided {day} ~13 tok: Kiwi plan for the rollout"),
            format!("[{second}] #2 a fact derived {day} ~13 tok: Kiwi steps of the rollout"),
            "refs: abc1234=2 db.pool=2".to_string(),
            format!("next: sm show {first} {second}; sm related {first}"),
        ]
    );
}

#[test]
fn query_json_follows_the_i3_schema() {
    let c = Cli::new();
    let (first, _, _) = c.footer_case();
    let out = c
        .ok(&[
            "query", "R6 yak", "--repo", "a", "--limit", "1", "--format", "json", "--bias",
            "decided",
        ])
        .json();
    let as_of = out["results"][0]["as_of"].clone();
    let score = out["results"][0]["why"]["score"].clone();
    assert_eq!(
        out,
        json!({
            "query": "R6 yak",
            "bias": ["decided"],
            "scope": {"repo": "a", "all_repos": false},
            "results": [{
                "rank": 1, "id": first, "repo": "a", "kind": "decision", "tag": "decided", "as_of": as_of,
                "status": "active", "title": "Kiwi plan for the rollout", "snippet": "Kiwi plan for the rollout",
                "refs": ["R6", "abc1234", "db.pool"], "tokens": 13,
                "why": {"ref_hits": ["R6"], "phrase": false, "terms_matched": ["r6"], "terms_missing": ["yak"],
                        "bias": ["decided"], "score": score},
            }],
            "missing": [{"term": "yak", "in_scope": 1}],
            "more": {"beyond_limit": 2, "pool_full": false, "other_repos": {"b": 2, "c": 1}, "other_repos_pool_full": false},
            "refs": [],
            "next": [format!("sm show {first}"), format!("sm related {first}")],
        })
    );
    assert!(score.is_f64() && score.as_f64().unwrap() > 1.0, "{score}");
}

#[test]
fn show_prints_several_items_in_the_order_given() {
    let c = Cli::new();
    let a = c.put("Alpha body", &["--tag", "derived"]);
    let b = c.put("Beta body", &["--tag", "derived"]);
    let o = c.ok(&["show", &b, &a, &b]);
    assert_eq!(
        (id_lines(&o, "id: "), o.err.as_str()),
        (vec![format!("id: {b}"), format!("id: {a}")], "")
    );
    // one blank line between items
    assert!(
        o.out.contains(&format!("\n\nBeta body\n\nid: {a}\n")),
        "{}",
        o.out
    );
}

#[test]
fn show_names_each_missing_id_on_stderr_and_exits_1() {
    let c = Cli::new();
    let a = c.put("Alpha body", &["--tag", "derived"]);
    let o = c.run(&["show", "m_gone1", &a, "m_gone2"]);
    assert_eq!(o.code, 1);
    assert_eq!(id_lines(&o, "id: "), [format!("id: {a}")]);
    assert_eq!(o.err, "not found: m_gone1\nnot found: m_gone2\n");
}

#[test]
fn related_lists_the_chain_shared_refs_and_the_session() {
    let c = Cli::new();
    let sess = "0a1b2c3d-1111-2222-3333-444455556666";
    let old = c.put("Plan v1", &["--tag", "derived", "--refs", "R6"]);
    let mid = c.put(
        "Plan v2",
        &[
            "--tag",
            "derived",
            "--refs",
            "R6 abc1234 common.md",
            "--supersedes",
            &old,
            "--source",
            &format!("handoff:20261101-090000-{sess}.md"),
            "--as-of",
            "2026-11-01",
        ],
    );
    let new = c.put(
        "Plan v3",
        &[
            "--tag",
            "derived",
            "--supersedes",
            &mid,
            "--as-of",
            "2026-10-01",
        ],
    );
    let both = c.put(
        "Shares two refs",
        &[
            "--tag",
            "derived",
            "--refs",
            "R6 abc1234",
            "--as-of",
            "2026-09-01",
        ],
    );
    let one_newer = c.put(
        "Shares one ref",
        &["--tag", "derived", "--refs", "R6", "--as-of", "2026-11-02"],
    );
    let same_session = c.put(
        "Task log of the session",
        &[
            "--tag",
            "derived",
            "--as-of",
            "2026-08-01",
            "--source",
            "task:20261101-090000-0a1b2c3d-plan-1a2b",
        ],
    );
    for n in 0..21 {
        c.put(
            &format!("Common path holder {n}"),
            &["--tag", "derived", "--refs", "common.md"],
        );
    }
    c.put(
        "Other session",
        &[
            "--tag",
            "derived",
            "--source",
            "task:20261101-090000-ffffffff-x-1a2b",
        ],
    );

    let out = c.ok(&["related", &mid, "--format", "json"]).json();
    assert_eq!(out["id"], json!(mid));
    assert_eq!(out["skipped_refs"], json!(["common.md"]));
    let rows: Vec<Value> = out["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| json!([r["id"], r["relation"], r["shared_refs"]]))
        .collect();
    assert_eq!(
        rows,
        vec![
            // the shared-ref scan reads active items only, so the superseded v1 shows as chain only
            json!([old, ["replaces"], []]),
            json!([new, ["replaced-by"], []]),
            json!([both, ["refs"], ["R6", "abc1234"]]),
            json!([one_newer, ["refs"], ["R6"]]),
            json!([same_session, ["session"], []]),
        ]
    );
    let statuses: Vec<&Value> = out["results"].as_array().unwrap()[..2]
        .iter()
        .map(|r| &r["status"])
        .collect();
    assert_eq!(statuses, ["superseded", "active"]);

    let o = c.ok(&["related", &mid, "--limit", "2"]);
    let lines = o.lines();
    assert!(
        lines[0].starts_with(&format!("related to [{mid}] a fact 2026-11-01: Plan v2")),
        "{}",
        lines[0]
    );
    // the chain ranks first, newest first inside it (v1 was put today, v3 carries 2026-10-01)
    assert!(
        lines[1].starts_with(&format!("[{old}] #1 "))
            && lines[1].ends_with(" rel=replaces status=superseded"),
        "{}",
        lines[1]
    );
    // "Plan v3": 7 title + 7 body chars = 14 / 4 -> 4 tokens
    let new_as_of =
        simple_memory::core::item::validate_as_of(Some("2026-10-01"), false, false).unwrap();
    assert_eq!(
        lines[3],
        format!("[{new}] #2 repo=a kind=fact tag=derived as_of={new_as_of} ~4 tok rel=replaced-by")
    );
    assert_eq!(
        &lines[5..],
        [
            "skipped refs (too common): common.md".to_string(),
            format!("next: sm show {old} {new}")
        ]
    );
}

#[test]
fn related_with_an_unknown_id_exits_1() {
    let c = Cli::new();
    let o = c.run(&["related", "m_gone"]);
    assert_eq!((o.code, o.err.as_str()), (1, "not found: m_gone\n"));
    let o = c.run(&["related", "m_gone", "--limit", "0"]);
    assert_eq!(
        (o.code, o.err.as_str()),
        (1, "error: --limit must be at least 1\n")
    );
}

#[test]
fn query_footer_commands_carry_every_filter_flag() {
    let c = Cli::new();
    c.put("Kiwi one", &["--tag", "derived"]);
    c.put("Kiwi two", &["--tag", "derived"]);
    let o = c.ok(&[
        "query",
        "kiwi",
        "--repo",
        "a",
        "--limit",
        "1",
        "--tag",
        "derived",
        "--kind",
        "fact",
        "--include-history",
        "--recent",
    ]);
    assert_eq!(
        o.more(),
        [
            "more: 1 beyond --limit 1 -> sm query \"kiwi\" --repo a --tag derived --kind fact --include-history --recent --limit 2"
        ]
    );
    let o = c.ok(&["query", "kiwi", "--all", "--limit", "1"]);
    assert_eq!(
        o.more(),
        ["more: 1 beyond --limit 1 -> sm query \"kiwi\" --all --limit 2"]
    );
}

#[test]
fn query_footer_quotes_a_query_with_shell_characters() {
    let c = Cli::new();
    c.put("Kiwi one", &["--tag", "derived"]);
    c.put("Kiwi two", &["--tag", "derived"]);
    let o = c.ok(&["query", "kiwi \"$x\"", "--repo", "a", "--limit", "1"]);
    assert_eq!(
        o.more(),
        ["more: 1 beyond --limit 1 -> sm query 'kiwi \"$x\"' --repo a --limit 2"]
    );
}

#[test]
fn query_why_names_the_phrase_and_the_accent_hit() {
    let c = Cli::new();
    c.put("Café deploy notes", &["--tag", "derived"]);
    let o = c.ok(&["query", "café deploy", "--repo", "a"]);
    assert!(
        o.lines()[0].contains(" why=phrase,accent,terms:2/2,bias:fact,recent"),
        "{}",
        o.lines()[0]
    );
}

#[test]
fn query_with_no_word_matches_by_ref_and_prints_no_terms() {
    let c = Cli::new();
    let item = c.put("Plus signs", &["--tag", "derived", "--refs", "+++"]);
    let o = c.ok(&["query", "+++", "--repo", "a"]);
    let lines = o.lines();
    assert!(
        lines[0].ends_with(" ~5 tok why=ref:+++,bias:fact,recent refs=+++"),
        "{}",
        lines[0]
    );
    assert_eq!(
        &lines[1..],
        [
            "  Plus signs".to_string(),
            format!("next: sm show {item}; sm related {item}")
        ]
    );
    // from repo b the ref match in repo a is the only match, so the footer points there
    assert_eq!(
        c.ok(&["query", "+++", "--repo", "b"]).out,
        "more: other repos a=1 -> sm query \"+++\" --all\n"
    );
}

#[test]
fn query_brief_and_json_cut_long_text() {
    let c = Cli::new();
    let title = format!("Kiwi {}", "t".repeat(200));
    c.put(
        &format!("Kiwi {}", "w".repeat(400)),
        &["--tag", "derived", "--title", &title],
    );
    let o = c.ok(&["query", "kiwi", "--repo", "a", "--format", "brief"]);
    // brief titles stop at 160 characters, the last 3 of them "..."
    assert!(
        o.lines()[0].ends_with(&format!(": Kiwi {}...", "t".repeat(152))),
        "{}",
        o.lines()[0]
    );
    let snippet = c
        .ok(&["query", "kiwi", "--repo", "a", "--format", "json"])
        .json()["results"][0]["snippet"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        (snippet.chars().count(), &snippet[snippet.len() - 3..]),
        (300, "...")
    );
}

#[test]
fn related_of_an_item_with_no_link_prints_only_its_line() {
    // "note:None" holds the text None, which a session lookup with no key would match
    let c = Cli::new();
    let lone = c.put("Lone item", &["--tag", "derived", "--source", "note:x"]);
    c.put("Other item", &["--tag", "derived", "--source", "note:None"]);
    let o = c.ok(&["related", &lone]);
    assert!(o.lines()[1..].is_empty(), "{}", o.out);
}

#[test]
fn related_of_an_active_item_leaves_out_itself_and_sources_that_only_hold_the_hex() {
    let c = Cli::new();
    let first = c.put(
        "Session card",
        &[
            "--tag",
            "derived",
            "--refs",
            "R9",
            "--source",
            "session:0a1b2c3d-1111-2222-3333-444455556666",
        ],
    );
    let second = c.put(
        "Task of that session",
        &[
            "--tag",
            "derived",
            "--refs",
            "R9",
            "--source",
            "task:20261101-090000-0a1b2c3d-x-1a2b",
        ],
    );
    c.put(
        "Note naming the hex",
        &["--tag", "derived", "--source", "note:0a1b2c3d"],
    );
    let out = c.ok(&["related", &first, "--format", "json"]).json();
    let rows: Vec<Value> = out["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| json!([r["id"], r["relation"], r["shared_refs"]]))
        .collect();
    assert_eq!(rows, vec![json!([second, ["refs", "session"], ["R9"]])]);
}

#[test]
fn query_more_counts_ranked_candidates_and_marks_a_full_pool() {
    // 60 items hold "nebula" and 5 more hold only "pulsar"; the FTS pool keeps the best 50 per index
    let c = Cli::new();
    for i in 0..60 {
        c.put(&format!("Nebula note {i}"), &["--tag", "derived"]);
    }
    for i in 0..5 {
        c.put(&format!("Pulsar note {i}"), &["--tag", "derived"]);
    }
    let o = c.ok(&["query", "nebula", "--repo", "a", "--bias", "recency"]);
    assert_eq!(
        o.more(),
        [
            "more: 46+ beyond --limit 4 (pool of 50 full) -> sm query \"nebula\" --repo a --bias recency --limit 14"
        ]
    );
    let o = c.ok(&["query", "pulsar", "--repo", "a", "--limit", "2"]);
    assert_eq!(
        o.more(),
        ["more: 3 beyond --limit 2 -> sm query \"pulsar\" --repo a --limit 5"]
    );
    let more = c
        .ok(&[
            "query", "nebula", "--repo", "a", "--bias", "recency", "--format", "json",
        ])
        .json()["more"]
        .clone();
    assert_eq!(
        more,
        json!({"beyond_limit": 46, "pool_full": true, "other_repos": {}, "other_repos_pool_full": true})
    );
    // the default bias (and any tag/kind bias) widens the pool to 100, so all 60 rank and the count is exact
    let more = c
        .ok(&["query", "nebula", "--repo", "a", "--format", "json"])
        .json()["more"]
        .clone();
    assert_eq!(
        more,
        json!({"beyond_limit": 56, "pool_full": false, "other_repos": {}, "other_repos_pool_full": false})
    );
}

#[test]
fn query_more_says_a_full_pool_also_when_limit_takes_the_whole_pool() {
    // 60 items hold "nebula"; with --bias recency the pool keeps the best 50, so --limit 50 and
    // --limit 100 both show all 50 and beyond_limit is 0 although 10 more items match
    let c = Cli::new();
    for i in 0..60 {
        c.put(&format!("Nebula note {i}"), &["--tag", "derived"]);
    }
    for i in 0..5 {
        c.put(&format!("Pulsar note {i}"), &["--tag", "derived"]);
    }
    let want = [
        "more: matches may exist beyond the full pool of 50 -> narrow the query: sm query \"nebula\" --repo a --bias recency",
    ];
    for limit in ["50", "100"] {
        let o = c.ok(&[
            "query", "nebula", "--repo", "a", "--bias", "recency", "--limit", limit,
        ]);
        assert_eq!(o.more(), want, "--limit {limit}");
        let o = c.ok(&[
            "query", "nebula", "--repo", "a", "--bias", "recency", "--limit", limit, "--format",
            "brief",
        ]);
        assert_eq!(o.more(), want, "brief --limit {limit}");
    }
    // JSON keeps its fields: beyond_limit 0 and pool_full true
    let more = c
        .ok(&[
            "query", "nebula", "--repo", "a", "--bias", "recency", "--limit", "100", "--format",
            "json",
        ])
        .json()["more"]
        .clone();
    assert_eq!(
        more,
        json!({"beyond_limit": 0, "pool_full": true, "other_repos": {}, "other_repos_pool_full": true})
    );
    // 5 matches fill no pool: --limit 10 shows all of them and prints no more line
    let o = c.ok(&["query", "pulsar", "--repo", "a", "--limit", "10"]);
    assert_eq!(o.more(), Vec::<&str>::new());
}

#[test]
fn query_in_nfd_ranks_like_the_same_words_in_nfc() {
    // The accented item is stored first, its unaccented twin last; "đường cao tốc" in NFD
    // (macOS file names, some input methods) must keep the accent and phrase tiers of the NFC query.
    use unicode_normalization::UnicodeNormalization;
    let c = Cli::new();
    for i in 0..5 {
        c.put(&format!("báo cáo tuần {i}"), &["--tag", "derived"]);
    }
    c.put("đường cao tốc Hà Nội báo cáo dự án", &["--tag", "derived"]);
    c.put("duong cao toc ha noi bao cao", &["--tag", "derived"]);
    let nfc = "đường cao tốc";
    let nfd: String = nfc.nfd().collect();
    assert_ne!(nfd, nfc);
    let top = |q: &str| {
        let r = c
            .ok(&[
                "query", q, "--repo", "a", "--limit", "2", "--format", "json",
            ])
            .json();
        let hit = &r["results"][0];
        (
            hit["snippet"]
                .as_str()
                .unwrap()
                .chars()
                .take(13)
                .collect::<String>(),
            hit["why"]["phrase"].clone(),
            hit["why"]["score"].clone(),
        )
    };
    let (title, phrase, score) = top(nfc);
    assert_eq!((title.as_str(), phrase), ("đường cao tốc", json!(true)));
    // the NFD query scores byte for byte like the NFC one: the accent tier (not in the JSON) is in the score
    assert_eq!(top(&nfd), ("đường cao tốc".to_string(), json!(true), score));
}

#[test]
fn query_other_repos_count_only_the_best_matches_across_all_repos() {
    // Repo b holds 60 weak matches (the word once in a long body); repo c and the scoped repo a
    // hold strong ones. The pool across all repos keeps the best 50: a 1, c 2, b 47.
    let c = Cli::new();
    let filler: String = (0..80)
        .map(|n| format!("filler{n}"))
        .collect::<Vec<_>>()
        .join(" ");
    for i in 0..60 {
        c.ok(&[
            "put",
            "--repo",
            "b",
            "--tag",
            "derived",
            "--title",
            &format!("Weak {i}"),
            &format!("nebula {i} {filler}"),
        ]);
    }
    for i in 0..2 {
        c.ok(&[
            "put",
            "--repo",
            "c",
            "--tag",
            "derived",
            &format!("Nebula nebula nebula {i}"),
        ]);
    }
    c.put("Nebula nebula nebula here", &["--tag", "derived"]);
    let o = c.ok(&["query", "nebula", "--repo", "a", "--bias", "recency"]);
    assert_eq!(
        o.more(),
        ["more: other repos b=47+ c=2+ -> sm query \"nebula\" --all --bias recency"]
    );
    let more = c
        .ok(&[
            "query", "nebula", "--repo", "a", "--bias", "recency", "--format", "json",
        ])
        .json()["more"]
        .clone();
    assert_eq!(
        more,
        json!({"beyond_limit": 0, "pool_full": false, "other_repos": {"b": 47, "c": 2}, "other_repos_pool_full": true})
    );
}

#[test]
fn query_bias_given_twice_still_rejects_an_unknown_value() {
    let c = Cli::new();
    let o = c.run(&["query", "kiwi", "--bias", "executed", "--bias", "newest"]);
    assert_eq!(o.code, 2);
    assert!(
        o.err.contains("argument --bias: Invalid bias 'newest'"),
        "{}",
        o.err
    );
}

#[test]
fn put_with_two_refs_flags_stores_both_refs() {
    let c = Cli::new();
    let id = c.put(
        "Walrus two refs flags.",
        &["--tag", "decided", "--refs", "PROJ-88", "--refs", "b/c.py"],
    );
    let o = c.ok(&["show", &id]);
    assert!(o.out.contains("\nrefs: PROJ-88 b/c.py\n"), "{}", o.out);
}

#[test]
fn query_footer_re_emits_a_repeated_filter_as_one_valid_flag() {
    let c = Cli::new();
    c.footer_case();
    let a = [
        "--repo", "a", "--limit", "1", "--kind", "fact", "--kind", "decision",
    ];
    let mut v = vec!["query", "R6 yak zebra"];
    v.extend(a);
    let o = c.ok(&v);
    assert!(
        o.out
            .contains("missing: yak=1 -> sm query \"yak\" --repo a --kind fact,decision;"),
        "{}",
        o.out
    );
    let o = c.ok(&["query", "yak", "--repo", "a", "--kind", "fact,decision"]);
    assert_eq!(o.out.matches("[m_").count(), 1, "{}", o.out);
}

#[test]
fn default_bias_favors_recency_durable_kinds_and_strong_evidence_and_respects_config() {
    let c = Cli::new();
    let old_exec_fact = c.put(
        "Quokka cache note A",
        &[
            "--kind",
            "fact",
            "--tag",
            "executed",
            "--evidence",
            "cmd",
            "--as-of",
            "2026-01-01",
        ],
    );
    let exec_fact = c.put(
        "Quokka cache note B",
        &[
            "--kind",
            "fact",
            "--tag",
            "executed",
            "--evidence",
            "cmd",
            "--as-of",
            "2026-11-01",
        ],
    );
    let der_fact = c.put(
        "Quokka cache note C",
        &[
            "--kind",
            "fact",
            "--tag",
            "derived",
            "--as-of",
            "2026-11-01",
        ],
    );
    let handoff = c.put(
        "Quokka cache note D",
        &[
            "--kind",
            "handoff",
            "--tag",
            "derived",
            "--as-of",
            "2026-11-01",
        ],
    );
    let ep = c.put(
        "Quokka cache note E",
        &[
            "--kind",
            "episode",
            "--tag",
            "assumed",
            "--as-of",
            "2026-11-01",
        ],
    );
    let out = c
        .ok(&[
            "query",
            "quokka cache",
            "--repo",
            "a",
            "--limit",
            "10",
            "--format",
            "json",
        ])
        .json();
    let ids: Vec<&str> = out["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids[0], exec_fact);
    assert_eq!(ids[1], old_exec_fact);
    assert_eq!(ids[2], der_fact);
    assert!(ids[3..].contains(&handoff.as_str()) && ids[3..].contains(&ep.as_str()));

    std::fs::write(
        c.dir.path().join("config.toml"),
        "[query]\ndefault_bias = [\"handoff\"]\n",
    )
    .unwrap();
    let custom = c
        .ok(&["query", "quokka cache", "--repo", "a", "--format", "json"])
        .json();
    assert_eq!(custom["bias"], json!(["handoff"]));
    assert_eq!(custom["results"][0]["id"], handoff);
    let empty_bias = c
        .ok(&[
            "query",
            "quokka cache",
            "--repo",
            "a",
            "--bias",
            "",
            "--format",
            "json",
        ])
        .json();
    assert_eq!(empty_bias["bias"], json!(["handoff"]));
    assert_eq!(empty_bias["results"][0]["id"], handoff);

    std::fs::write(
        c.dir.path().join("config.toml"),
        "[query]\ndefault_bias = []\n",
    )
    .unwrap();
    let empty_cfg = c
        .ok(&[
            "query",
            "quokka cache",
            "--repo",
            "a",
            "--limit",
            "10",
            "--format",
            "json",
        ])
        .json();
    assert_eq!(empty_cfg["results"][0]["id"], exec_fact);
    assert_eq!(empty_cfg["results"][1]["id"], old_exec_fact);

    let overridden = c
        .ok(&[
            "query",
            "quokka cache",
            "--repo",
            "a",
            "--bias",
            "relevance",
            "--format",
            "json",
        ])
        .json();
    assert_eq!(overridden["bias"], json!(["relevance"]));

    std::fs::write(
        c.dir.path().join("config.toml"),
        "[query]\ndefault_bias = [\"bogus\"]\n",
    )
    .unwrap();
    let bad = c.run(&["query", "quokka", "--repo", "a"]);
    assert_eq!(bad.code, 1);
    assert!(
        bad.err
            .contains("error: [query] default_bias: Invalid bias 'bogus'"),
        "{}",
        bad.err
    );
}
