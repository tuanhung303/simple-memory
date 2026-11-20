"use strict";

const fs = require("node:fs");
const path = require("node:path");

// Target = { key: string, pkg: string, binName: "sm", assetName: string, rustTarget: string }
const TARGETS = Object.freeze({
  "darwin-arm64": Object.freeze({
    key: "darwin-arm64",
    pkg: "simple-memory-darwin-arm64",
    binName: "sm",
    assetName: "sm-darwin-arm64.tar.gz",
    rustTarget: "aarch64-apple-darwin",
  }),
  "darwin-x64": Object.freeze({
    key: "darwin-x64",
    pkg: "simple-memory-darwin-x64",
    binName: "sm",
    assetName: "sm-darwin-x64.tar.gz",
    rustTarget: "x86_64-apple-darwin",
  }),
  "linux-x64": Object.freeze({
    key: "linux-x64",
    pkg: "simple-memory-linux-x64",
    binName: "sm",
    assetName: "sm-linux-x64.tar.gz",
    rustTarget: "x86_64-unknown-linux-gnu",
  }),
  "linux-arm64": Object.freeze({
    key: "linux-arm64",
    pkg: "simple-memory-linux-arm64",
    binName: "sm",
    assetName: "sm-linux-arm64.tar.gz",
    rustTarget: "aarch64-unknown-linux-gnu",
  }),
});

function isRegularFile(filePath) {
  if (typeof filePath !== "string" || filePath.length === 0) {
    return false;
  }
  try {
    return fs.statSync(filePath).isFile();
  } catch {
    return false;
  }
}

function resolveTarget(platform = process.platform, arch = process.arch) {
  const key = `${platform}-${arch}`;
  return Object.prototype.hasOwnProperty.call(TARGETS, key) ? TARGETS[key] : null;
}

// ResolveResult =
//   | { ok: true, path: string, source: "env" | "optionalDependency" | "vendor" }
//   | { ok: false, reason: string, target: Target | null }
function resolveBinary(options = {}) {
  const {
    env = process.env,
    platform = process.platform,
    arch = process.arch,
    resolvePkg = require.resolve,
    vendorDir = path.resolve(__dirname, "..", "vendor"),
    isFile = isRegularFile,
  } = options;

  const target = resolveTarget(platform, arch);

  const envPath = env && typeof env.SM_BINARY_PATH === "string" ? env.SM_BINARY_PATH : "";
  if (envPath.length > 0) {
    if (isFile(envPath)) {
      return { ok: true, path: path.resolve(envPath), source: "env" };
    }
    return {
      ok: false,
      reason: `SM_BINARY_PATH is not an existing file: ${envPath}`,
      target,
    };
  }

  if (!target) {
    return {
      ok: false,
      reason: `Unsupported platform: ${platform}-${arch}`,
      target: null,
    };
  }

  try {
    const resolved = resolvePkg(`${target.pkg}/bin/${target.binName}`);
    if (isFile(resolved)) {
      return { ok: true, path: resolved, source: "optionalDependency" };
    }
  } catch (err) {
    if (
      !err ||
      (err.code !== "MODULE_NOT_FOUND" && err.code !== "ERR_PACKAGE_PATH_NOT_EXPORTED")
    ) {
      throw err;
    }
  }

  const vendorPath = path.join(vendorDir, target.binName);
  if (isFile(vendorPath)) {
    return { ok: true, path: vendorPath, source: "vendor" };
  }

  return {
    ok: false,
    reason: `No prebuilt sm binary found for ${target.key} (tried ${target.pkg} and ${vendorPath})`,
    target,
  };
}

module.exports = {
  TARGETS,
  isRegularFile,
  resolveTarget,
  resolveBinary,
};
