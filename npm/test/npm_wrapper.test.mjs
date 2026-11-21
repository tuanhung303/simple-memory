import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import crypto from "node:crypto";
import fs from "node:fs";
import http from "node:http";
import { createRequire } from "node:module";
import os from "node:os";
import path from "node:path";
import { Readable } from "node:stream";
import { describe, it } from "node:test";
import { fileURLToPath } from "node:url";
import zlib from "node:zlib";

const require = createRequire(import.meta.url);
const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const REPO_ROOT = path.resolve(__dirname, "..", "..");
const BIN_SM = path.join(REPO_ROOT, "npm", "simple-memory", "bin", "sm.js");
const CHECK_UPDATE_SCRIPT = path.join(REPO_ROOT, "npm", "simple-memory", "scripts", "check-update.js");
const STAGE_SCRIPT = path.join(REPO_ROOT, "scripts", "stage_npm.sh");
const PKG_VERSION = require(path.join(REPO_ROOT, "npm", "simple-memory", "package.json")).version;

const {
  TARGETS,
  isRegularFile,
  resolveTarget,
  resolveBinary,
} = require("../simple-memory/lib/platform.js");

const {
  DEFAULT_TTL_MS,
  parseSemver,
  isNewerVersion,
  shouldCheckUpdate,
  cachePathForEnv,
  readUpdateCache,
  isCacheStale,
  formatUpdateNotice,
  maybeNotifyAndRefresh,
} = require("../simple-memory/lib/update.js");

const {
  parseSha256Sums,
  readStreamWithLimit,
  extractBinaryFromTarGz,
  runPostinstall,
} = require("../simple-memory/scripts/postinstall.js");

const { fetchAndWriteUpdateCache } = require("../simple-memory/scripts/check-update.js");

function withTempDir(fn) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "sm-npm-test-"));
  try {
    return fn(dir);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

async function withTempDirAsync(fn) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "sm-npm-test-"));
  try {
    return await fn(dir);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

function buildUstarHeader(name, size, typeflag = "0", mode = 0o755) {
  const header = Buffer.alloc(512, 0);
  header.write(name, 0, Math.min(Buffer.byteLength(name), 100), "utf8");
  header.write(mode.toString(8).padStart(7, "0") + "\0", 100, 8, "ascii");
  header.write("0000000\0", 108, 8, "ascii");
  header.write("0000000\0", 116, 8, "ascii");
  header.write(size.toString(8).padStart(11, "0") + "\0", 124, 12, "ascii");
  header.write("00000000000\0", 136, 12, "ascii");
  header.fill(0x20, 148, 156);
  header.write(typeflag, 156, 1, "ascii");
  header.write("ustar\0", 257, 6, "ascii");
  header.write("00", 263, 2, "ascii");
  let sum = 0;
  for (let i = 0; i < 512; i += 1) {
    sum += header[i];
  }
  header.write(sum.toString(8).padStart(6, "0") + "\0 ", 148, 8, "ascii");
  return header;
}

function buildTarGz(entries) {
  const blocks = [];
  for (const entry of entries) {
    const data = entry.data ? Buffer.from(entry.data) : Buffer.alloc(0);
    blocks.push(buildUstarHeader(entry.name, data.length, entry.typeflag ?? "0", entry.mode ?? 0o755));
    if (data.length > 0) {
      blocks.push(data);
      const rem = data.length % 512;
      if (rem !== 0) {
        blocks.push(Buffer.alloc(512 - rem, 0));
      }
    }
  }
  blocks.push(Buffer.alloc(1024, 0));
  return zlib.gzipSync(Buffer.concat(blocks));
}

function makePaxRecord(key, value) {
  const tail = ` ${key}=${value}\n`;
  let len = Buffer.byteLength(tail) + 1;
  while (Buffer.byteLength(`${len}${tail}`) !== len) {
    len = Buffer.byteLength(`${len}${tail}`);
  }
  return Buffer.from(`${len}${tail}`, "utf8");
}

function sha256Hex(buf) {
  return crypto.createHash("sha256").update(buf).digest("hex");
}

function makeElfHeader(machine, suffix = Buffer.alloc(0)) {
  const hdr = Buffer.alloc(64, 0);
  hdr.write("\x7fELF", 0, "binary");
  hdr[4] = 2; // 64-bit
  hdr[5] = 1; // little-endian
  hdr[6] = 1; // ELF version
  hdr.writeUInt16LE(2, 16); // ET_EXEC
  hdr.writeUInt16LE(machine, 18);
  hdr.writeUInt32LE(1, 20);
  return Buffer.concat([hdr, suffix]);
}

function makeMachOHeader(cpuType, cpuSubtype, dylibPath = null, suffix = Buffer.alloc(0)) {
  let lc = Buffer.alloc(0);
  let ncmds = 0;
  if (dylibPath) {
    const nameBuf = Buffer.concat([Buffer.from(dylibPath, "utf8"), Buffer.from([0])]);
    const rawLen = 24 + nameBuf.length;
    const cmdsize = (rawLen + 7) & ~7;
    lc = Buffer.alloc(cmdsize, 0);
    lc.writeUInt32LE(0x0c, 0); // LC_LOAD_DYLIB
    lc.writeUInt32LE(cmdsize, 4);
    lc.writeUInt32LE(24, 8);
    lc.writeUInt32LE(0, 12);
    lc.writeUInt32LE(0x00010000, 16);
    lc.writeUInt32LE(0x00010000, 20);
    nameBuf.copy(lc, 24);
    ncmds = 1;
  }
  const hdr = Buffer.alloc(32, 0);
  hdr.writeUInt32LE(0xfeedfacf, 0); // MH_MAGIC_64
  hdr.writeUInt32LE(cpuType, 4);
  hdr.writeUInt32LE(cpuSubtype, 8);
  hdr.writeUInt32LE(2, 12); // MH_EXECUTE
  hdr.writeUInt32LE(ncmds, 16);
  hdr.writeUInt32LE(lc.length, 20);
  return Buffer.concat([hdr, lc, suffix]);
}

function makeFixtureRepo(dir, cargoVersion = PKG_VERSION) {
  fs.mkdirSync(path.join(dir, "rust"), { recursive: true });
  fs.writeFileSync(
    path.join(dir, "rust", "Cargo.toml"),
    `[package]\nname = "simple-memory"\nversion = "${cargoVersion}"\nedition = "2024"\n`
  );
  fs.writeFileSync(path.join(dir, "LICENSE"), "MIT License\n");
  fs.writeFileSync(path.join(dir, "README.md"), "# simple-memory\n\nExample readme.\n");
  fs.cpSync(path.join(REPO_ROOT, "npm", "simple-memory"), path.join(dir, "npm", "simple-memory"), {
    recursive: true,
  });
  fs.cpSync(path.join(REPO_ROOT, "npm", "platforms"), path.join(dir, "npm", "platforms"), {
    recursive: true,
  });
}

function spawnAsync(cmd, args, options = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(cmd, args, {
      ...options,
      stdio: ["ignore", "pipe", "pipe"],
    });
    const outChunks = [];
    const errChunks = [];
    child.stdout.on("data", (c) => outChunks.push(c));
    child.stderr.on("data", (c) => errChunks.push(c));
    child.on("error", reject);
    child.on("close", (status, signal) => {
      resolve({
        status,
        signal,
        stdout: Buffer.concat(outChunks).toString("utf8"),
        stderr: Buffer.concat(errChunks).toString("utf8"),
      });
    });
  });
}

async function runWithStderrPty(cmdArgs, env) {
  const pyCode = `
import errno, json, os, pty, select, subprocess, sys, termios
cmd = json.loads(sys.argv[1])
master_fd, slave_fd = pty.openpty()
attrs = termios.tcgetattr(slave_fd)
attrs[1] &= ~termios.ONLCR
termios.tcsetattr(slave_fd, termios.TCSANOW, attrs)
proc = subprocess.Popen(cmd, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=slave_fd)
os.close(slave_fd)
out_fd = proc.stdout.fileno()
out_chunks = []
err_chunks = []
open_fds = [out_fd, master_fd]
while open_fds:
    r, _, _ = select.select(open_fds, [], [], 0.2)
    if not r and proc.poll() is not None:
        break
    for fd in r:
        try:
            data = os.read(fd, 4096)
        except OSError as e:
            if e.errno == errno.EIO:
                data = b""
            else:
                raise
        if not data:
            open_fds.remove(fd)
        elif fd == out_fd:
            out_chunks.append(data)
        else:
            err_chunks.append(data)
rc = proc.wait()
proc.stdout.close()
os.close(master_fd)
sys.stdout.write(json.dumps({
    "status": rc,
    "stdout": b"".join(out_chunks).decode("utf-8", "replace"),
    "stderr": b"".join(err_chunks).decode("utf-8", "replace"),
}))
`;
  const run = await spawnAsync("python3", ["-c", pyCode, JSON.stringify(cmdArgs)], { env });
  assert.equal(run.status, 0, `pty runner failed:\nstderr: ${run.stderr}\nstdout: ${run.stdout}`);
  return JSON.parse(run.stdout);
}

async function pollUntil(predicate, timeoutMs = 5000, intervalMs = 20) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (predicate()) {
      return;
    }
    await new Promise((r) => setTimeout(r, intervalMs));
  }
  throw new Error(`Timed out after ${timeoutMs}ms waiting for condition`);
}

function createBufferWriter() {
  let out = "";
  return {
    write(chunk) {
      out += String(chunk);
    },
    text() {
      return out;
    },
  };
}

async function startServer(handler) {
  const server = http.createServer(handler);
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const addr = server.address();
  return {
    server,
    baseUrl: `http://127.0.0.1:${addr.port}`,
    close: () =>
      new Promise((resolve) => {
        server.close(resolve);
        server.closeAllConnections?.();
      }),
  };
}

