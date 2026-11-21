"use strict";

const crypto = require("node:crypto");
const fs = require("node:fs");
const path = require("node:path");
const zlib = require("node:zlib");
const { isRegularFile, resolveBinary, resolveTarget } = require("../lib/platform");

const BUILD_HINT = `build from source (git clone ${require("../package.json").repository.url.replace(/^git\+/, "")}, cd simple-memory, then run ./scripts/install.sh)`;
const DEFAULT_TIMEOUT_MS = 15000;
const MAX_SUMS_BYTES = 1024 * 1024;
const MAX_DOWNLOAD_BYTES = 50 * 1024 * 1024;
const MAX_UNCOMPRESSED_BYTES = 100 * 1024 * 1024;
const DEFAULT_RELEASE_BASE = "https://github.com/tuanhung303/simple-memory/releases/download";

function parseTimeoutMs(raw) {
  if (typeof raw !== "string" || raw.trim().length === 0) {
    return DEFAULT_TIMEOUT_MS;
  }
  const value = Number(raw);
  if (!Number.isFinite(value) || value <= 0) {
    return DEFAULT_TIMEOUT_MS;
  }
  return Math.floor(value);
}

function parseSha256Sums(text, assetName) {
  let matched = null;
  for (const rawLine of text.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (line.length === 0) {
      continue;
    }
    const m = /^([0-9a-fA-F]{64})\s+\*?(.+)$/.exec(line);
    if (!m) {
      continue;
    }
    const digest = m[1].toLowerCase();
    const file = m[2].trim();
    if (file === assetName || file === `./${assetName}`) {
      if (matched !== null && matched !== digest) {
        throw new Error(`Conflicting SHA256SUMS entries for ${assetName}`);
      }
      matched = digest;
    }
  }
  return matched;
}

async function readStreamWithLimit(body, maxBytes) {
  if (!body) {
    return Buffer.alloc(0);
  }
  const chunks = [];
  let total = 0;
  for await (const chunk of body) {
    const buf = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    total += buf.length;
    if (total > maxBytes) {
      if (typeof body.cancel === "function") {
        await body.cancel().catch(() => {});
      } else if (typeof body.destroy === "function") {
        body.destroy();
      }
      const err = new Error(`Download exceeded limit of ${maxBytes} bytes`);
      err.code = "ERR_SIZE_LIMIT";
      throw err;
    }
    chunks.push(buf);
  }
  return Buffer.concat(chunks, total);
}

function isZeroBlock(buf) {
  for (let i = 0; i < buf.length; i += 1) {
    if (buf[i] !== 0) {
      return false;
    }
  }
  return true;
}

function readCString(buf, start, end) {
  let stop = start;
  while (stop < end && buf[stop] !== 0) {
    stop += 1;
  }
  return buf.toString("utf8", start, stop);
}

function parseOctal(buf, start, end) {
  const raw = readCString(buf, start, end).trim();
  if (raw.length === 0) {
    return 0;
  }
  if (!/^[0-7]+$/.test(raw)) {
    throw new Error(`Invalid octal field in tar header: ${raw}`);
  }
  return Number.parseInt(raw, 8);
}

function verifyTarChecksum(header) {
  const stored = parseOctal(header, 148, 156);
  let sum = 0;
  for (let i = 0; i < 512; i += 1) {
    sum += i >= 148 && i < 156 ? 0x20 : header[i];
  }
  if (sum !== stored) {
    throw new Error("Invalid tar header checksum");
  }
}

function parsePaxPath(data) {
  let offset = 0;
  let paxPath = null;
  while (offset < data.length) {
    const spaceIdx = data.indexOf(0x20, offset);
    if (spaceIdx === -1) {
      break;
    }
    const len = Number.parseInt(data.toString("ascii", offset, spaceIdx), 10);
    if (!Number.isFinite(len) || len <= 0 || offset + len > data.length) {
      throw new Error("Invalid PAX extended header");
    }
    const record = data.toString("utf8", spaceIdx + 1, offset + len);
    const body = record.endsWith("\n") ? record.slice(0, -1) : record;
    const eqIdx = body.indexOf("=");
    if (eqIdx > 0 && body.slice(0, eqIdx) === "path") {
      paxPath = body.slice(eqIdx + 1);
    }
    offset += len;
  }
  return paxPath;
}

function assertSafeEntryPath(entryPath) {
  if (
    entryPath.length === 0 ||
    entryPath.includes("\0") ||
    entryPath.startsWith("/") ||
    entryPath.startsWith("\\")
  ) {
    throw new Error(`Unsafe archive entry path: ${entryPath}`);
  }
  const segments = entryPath.split(/[\\/]/);
  if (segments.includes("..")) {
    throw new Error(`Unsafe archive entry path: ${entryPath}`);
  }
}

