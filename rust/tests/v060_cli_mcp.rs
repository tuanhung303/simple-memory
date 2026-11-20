//! End-to-end tests for simple-memory v0.6.0 CLI ergonomics, `sm show --format json`,
//! `sm list` cross-repo empty hint, `sm stats` retired split, and `sm update [--check]`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

struct Fx {
    tmp: tempfile::TempDir,
}

impl Fx {
    fn new() -> Fx {
        Fx {
            tmp: tempfile::tempdir().unwrap(),
        }
    }

    fn root(&self) -> &Path {
        self.tmp.path()
    }

    fn home(&self) -> PathBuf {
        self.root().join("sm-home")
    }

    fn sm_env(&self, args: &[&str], extra_env: &[(&str, &str)]) -> (i32, String, String) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sm"));
        cmd.args(args)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap())
            .env("HOME", self.root())
            .env("SIMPLE_MEMORY_HOME", self.home())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(self.root());
        for key in ["TZ", "SM_UTC_OFFSET"] {
            if let Some(val) = std::env::var_os(key) {
                cmd.env(key, val);
            }
        }
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8(out.stdout).unwrap(),
            String::from_utf8(out.stderr).unwrap(),
        )
    }

    fn sm(&self, args: &[&str]) -> (i32, String, String) {
        self.sm_env(args, &[])
    }

    fn put(&self, args: &[&str]) -> String {
        let mut full = vec!["put"];
        full.extend_from_slice(args);
        let (code, out, err) = self.sm(&full);
        assert_eq!(code, 0, "sm put failed: {err}");
        out.trim().to_string()
    }
}