describe("lib/platform.js", () => {
  it("resolves all four supported platform/arch targets and rejects unsupported ones", () => {
    assert.deepEqual(resolveTarget("darwin", "arm64"), {
      key: "darwin-arm64",
      pkg: "simple-memory-darwin-arm64",
      binName: "sm",
      assetName: "sm-darwin-arm64.tar.gz",
      rustTarget: "aarch64-apple-darwin",
    });
    assert.deepEqual(resolveTarget("darwin", "x64"), {
      key: "darwin-x64",
      pkg: "simple-memory-darwin-x64",
      binName: "sm",
      assetName: "sm-darwin-x64.tar.gz",
      rustTarget: "x86_64-apple-darwin",
    });
    assert.deepEqual(resolveTarget("linux", "x64"), {
      key: "linux-x64",
      pkg: "simple-memory-linux-x64",
      binName: "sm",
      assetName: "sm-linux-x64.tar.gz",
      rustTarget: "x86_64-unknown-linux-gnu",
    });
    assert.deepEqual(resolveTarget("linux", "arm64"), {
      key: "linux-arm64",
      pkg: "simple-memory-linux-arm64",
      binName: "sm",
      assetName: "sm-linux-arm64.tar.gz",
      rustTarget: "aarch64-unknown-linux-gnu",
    });
    assert.equal(Object.keys(TARGETS).length, 4);
    assert.equal(resolveTarget("win32", "x64"), null);
    assert.equal(resolveTarget("freebsd", "x64"), null);
    assert.equal(resolveTarget("toString", "valueOf"), null);
  });

  it("isRegularFile accepts regular files and symlinks to files, rejects directories and missing paths", () => {
    withTempDir((dir) => {
      const file = path.join(dir, "sm");
      const link = path.join(dir, "sm-link");
      const subdir = path.join(dir, "subdir");
      fs.writeFileSync(file, "bin");
      fs.symlinkSync(file, link);
      fs.mkdirSync(subdir);

      assert.equal(isRegularFile(file), true);
      assert.equal(isRegularFile(link), true);
      assert.equal(isRegularFile(subdir), false);
      assert.equal(isRegularFile(path.join(dir, "missing")), false);
      assert.equal(isRegularFile(""), false);
    });
  });

  it("resolveBinary enforces SM_BINARY_PATH precedence and fails loud on missing or directory path", () => {
    withTempDir((dir) => {
      const envBin = path.join(dir, "env-sm");
      const optBin = path.join(dir, "opt-sm");
      const vendorDir = path.join(dir, "vendor");
      fs.mkdirSync(vendorDir);
      const vendorBin = path.join(vendorDir, "sm");
      fs.writeFileSync(envBin, "env");
      fs.writeFileSync(optBin, "opt");
      fs.writeFileSync(vendorBin, "vendor");

      // Valid SM_BINARY_PATH wins even on unsupported OS.
      assert.deepEqual(
        resolveBinary({
          env: { SM_BINARY_PATH: envBin },
          platform: "win32",
          arch: "x64",
          resolvePkg: () => optBin,
          vendorDir,
        }),
        { ok: true, path: envBin, source: "env" }
      );

      // Missing SM_BINARY_PATH fails loud instead of falling back to optionalDep or vendor.
      const missingRes = resolveBinary({
        env: { SM_BINARY_PATH: path.join(dir, "does-not-exist") },
        platform: "darwin",
        arch: "arm64",
        resolvePkg: () => optBin,
        vendorDir,
      });
      assert.equal(missingRes.ok, false);
      assert.match(missingRes.reason, /SM_BINARY_PATH is not an existing file/);

      // Directory SM_BINARY_PATH fails loud instead of falling back.
      const dirRes = resolveBinary({
        env: { SM_BINARY_PATH: dir },
        platform: "darwin",
        arch: "arm64",
        resolvePkg: () => optBin,
        vendorDir,
      });
      assert.equal(dirRes.ok, false);
      assert.match(dirRes.reason, /SM_BINARY_PATH is not an existing file/);

      // Empty SM_BINARY_PATH falls through to optionalDependency.
      assert.deepEqual(
        resolveBinary({
          env: { SM_BINARY_PATH: "" },
          platform: "darwin",
          arch: "arm64",
          resolvePkg: (spec) => {
            assert.equal(spec, "simple-memory-darwin-arm64/bin/sm");
            return optBin;
          },
          vendorDir,
        }),
        { ok: true, path: optBin, source: "optionalDependency" }
      );
    });
  });

  it("resolveBinary falls back to vendor on MODULE_NOT_FOUND/ERR_PACKAGE_PATH_NOT_EXPORTED and rethrows unexpected errors", () => {
    withTempDir((dir) => {
      const vendorDir = path.join(dir, "vendor");
      fs.mkdirSync(vendorDir);
      const vendorBin = path.join(vendorDir, "sm");
      fs.writeFileSync(vendorBin, "vendor");

      for (const code of ["MODULE_NOT_FOUND", "ERR_PACKAGE_PATH_NOT_EXPORTED"]) {
        const res = resolveBinary({
          env: {},
          platform: "linux",
          arch: "x64",
          resolvePkg: () => {
            const err = new Error(code);
            err.code = code;
            throw err;
          },
          vendorDir,
        });
        assert.deepEqual(res, { ok: true, path: vendorBin, source: "vendor" });
      }

      assert.throws(
        () =>
          resolveBinary({
            env: {},
            platform: "linux",
            arch: "x64",
            resolvePkg: () => {
              const err = new Error("permission denied");
              err.code = "EACCES";
              throw err;
            },
            vendorDir,
          }),
        { code: "EACCES" }
      );

      // Non-file resolvePkg target falls through to vendor binary (M04).
      const optDir = path.join(dir, "opt-dir");
      fs.mkdirSync(optDir);
      assert.deepEqual(
        resolveBinary({
          env: {},
          platform: "linux",
          arch: "x64",
          resolvePkg: () => optDir,
          vendorDir,
        }),
        { ok: true, path: vendorBin, source: "vendor" }
      );

      // Bare relative SM_BINARY_PATH resolves to an absolute path in cwd.
      const prevCwd = process.cwd();
      try {
        process.chdir(dir);
        fs.writeFileSync(path.join(dir, "sm"), "cwd-bin");
        assert.deepEqual(
          resolveBinary({
            env: { SM_BINARY_PATH: "sm" },
            platform: "linux",
            arch: "x64",
            vendorDir,
          }),
          { ok: true, path: path.join(fs.realpathSync(dir), "sm"), source: "env" }
        );
      } finally {
        process.chdir(prevCwd);
      }

      // Directory at vendor/sm is rejected.
      fs.rmSync(vendorBin);
      fs.mkdirSync(vendorBin);
      const noBin = resolveBinary({
        env: {},
        platform: "linux",
        arch: "x64",
        resolvePkg: () => {
          const err = new Error("missing");
          err.code = "MODULE_NOT_FOUND";
          throw err;
        },
        vendorDir,
      });
      assert.equal(noBin.ok, false);
      assert.equal(noBin.target.key, "linux-x64");

      const unsupported = resolveBinary({
        env: {},
        platform: "win32",
        arch: "x64",
        resolvePkg: () => {
          throw new Error("should not resolve unsupported platform");
        },
        vendorDir,
      });
      assert.equal(unsupported.ok, false);
      assert.equal(unsupported.target, null);
    });
  });
});

