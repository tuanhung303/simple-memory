#!/usr/bin/env node
"use strict";

const { spawn } = require("node:child_process");
const os = require("node:os");
const { resolveBinary } = require("../lib/platform");

const FORWARDED_SIGNALS = ["SIGINT", "SIGTERM", "SIGHUP"];

const resolved = resolveBinary();
if (!resolved.ok) {
  process.stderr.write(
    `simple-memory: ${resolved.reason} (${process.platform}-${process.arch}).\n` +
      "Build from source with ./scripts/install.sh or set SM_BINARY_PATH to an existing sm binary.\n"
  );
  process.exit(1);
}

const child = spawn(resolved.path, process.argv.slice(2), { stdio: "inherit" });

const handlers = new Map();
for (const sig of FORWARDED_SIGNALS) {
  const handler = () => {
    if (child.exitCode === null && child.signalCode === null) {
      try {
        child.kill(sig);
      } catch {
        // Child exited between check and signal delivery.
      }
    }
  };
  handlers.set(sig, handler);
  process.on(sig, handler);
}

function detachSignalHandlers() {
  for (const [sig, handler] of handlers) {
    process.removeListener(sig, handler);
  }
}

child.on("error", (err) => {
  detachSignalHandlers();
  process.stderr.write(
    `simple-memory: failed to execute ${resolved.path}: ${err && err.message ? err.message : String(err)}\n`
  );
  process.exit(1);
});

child.on("exit", (code, signal) => {
  detachSignalHandlers();
  if (signal) {
    process.removeAllListeners(signal);
    try {
      process.kill(process.pid, signal);
    } catch {
      // Fall through to 128 + signal exit code below.
    }
    // Node ignores SIGPIPE by default, so self-kill returns without terminating.
    const sigNum = os.constants.signals[signal] || 1;
    process.exit(128 + sigNum);
  }
  process.exit(code === null ? 1 : code);
});
