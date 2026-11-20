"use strict";

const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

// Semver = { major: number, minor: number, patch: number }
// UpdateCache = { checked_at: number, latest: string }
// NotifyResult = { notified: boolean, refreshed: boolean, latest: string | null }
const DEFAULT_TTL_MS = 86400_000;
const SKIP_SUBCOMMANDS = new Set(["mcp", "hook", "hooks", "update"]);
const SKIP_FLAGS = new Set(["--version", "-v", "-h", "--help"]);

function parseSemver(v) {
  if (typeof v !== "string") {
    return null;
  }
  const m = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.exec(v);
  if (!m) {
    return null;
  }
  const major = Number(m[1]);
  const minor = Number(m[2]);
  const patch = Number(m[3]);
  if (
    !Number.isSafeInteger(major) ||
    !Number.isSafeInteger(minor) ||
    !Number.isSafeInteger(patch)
  ) {
    return null;
  }
  return { major, minor, patch };
}

function isNewerVersion(latest, current) {
  const a = parseSemver(latest);
  const b = parseSemver(current);
  if (!a || !b) {
    return false;
  }
  if (a.major !== b.major) {
    return a.major > b.major;
  }
  if (a.minor !== b.minor) {
    return a.minor > b.minor;
  }
  return a.patch > b.patch;
}

function shouldCheckUpdate(options = {}) {
  const { argv = [], env = {}, isStderrTTY = false } = options;
  if (isStderrTTY !== true) {
    return false;
  }
  if (env && env.SM_NO_UPDATE_CHECK === "1") {
    return false;
  }
  const ciRaw = env && typeof env.CI === "string" ? env.CI.trim().toLowerCase() : "";
  if (ciRaw.length > 0 && ciRaw !== "0" && ciRaw !== "false") {
    return false;
  }
  if (!Array.isArray(argv)) {
    return false;
  }
  if (argv.length > 0 && SKIP_SUBCOMMANDS.has(argv[0])) {
    return false;
  }
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === "--") {
      break;
    }
    if (SKIP_FLAGS.has(arg)) {
      return false;
    }
    if (arg === "--format=json") {
      return false;
    }
    if (arg === "--format" && argv[i + 1] === "json") {
      return false;
    }
  }
  return true;
}

function expandTilde(p, homeDir) {
  if (p === "~") {
    return homeDir;
  }
  if (p.startsWith("~/") || p.startsWith("~\\")) {
    return path.join(homeDir, p.slice(2));
  }
  return p;
}

function cachePathForEnv(env = process.env, homeDir = os.homedir()) {
  const smHome =
    env && typeof env.SIMPLE_MEMORY_HOME === "string" && env.SIMPLE_MEMORY_HOME.trim().length > 0
      ? expandTilde(env.SIMPLE_MEMORY_HOME.trim(), homeDir)
      : path.join(homeDir, ".simple-memory");
  return path.join(smHome, "update-check.json");
}

function readUpdateCache(cachePath) {
  if (typeof cachePath !== "string" || cachePath.length === 0) {
    return null;
  }
  let raw;
  try {
    raw = fs.readFileSync(cachePath, "utf8");
  } catch {
    return null;
  }
  let parsed;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return null;
  }
  if (
    !parsed ||
    typeof parsed !== "object" ||
    typeof parsed.checked_at !== "number" ||
    !Number.isFinite(parsed.checked_at) ||
    parsed.checked_at < 0 ||
    parseSemver(parsed.latest) === null
  ) {
    return null;
  }
  return { checked_at: parsed.checked_at, latest: parsed.latest };
}

function isCacheStale(cache, nowMs = Date.now(), ttlMs = DEFAULT_TTL_MS) {
  if (!cache || typeof cache.checked_at !== "number" || !Number.isFinite(cache.checked_at)) {
    return true;
  }
  return nowMs - cache.checked_at >= ttlMs;
}

function formatUpdateNotice(current, latest) {
  return (
    `Update available: ${current} -> ${latest}. ` +
    "Run: sm update (or npm install -g simple-memory@latest; disable: SM_NO_UPDATE_CHECK=1)\n"
  );
}

function maybeNotifyAndRefresh(options = {}) {
  const {
    argv = [],
    env = process.env,
    currentVersion = "",
    isStderrTTY = false,
    stderr = process.stderr,
    spawnRefresh = null,
    homeDir = os.homedir(),
    nowMs = Date.now(),
    ttlMs = DEFAULT_TTL_MS,
  } = options;

  if (!shouldCheckUpdate({ argv, env, isStderrTTY })) {
    return { notified: false, refreshed: false, latest: null };
  }

  const cachePath = cachePathForEnv(env, homeDir);
  const cache = readUpdateCache(cachePath);
  let notified = false;
  let refreshed = false;

  if (cache && isNewerVersion(cache.latest, currentVersion)) {
    try {
      stderr.write(formatUpdateNotice(currentVersion, cache.latest));
      notified = true;
    } catch {
      // Best-effort stderr notice must never alter command exit status.
    }
  }

  if (isCacheStale(cache, nowMs, ttlMs) && typeof spawnRefresh === "function") {
    try {
      spawnRefresh(cachePath);
      refreshed = true;
    } catch {
      // Best-effort background refresh must never alter command exit status.
    }
  }

  return { notified, refreshed, latest: cache ? cache.latest : null };
}

module.exports = {
  DEFAULT_TTL_MS,
  parseSemver,
  isNewerVersion,
  shouldCheckUpdate,
  cachePathForEnv,
  readUpdateCache,
  isCacheStale,
  formatUpdateNotice,
  maybeNotifyAndRefresh,
};