describe("bin/sm.js launcher", () => {
  it("passes argv, stdin, and raw JSON-RPC stdout through with zero wrapper output", () => {
    withTempDir((dir) => {
      const stub = path.join(dir, "sm");
      fs.writeFileSync(
        stub,
        `#!${process.execPath}
const chunks = [];
process.stdin.on("data", (c) => chunks.push(c));
process.stdin.on("end", () => {
  const payload = {
    jsonrpc: "2.0",
    id: 1,
    result: { argv: process.argv.slice(2), stdin: Buffer.concat(chunks).toString("utf8") },
  };
  process.stdout.write(JSON.stringify(payload) + "\\n");
});
`,
        { mode: 0o755 }
      );
      fs.chmodSync(stub, 0o755);

      const input = '{"jsonrpc":"2.0","id":1,"method":"initialize"}\n';
      const run = spawnSync(process.execPath, [BIN_SM, "mcp", "--flag", "a b", ""], {
        env: { ...process.env, SM_BINARY_PATH: stub },
        input,
        encoding: "utf8",
      });

      assert.equal(run.status, 0);
      assert.equal(run.stderr, "");
      assert.deepEqual(JSON.parse(run.stdout), {
        jsonrpc: "2.0",
        id: 1,
        result: { argv: ["mcp", "--flag", "a b", ""], stdin: input },
      });
    });
  });

  it("propagates non-zero child exit codes", () => {
    withTempDir((dir) => {
      const stub = path.join(dir, "sm");
      fs.writeFileSync(stub, "#!/bin/sh\nexit 42\n", { mode: 0o755 });
      fs.chmodSync(stub, 0o755);

      const run = spawnSync(process.execPath, [BIN_SM, "--version"], {
        env: { ...process.env, SM_BINARY_PATH: stub },
        encoding: "utf8",
      });
      assert.equal(run.status, 42);
      assert.equal(run.stdout, "");
      assert.equal(run.stderr, "");
    });
  });

  it("resolves bare relative SM_BINARY_PATH against cwd rather than PATH", () => {
    withTempDir((dir) => {
      const cwdDir = path.join(dir, "cwd");
      const pathDir = path.join(dir, "path");
      fs.mkdirSync(cwdDir);
      fs.mkdirSync(pathDir);
      const cwdBin = path.join(cwdDir, "sm");
      const pathBin = path.join(pathDir, "sm");
      fs.writeFileSync(cwdBin, "#!/bin/sh\necho FROM_CWD\n", { mode: 0o755 });
      fs.writeFileSync(pathBin, "#!/bin/sh\necho FROM_PATH\n", { mode: 0o755 });
      fs.chmodSync(cwdBin, 0o755);
      fs.chmodSync(pathBin, 0o755);

      const run = spawnSync(process.execPath, [BIN_SM], {
        cwd: cwdDir,
        env: { ...process.env, PATH: `${pathDir}:${process.env.PATH}`, SM_BINARY_PATH: "sm" },
        encoding: "utf8",
      });
      assert.equal(run.status, 0, run.stderr);
      assert.equal(run.stdout.trim(), "FROM_CWD");
    });
  });

  it("forwards SIGTERM and SIGINT to child and exits with matching signal status", async () => {
    for (const sig of ["SIGTERM", "SIGINT"]) {
      await withTempDirAsync(async (dir) => {
        const readyFile = path.join(dir, "ready");
        const caughtFile = path.join(dir, "caught");
        const stub = path.join(dir, "sm");
        fs.writeFileSync(
          stub,
          `#!${process.execPath}
const fs = require("node:fs");
process.on(${JSON.stringify(sig)}, () => {
  fs.writeFileSync(${JSON.stringify(caughtFile)}, "caught " + ${JSON.stringify(sig)});
  process.removeAllListeners(${JSON.stringify(sig)});
  process.kill(process.pid, ${JSON.stringify(sig)});
});
fs.writeFileSync(${JSON.stringify(readyFile)}, String(process.pid));
setInterval(() => {}, 10000);
`,
          { mode: 0o755 }
        );
        fs.chmodSync(stub, 0o755);

        const child = spawn(process.execPath, [BIN_SM], {
          env: { ...process.env, SM_BINARY_PATH: stub },
          stdio: ["ignore", "pipe", "pipe"],
        });

        let grandchildPid = null;
        try {
          await pollUntil(() => fs.existsSync(readyFile) && fs.readFileSync(readyFile, "utf8").length > 0);
          grandchildPid = Number(fs.readFileSync(readyFile, "utf8").trim());
          child.kill(sig);

          const [code, signal] = await new Promise((resolve, reject) => {
            const timer = setTimeout(() => {
              child.kill("SIGKILL");
              reject(new Error(`Timed out waiting for bin/sm.js to exit after ${sig}`));
            }, 4000);
            child.once("exit", (c, s) => {
              clearTimeout(timer);
              resolve([c, s]);
            });
          });

          assert.equal(fs.readFileSync(caughtFile, "utf8"), `caught ${sig}`);
          assert.equal(signal, sig);
          assert.equal(code, null);
        } finally {
          child.stdout?.destroy();
          child.stderr?.destroy();
          try {
            child.kill("SIGKILL");
          } catch {}
          if (grandchildPid && Number.isInteger(grandchildPid)) {
            try {
              process.kill(grandchildPid, "SIGKILL");
            } catch {}
          }
        }
      });
    }
  });

  it("exits with code 141 when child terminates via SIGPIPE", () => {
    withTempDir((dir) => {
      const stub = path.join(dir, "sm");
      fs.writeFileSync(stub, "#!/bin/sh\nkill -PIPE $$\n", { mode: 0o755 });
      fs.chmodSync(stub, 0o755);

      const run = spawnSync(process.execPath, [BIN_SM], {
        env: { ...process.env, SM_BINARY_PATH: stub },
        encoding: "utf8",
      });

      assert.equal(run.status, 141);
      assert.equal(run.stdout, "");
    });
  });

  it("prints actionable stderr and exits 1 for missing, directory, and non-executable binaries", () => {
    withTempDir((dir) => {
      const missingPath = path.join(dir, "missing-sm");
      const missing = spawnSync(process.execPath, [BIN_SM], {
        env: { ...process.env, SM_BINARY_PATH: missingPath },
        encoding: "utf8",
      });
      assert.equal(missing.status, 1);
      assert.equal(missing.stdout, "");
      assert.match(missing.stderr, new RegExp(`${process.platform}-${process.arch}`));
      assert.match(missing.stderr, /\.\/scripts\/install\.sh/);
      assert.match(
        missing.stderr,
        /git clone https:\/\/github\.com\/tuanhung303\/simple-memory\.git, cd simple-memory, then run/
      );
      assert.match(missing.stderr, /SM_BINARY_PATH/);

      const dirRun = spawnSync(process.execPath, [BIN_SM], {
        env: { ...process.env, SM_BINARY_PATH: dir },
        encoding: "utf8",
      });
      assert.equal(dirRun.status, 1);
      assert.equal(dirRun.stdout, "");
      assert.match(dirRun.stderr, /SM_BINARY_PATH is not an existing file/);

      const nonExec = path.join(dir, "no-exec-sm");
      fs.writeFileSync(nonExec, "#!/bin/sh\necho hi\n", { mode: 0o644 });
      fs.chmodSync(nonExec, 0o644);
      const eacces = spawnSync(process.execPath, [BIN_SM], {
        env: { ...process.env, SM_BINARY_PATH: nonExec },
        encoding: "utf8",
      });
      assert.equal(eacces.status, 1);
      assert.equal(eacces.stdout, "");
      assert.match(eacces.stderr, /failed to execute/);
    });
  });

  it("passes SM_INSTALL_METHOD=npm to child and handles TTY vs non-TTY update notices and detached refresh", async () => {
    const reqs = [];
    const srv = await startServer((req, res) => {
      reqs.push(req.url);
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ name: "simple-memory", version: "0.7.1" }));
    });
    try {
      await withTempDirAsync(async (dir) => {
        const stub = path.join(dir, "sm");
        fs.writeFileSync(
          stub,
          `#!${process.execPath}
process.stdout.write(JSON.stringify({
  installMethod: process.env.SM_INSTALL_METHOD || "",
  argv: process.argv.slice(2),
}) + "\\n");
`,
          { mode: 0o755 }
        );
        fs.chmodSync(stub, 0o755);

        const smHome = path.join(dir, "sm-home");
        fs.mkdirSync(smHome, { recursive: true });
        const cachePath = path.join(smHome, "update-check.json");

        // Non-TTY run passes SM_INSTALL_METHOD=npm, emits zero stderr, and triggers zero HTTP calls.
        fs.writeFileSync(cachePath, JSON.stringify({ checked_at: Date.now(), latest: "0.7.0" }));
        const nonTtyEnv = { ...process.env, SM_BINARY_PATH: stub, SIMPLE_MEMORY_HOME: smHome, SM_UPDATE_CHECK_URL: srv.baseUrl };
        delete nonTtyEnv.CI;
        delete nonTtyEnv.SM_NO_UPDATE_CHECK;
        const nonTtyRun = spawnSync(process.execPath, [BIN_SM, "list"], {
          env: nonTtyEnv,
          encoding: "utf8",
        });
        assert.equal(nonTtyRun.status, 0);
        assert.equal(nonTtyRun.stderr, "");
        assert.deepEqual(JSON.parse(nonTtyRun.stdout), { installMethod: "npm", argv: ["list"] });
        assert.equal(reqs.length, 0);

        // TTY run with fresh cache newer than this package prints exact 1-line stderr notice and keeps stdout clean.
        const ttyRun = await runWithStderrPty([process.execPath, BIN_SM, "list"], nonTtyEnv);
        assert.equal(ttyRun.status, 0);
        assert.deepEqual(JSON.parse(ttyRun.stdout), { installMethod: "npm", argv: ["list"] });
        assert.equal(
          ttyRun.stderr,
          `Update available: ${PKG_VERSION} -> 0.7.0. Run: sm update (or npm install -g simple-memory@latest; disable: SM_NO_UPDATE_CHECK=1)\n`
        );
        assert.equal(reqs.length, 0);

        // TTY suppression guards: SM_NO_UPDATE_CHECK=1, CI=1, mcp, hook, hooks, update, --format json, --version.
        const suppressedCases = [
          { args: ["list"], extraEnv: { SM_NO_UPDATE_CHECK: "1" } },
          { args: ["list"], extraEnv: { CI: "1" } },
          { args: ["mcp"], extraEnv: {} },
          { args: ["hook", "prompt"], extraEnv: {} },
          { args: ["hooks", "print", "mcp"], extraEnv: {} },
          { args: ["update", "--check"], extraEnv: {} },
          { args: ["query", "abc", "--format", "json"], extraEnv: {} },
          { args: ["query", "abc", "--format=json"], extraEnv: {} },
          { args: ["--version"], extraEnv: {} },
          { args: ["-v"], extraEnv: {} },
        ];
        const suppressedRuns = await Promise.all(
          suppressedCases.map((c) =>
            runWithStderrPty([process.execPath, BIN_SM, ...c.args], {
              ...nonTtyEnv,
              ...c.extraEnv,
            }).then((r) => ({ c, r }))
          )
        );
        for (const { c, r } of suppressedRuns) {
          assert.equal(r.status, 0);
          assert.deepEqual(JSON.parse(r.stdout), { installMethod: "npm", argv: c.args });
          assert.equal(r.stderr, "", `expected empty stderr for ${JSON.stringify(c)}`);
        }

        // Signal exit on TTY must not print update notice even with a newer cache.
        const sigStub = path.join(dir, "sm-sig");
        fs.writeFileSync(sigStub, "#!/bin/sh\nkill -PIPE $$\n", { mode: 0o755 });
        fs.chmodSync(sigStub, 0o755);
        const sigRun = await runWithStderrPty([process.execPath, BIN_SM, "list"], {
          ...nonTtyEnv,
          SM_BINARY_PATH: sigStub,
        });
        assert.equal(sigRun.status, 141);
        assert.equal(sigRun.stdout, "");
        assert.equal(sigRun.stderr, "");

        // Missing cache on TTY spawns detached check-update.js; next TTY run prints cached notice with no new HTTP call.
        fs.rmSync(cachePath);
        const firstTty = await runWithStderrPty([process.execPath, BIN_SM, "list"], nonTtyEnv);
        assert.equal(firstTty.status, 0);
        assert.equal(firstTty.stderr, "");
        await pollUntil(() => fs.existsSync(cachePath));
        const written = JSON.parse(fs.readFileSync(cachePath, "utf8"));
        assert.equal(written.latest, "0.7.1");
        assert.equal(typeof written.checked_at, "number");
        assert.equal(reqs.length, 1);

        const secondTty = await runWithStderrPty([process.execPath, BIN_SM, "list"], nonTtyEnv);
        assert.equal(secondTty.status, 0);
        assert.equal(
          secondTty.stderr,
          `Update available: ${PKG_VERSION} -> 0.7.1. Run: sm update (or npm install -g simple-memory@latest; disable: SM_NO_UPDATE_CHECK=1)\n`
        );
        assert.equal(reqs.length, 1);
      });
    } finally {
      await srv.close();
    }
  });
});