function extractBinaryFromTarGz(
  tarGzBytes,
  targetName = "sm",
  maxUncompressedBytes = MAX_UNCOMPRESSED_BYTES
) {
  const tarBytes = zlib.gunzipSync(tarGzBytes, {
    maxOutputLength: maxUncompressedBytes,
  });
  if (tarBytes.length > maxUncompressedBytes) {
    const err = new Error(`Uncompressed archive exceeded limit of ${maxUncompressedBytes} bytes`);
    err.code = "ERR_SIZE_LIMIT";
    throw err;
  }

  let offset = 0;
  let pendingPaxPath = null;
  let extracted = null;

  while (offset + 512 <= tarBytes.length) {
    const header = tarBytes.subarray(offset, offset + 512);
    if (isZeroBlock(header)) {
      break;
    }

    verifyTarChecksum(header);

    const name = readCString(header, 0, 100);
    const size = parseOctal(header, 124, 136);
    const typeflag = String.fromCharCode(header[156]);
    const magic = readCString(header, 257, 263);
    const prefix = magic.startsWith("ustar") ? readCString(header, 345, 500) : "";
    const rawPath = prefix.length > 0 ? `${prefix}/${name}` : name;

    const dataStart = offset + 512;
    const dataEnd = dataStart + size;
    if (dataEnd > tarBytes.length) {
      throw new Error("Truncated tar archive entry");
    }
    const nextOffset = dataStart + Math.ceil(size / 512) * 512;

    // PAX extended headers may accompany files produced by bsdtar or GNU tar.
    if (typeflag === "x" || typeflag === "g") {
      if (typeflag === "x") {
        const paxPath = parsePaxPath(tarBytes.subarray(dataStart, dataEnd));
        if (paxPath !== null) {
          pendingPaxPath = paxPath;
        }
      }
      offset = nextOffset;
      continue;
    }

    const entryPath = pendingPaxPath !== null ? pendingPaxPath : rawPath;
    pendingPaxPath = null;

    assertSafeEntryPath(entryPath);

    if (typeflag === "5") {
      if (entryPath === targetName || entryPath === `./${targetName}`) {
        throw new Error(`Archive entry ${entryPath} is a directory, expected regular file`);
      }
      offset = nextOffset;
      continue;
    }

    if (typeflag !== "0" && typeflag !== "\0") {
      throw new Error(`Unsupported tar entry type '${typeflag}' for ${entryPath}`);
    }

    if (entryPath === targetName || entryPath === `./${targetName}`) {
      if (extracted !== null) {
        throw new Error(`Archive contains duplicate entries for ${targetName}`);
      }
      extracted = Buffer.from(tarBytes.subarray(dataStart, dataEnd));
    }

    offset = nextOffset;
  }

  if (extracted === null) {
    throw new Error(`Archive does not contain regular file ${targetName}`);
  }
  if (extracted.length === 0) {
    throw new Error(`Archive entry ${targetName} is empty`);
  }
  return extracted;
}

function readVendorVersion(versionPath) {
  try {
    return fs.readFileSync(versionPath, "utf8").trim();
  } catch (err) {
    if (err && (err.code === "ENOENT" || err.code === "ENOTDIR")) {
      return null;
    }
    throw err;
  }
}

function unlinkQuietly(filePath) {
  try {
    fs.unlinkSync(filePath);
  } catch (err) {
    if (!err || err.code !== "ENOENT") {
      throw err;
    }
  }
}

