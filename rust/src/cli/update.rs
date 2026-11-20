//! The `update` command: check the npm registry for a newer release and optionally upgrade.

use std::cmp::Ordering;
use std::io::Write;
use std::process::Command;

use serde_json::{Value, json};

use crate::clock;
use crate::config::get_home;
use crate::error::SmError;
use crate::py::argparse::Namespace;
use crate::py::io::Out;

const DEFAULT_UPDATE_URL: &str = "https://registry.npmjs.org/simple-memory/latest";
const UPDATE_CACHE_FILE: &str = "update-check.json";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Parsed SemVer 2.0.0 `(major, minor, patch, prerelease)`; build metadata is ignored for ordering.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SemVer {
    major: u64,
    minor: u64,
    patch: u64,
    pre: Option<Vec<String>>,
}

impl SemVer {
    fn parse(raw: &str) -> Option<SemVer> {
        let s = raw.trim();
        let s = s
            .strip_prefix('v')
            .or_else(|| s.strip_prefix('V'))
            .unwrap_or(s);
        let without_build = s.split_once('+').map_or(s, |(left, _)| left);
        let (core, pre_raw) = match without_build.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (without_build, None),
        };
        let mut parts = core.split('.');
        let major = parse_uint(parts.next()?)?;
        let minor = parse_uint(parts.next()?)?;
        let patch = parse_uint(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        let pre = match pre_raw {
            None => None,
            Some(p) => {
                if p.is_empty() {
                    return None;
                }
                let ids: Vec<String> = p.split('.').map(String::from).collect();
                if ids.iter().any(|id| id.is_empty()) {
                    return None;
                }
                Some(ids)
            }
        };
        Some(SemVer {
            major,
            minor,
            patch,
            pre,
        })
    }
}

fn parse_uint(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u64>().ok()
}

fn cmp_pre_id(a: &str, b: &str) -> Ordering {
    match (parse_uint(a), parse_uint(b)) {
        (Some(na), Some(nb)) => na.cmp(&nb),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.cmp(b),
    }
}

impl Ord for SemVer {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => {
                    for (ia, ib) in a.iter().zip(b.iter()) {
                        let ord = cmp_pre_id(ia, ib);
                        if ord != Ordering::Equal {
                            return ord;
                        }
                    }
                    a.len().cmp(&b.len())
                }
            })
    }
}

impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn update_url() -> String {
    std::env::var("SM_UPDATE_CHECK_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_UPDATE_URL.to_string())
}

fn parse_registry_version(bytes: &[u8]) -> Result<(String, SemVer), String> {
    let val: Value =
        serde_json::from_slice(bytes).map_err(|e| format!("invalid registry JSON: {e}"))?;
    let raw = val
        .get("version")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "registry response missing string field 'version'".to_string())?;
    let parsed =
        SemVer::parse(raw).ok_or_else(|| format!("invalid version in registry response: {raw}"))?;
    let canonical = if parsed.pre.is_none() {
        format!("{}.{}.{}", parsed.major, parsed.minor, parsed.patch)
    } else {
        raw.strip_prefix('v')
            .or_else(|| raw.strip_prefix('V'))
            .unwrap_or(raw)
            .to_string()
    };
    Ok((canonical, parsed))
}

fn fetch_latest_version(url: &str) -> Result<(String, SemVer), String> {
    let output = Command::new("curl")
        .args(["-fsSL", "--max-time", "5", url])
        .output()
        .map_err(|e| format!("failed to run curl: {e}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let reason = if detail.is_empty() {
            format!("curl exited with status {}", output.status)
        } else {
            detail
        };
        return Err(reason);
    }
    parse_registry_version(&output.stdout)
}

fn write_cache(home: &std::path::Path, latest: &str) -> Result<(), SmError> {
    std::fs::create_dir_all(home).map_err(|e| SmError::from_io(&e, Some(home)))?;
    let checked_at = clock::now_utc().timestamp_millis();
    let payload = json!({
        "checked_at": checked_at,
        "latest": latest,
    });
    let mut body = serde_json::to_string(&payload)
        .map_err(|e| SmError::value(format!("cannot encode update cache: {e}")))?;
    body.push('\n');
    let mut tmp = tempfile::Builder::new()
        .prefix("update-check.json.")
        .suffix(".tmp")
        .rand_bytes(8)
        .tempfile_in(home)
        .map_err(|e| SmError::from_io(&e, Some(home)))?;
    tmp.as_file_mut()
        .write_all(body.as_bytes())
        .map_err(|e| SmError::from_io(&e, Some(tmp.path())))?;
    let target = home.join(UPDATE_CACHE_FILE);
    tmp.persist(&target)
        .map_err(|e| SmError::from_io(&e.error, Some(&target)))?;
    Ok(())
}