describe("lib/update.js", () => {
  it("parseSemver and isNewerVersion compare major.minor.patch numerically and reject prereleases or malformed strings", () => {
    assert.deepEqual(parseSemver("0.6.0"), { major: 0, minor: 6, patch: 0 });
    assert.deepEqual(parseSemver("12.34.56"), { major: 12, minor: 34, patch: 56 });

    for (const bad of [
      "0.7.0-rc.1",
      "0.7.0-alpha",
      "0.7.0+build.1",
      "v0.7.0",
      "0.7",
      "0.7.0.1",
      "01.2.3",
      " 0.7.0",
      "",
      null,
      undefined,
      123,
      "99999999999999999999.0.0",
    ]) {
      assert.equal(parseSemver(bad), null, `expected null for ${String(bad)}`);
      assert.equal(isNewerVersion(bad, "0.6.0"), false);
      assert.equal(isNewerVersion("0.7.0", bad), false);
    }

    assert.equal(isNewerVersion("0.6.1", "0.6.0"), true);
    assert.equal(isNewerVersion("0.7.0", "0.6.9"), true);
    assert.equal(isNewerVersion("0.10.0", "0.9.9"), true);
    assert.equal(isNewerVersion("1.0.0", "0.99.99"), true);
    assert.equal(isNewerVersion("2.0.0", "1.9.9"), true);
    assert.equal(isNewerVersion("0.6.0", "0.6.0"), false);
    assert.equal(isNewerVersion("0.5.9", "0.6.0"), false);
    assert.equal(isNewerVersion("0.6.0", "0.7.0"), false);
    assert.equal(isNewerVersion("1.0.0", "2.0.0"), false);
  });

  it("shouldCheckUpdate enforces TTY, SM_NO_UPDATE_CHECK, CI, subcommand, JSON format, and version/help guards", () => {
    // Normal interactive command.
    assert.equal(shouldCheckUpdate({ argv: ["query", "auth"], env: {}, isStderrTTY: true }), true);
    assert.equal(shouldCheckUpdate({ argv: [], env: {}, isStderrTTY: true }), true);
    assert.equal(shouldCheckUpdate({ argv: ["query", "mcp"], env: {}, isStderrTTY: true }), true);
    assert.equal(shouldCheckUpdate({ argv: ["query", "foo", "--format", "text"], env: {}, isStderrTTY: true }), true);
    assert.equal(shouldCheckUpdate({ argv: ["query", "foo", "--format"], env: {}, isStderrTTY: true }), true);
    assert.equal(shouldCheckUpdate({ argv: ["put", "--", "--help"], env: {}, isStderrTTY: true }), true);
    assert.equal(shouldCheckUpdate({ argv: ["query", "--", "-v"], env: {}, isStderrTTY: true }), true);

    // Non-TTY.
    assert.equal(shouldCheckUpdate({ argv: ["list"], env: {}, isStderrTTY: false }), false);
    assert.equal(shouldCheckUpdate({ argv: ["list"], env: {}, isStderrTTY: undefined }), false);

    // SM_NO_UPDATE_CHECK.
    assert.equal(
      shouldCheckUpdate({ argv: ["list"], env: { SM_NO_UPDATE_CHECK: "1" }, isStderrTTY: true }),
      false
    );
    assert.equal(
      shouldCheckUpdate({ argv: ["list"], env: { SM_NO_UPDATE_CHECK: "0" }, isStderrTTY: true }),
      true
    );

    // CI env values.
    for (const ci of ["1", "true", "TRUE", "yes", "github"]) {
      assert.equal(
        shouldCheckUpdate({ argv: ["list"], env: { CI: ci }, isStderrTTY: true }),
        false,
        `expected CI=${ci} to suppress`
      );
    }
    for (const ci of ["", "0", "false", "FALSE", "  "]) {
      assert.equal(
        shouldCheckUpdate({ argv: ["list"], env: { CI: ci }, isStderrTTY: true }),
        true,
        `expected CI=${JSON.stringify(ci)} to allow`
      );
    }

    // Non-array argv guard.
    assert.equal(shouldCheckUpdate({ argv: null, env: {}, isStderrTTY: true }), false);

    // Skipped subcommands.
    for (const sub of [["mcp"], ["hook", "prompt"], ["hooks", "print", "agy"], ["update"], ["update", "--check"]]) {
      assert.equal(
        shouldCheckUpdate({ argv: sub, env: {}, isStderrTTY: true }),
        false,
        `expected subcommand ${sub[0]} to suppress`
      );
    }

    // Skipped flags.
    for (const args of [
      ["--version"],
      ["-v"],
      ["-h"],
      ["--help"],
      ["query", "--help"],
      ["query", "-h"],
      ["query", "foo", "--format", "json"],
      ["show", "m_1", "--format=json"],
    ]) {
      assert.equal(
        shouldCheckUpdate({ argv: args, env: {}, isStderrTTY: true }),
        false,
        `expected args ${JSON.stringify(args)} to suppress`
      );
    }
  });

  it("cachePathForEnv, readUpdateCache, isCacheStale, formatUpdateNotice, and maybeNotifyAndRefresh handle normal, boundary, and corrupt cases", () => {
    assert.equal(
      cachePathForEnv({ SIMPLE_MEMORY_HOME: "/tmp/custom-sm" }, "/tmp/ignored-home"),
      path.join("/tmp/custom-sm", "update-check.json")
    );
    assert.equal(
      cachePathForEnv({ SIMPLE_MEMORY_HOME: "~/tilde-home" }, "/tmp/base-home"),
      path.join("/tmp/base-home", "tilde-home", "update-check.json")
    );
    assert.equal(
      cachePathForEnv({ SIMPLE_MEMORY_HOME: "~" }, "/tmp/base-home"),
      path.join("/tmp/base-home", "update-check.json")
    );
    assert.equal(
      cachePathForEnv({ SIMPLE_MEMORY_HOME: "  " }, "/tmp/base-home"),
      path.join("/tmp/base-home", ".simple-memory", "update-check.json")
    );
    assert.equal(
      cachePathForEnv({}, "/tmp/base-home"),
      path.join("/tmp/base-home", ".simple-memory", "update-check.json")
    );

    const t0 = 1_700_000_000_000;
    assert.equal(DEFAULT_TTL_MS, 86_400_000);
    assert.equal(isCacheStale(null, t0), true);
    assert.equal(isCacheStale({ checked_at: t0, latest: "0.6.0" }, t0), false);
    assert.equal(isCacheStale({ checked_at: t0, latest: "0.6.0" }, t0 + DEFAULT_TTL_MS - 1), false);
    assert.equal(isCacheStale({ checked_at: t0, latest: "0.6.0" }, t0 + DEFAULT_TTL_MS), true);
    assert.equal(isCacheStale({ checked_at: t0, latest: "0.6.0" }, t0 + DEFAULT_TTL_MS + 1), true);

    assert.equal(
      formatUpdateNotice("0.6.0", "0.7.0"),
      "Update available: 0.6.0 -> 0.7.0. Run: sm update (or npm install -g simple-memory@latest; disable: SM_NO_UPDATE_CHECK=1)\n"
    );

    withTempDir((dir) => {
      const missing = path.join(dir, "missing.json");
      assert.equal(readUpdateCache(missing), null);
      assert.equal(readUpdateCache(""), null);
      assert.equal(readUpdateCache(dir), null);

      const badJson = path.join(dir, "bad.json");
      fs.writeFileSync(badJson, "{not json");
      assert.equal(readUpdateCache(badJson), null);

      const badShape = path.join(dir, "bad-shape.json");
      for (const content of [
        JSON.stringify({ checked_at: "123", latest: "0.7.0" }),
        JSON.stringify({ checked_at: -1, latest: "0.7.0" }),
        JSON.stringify({ checked_at: t0, latest: "0.7.0-rc.1" }),
        JSON.stringify({ checked_at: t0 }),
        "null",
      ]) {
        fs.writeFileSync(badShape, content);
        assert.equal(readUpdateCache(badShape), null);
      }

      const valid = path.join(dir, "valid.json");
      fs.writeFileSync(valid, JSON.stringify({ checked_at: t0, latest: "0.7.0" }));
      assert.deepEqual(readUpdateCache(valid), { checked_at: t0, latest: "0.7.0" });

      // maybeNotifyAndRefresh: suppressed does nothing.
      const smHome = path.join(dir, "sm-home");
      fs.mkdirSync(smHome);
      const cacheFile = path.join(smHome, "update-check.json");
      const refreshCalls = [];
      const stderrSuppressed = createBufferWriter();
      assert.deepEqual(
        maybeNotifyAndRefresh({
          argv: ["list"],
          env: { SIMPLE_MEMORY_HOME: smHome, SM_NO_UPDATE_CHECK: "1" },
          currentVersion: "0.6.0",
          isStderrTTY: true,
          stderr: stderrSuppressed,
          spawnRefresh: (p) => refreshCalls.push(p),
          nowMs: t0,
        }),
        { notified: false, refreshed: false, latest: null }
      );
      assert.equal(stderrSuppressed.text(), "");
      assert.deepEqual(refreshCalls, []);

      // Missing cache refreshes without notifying.
      const stderrMissing = createBufferWriter();
      assert.deepEqual(
        maybeNotifyAndRefresh({
          argv: ["list"],
          env: { SIMPLE_MEMORY_HOME: smHome },
          currentVersion: "0.6.0",
          isStderrTTY: true,
          stderr: stderrMissing,
          spawnRefresh: (p) => refreshCalls.push(p),
          nowMs: t0,
        }),
        { notified: false, refreshed: true, latest: null }
      );
      assert.equal(stderrMissing.text(), "");
      assert.deepEqual(refreshCalls, [cacheFile]);

      // Fresh cache with newer version notifies and does not refresh.
      refreshCalls.length = 0;
      fs.writeFileSync(cacheFile, JSON.stringify({ checked_at: t0, latest: "0.7.0" }));
      const stderrFresh = createBufferWriter();
      assert.deepEqual(
        maybeNotifyAndRefresh({
          argv: ["list"],
          env: { SIMPLE_MEMORY_HOME: smHome },
          currentVersion: "0.6.0",
          isStderrTTY: true,
          stderr: stderrFresh,
          spawnRefresh: (p) => refreshCalls.push(p),
          nowMs: t0 + 1000,
        }),
        { notified: true, refreshed: false, latest: "0.7.0" }
      );
      assert.equal(stderrFresh.text(), formatUpdateNotice("0.6.0", "0.7.0"));
      assert.deepEqual(refreshCalls, []);

      // Fresh cache with same version stays silent and does not refresh.
      const stderrSame = createBufferWriter();
      assert.deepEqual(
        maybeNotifyAndRefresh({
          argv: ["list"],
          env: { SIMPLE_MEMORY_HOME: smHome },
          currentVersion: "0.7.0",
          isStderrTTY: true,
          stderr: stderrSame,
          spawnRefresh: (p) => refreshCalls.push(p),
          nowMs: t0 + 1000,
        }),
        { notified: false, refreshed: false, latest: "0.7.0" }
      );
      assert.equal(stderrSame.text(), "");
      assert.deepEqual(refreshCalls, []);

      // Stale cache with newer version both notifies and refreshes.
      const stderrStale = createBufferWriter();
      assert.deepEqual(
        maybeNotifyAndRefresh({
          argv: ["list"],
          env: { SIMPLE_MEMORY_HOME: smHome },
          currentVersion: "0.6.0",
          isStderrTTY: true,
          stderr: stderrStale,
          spawnRefresh: (p) => refreshCalls.push(p),
          nowMs: t0 + DEFAULT_TTL_MS,
        }),
        { notified: true, refreshed: true, latest: "0.7.0" }
      );
      assert.equal(stderrStale.text(), formatUpdateNotice("0.6.0", "0.7.0"));
      assert.deepEqual(refreshCalls, [cacheFile]);

      // Throwing stderr and spawnRefresh never propagate errors.
      assert.deepEqual(
        maybeNotifyAndRefresh({
          argv: ["list"],
          env: { SIMPLE_MEMORY_HOME: smHome },
          currentVersion: "0.6.0",
          isStderrTTY: true,
          stderr: {
            write() {
              throw new Error("EPIPE");
            },
          },
          spawnRefresh() {
            throw new Error("ENOENT");
          },
          nowMs: t0 + DEFAULT_TTL_MS,
        }),
        { notified: false, refreshed: false, latest: "0.7.0" }
      );
    });
  });
});