async function runPostinstall(options = {}) {
  const pkg = require("../package.json");
  const {
    env = process.env,
    platform = process.platform,
    arch = process.arch,
    version = pkg.version,
    vendorDir = path.resolve(__dirname, "..", "vendor"),
    resolvePkg = require.resolve,
    fetchImpl = globalThis.fetch,
    stderr = process.stderr,
    maxDownloadBytes = MAX_DOWNLOAD_BYTES,
    maxUncompressedBytes = MAX_UNCOMPRESSED_BYTES,
  } = options;

  if (env.SM_SKIP_DOWNLOAD === "1") {
    return 0;
  }

  const target = resolveTarget(platform, arch);
  if (!target) {
    stderr.write(
      `simple-memory: no prebuilt binary for ${platform}-${arch}; ${BUILD_HINT} or set SM_BINARY_PATH.\n`
    );
    return 0;
  }

  // Pass empty env so SM_BINARY_PATH never masks whether the optionalDependency is installed.
  const optResolved = resolveBinary({
    env: {},
    platform,
    arch,
    resolvePkg,
    vendorDir,
  });
  if (optResolved.ok && optResolved.source === "optionalDependency") {
    return 0;
  }

  const vendorBin = path.join(vendorDir, target.binName);
  const vendorVersionPath = path.join(vendorDir, ".version");
  if (isRegularFile(vendorBin) && readVendorVersion(vendorVersionPath) === version) {
    return 0;
  }

  const rawBaseUrl = env.SM_DOWNLOAD_BASE_URL || `${DEFAULT_RELEASE_BASE}/v${version}`;
  const baseUrl = rawBaseUrl.replace(/\/+$/, "");
  const timeoutMs = parseTimeoutMs(env.SM_DOWNLOAD_TIMEOUT_MS);

  const sumsUrl = `${baseUrl}/SHA256SUMS`;
  let sumsText;
  try {
    const sumsRes = await fetchImpl(sumsUrl, {
      signal: AbortSignal.timeout(timeoutMs),
    });
    if (!sumsRes.ok) {
      stderr.write(
        `simple-memory: skipped binary download (${sumsUrl} returned HTTP ${sumsRes.status}); ${BUILD_HINT} or set SM_BINARY_PATH.\n`
      );
      return 0;
    }
    const sumsBuf = await readStreamWithLimit(sumsRes.body, MAX_SUMS_BYTES);
    sumsText = sumsBuf.toString("utf8");
  } catch (err) {
    if (err && err.code === "ERR_SIZE_LIMIT") {
      stderr.write(`simple-memory: SHA256SUMS exceeded size limit: ${err.message}\n`);
      return 1;
    }
    stderr.write(
      `simple-memory: skipped binary download (${err && err.message ? err.message : String(err)}); ${BUILD_HINT} or set SM_BINARY_PATH.\n`
    );
    return 0;
  }

  let expectedHash;
  try {
    expectedHash = parseSha256Sums(sumsText, target.assetName);
  } catch (err) {
    stderr.write(`simple-memory: ${err.message}\n`);
    return 1;
  }
  if (!expectedHash) {
    stderr.write(
      `simple-memory: SHA256SUMS fetched from ${sumsUrl} does not contain an entry for ${target.assetName}\n`
    );
    return 1;
  }

  const assetUrl = `${baseUrl}/${target.assetName}`;
  let tarGzBytes;
  try {
    const assetRes = await fetchImpl(assetUrl, {
      signal: AbortSignal.timeout(timeoutMs),
    });
    if (!assetRes.ok) {
      stderr.write(
        `simple-memory: skipped binary download (${assetUrl} returned HTTP ${assetRes.status}); ${BUILD_HINT} or set SM_BINARY_PATH.\n`
      );
      return 0;
    }
    const contentLengthHeader = assetRes.headers && assetRes.headers.get("content-length");
    if (contentLengthHeader !== null && contentLengthHeader !== undefined) {
      const declared = Number(contentLengthHeader);
      if (Number.isFinite(declared) && declared > maxDownloadBytes) {
        if (assetRes.body && typeof assetRes.body.cancel === "function") {
          await assetRes.body.cancel().catch(() => {});
        }
        stderr.write(
          `simple-memory: ${target.assetName} Content-Length ${declared} exceeds limit of ${maxDownloadBytes} bytes\n`
        );
        return 1;
      }
    }
    tarGzBytes = await readStreamWithLimit(assetRes.body, maxDownloadBytes);
  } catch (err) {
    if (err && err.code === "ERR_SIZE_LIMIT") {
      stderr.write(`simple-memory: ${target.assetName} ${err.message}\n`);
      return 1;
    }
    stderr.write(
      `simple-memory: skipped binary download (${err && err.message ? err.message : String(err)}); ${BUILD_HINT} or set SM_BINARY_PATH.\n`
    );
    return 0;
  }

  const actualHash = crypto.createHash("sha256").update(tarGzBytes).digest("hex");
  if (actualHash !== expectedHash) {
    stderr.write(
      `simple-memory: SHA-256 mismatch for ${target.assetName}: expected ${expectedHash}, got ${actualHash}\n`
    );
    return 1;
  }

  let binaryBytes;
  try {
    binaryBytes = extractBinaryFromTarGz(tarGzBytes, target.binName, maxUncompressedBytes);
  } catch (err) {
    stderr.write(
      `simple-memory: failed to extract ${target.binName} from ${target.assetName}: ${err && err.message ? err.message : String(err)}\n`
    );
    return 1;
  }

  fs.mkdirSync(vendorDir, { recursive: true });
  const nonce = `${process.pid}.${crypto.randomBytes(6).toString("hex")}`;
  const tmpBin = path.join(vendorDir, `.sm.tmp.${nonce}`);
  const tmpVer = path.join(vendorDir, `.version.tmp.${nonce}`);
  try {
    fs.writeFileSync(tmpBin, binaryBytes, { mode: 0o755 });
    fs.chmodSync(tmpBin, 0o755);
    fs.renameSync(tmpBin, vendorBin);
    fs.writeFileSync(tmpVer, `${version}\n`, { mode: 0o644 });
    fs.renameSync(tmpVer, vendorVersionPath);
  } finally {
    unlinkQuietly(tmpBin);
    unlinkQuietly(tmpVer);
  }

  return 0;
}

if (require.main === module) {
  runPostinstall().then(
    (code) => {
      process.exit(code);
    },
    (err) => {
      process.stderr.write(
        `simple-memory: postinstall error: ${err && err.message ? err.message : String(err)}\n`
      );
      process.exit(1);
    }
  );
}

module.exports = {
  DEFAULT_TIMEOUT_MS,
  MAX_DOWNLOAD_BYTES,
  MAX_UNCOMPRESSED_BYTES,
  parseSha256Sums,
  readStreamWithLimit,
  extractBinaryFromTarGz,
  runPostinstall,
};
