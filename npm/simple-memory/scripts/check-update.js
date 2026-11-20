"use strict";

const crypto = require("node:crypto");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { cachePathForEnv, parseSemver } = require("../lib/update");

const DEFAULT_UPDATE_URL = "https://registry.npmjs.org/simple-memory/latest";
const DEFAULT_TIMEOUT_MS = 3000;
const MAX_RESPONSE_BYTES = 256 * 1024;

async function readTextWithLimit(body, maxBytes) {
  if (!body) {
    return "";
  }
  const chunks = [];
  let total = 0;
  for await (const chunk of body) {
    const buf = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    total += buf.length;
    if (total > maxBytes) {
      if (typeof body.cancel === "function") {
        await body.cancel().catch(() => {});
      }
      throw new Error("Response exceeded size limit");
    }
    chunks.push(buf);
  }
  return Buffer.concat(chunks, total).toString("utf8");
}

function writeCacheAtomic(cachePath, checkedAt, latest) {
  const dir = path.dirname(cachePath);
  fs.mkdirSync(dir, { recursive: true });
  const nonce = `${process.pid}.${crypto.randomBytes(6).toString("hex")}`;
  const tmpPath = path.join(dir, `.${path.basename(cachePath)}.tmp.${nonce}`);
  const payload = JSON.stringify({ checked_at: checkedAt, latest }) + "\n";
  try {
    fs.writeFileSync(tmpPath, payload, { mode: 0o644 });
    fs.renameSync(tmpPath, cachePath);
  } finally {
    try {
      fs.unlinkSync(tmpPath);
    } catch {
      // Temp file was already renamed or never created.
    }
  }
}

async function fetchAndWriteUpdateCache(options = {}) {
  const {
    cachePath,
    env = process.env,
    url = (env && env.SM_UPDATE_CHECK_URL) || DEFAULT_UPDATE_URL,
    timeoutMs = DEFAULT_TIMEOUT_MS,
    fetchImpl = globalThis.fetch,
    nowMs = null,
    maxBytes = MAX_RESPONSE_BYTES,
  } = options;

  if (typeof cachePath !== "string" || cachePath.length === 0) {
    return false;
  }

  try {
    const res = await fetchImpl(url, {
      headers: { accept: "application/json" },
      signal: AbortSignal.timeout(timeoutMs),
    });
    if (!res || !res.ok) {
      if (res && res.body && typeof res.body.cancel === "function") {
        await res.body.cancel().catch(() => {});
      }
      return false;
    }
    const text = await readTextWithLimit(res.body, maxBytes);
    const data = JSON.parse(text);
    const version = data && typeof data.version === "string" ? data.version.trim() : "";
    if (!parseSemver(version)) {
      return false;
    }
    const checkedAt = typeof nowMs === "number" ? nowMs : Date.now();
    writeCacheAtomic(cachePath, checkedAt, version);
    return true;
  } catch {
    return false;
  }
}

if (require.main === module) {
  const cachePath = process.argv[2] || cachePathForEnv(process.env, os.homedir());
  fetchAndWriteUpdateCache({ cachePath, env: process.env }).then(
    () => process.exit(0),
    () => process.exit(0)
  );
}

module.exports = {
  DEFAULT_UPDATE_URL,
  DEFAULT_TIMEOUT_MS,
  MAX_RESPONSE_BYTES,
  fetchAndWriteUpdateCache,
};