describe("scripts/check-update.js", () => {
  it("fetches latest version from HTTP mock, writes atomic cache, and exits 0 silently on 500, invalid JSON, prerelease, timeout, or offline", async () => {
    let mode = "ok";
    const srv = await startServer((_req, res) => {
      if (mode === "ok") {
        res.writeHead(200, { "content-type": "application/json" });
        res.end(JSON.stringify({ name: "simple-memory", version: "0.8.0" }));
      } else if (mode === "500") {
        res.writeHead(500, { "content-type": "application/json" });
        res.end(JSON.stringify({ version: "9.9.9", error: "server error" }));
      } else if (mode === "bad-json") {
        res.writeHead(200, { "content-type": "application/json" });
        res.end("{broken");
      } else if (mode === "prerelease") {
        res.writeHead(200, { "content-type": "application/json" });
        res.end(JSON.stringify({ version: "0.9.0-beta.1" }));
      } else if (mode === "oversized") {
        res.writeHead(200, { "content-type": "application/json" });
        res.end(JSON.stringify({ version: "0.9.0", pad: "x".repeat(4096) }));
      } else if (mode === "hang") {
        // Leave open until AbortSignal fires.
      }
    });

    try {
      await withTempDirAsync(async (dir) => {
        const cachePath = path.join(dir, "nested", "update-check.json");

        // CLI invocation writes valid atomic cache with zero stdout/stderr.
        const okCli = await spawnAsync(process.execPath, [CHECK_UPDATE_SCRIPT, cachePath], {
          env: { ...process.env, SM_UPDATE_CHECK_URL: srv.baseUrl },
        });
        assert.equal(okCli.status, 0);
        assert.equal(okCli.stdout, "");
        assert.equal(okCli.stderr, "");
        const parsed = JSON.parse(fs.readFileSync(cachePath, "utf8"));
        assert.equal(parsed.latest, "0.8.0");
        assert.equal(typeof parsed.checked_at, "number");
        assert.deepEqual(fs.readdirSync(path.dirname(cachePath)), ["update-check.json"]);

        // CLI invocation with no positional cachePath falls back to SIMPLE_MEMORY_HOME.
        const envSmHome = path.join(dir, "env-home");
        const envCli = await spawnAsync(process.execPath, [CHECK_UPDATE_SCRIPT], {
          env: { ...process.env, SIMPLE_MEMORY_HOME: envSmHome, SM_UPDATE_CHECK_URL: srv.baseUrl },
        });
        assert.equal(envCli.status, 0);
        assert.equal(envCli.stdout, "");
        assert.equal(envCli.stderr, "");
        const envParsed = JSON.parse(
          fs.readFileSync(path.join(envSmHome, "update-check.json"), "utf8")
        );
        assert.equal(envParsed.latest, "0.8.0");

        // Existing cache must remain intact across 500, bad JSON, prerelease, oversized, timeout, and offline errors.
        const snapshot = fs.readFileSync(cachePath, "utf8");

        for (const errMode of ["500", "bad-json", "prerelease"]) {
          mode = errMode;
          const cliRun = await spawnAsync(process.execPath, [CHECK_UPDATE_SCRIPT, cachePath], {
            env: { ...process.env, SM_UPDATE_CHECK_URL: srv.baseUrl },
          });
          assert.equal(cliRun.status, 0, `mode ${errMode} must exit 0`);
          assert.equal(cliRun.stdout, "");
          assert.equal(cliRun.stderr, "");
          assert.equal(fs.readFileSync(cachePath, "utf8"), snapshot);
        }

        mode = "oversized";
        assert.equal(
          await fetchAndWriteUpdateCache({ cachePath, url: srv.baseUrl, maxBytes: 512 }),
          false
        );
        assert.equal(fs.readFileSync(cachePath, "utf8"), snapshot);

        mode = "hang";
        assert.equal(
          await fetchAndWriteUpdateCache({ cachePath, url: srv.baseUrl, timeoutMs: 60 }),
          false
        );
        assert.equal(fs.readFileSync(cachePath, "utf8"), snapshot);

        assert.equal(await fetchAndWriteUpdateCache({ cachePath: "", url: srv.baseUrl }), false);

        // Offline / unreachable port exits 0 silently.
        const deadSrv = await startServer((_req, res) => res.end());
        const deadUrl = deadSrv.baseUrl;
        await deadSrv.close();
        const offlineCli = await spawnAsync(process.execPath, [CHECK_UPDATE_SCRIPT, cachePath], {
          env: { ...process.env, SM_UPDATE_CHECK_URL: deadUrl },
        });
        assert.equal(offlineCli.status, 0);
        assert.equal(offlineCli.stdout, "");
        assert.equal(offlineCli.stderr, "");
        assert.equal(fs.readFileSync(cachePath, "utf8"), snapshot);
      });
    } finally {
      await srv.close();
    }
  });
});