fn write_exec(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

#[test]
fn version_flags_print_060() {
    let fx = Fx::new();
    for flag in ["--version", "-v"] {
        let (code, out, err) = fx.sm(&[flag]);
        assert_eq!((code, out.as_str(), err.as_str()), (0, "sm 0.6.0\n", ""));
    }
    let (code, _, err) = fx.sm(&["-v=1"]);
    assert_eq!(code, 2);
    assert!(
        err.contains("argument -v/--version: ignored explicit argument '1'"),
        "{err}"
    );
}

#[test]
fn query_unquoted_multi_word_search_alias_and_in_repo_conflict() {
    let fx = Fx::new();
    let id = fx.put(&[
        "--repo",
        "anvil",
        "--tag",
        "decided",
        "Cobalt rollout checklist",
    ]);

    let (c1, quoted, e1) = fx.sm(&[
        "query",
        "cobalt rollout",
        "--repo",
        "anvil",
        "--format",
        "json",
    ]);
    assert_eq!(c1, 0, "{e1}");
    let (c2, unquoted, e2) = fx.sm(&[
        "query", "cobalt", "rollout", "--repo", "anvil", "--format", "json",
    ]);
    assert_eq!(c2, 0, "{e2}");
    let (c3, alias, e3) = fx.sm(&[
        "search", "cobalt", "rollout", "--repo", "anvil", "--format", "json",
    ]);
    assert_eq!(c3, 0, "{e3}");

    let v1: Value = serde_json::from_str(&quoted).unwrap();
    let v2: Value = serde_json::from_str(&unquoted).unwrap();
    let v3: Value = serde_json::from_str(&alias).unwrap();
    assert_eq!(v1, v2);
    assert_eq!(v1, v3);
    assert_eq!(v1["results"][0]["id"], id);

    // --in and --repo are mutually exclusive (exit 2) on both query and search.
    let (code, _, err) = fx.sm(&["query", "cobalt", "--in", "acme", "--repo", "anvil"]);
    assert_eq!(code, 2);
    assert!(
        err.contains("sm query: error: argument --repo: not allowed with argument --in"),
        "{err}"
    );
    let (code, _, err) = fx.sm(&["search", "cobalt", "--repo", "anvil", "--in", "acme"]);
    assert_eq!(code, 2);
    assert!(
        err.contains("sm search: error: argument --in: not allowed with argument --repo"),
        "{err}"
    );
}

#[test]
fn query_brief_includes_repo_and_superseded_or_retired_markers() {
    let fx = Fx::new();
    let utc = &[("SM_UTC_OFFSET", "+00:00")];
    let put_utc = |args: &[&str]| -> String {
        let mut full = vec!["put"];
        full.extend_from_slice(args);
        let (code, out, err) = fx.sm_env(&full, utc);
        assert_eq!(code, 0, "sm put failed: {err}");
        out.trim().to_string()
    };
    let old = put_utc(&[
        "--repo",
        "anvil",
        "--tag",
        "assumed",
        "--as-of",
        "2026-06-01T10:00:00Z",
        "Cobalt cache old",
    ]);
    let cur = put_utc(&[
        "--repo",
        "anvil",
        "--tag",
        "decided",
        "--as-of",
        "2026-06-02T10:00:00Z",
        "Cobalt cache current",
    ]);
    let ret = put_utc(&[
        "--repo",
        "anvil",
        "--tag",
        "assumed",
        "--as-of",
        "2026-06-03T10:00:00Z",
        "Cobalt cache retired",
    ]);
    assert_eq!(fx.sm_env(&["supersede", &old, &cur], utc).0, 0);
    assert_eq!(
        fx.sm_env(&["retire", &ret, "--reason", "obsolete"], utc).0,
        0
    );

    let (code, out, err) = fx.sm_env(
        &[
            "search",
            "cobalt",
            "cache",
            "--repo",
            "anvil",
            "--include-history",
            "--format",
            "brief",
        ],
        utc,
    );
    assert_eq!(code, 0, "{err}");
    let hit_lines: Vec<&str> = out.lines().filter(|l| l.starts_with('[')).collect();
    assert_eq!(hit_lines.len(), 3, "{out}");

    let cur_line = hit_lines
        .iter()
        .find(|l| l.starts_with(&format!("[{cur}]")))
        .unwrap();
    assert!(
        cur_line.contains(" anvil fact decided 2026-06-02 "),
        "{cur_line}"
    );
    assert!(cur_line.ends_with(": Cobalt cache current"), "{cur_line}");
    assert!(!cur_line.contains("[superseded_by="), "{cur_line}");
    assert!(!cur_line.contains("[retired:"), "{cur_line}");

    let old_line = hit_lines
        .iter()
        .find(|l| l.starts_with(&format!("[{old}]")))
        .unwrap();
    assert!(
        old_line.contains(" anvil fact assumed 2026-06-01 "),
        "{old_line}"
    );
    assert!(
        old_line.contains(&format!("[superseded_by={cur}]: Cobalt cache old")),
        "{old_line}"
    );

    let ret_line = hit_lines
        .iter()
        .find(|l| l.starts_with(&format!("[{ret}]")))
        .unwrap();
    assert!(
        ret_line.contains(" anvil fact assumed 2026-06-03 "),
        "{ret_line}"
    );
    assert!(
        ret_line.contains("[retired: obsolete]: Cobalt cache retired"),
        "{ret_line}"
    );
}

#[test]
fn show_format_json_emits_full_provenance_array_and_handles_missing() {
    let fx = Fx::new();
    let utc = &[("SM_UTC_OFFSET", "+00:00")];
    let put_utc = |args: &[&str]| -> String {
        let mut full = vec!["put"];
        full.extend_from_slice(args);
        let (code, out, err) = fx.sm_env(&full, utc);
        assert_eq!(code, 0, "sm put failed: {err}");
        out.trim().to_string()
    };
    let id1 = put_utc(&[
        "--repo",
        "anvil",
        "--kind",
        "decision",
        "--tag",
        "verified",
        "--evidence",
        "cargo test",
        "--title",
        "Pool size decision",
        "--refs",
        "T-11,T-12",
        "--tags",
        "db,pool",
        "--source",
        "task:T-11",
        "--as-of",
        "2026-06-10T08:00:00Z",
        "Pool size is 16 connections",
    ]);
    let id2 = put_utc(&[
        "--repo",
        "beta",
        "--tag",
        "assumed",
        "--as-of",
        "2026-06-11T08:00:00Z",
        "Second note body",
    ]);

    let (code, out, err) = fx.sm_env(&["show", &id2, &id1, "--format", "json"], utc);
    assert_eq!((code, err.as_str()), (0, ""));
    let arr: Vec<Value> = serde_json::from_str(&out).unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["id"], id2);
    assert_eq!(
        arr[1],
        json!({
            "id": id1,
            "repo": "anvil",
            "kind": "decision",
            "title": "Pool size decision",
            "tag": "verified",
            "evidence": "cargo test",
            "as_of": "2026-06-10T08:00:00+00:00",
            "source": "task:T-11",
            "refs": ["T-11", "T-12"],
            "tags": "db,pool",
            "status": "active",
            "superseded_by": null,
            "body": "Pool size is 16 connections",
        })
    );

    // Partial missing: exit 1, stdout stays valid JSON array of found items, stderr lists missing.
    let (code, out, err) = fx.sm(&["show", &id1, "m_missing", "--format", "json"]);
    assert_eq!(code, 1);
    assert_eq!(err, "not found: m_missing\n");
    let partial: Vec<Value> = serde_json::from_str(&out).unwrap();
    assert_eq!(partial.len(), 1);
    assert_eq!(partial[0]["id"], id1);

    // All missing: exit 1, stdout is empty JSON array.
    let (code, out, err) = fx.sm(&["show", "m_missing", "--format", "json"]);
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (1, "[]\n", "not found: m_missing\n")
    );

    // Invalid format exits 2.
    let (code, _, err) = fx.sm(&["show", &id1, "--format", "xml"]);
    assert_eq!(code, 2);
    assert!(err.contains("invalid choice: 'xml'"), "{err}");
}