pub fn run(ns: &Namespace, out: &mut Out) -> Result<i32, SmError> {
    let check_only = ns.get_bool("check");
    let url = update_url();
    let (latest_str, latest) = match fetch_latest_version(&url) {
        Ok(v) => v,
        Err(reason) => {
            out.eprint(&format!(
                "sm update: unable to check for updates ({reason})"
            ))?;
            return Ok(1);
        }
    };
    let home = get_home(None)?;
    if latest.pre.is_none() {
        write_cache(&home.to_os(), &latest_str)?;
    }

    let current = SemVer::parse(CURRENT_VERSION)
        .ok_or_else(|| SmError::value(format!("invalid package version: {CURRENT_VERSION}")))?;
    if latest <= current {
        out.print(&format!(
            "simple-memory {CURRENT_VERSION} is up to date (latest: {latest_str})"
        ))?;
        return Ok(0);
    }

    let is_npm = std::env::var("SM_INSTALL_METHOD")
        .ok()
        .is_some_and(|v| v.trim() == "npm");
    out.print(&format!(
        "update available: simple-memory {CURRENT_VERSION} -> {latest_str}"
    ))?;
    if check_only {
        if is_npm {
            out.print("run: sm update  (or: npm install -g simple-memory@latest)")?;
        } else {
            out.print("run: git pull && ./scripts/install.sh")?;
        }
        return Ok(0);
    }
    if !is_npm {
        out.print("source install detected; update with:")?;
        out.print("  git pull && ./scripts/install.sh")?;
        return Ok(0);
    }

    out.print("updating via npm install -g simple-memory@latest...")?;
    let output = match Command::new("npm")
        .args(["install", "-g", "simple-memory@latest"])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            out.eprint(&format!("sm update: failed to run npm: {e}"))?;
            return Ok(1);
        }
    };
    if !output.stdout.is_empty() {
        out.write(&String::from_utf8_lossy(&output.stdout))?;
    }
    if !output.stderr.is_empty() {
        out.ewrite(&String::from_utf8_lossy(&output.stderr))?;
    }
    if !output.status.success() {
        out.eprint(&format!(
            "sm update: npm install failed (status {})",
            output.status
        ))?;
        return Ok(1);
    }
    out.print(&format!("updated simple-memory to {latest_str}"))?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_parsing_and_ordering() {
        let v = |s: &str| SemVer::parse(s).unwrap_or_else(|| panic!("expected valid semver: {s}"));
        assert!(v("0.6.0") < v("0.6.1"));
        assert!(v("0.6.0") < v("0.7.0"));
        assert!(v("0.6.0") < v("0.10.0"));
        assert!(v("0.99.99") < v("1.0.0"));
        assert_eq!(v("v0.6.0"), v("0.6.0"));
        assert_eq!(v("0.6.0+build.1"), v("0.6.0+build.99"));
        assert!(v("0.6.0-rc.1") < v("0.6.0"));
        assert!(v("0.6.0-rc.2") < v("0.6.0-rc.10"));
        assert!(v("0.6.0-1") < v("0.6.0-alpha"));
        assert!(v("0.6.0-alpha") < v("0.6.0-alpha.1"));

        for bad in [
            "",
            "0.6",
            "0.6.0.1",
            "a.b.c",
            "0.6.-1",
            "0.6.0-",
            "0.6.0-a..b",
        ] {
            assert_eq!(SemVer::parse(bad), None, "should reject {bad:?}");
        }
    }

    #[test]
    fn parse_registry_version_validates_json_and_semver() {
        let (raw, sv) = parse_registry_version(br#"{"name":"simple-memory","version":"0.7.0"}"#)
            .expect("valid payload");
        assert_eq!(raw, "0.7.0");
        assert_eq!(sv, SemVer::parse("0.7.0").unwrap());
        let (vraw, vsv) =
            parse_registry_version(br#"{"version":"v0.7.0+build.1"}"#).expect("v-prefixed payload");
        assert_eq!(vraw, "0.7.0");
        assert_eq!(vsv, SemVer::parse("0.7.0").unwrap());

        assert!(parse_registry_version(b"not json").is_err());
        assert!(parse_registry_version(br#"{"name":"simple-memory"}"#).is_err());
        assert!(parse_registry_version(br#"{"version": 123}"#).is_err());
        assert!(parse_registry_version(br#"{"version": ""}"#).is_err());
        assert!(parse_registry_version(br#"{"version": "bad-ver"}"#).is_err());
    }

    #[test]
    fn write_cache_writes_atomically_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("sm-home");
        write_cache(&home, "0.7.0").unwrap();
        write_cache(&home, "0.8.0").unwrap();
        let text = std::fs::read_to_string(home.join(UPDATE_CACHE_FILE)).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["latest"], "0.8.0");
        let ts = v["checked_at"].as_i64().unwrap();
        assert!(ts > 1_700_000_000_000, "unexpected epoch ms ts: {ts}");
        // No leftover temp files in home.
        let entries: Vec<_> = std::fs::read_dir(&home)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries, vec![UPDATE_CACHE_FILE]);
    }
}