describe("scripts/postinstall.js", () => {
  it("parseSha256Sums handles standard, binary-star, ./ prefix, uppercase, missing, and conflicts", () => {
    const h1 = "A".repeat(64);
    const h2 = "b".repeat(64);
    const body = [
      `${h1}  sm-darwin-arm64.tar.gz`,
      `${h2} *./sm-linux-x64.tar.gz`,
      "not-a-valid-line",
    ].join("\n");

    assert.equal(parseSha256Sums(body, "sm-darwin-arm64.tar.gz"), "a".repeat(64));
    assert.equal(parseSha256Sums(body, "sm-linux-x64.tar.gz"), h2);
    assert.equal(parseSha256Sums(body, "sm-darwin-x64.tar.gz"), null);

    assert.throws(
      () =>
        parseSha256Sums(
          `${"a".repeat(64)}  sm-darwin-arm64.tar.gz\n${"c".repeat(64)}  ./sm-darwin-arm64.tar.gz\n`,
          "sm-darwin-arm64.tar.gz"
        ),
      /Conflicting SHA256SUMS/
    );
  });

  it("readStreamWithLimit allows exact maxBytes and rejects maxBytes + 1", async () => {
    const exact = Readable.from([Buffer.from("abcd"), Buffer.from("efgh")]);
    const buf = await readStreamWithLimit(exact, 8);
    assert.equal(buf.toString("utf8"), "abcdefgh");

    const over = Readable.from([Buffer.from("abcd"), Buffer.from("efghi")]);
    await assert.rejects(() => readStreamWithLimit(over, 8), { code: "ERR_SIZE_LIMIT" });
  });

  it("extractBinaryFromTarGz extracts regular files (including PAX headers) and rejects unsafe entries", () => {
    const expected = Buffer.from("#!/bin/sh\necho ok\n");

    const plain = buildTarGz([{ name: "./sm", data: expected }]);
    assert.deepEqual(extractBinaryFromTarGz(plain, "sm"), expected);

    const paxData = Buffer.concat([makePaxRecord("mtime", "1700000000"), makePaxRecord("path", "sm")]);
    const withPax = buildTarGz([
      { name: "PaxHeaders.0/sm", data: paxData, typeflag: "x", mode: 0o644 },
      { name: "ignored-name", data: expected, typeflag: "0", mode: 0o755 },
    ]);
    assert.deepEqual(extractBinaryFromTarGz(withPax, "sm"), expected);

    const withSymlink = buildTarGz([{ name: "sm", typeflag: "2" }]);
    assert.throws(() => extractBinaryFromTarGz(withSymlink, "sm"), /Unsupported tar entry type '2'/);

    const withHardlink = buildTarGz([{ name: "sm", typeflag: "1" }]);
    assert.throws(() => extractBinaryFromTarGz(withHardlink, "sm"), /Unsupported tar entry type '1'/);

    const withTraversal = buildTarGz([{ name: "../sm", data: expected }]);
    assert.throws(() => extractBinaryFromTarGz(withTraversal, "sm"), /Unsafe archive entry path/);

    const withAbsPath = buildTarGz([{ name: "/tmp/sm", data: expected }]);
    assert.throws(() => extractBinaryFromTarGz(withAbsPath, "sm"), /Unsafe archive entry path/);

    const withDirNamedSm = buildTarGz([{ name: "sm", typeflag: "5" }]);
    assert.throws(() => extractBinaryFromTarGz(withDirNamedSm, "sm"), /is a directory/);

    const missingSm = buildTarGz([{ name: "other", data: expected }]);
    assert.throws(() => extractBinaryFromTarGz(missingSm, "sm"), /does not contain regular file sm/);

    const duplicateSm = buildTarGz([
      { name: "sm", data: expected },
      { name: "./sm", data: expected },
    ]);
    assert.throws(() => extractBinaryFromTarGz(duplicateSm, "sm"), /duplicate entries for sm/);

    const emptySm = buildTarGz([{ name: "sm", data: Buffer.alloc(0) }]);
    assert.throws(() => extractBinaryFromTarGz(emptySm, "sm"), /Archive entry sm is empty/);

    const badChecksumTar = zlib.gunzipSync(buildTarGz([{ name: "sm", data: expected }]));
    badChecksumTar[148] = badChecksumTar[148] === 0x30 ? 0x31 : 0x30;
    assert.throws(
      () => extractBinaryFromTarGz(zlib.gzipSync(badChecksumTar), "sm"),
      /Invalid tar header checksum/
    );

    const bomb = zlib.gzipSync(Buffer.alloc(4096, 0x41));
    assert.throws(() => extractBinaryFromTarGz(bomb, "sm", 1024), { code: "ERR_BUFFER_TOO_LARGE" });

    // Real host tar -czf (bsdtar on macOS, GNU tar on Linux) extracts exact bytes.
    withTempDir((dir) => {
      const srcBin = path.join(dir, "sm");
      const tarPath = path.join(dir, "sm.tar.gz");
      const payload = Buffer.from("#!/bin/sh\necho real-tar-ok\n");
      fs.writeFileSync(srcBin, payload, { mode: 0o755 });
      const tarRun = spawnSync("tar", ["-czf", tarPath, "-C", dir, "sm"], {
        env: { ...process.env, COPYFILE_DISABLE: "1" },
        encoding: "utf8",
      });
      assert.equal(tarRun.status, 0, tarRun.stderr);
      assert.deepEqual(extractBinaryFromTarGz(fs.readFileSync(tarPath), "sm"), payload);
    });
  });

  it("exits 0 when invoked as CLI subprocess with SM_SKIP_DOWNLOAD=1", () => {
    const postinstallPath = path.join(REPO_ROOT, "npm", "simple-memory", "scripts", "postinstall.js");
    const run = spawnSync(process.execPath, [postinstallPath], {
      env: { ...process.env, SM_SKIP_DOWNLOAD: "1" },
      encoding: "utf8",
    });
    assert.equal(run.status, 0, run.stderr);
    assert.equal(run.stdout, "");
    assert.equal(run.stderr, "");
  });

  it("skips download with zero HTTP traffic on SM_SKIP_DOWNLOAD=1, optionalDependency (even with bad SM_BINARY_PATH), and unsupported platform", async () => {
    let requests = 0;
    const srv = await startServer((_req, res) => {
      requests += 1;
      res.writeHead(500);
      res.end();
    });
    try {
      await withTempDirAsync(async (dir) => {
        const optBin = path.join(dir, "opt-sm");
        fs.writeFileSync(optBin, "opt");
        const vendorDir = path.join(dir, "vendor");

        const skipCode = await runPostinstall({
          env: { SM_SKIP_DOWNLOAD: "1", SM_DOWNLOAD_BASE_URL: srv.baseUrl },
          platform: "darwin",
          arch: "arm64",
          version: "0.5.0",
          vendorDir,
          resolvePkg: () => {
            const err = new Error("missing");
            err.code = "MODULE_NOT_FOUND";
            throw err;
          },
          stderr: createBufferWriter(),
        });
        assert.equal(skipCode, 0);

        const optCode = await runPostinstall({
          env: {
            SM_BINARY_PATH: path.join(dir, "nonexistent-override"),
            SM_DOWNLOAD_BASE_URL: srv.baseUrl,
          },
          platform: "darwin",
          arch: "arm64",
          version: "0.5.0",
          vendorDir,
          resolvePkg: () => optBin,
          stderr: createBufferWriter(),
        });
        assert.equal(optCode, 0);

        const unsupErr = createBufferWriter();
        const unsupCode = await runPostinstall({
          env: { SM_DOWNLOAD_BASE_URL: srv.baseUrl },
          platform: "win32",
          arch: "x64",
          version: "0.5.0",
          vendorDir,
          resolvePkg: () => {
            throw new Error("should not be called");
          },
          stderr: unsupErr,
        });
        assert.equal(unsupCode, 0);
        assert.match(unsupErr.text(), /no prebuilt binary for win32-x64/);
        assert.match(
          unsupErr.text(),
          /git clone https:\/\/github\.com\/tuanhung303\/simple-memory\.git, cd simple-memory, then run \.\/scripts\/install\.sh/
        );
        assert.equal(requests, 0);
      });
    } finally {
      await srv.close();
    }
  });

  it("downloads through HTTP 302 redirect, verifies SHA256, is idempotent on rerun, and upgrades on version change", async () => {
    const binaryV1 = Buffer.from("#!/bin/sh\necho sm-0.5.0\n");
    const binaryV2 = Buffer.from("#!/bin/sh\necho sm-0.5.1\n");
    let currentTarGz = buildTarGz([{ name: "sm", data: binaryV1 }]);
    let currentSums = `${sha256Hex(currentTarGz)}  sm-linux-x64.tar.gz\n`;
    const hits = [];

    const srv = await startServer((req, res) => {
      hits.push(req.url);
      if (req.url === "/v/SHA256SUMS") {
        res.writeHead(200, { "content-type": "text/plain" });
        res.end(currentSums);
        return;
      }
      if (req.url === "/v/sm-linux-x64.tar.gz") {
        res.writeHead(302, { location: "/cdn/sm-linux-x64.tar.gz" });
        res.end();
        return;
      }
      if (req.url === "/cdn/sm-linux-x64.tar.gz") {
        res.writeHead(200, { "content-type": "application/gzip" });
        res.end(currentTarGz);
        return;
      }
      res.writeHead(404);
      res.end("not found");
    });

    try {
      await withTempDirAsync(async (dir) => {
        const vendorDir = path.join(dir, "nested", "vendor");
        const missingPkg = () => {
          const err = new Error("missing");
          err.code = "MODULE_NOT_FOUND";
          throw err;
        };

        // Pass trailing slash to test URL normalization.
        const rc1 = await runPostinstall({
          env: { SM_DOWNLOAD_BASE_URL: `${srv.baseUrl}/v///` },
          platform: "linux",
          arch: "x64",
          version: "0.5.0",
          vendorDir,
          resolvePkg: missingPkg,
          stderr: createBufferWriter(),
        });
        assert.equal(rc1, 0);
        assert.deepEqual(hits, [
          "/v/SHA256SUMS",
          "/v/sm-linux-x64.tar.gz",
          "/cdn/sm-linux-x64.tar.gz",
        ]);
        const vendorBin = path.join(vendorDir, "sm");
        assert.deepEqual(fs.readFileSync(vendorBin), binaryV1);
        assert.equal(fs.statSync(vendorBin).mode & 0o777, 0o755);
        assert.equal(fs.readFileSync(path.join(vendorDir, ".version"), "utf8").trim(), "0.5.0");
        assert.deepEqual(fs.readdirSync(vendorDir).sort(), [".version", "sm"]);

        // Second run with same version is idempotent with zero HTTP calls.
        hits.length = 0;
        const rc2 = await runPostinstall({
          env: { SM_DOWNLOAD_BASE_URL: `${srv.baseUrl}/v` },
          platform: "linux",
          arch: "x64",
          version: "0.5.0",
          vendorDir,
          resolvePkg: missingPkg,
          stderr: createBufferWriter(),
        });
        assert.equal(rc2, 0);
        assert.deepEqual(hits, []);

        // Upgrade version triggers fresh download and atomic replacement.
        currentTarGz = buildTarGz([{ name: "./sm", data: binaryV2 }]);
        currentSums = `${sha256Hex(currentTarGz)}  sm-linux-x64.tar.gz\n`;
        const rc3 = await runPostinstall({
          env: { SM_DOWNLOAD_BASE_URL: `${srv.baseUrl}/v` },
          platform: "linux",
          arch: "x64",
          version: "0.5.1",
          vendorDir,
          resolvePkg: missingPkg,
          stderr: createBufferWriter(),
        });
        assert.equal(rc3, 0);
        assert.deepEqual(fs.readFileSync(vendorBin), binaryV2);
        assert.equal(fs.readFileSync(path.join(vendorDir, ".version"), "utf8").trim(), "0.5.1");
      });
    } finally {
      await srv.close();
    }
  });

  it("exits 0 with warning on HTTP 404/500/timeout, and exits 1 without writing vendor/sm on SHA256 or size failure", async () => {
    const validTar = buildTarGz([{ name: "sm", data: Buffer.from("binary-data") }]);
    const missingPkg = () => {
      const err = new Error("missing");
      err.code = "MODULE_NOT_FOUND";
      throw err;
    };

    let mode = "404";
    const srv = await startServer((req, res) => {
      if (mode === "404") {
        res.writeHead(404);
        res.end("missing");
      } else if (mode === "500") {
        res.writeHead(500);
        res.end("boom");
      } else if (mode === "tar-404") {
        if (req.url.endsWith("/SHA256SUMS")) {
          res.writeHead(200);
          res.end(`${sha256Hex(validTar)}  sm-linux-x64.tar.gz\n`);
        } else {
          res.writeHead(404);
          res.end("tar missing");
        }
      } else if (mode === "content-length-oversized") {
        if (req.url.endsWith("/SHA256SUMS")) {
          res.writeHead(200);
          res.end(`${sha256Hex(validTar)}  sm-linux-x64.tar.gz\n`);
        } else {
          res.writeHead(200, { "Content-Length": "9999999" });
          res.flushHeaders();
        }
      } else if (mode === "timeout") {
        // Hold open until client aborts.
      } else if (mode === "missing-entry") {
        res.writeHead(200);
        res.end(`${"0".repeat(64)}  sm-darwin-x64.tar.gz\n`);
      } else if (mode === "bad-hash") {
        if (req.url.endsWith("/SHA256SUMS")) {
          res.writeHead(200);
          res.end(`${"f".repeat(64)}  sm-linux-x64.tar.gz\n`);
        } else {
          res.writeHead(200);
          res.end(validTar);
        }
      } else if (mode === "oversized") {
        if (req.url.endsWith("/SHA256SUMS")) {
          res.writeHead(200);
          res.end(`${sha256Hex(validTar)}  sm-linux-x64.tar.gz\n`);
        } else {
          res.writeHead(200);
          res.end(Buffer.alloc(2048, 0x42));
        }
      }
    });

    try {
      await withTempDirAsync(async (dir) => {
        for (const [m, timeoutMs, wantPattern] of [
          ["404", "5000", /HTTP 404/],
          ["500", "5000", /HTTP 500/],
          ["tar-404", "5000", /HTTP 404/],
          ["timeout", "60", /\.\/scripts\/install\.sh/],
        ]) {
          mode = m;
          const vendorDir = path.join(dir, `vendor-${m}`);
          const errOut = createBufferWriter();
          const rc = await runPostinstall({
            env: {
              SM_DOWNLOAD_BASE_URL: srv.baseUrl,
              SM_DOWNLOAD_TIMEOUT_MS: timeoutMs,
            },
            platform: "linux",
            arch: "x64",
            version: "0.5.0",
            vendorDir,
            resolvePkg: missingPkg,
            stderr: errOut,
          });
          assert.equal(rc, 0, `mode ${m} should warn and exit 0`);
          assert.match(errOut.text(), /\.\/scripts\/install\.sh/);
          assert.match(errOut.text(), /git clone https:\/\/github\.com\/tuanhung303\/simple-memory\.git, cd simple-memory/);
          assert.match(errOut.text(), wantPattern);
          assert.equal(fs.existsSync(path.join(vendorDir, "sm")), false);
        }

        mode = "content-length-oversized";
        const vCl = path.join(dir, "vendor-cl-oversized");
        const errCl = createBufferWriter();
        assert.equal(
          await runPostinstall({
            env: { SM_DOWNLOAD_BASE_URL: srv.baseUrl },
            platform: "linux",
            arch: "x64",
            version: "0.5.0",
            vendorDir: vCl,
            resolvePkg: missingPkg,
            maxDownloadBytes: 1024,
            stderr: errCl,
          }),
          1
        );
        assert.match(errCl.text(), /Content-Length 9999999 exceeds limit of 1024/);
        assert.equal(fs.existsSync(path.join(vCl, "sm")), false);

        mode = "missing-entry";
        const vMissing = path.join(dir, "vendor-missing-entry");
        const errMissing = createBufferWriter();
        assert.equal(
          await runPostinstall({
            env: { SM_DOWNLOAD_BASE_URL: srv.baseUrl },
            platform: "linux",
            arch: "x64",
            version: "0.5.0",
            vendorDir: vMissing,
            resolvePkg: missingPkg,
            stderr: errMissing,
          }),
          1
        );
        assert.match(errMissing.text(), /does not contain an entry for sm-linux-x64\.tar\.gz/);
        assert.equal(fs.existsSync(path.join(vMissing, "sm")), false);

        mode = "bad-hash";
        const vBadHash = path.join(dir, "vendor-bad-hash");
        const errBadHash = createBufferWriter();
        assert.equal(
          await runPostinstall({
            env: { SM_DOWNLOAD_BASE_URL: srv.baseUrl },
            platform: "linux",
            arch: "x64",
            version: "0.5.0",
            vendorDir: vBadHash,
            resolvePkg: missingPkg,
            stderr: errBadHash,
          }),
          1
        );
        assert.match(errBadHash.text(), /SHA-256 mismatch/);
        assert.equal(fs.existsSync(path.join(vBadHash, "sm")), false);

        mode = "oversized";
        const vOversized = path.join(dir, "vendor-oversized");
        const errOversized = createBufferWriter();
        assert.equal(
          await runPostinstall({
            env: { SM_DOWNLOAD_BASE_URL: srv.baseUrl },
            platform: "linux",
            arch: "x64",
            version: "0.5.0",
            vendorDir: vOversized,
            resolvePkg: missingPkg,
            stderr: errOversized,
            maxDownloadBytes: 512,
          }),
          1
        );
        assert.match(errOversized.text(), /exceeds limit|exceeded limit/);
        assert.equal(fs.existsSync(path.join(vOversized, "sm")), false);
      });
    } finally {
      await srv.close();
    }
  });
});