#[test]
fn list_empty_repo_prints_other_repos_hint_only_when_appropriate() {
    let fx = Fx::new();

    // Completely empty store prints nothing.
    let (code, out, err) = fx.sm(&["list", "--repo", "anvil"]);
    assert_eq!((code, out.as_str(), err.as_str()), (0, "", ""));

    // Seed items in other repos: 2 active in beta, 1 active in global, 1 retired in gamma.
    fx.put(&["--repo", "beta", "--tag", "decided", "Beta item 1"]);
    fx.put(&["--repo", "beta", "--tag", "decided", "Beta item 2"]);
    fx.put(&["--repo", "global", "--tag", "decided", "Global item 1"]);
    let g_ret = fx.put(&["--repo", "gamma", "--tag", "assumed", "Gamma retired"]);
    assert_eq!(fx.sm(&["retire", &g_ret]).0, 0);

    // Empty repo `anvil` prints hint sorted by count DESC, ignoring retired-only `gamma`.
    let (code, out, err) = fx.sm(&["list", "--repo", "anvil"]);
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (0, "other repos: beta=2 global=1 -> sm list --all\n", "")
    );
    let (code, out, err) = fx.sm(&["list", "--repo", "anvil", "--format", "text"]);
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (0, "other repos: beta=2 global=1 -> sm list --all\n", "")
    );

    // JSON format stays pure JSON without raw hint text.
    let (code, out, err) = fx.sm(&["list", "--repo", "anvil", "--format", "json"]);
    assert_eq!((code, err.as_str()), (0, ""));
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["total"], 0);
    assert_eq!(v["items"], json!([]));

    // Offset past total on non-empty repo `beta` prints nothing.
    let (code, out, err) = fx.sm(&["list", "--repo", "beta", "--offset", "99"]);
    assert_eq!((code, out.as_str(), err.as_str()), (0, "", ""));

    // `--all` with 0 matches prints nothing.
    let (code, out, err) = fx.sm(&["list", "--all", "--kind", "handoff"]);
    assert_eq!((code, out.as_str(), err.as_str()), (0, "", ""));

    // `--in` with 0 matches prints nothing.
    std::fs::write(
        fx.home().join("config.toml"),
        "[scopes]\nacme = [\"repo:anvil\"]\n",
    )
    .unwrap();
    let (code, out, err) = fx.sm(&["list", "--in", "acme"]);
    assert_eq!((code, out.as_str(), err.as_str()), (0, "", ""));
}