describe("scripts/stage_npm.sh", () => {
  it("stages, packs, and verifies packages with dynamic optionalDependencies and 0755 modes", () => {
    withTempDir((dir) => {
      const repo = path.join(dir, "repo");
      const out = path.join(dir, "stage");
      makeFixtureRepo(repo);

      const linuxBin = path.join(dir, "sm-linux-x64");
      const darwinBin = path.join(dir, "sm-darwin-arm64");
      fs.writeFileSync(linuxBin, makeElfHeader(62));
      fs.writeFileSync(darwinBin, makeMachOHeader(0x0100000c, 0, "/usr/lib/libSystem.B.dylib"));
      fs.chmodSync(linuxBin, 0o644);
      fs.chmodSync(darwinBin, 0o644);

      const res = spawnSync(
        "bash",
        [
          STAGE_SCRIPT,
          "--repo",
          repo,
          "--out",
          out,
          "--bin",
          `linux-x64=${linuxBin}`,
          "--bin",
          `darwin-arm64=${darwinBin}`,
          "--pack",
          "--verify-pack",
        ],
        { encoding: "utf8" }
      );
      assert.equal(res.status, 0, `stderr:\n${res.stderr}\nstdout:\n${res.stdout}`);
      assert.match(res.stdout, /verified 3 tarball\(s\)/);

      const stagedMainPkg = JSON.parse(
        fs.readFileSync(path.join(out, "packages", "simple-memory", "package.json"), "utf8")
      );
      assert.deepEqual(stagedMainPkg.optionalDependencies, {
        "simple-memory-darwin-arm64": PKG_VERSION,
        "simple-memory-linux-x64": PKG_VERSION,
      });
      assert.equal(
        fs.statSync(path.join(out, "packages", "simple-memory-linux-x64", "bin", "sm")).mode & 0o777,
        0o755
      );
      assert.equal(
        fs.statSync(path.join(out, "packages", "simple-memory", "bin", "sm.js")).mode & 0o777,
        0o755
      );
      assert.equal(
        fs.existsSync(path.join(out, "packages", "simple-memory", "lib", "update.js")),
        true
      );
      assert.equal(
        fs.existsSync(path.join(out, "packages", "simple-memory", "scripts", "check-update.js")),
        true
      );
    });
  });

  it("refuses unsafe --out, version mismatch, wrong arch, non-system dylib, and /Users/ paths in binaries", () => {
    withTempDir((dir) => {
      const repo = path.join(dir, "repo");
      const out = path.join(dir, "stage");
      makeFixtureRepo(repo);

      const linuxBin = path.join(dir, "sm-linux-x64");
      fs.writeFileSync(linuxBin, makeElfHeader(62));

      // Unsafe --out values.
      for (const badOut of ["/", repo, os.homedir()]) {
        const r = spawnSync(
          "bash",
          [STAGE_SCRIPT, "--repo", repo, "--out", badOut, "--bin", `linux-x64=${linuxBin}`],
          { encoding: "utf8" }
        );
        assert.notEqual(r.status, 0, `--out ${badOut} must be refused`);
        assert.match(r.stderr, /refusing unsafe --out/);
      }

      // Wrong binary arch/OS.
      const wrongArch = spawnSync(
        "bash",
        [STAGE_SCRIPT, "--repo", repo, "--out", out, "--bin", `darwin-arm64=${linuxBin}`],
        { encoding: "utf8" }
      );
      assert.notEqual(wrongArch.status, 0);
      assert.match(wrongArch.stderr, /binary arch\/OS mismatch/);

      // Non-system dylib on macOS host.
      if (process.platform === "darwin") {
        const badDylibBin = path.join(dir, "sm-bad-dylib");
        fs.writeFileSync(
          badDylibBin,
          makeMachOHeader(0x0100000c, 0, "/opt/homebrew/lib/libexample.dylib")
        );
        const badDylib = spawnSync(
          "bash",
          [STAGE_SCRIPT, "--repo", repo, "--out", out, "--bin", `darwin-arm64=${badDylibBin}`],
          { encoding: "utf8" }
        );
        assert.notEqual(badDylib.status, 0);
        assert.match(badDylib.stderr, /non-system dylib/);
      }

      // Hardcoded /Users/ path inside compiled binary caught by --verify-pack.
      const leakedBin = path.join(dir, "sm-leaked");
      fs.writeFileSync(leakedBin, makeElfHeader(62, Buffer.from("/Users/example/src/main.rs\0")));
      const leaked = spawnSync(
        "bash",
        [
          STAGE_SCRIPT,
          "--repo",
          repo,
          "--out",
          out,
          "--bin",
          `linux-x64=${leakedBin}`,
          "--pack",
          "--verify-pack",
        ],
        { encoding: "utf8" }
      );
      assert.notEqual(leaked.status, 0);
      assert.match(leaked.stderr, /hardcoded host path '\/Users\/' found/);

      // Version mismatch: main package.json differs from Cargo.toml while platform matches.
      const mainMismatchRepo = path.join(dir, "main-mismatch-repo");
      makeFixtureRepo(mainMismatchRepo);
      const mainPkgPath = path.join(mainMismatchRepo, "npm", "simple-memory", "package.json");
      const mainPkgData = JSON.parse(fs.readFileSync(mainPkgPath, "utf8"));
      mainPkgData.version = "0.9.9";
      fs.writeFileSync(mainPkgPath, JSON.stringify(mainPkgData, null, 2));
      const mainMismatch = spawnSync(
        "bash",
        [STAGE_SCRIPT, "--repo", mainMismatchRepo, "--out", out, "--bin", `linux-x64=${linuxBin}`],
        { encoding: "utf8" }
      );
      assert.notEqual(mainMismatch.status, 0);
      assert.match(mainMismatch.stderr, /version mismatch.*simple-memory\/package\.json/);

      // Version mismatch: platform package.json differs from Cargo.toml while main matches.
      const platMismatchRepo = path.join(dir, "plat-mismatch-repo");
      makeFixtureRepo(platMismatchRepo);
      const platPkgPath = path.join(
        platMismatchRepo,
        "npm",
        "platforms",
        "linux-x64",
        "package.json"
      );
      const platPkgData = JSON.parse(fs.readFileSync(platPkgPath, "utf8"));
      platPkgData.version = "0.9.9";
      fs.writeFileSync(platPkgPath, JSON.stringify(platPkgData, null, 2));
      const platMismatch = spawnSync(
        "bash",
        [STAGE_SCRIPT, "--repo", platMismatchRepo, "--out", out, "--bin", `linux-x64=${linuxBin}`],
        { encoding: "utf8" }
      );
      assert.notEqual(platMismatch.status, 0);
      assert.match(platMismatch.stderr, /version mismatch.*platforms\/linux-x64\/package\.json/);
    });
  });

  it("--verify-pack rejects forbidden .env/vendor members, non-0755 binaries, and missing required files", () => {
    withTempDir((dir) => {
      const cases = [
        {
          label: "forbidden .env",
          entries: [
            {
              name: "package/package.json",
              data: Buffer.from(JSON.stringify({ name: "simple-memory", version: "0.6.0" })),
              mode: 0o644,
            },
            { name: "package/.env", data: Buffer.from("NPM_TOKEN=example\n"), mode: 0o644 },
          ],
          pattern: /forbidden member.*package\/\.env/,
        },
        {
          label: "forbidden vendor/",
          entries: [
            {
              name: "package/package.json",
              data: Buffer.from(JSON.stringify({ name: "simple-memory", version: "0.6.0" })),
              mode: 0o644,
            },
            { name: "package/vendor/sm", data: Buffer.from("bin"), mode: 0o755 },
          ],
          pattern: /forbidden member.*package\/vendor\/sm/,
        },
        {
          label: "non-0755 main bin/sm.js",
          entries: [
            {
              name: "package/package.json",
              data: Buffer.from(JSON.stringify({ name: "simple-memory", version: "0.6.0" })),
              mode: 0o644,
            },
            { name: "package/bin/sm.js", data: Buffer.from("#!/usr/bin/env node\n"), mode: 0o644 },
            { name: "package/lib/platform.js", data: Buffer.from("// lib\n"), mode: 0o644 },
            { name: "package/lib/update.js", data: Buffer.from("// update\n"), mode: 0o644 },
            {
              name: "package/scripts/postinstall.js",
              data: Buffer.from("// postinstall\n"),
              mode: 0o644,
            },
            {
              name: "package/scripts/check-update.js",
              data: Buffer.from("// check-update\n"),
              mode: 0o644,
            },
            { name: "package/LICENSE", data: Buffer.from("MIT\n"), mode: 0o644 },
            { name: "package/README.md", data: Buffer.from("# hi\n"), mode: 0o644 },
          ],
          pattern: /expected mode 0755 for package\/bin\/sm\.js/,
        },
        {
          label: "non-0755 platform bin/sm",
          entries: [
            {
              name: "package/package.json",
              data: Buffer.from(
                JSON.stringify({ name: "simple-memory-linux-x64", version: "0.6.0" })
              ),
              mode: 0o644,
            },
            { name: "package/bin/sm", data: makeElfHeader(62), mode: 0o644 },
            { name: "package/LICENSE", data: Buffer.from("MIT\n"), mode: 0o644 },
            { name: "package/README.md", data: Buffer.from("# hi\n"), mode: 0o644 },
          ],
          pattern: /expected mode 0755 for package\/bin\/sm/,
        },
        {
          label: "missing LICENSE",
          entries: [
            {
              name: "package/package.json",
              data: Buffer.from(
                JSON.stringify({ name: "simple-memory-linux-x64", version: "0.6.0" })
              ),
              mode: 0o644,
            },
            { name: "package/bin/sm", data: makeElfHeader(62), mode: 0o755 },
            { name: "package/README.md", data: Buffer.from("# hi\n"), mode: 0o644 },
          ],
          pattern: /unexpected file set.*missing=\['package\/LICENSE'\]/,
        },
        {
          label: "symlink member inside tarball",
          entries: [
            {
              name: "package/package.json",
              data: Buffer.from(
                JSON.stringify({ name: "simple-memory-linux-x64", version: "0.6.0" })
              ),
              mode: 0o644,
            },
            { name: "package/bin/sm", typeflag: "2", mode: 0o777 },
            { name: "package/LICENSE", data: Buffer.from("MIT\n"), mode: 0o644 },
            { name: "package/README.md", data: Buffer.from("# hi\n"), mode: 0o644 },
          ],
          pattern: /forbidden link entry.*package\/bin\/sm/,
        },
        {
          label: "hardcoded /home/ host path in binary",
          entries: [
            {
              name: "package/package.json",
              data: Buffer.from(
                JSON.stringify({ name: "simple-memory-linux-x64", version: "0.6.0" })
              ),
              mode: 0o644,
            },
            {
              name: "package/bin/sm",
              data: Buffer.concat([makeElfHeader(62), Buffer.from("/home/runner/work/sm\0")]),
              mode: 0o755,
            },
            { name: "package/LICENSE", data: Buffer.from("MIT\n"), mode: 0o644 },
            { name: "package/README.md", data: Buffer.from("# hi\n"), mode: 0o644 },
          ],
          pattern: /hardcoded host path '\/home\/'/,
        },
        {
          label: "missing declared optionalDependency tarball",
          entries: [
            {
              name: "package/package.json",
              data: Buffer.from(
                JSON.stringify({
                  name: "simple-memory",
                  version: "0.6.0",
                  optionalDependencies: { "simple-memory-linux-x64": "0.6.0" },
                })
              ),
              mode: 0o644,
            },
            { name: "package/bin/sm.js", data: Buffer.from("#!/usr/bin/env node\n"), mode: 0o755 },
            { name: "package/lib/platform.js", data: Buffer.from("// lib\n"), mode: 0o644 },
            { name: "package/lib/update.js", data: Buffer.from("// update\n"), mode: 0o644 },
            { name: "package/scripts/postinstall.js", data: Buffer.from("// pi\n"), mode: 0o644 },
            {
              name: "package/scripts/check-update.js",
              data: Buffer.from("// check-update\n"),
              mode: 0o644,
            },
            { name: "package/LICENSE", data: Buffer.from("MIT\n"), mode: 0o644 },
            { name: "package/README.md", data: Buffer.from("# hi\n"), mode: 0o644 },
          ],
          pattern: /optionalDependency simple-memory-linux-x64 has no matching tarball/,
        },
        {
          label: "missing package/lib/update.js and package/scripts/check-update.js in main tarball",
          entries: [
            {
              name: "package/package.json",
              data: Buffer.from(JSON.stringify({ name: "simple-memory", version: "0.6.0" })),
              mode: 0o644,
            },
            { name: "package/bin/sm.js", data: Buffer.from("#!/usr/bin/env node\n"), mode: 0o755 },
            { name: "package/lib/platform.js", data: Buffer.from("// lib\n"), mode: 0o644 },
            { name: "package/scripts/postinstall.js", data: Buffer.from("// pi\n"), mode: 0o644 },
            { name: "package/LICENSE", data: Buffer.from("MIT\n"), mode: 0o644 },
            { name: "package/README.md", data: Buffer.from("# hi\n"), mode: 0o644 },
          ],
          pattern: /unexpected file set.*package\/lib\/update\.js.*package\/scripts\/check-update\.js/,
        },
      ];

      for (let i = 0; i < cases.length; i += 1) {
        const c = cases[i];
        const out = path.join(dir, `stage-${i}`);
        const tarballs = path.join(out, "tarballs");
        fs.mkdirSync(tarballs, { recursive: true });
        fs.writeFileSync(path.join(tarballs, "pkg-0.6.0.tgz"), buildTarGz(c.entries));

        const res = spawnSync("bash", [STAGE_SCRIPT, "--out", out, "--verify-pack"], {
          encoding: "utf8",
        });
        assert.notEqual(res.status, 0, `${c.label} must fail --verify-pack`);
        assert.match(res.stderr, c.pattern, c.label);
      }

      // Orphan extra platform tarball not declared in simple-memory optionalDependencies is rejected.
      const orphanOut = path.join(dir, "stage-orphan");
      const orphanTarballs = path.join(orphanOut, "tarballs");
      fs.mkdirSync(orphanTarballs, { recursive: true });
      fs.writeFileSync(
        path.join(orphanTarballs, "simple-memory-0.6.0.tgz"),
        buildTarGz([
          {
            name: "package/package.json",
            data: Buffer.from(
              JSON.stringify({
                name: "simple-memory",
                version: "0.6.0",
                optionalDependencies: { "simple-memory-linux-x64": "0.6.0" },
              })
            ),
            mode: 0o644,
          },
          { name: "package/bin/sm.js", data: Buffer.from("#!/usr/bin/env node\n"), mode: 0o755 },
          { name: "package/lib/platform.js", data: Buffer.from("// lib\n"), mode: 0o644 },
          { name: "package/lib/update.js", data: Buffer.from("// update\n"), mode: 0o644 },
          { name: "package/scripts/postinstall.js", data: Buffer.from("// pi\n"), mode: 0o644 },
          {
            name: "package/scripts/check-update.js",
            data: Buffer.from("// check-update\n"),
            mode: 0o644,
          },
          { name: "package/LICENSE", data: Buffer.from("MIT\n"), mode: 0o644 },
          { name: "package/README.md", data: Buffer.from("# hi\n"), mode: 0o644 },
        ])
      );
      fs.writeFileSync(
        path.join(orphanTarballs, "simple-memory-linux-x64-0.6.0.tgz"),
        buildTarGz([
          {
            name: "package/package.json",
            data: Buffer.from(JSON.stringify({ name: "simple-memory-linux-x64", version: "0.6.0" })),
            mode: 0o644,
          },
          { name: "package/bin/sm", data: makeElfHeader(62), mode: 0o755 },
          { name: "package/LICENSE", data: Buffer.from("MIT\n"), mode: 0o644 },
          { name: "package/README.md", data: Buffer.from("# hi\n"), mode: 0o644 },
        ])
      );
      fs.writeFileSync(
        path.join(orphanTarballs, "simple-memory-linux-arm64-0.4.0.tgz"),
        buildTarGz([
          {
            name: "package/package.json",
            data: Buffer.from(JSON.stringify({ name: "simple-memory-linux-arm64", version: "0.4.0" })),
            mode: 0o644,
          },
          { name: "package/bin/sm", data: makeElfHeader(183), mode: 0o755 },
          { name: "package/LICENSE", data: Buffer.from("MIT\n"), mode: 0o644 },
          { name: "package/README.md", data: Buffer.from("# hi\n"), mode: 0o644 },
        ])
      );
      const orphanRes = spawnSync("bash", [STAGE_SCRIPT, "--out", orphanOut, "--verify-pack"], {
        encoding: "utf8",
      });
      assert.notEqual(orphanRes.status, 0);
      assert.match(orphanRes.stderr, /orphan platform tarball/);
    });
  });

  it("--publish reads .env safely without executing shell code, preflights whoami + dry-run, orders platforms before main, and cleans .npmrc", () => {
    withTempDir((dir) => {
      const repo = path.join(dir, "repo");
      const out = path.join(dir, "stage");
      makeFixtureRepo(repo);

      const linuxBin = path.join(dir, "sm-linux-x64");
      fs.writeFileSync(linuxBin, makeElfHeader(62));

      const pack = spawnSync(
        "bash",
        [
          STAGE_SCRIPT,
          "--repo",
          repo,
          "--out",
          out,
          "--bin",
          `linux-x64=${linuxBin}`,
          "--pack",
          "--verify-pack",
        ],
        { encoding: "utf8" }
      );
      assert.equal(pack.status, 0, pack.stderr);

      const pwnFile = path.join(dir, "pwned");
      const sampleToken = "tok_example_publish_12345";
      fs.writeFileSync(
        path.join(repo, ".env"),
        `# comment\nOTHER=$(touch ${pwnFile})\nNPM_TOKEN="${sampleToken}"\r\n`
      );

      const fakeBinDir = path.join(dir, "fake-bin");
      fs.mkdirSync(fakeBinDir);
      const logFile = path.join(dir, "npm-calls.jsonl");
      const fakeNpm = path.join(fakeBinDir, "npm");
      fs.writeFileSync(
        fakeNpm,
        `#!${process.execPath}
const fs = require("node:fs");
const args = process.argv.slice(2);
const ucIdx = args.indexOf("--userconfig");
let ucPath = null;
let ucMode = null;
let ucContent = null;
if (ucIdx !== -1 && args[ucIdx + 1]) {
  ucPath = args[ucIdx + 1];
  ucMode = fs.statSync(ucPath).mode & 0o777;
  ucContent = fs.readFileSync(ucPath, "utf8");
}
fs.appendFileSync(${JSON.stringify(logFile)}, JSON.stringify({ args, ucPath, ucMode, ucContent }) + "\\n");
if (args[0] === "whoami") {
  process.stdout.write("example-user\\n");
}
`,
        { mode: 0o755 }
      );
      fs.chmodSync(fakeNpm, 0o755);

      const env = { ...process.env, PATH: `${fakeBinDir}:${process.env.PATH}` };
      delete env.NPM_TOKEN;

      // --publish --dry-run stops after whoami + dry-run preflight.
      const dryRun = spawnSync(
        "bash",
        [STAGE_SCRIPT, "--repo", repo, "--out", out, "--publish", "--dry-run"],
        { env, encoding: "utf8" }
      );
      assert.equal(dryRun.status, 0, `stderr:\n${dryRun.stderr}\nstdout:\n${dryRun.stdout}`);
      assert.equal(fs.existsSync(pwnFile), false);
      assert.equal(dryRun.stdout.includes(sampleToken), false);
      assert.equal(dryRun.stderr.includes(sampleToken), false);

      const dryCalls = fs
        .readFileSync(logFile, "utf8")
        .trim()
        .split("\n")
        .map((l) => JSON.parse(l));
      assert.equal(dryCalls.length, 3);
      assert.equal(dryCalls[0].args[0], "whoami");
      assert.equal(dryCalls[1].args[0], "publish");
      assert.ok(dryCalls[1].args.includes("--dry-run"));
      assert.equal(path.basename(dryCalls[1].args.at(-1)), `simple-memory-linux-x64-${PKG_VERSION}.tgz`);
      assert.equal(dryCalls[2].args[0], "publish");
      assert.ok(dryCalls[2].args.includes("--dry-run"));
      assert.equal(path.basename(dryCalls[2].args.at(-1)), `simple-memory-${PKG_VERSION}.tgz`);
      assert.equal(dryCalls[0].ucMode, 0o600);
      assert.equal(
        dryCalls[0].ucContent,
        `registry=https://registry.npmjs.org/\n//registry.npmjs.org/:_authToken=${sampleToken}\n`
      );
      assert.equal(fs.existsSync(dryCalls[0].ucPath), false, "temp .npmrc must be deleted on exit");

      // Full --publish runs preflight then publishes platform package before main package.
      fs.rmSync(logFile);
      const liveRun = spawnSync(
        "bash",
        [STAGE_SCRIPT, "--repo", repo, "--out", out, "--publish"],
        { env, encoding: "utf8" }
      );
      assert.equal(liveRun.status, 0, `stderr:\n${liveRun.stderr}\nstdout:\n${liveRun.stdout}`);
      assert.equal(liveRun.stdout.includes(sampleToken), false);
      assert.equal(liveRun.stderr.includes(sampleToken), false);

      const liveCalls = fs
        .readFileSync(logFile, "utf8")
        .trim()
        .split("\n")
        .map((l) => JSON.parse(l));
      assert.equal(liveCalls.length, 5);
      assert.equal(liveCalls[0].args[0], "whoami");
      assert.ok(liveCalls[1].args.includes("--dry-run"));
      assert.ok(liveCalls[2].args.includes("--dry-run"));
      assert.equal(liveCalls[3].args.includes("--dry-run"), false);
      assert.equal(path.basename(liveCalls[3].args.at(-1)), `simple-memory-linux-x64-${PKG_VERSION}.tgz`);
      assert.equal(liveCalls[4].args.includes("--dry-run"), false);
      assert.equal(path.basename(liveCalls[4].args.at(-1)), `simple-memory-${PKG_VERSION}.tgz`);
      assert.equal(fs.existsSync(liveCalls[0].ucPath), false);

      // Whitespace in token is refused.
      fs.writeFileSync(path.join(repo, ".env"), 'NPM_TOKEN="bad token with spaces"\n');
      const badTok = spawnSync(
        "bash",
        [STAGE_SCRIPT, "--repo", repo, "--out", out, "--publish", "--dry-run"],
        { env, encoding: "utf8" }
      );
      assert.notEqual(badTok.status, 0);
      assert.match(badTok.stderr, /whitespace/);

      // Preflight dry-run failure stops before any live publish and still deletes temp .npmrc.
      fs.writeFileSync(path.join(repo, ".env"), `NPM_TOKEN="${sampleToken}"\n`);
      fs.rmSync(logFile);
      fs.writeFileSync(
        fakeNpm,
        `#!${process.execPath}
const fs = require("node:fs");
const args = process.argv.slice(2);
const ucIdx = args.indexOf("--userconfig");
const ucPath = ucIdx !== -1 ? args[ucIdx + 1] : null;
fs.appendFileSync(${JSON.stringify(logFile)}, JSON.stringify({ args, ucPath }) + "\\n");
if (args[0] === "publish" && args.includes("--dry-run")) {
  process.stderr.write("simulated dry-run failure\\n");
  process.exit(1);
}
`,
        { mode: 0o755 }
      );
      fs.chmodSync(fakeNpm, 0o755);
      const failPreflight = spawnSync(
        "bash",
        [STAGE_SCRIPT, "--repo", repo, "--out", out, "--publish"],
        { env, encoding: "utf8" }
      );
      assert.notEqual(failPreflight.status, 0);
      const failCalls = fs
        .readFileSync(logFile, "utf8")
        .trim()
        .split("\n")
        .map((l) => JSON.parse(l));
      assert.equal(failCalls.length, 2);
      assert.equal(failCalls[0].args[0], "whoami");
      assert.ok(failCalls[1].args.includes("--dry-run"));
      assert.equal(fs.existsSync(failCalls[0].ucPath), false, "temp .npmrc must be deleted on error");
    });
  });
});