#[test]
fn stats_separates_retired_from_superseded_in_text_and_json() {
    let fx = Fx::new();
    let old = fx.put(&["--repo", "anvil", "--tag", "assumed", "Item old"]);
    let cur = fx.put(&["--repo", "anvil", "--tag", "decided", "Item current"]);
    let keep = fx.put(&["--repo", "anvil", "--tag", "decided", "Item keep"]);
    assert_eq!(fx.sm(&["supersede", &old, &cur]).0, 0);

    // retired == 0: text omits `retired=` and JSON omits `"retired"` for backward compatibility.
    let (code, text, err) = fx.sm(&["stats"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        text.lines().next().unwrap(),
        "store: items=3 active=2 superseded=1"
    );
    let (code, jout, err) = fx.sm(&["stats", "--format", "json"]);
    assert_eq!(code, 0, "{err}");
    let j: Value = serde_json::from_str(&jout).unwrap();
    assert_eq!(j["store"]["status"], json!({"active": 2, "superseded": 1}));
    assert_eq!(j["store"]["active"], 2);

    // Retire `keep`: now 1 active, 1 superseded (replaced), 1 retired.
    assert_eq!(fx.sm(&["retire", &keep, "--reason", "done"]).0, 0);
    let (code, text, err) = fx.sm(&["stats"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        text.lines().next().unwrap(),
        "store: items=3 active=1 superseded=1 retired=1"
    );
    let (code, jout, err) = fx.sm(&["stats", "--format", "json"]);
    assert_eq!(code, 0, "{err}");
    let j: Value = serde_json::from_str(&jout).unwrap();
    assert_eq!(
        j["store"]["status"],
        json!({"active": 1, "superseded": 1, "retired": 1})
    );
    assert_eq!(j["store"]["active"], 1);

    // Separate store with 1 active, 0 replaced, 2 retired -> superseded=0 retired=2.
    let fx2 = Fx::new();
    fx2.put(&["--repo", "anvil", "--tag", "decided", "Alive"]);
    let r1 = fx2.put(&["--repo", "anvil", "--tag", "assumed", "Ret 1"]);
    let r2 = fx2.put(&["--repo", "anvil", "--tag", "assumed", "Ret 2"]);
    assert_eq!(fx2.sm(&["retire", &r1]).0, 0);
    assert_eq!(fx2.sm(&["retire", &r2]).0, 0);
    let (code, text, err) = fx2.sm(&["stats"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        text.lines().next().unwrap(),
        "store: items=3 active=1 superseded=0 retired=2"
    );
    let (code, jout, err) = fx2.sm(&["stats", "--format", "json"]);
    assert_eq!(code, 0, "{err}");
    let j: Value = serde_json::from_str(&jout).unwrap();
    assert_eq!(
        j["store"]["status"],
        json!({"active": 1, "superseded": 0, "retired": 2})
    );
}

#[test]
fn update_uses_curl_and_npm_stubs_writes_cache_and_handles_offline() {
    let fx = Fx::new();
    let bin_dir = fx.root().join("stub-bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let curl_log = fx.root().join("curl.log");
    let npm_log = fx.root().join("npm.log");
    let stub_path = format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap());

    // Stub curl reads mode/payload from env so one stub covers every scenario.
    write_exec(
        &bin_dir.join("curl"),
        "#!/bin/sh\n\
         printf '%s\\n' \"$*\" >> \"$STUB_CURL_LOG\"\n\
         if [ \"${STUB_CURL_RC:-0}\" != \"0\" ]; then\n\
           printf '%s\\n' \"${STUB_CURL_ERR:-could not resolve host}\" >&2\n\
           exit \"$STUB_CURL_RC\"\n\
         fi\n\
         printf '%s' \"$STUB_CURL_BODY\"\n",
    );
    write_exec(
        &bin_dir.join("npm"),
        "#!/bin/sh\n\
         printf '%s\\n' \"$*\" >> \"$STUB_NPM_LOG\"\n\
         if [ \"${STUB_NPM_RC:-0}\" != \"0\" ]; then\n\
           printf '%s\\n' \"npm ERR! permission denied\" >&2\n\
           exit \"$STUB_NPM_RC\"\n\
         fi\n\
         printf 'changed 1 package\\n'\n",
    );

    let url = "https://example.invalid/simple-memory/latest";
    let curl_log_s = curl_log.to_str().unwrap();
    let npm_log_s = npm_log.to_str().unwrap();

    // 1. Up to date (0.6.0): writes cache, does not call npm.
    let (code, out, err) = fx.sm_env(
        &["update", "--check"],
        &[
            ("PATH", &stub_path),
            ("SM_UPDATE_CHECK_URL", url),
            ("STUB_CURL_LOG", curl_log_s),
            ("STUB_NPM_LOG", npm_log_s),
            (
                "STUB_CURL_BODY",
                r#"{"name":"simple-memory","version":"0.6.0"}"#,
            ),
        ],
    );
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (0, "simple-memory 0.6.0 is up to date (latest: 0.6.0)\n", "")
    );
    let cache: Value = serde_json::from_str(
        &std::fs::read_to_string(fx.home().join("update-check.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(cache["latest"], "0.6.0");
    assert!(
        cache["checked_at"]
            .as_i64()
            .is_some_and(|ms| ms > 1_700_000_000_000),
        "expected epoch ms checked_at, got: {cache}"
    );
    assert_eq!(
        std::fs::read_to_string(&curl_log).unwrap(),
        format!("-fsSL --max-time 5 {url}\n")
    );
    assert!(!npm_log.exists());

    // 2. Newer release (0.7.0), source install, no --check: prints ./scripts/install.sh, never runs npm.
    let (code, out, err) = fx.sm_env(
        &["update"],
        &[
            ("PATH", &stub_path),
            ("SM_UPDATE_CHECK_URL", url),
            ("STUB_CURL_LOG", curl_log_s),
            ("STUB_NPM_LOG", npm_log_s),
            ("STUB_CURL_BODY", r#"{"version":"0.7.0"}"#),
        ],
    );
    assert_eq!((code, err.as_str()), (0, ""));
    assert!(
        out.contains("update available: simple-memory 0.6.0 -> 0.7.0"),
        "{out}"
    );
    assert!(out.contains("./scripts/install.sh"), "{out}");
    assert!(!npm_log.exists());
    let cache: Value = serde_json::from_str(
        &std::fs::read_to_string(fx.home().join("update-check.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(cache["latest"], "0.7.0");

    // 3. Newer release (0.7.0), SM_INSTALL_METHOD=npm with --check: prints npm hint, does NOT execute npm.
    let (code, out, err) = fx.sm_env(
        &["update", "--check"],
        &[
            ("PATH", &stub_path),
            ("SM_UPDATE_CHECK_URL", url),
            ("SM_INSTALL_METHOD", "npm"),
            ("STUB_CURL_LOG", curl_log_s),
            ("STUB_NPM_LOG", npm_log_s),
            ("STUB_CURL_BODY", r#"{"version":"0.7.0"}"#),
        ],
    );
    assert_eq!((code, err.as_str()), (0, ""));
    assert!(out.contains("npm install -g simple-memory@latest"), "{out}");
    assert!(!npm_log.exists());

    // 4. Newer release (0.7.0), SM_INSTALL_METHOD=npm without --check: executes npm install -g simple-memory@latest.
    let (code, out, err) = fx.sm_env(
        &["update"],
        &[
            ("PATH", &stub_path),
            ("SM_UPDATE_CHECK_URL", url),
            ("SM_INSTALL_METHOD", "npm"),
            ("STUB_CURL_LOG", curl_log_s),
            ("STUB_NPM_LOG", npm_log_s),
            ("STUB_CURL_BODY", r#"{"version":"0.7.0"}"#),
        ],
    );
    assert_eq!((code, err.as_str()), (0, ""));
    assert!(out.contains("updated simple-memory to 0.7.0"), "{out}");
    assert_eq!(
        std::fs::read_to_string(&npm_log).unwrap(),
        "install -g simple-memory@latest\n"
    );

    // 5. Failing npm install exits 1 with clean error.
    let (code, _, err) = fx.sm_env(
        &["update"],
        &[
            ("PATH", &stub_path),
            ("SM_UPDATE_CHECK_URL", url),
            ("SM_INSTALL_METHOD", "npm"),
            ("STUB_CURL_LOG", curl_log_s),
            ("STUB_NPM_LOG", npm_log_s),
            ("STUB_CURL_BODY", r#"{"version":"0.7.0"}"#),
            ("STUB_NPM_RC", "1"),
        ],
    );
    assert_eq!(code, 1);
    assert!(err.contains("sm update: npm install failed"), "{err}");

    // 6. Offline curl failure exits 1 gracefully without traceback and keeps existing cache intact.
    let (code, out, err) = fx.sm_env(
        &["update", "--check"],
        &[
            ("PATH", &stub_path),
            ("SM_UPDATE_CHECK_URL", url),
            ("STUB_CURL_LOG", curl_log_s),
            ("STUB_NPM_LOG", npm_log_s),
            ("STUB_CURL_RC", "6"),
            ("STUB_CURL_ERR", "Could not resolve host"),
        ],
    );
    assert_eq!((code, out.as_str()), (1, ""));
    assert!(
        err.contains("sm update: unable to check for updates (Could not resolve host)"),
        "{err}"
    );
    assert!(!err.contains("Traceback"), "{err}");
    let cache: Value = serde_json::from_str(
        &std::fs::read_to_string(fx.home().join("update-check.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(cache["latest"], "0.7.0");

    // 7. Malformed registry payload exits 1 gracefully.
    let (code, _, err) = fx.sm_env(
        &["update", "--check"],
        &[
            ("PATH", &stub_path),
            ("SM_UPDATE_CHECK_URL", url),
            ("STUB_CURL_LOG", curl_log_s),
            ("STUB_NPM_LOG", npm_log_s),
            ("STUB_CURL_BODY", r#"{"version":"not-a-semver"}"#),
        ],
    );
    assert_eq!(code, 1);
    assert!(
        err.contains("sm update: unable to check for updates (invalid version in registry response: not-a-semver)"),
        "{err}"
    );

    // 8. v-prefixed stable release canonicalizes to strict X.Y.Z in update-check.json; prerelease does not overwrite cache.
    let (code, out, err) = fx.sm_env(
        &["update", "--check"],
        &[
            ("PATH", &stub_path),
            ("SM_UPDATE_CHECK_URL", url),
            ("STUB_CURL_LOG", curl_log_s),
            ("STUB_NPM_LOG", npm_log_s),
            ("STUB_CURL_BODY", r#"{"version":"v0.8.0"}"#),
        ],
    );
    assert_eq!((code, err.as_str()), (0, ""));
    assert!(out.contains("0.6.0 -> 0.8.0"), "{out}");
    let cache: Value = serde_json::from_str(
        &std::fs::read_to_string(fx.home().join("update-check.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(cache["latest"], "0.8.0");
    let (code, _, err) = fx.sm_env(
        &["update", "--check"],
        &[
            ("PATH", &stub_path),
            ("SM_UPDATE_CHECK_URL", url),
            ("STUB_CURL_LOG", curl_log_s),
            ("STUB_NPM_LOG", npm_log_s),
            ("STUB_CURL_BODY", r#"{"version":"0.9.0-rc.1"}"#),
        ],
    );
    assert_eq!((code, err.as_str()), (0, ""));
    let cache: Value = serde_json::from_str(
        &std::fs::read_to_string(fx.home().join("update-check.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(cache["latest"], "0.8.0");
}

#[test]
fn list_empty_repo_hint_respects_all_active_filters() {
    let fx = Fx::new();
    let (c1, _, e1) = fx.sm(&[
        "put",
        "--repo",
        "beta",
        "--kind",
        "fact",
        "--tag",
        "decided",
        "beta fact 1",
    ]);
    assert_eq!(c1, 0, "{e1}");
    let (c2, id_ret, e2) = fx.sm(&[
        "put",
        "--repo",
        "beta",
        "--kind",
        "handoff",
        "--tag",
        "planned",
        "beta handoff to retire",
    ]);
    assert_eq!(c2, 0, "{e2}");
    let (c3, _, e3) = fx.sm(&["retire", id_ret.trim(), "--reason", "done"]);
    assert_eq!(c3, 0, "{e3}");

    // Filtering by --kind handoff (active): beta has 0 active handoffs -> NO false hint.
    let (code, out, err) = fx.sm(&["list", "--repo", "anvil", "--kind", "handoff"]);
    assert_eq!((code, out.as_str(), err.as_str()), (0, "", ""));

    // Filtering by future --since: 0 matches in beta -> NO false hint.
    let (code, out, err) = fx.sm(&["list", "--repo", "anvil", "--since", "2099-01-01"]);
    assert_eq!((code, out.as_str(), err.as_str()), (0, "", ""));

    // Filtering by --status retired: beta has 1 retired item -> accurate count AND carries --status retired.
    let (code, out, err) = fx.sm(&["list", "--repo", "anvil", "--status", "retired"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        out.trim(),
        "other repos: beta=1 -> sm list --all --status retired"
    );

    // Filtering by --kind fact: beta has 1 active fact -> accurate count AND carries --kind fact.
    let (code, out, err) = fx.sm(&["list", "--repo", "anvil", "--kind", "fact"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        out.trim(),
        "other repos: beta=1 -> sm list --all --kind fact"
    );
}
