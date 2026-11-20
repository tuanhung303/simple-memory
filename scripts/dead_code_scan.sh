#!/usr/bin/env bash
# Fail when the default build of the crate compiles an item that the binary never reaches.
# A `pub` item in a library never warns as dead code, so the scan checks a copy in which the
# `pub mod` and `pub use` lines of src/lib.rs are `pub(crate)` and only `cli::main` (what
# src/main.rs calls) stays public. Then rustc's dead_code lint lists every item only tests or
# the `parity` feature use; such an item belongs under `#[cfg(test)]` or
# `#[cfg(any(test, feature = "parity"))]`. The lint runs as `--force-warn dead_code` on the
# crate only, so `#[allow(dead_code)]`, `#[expect(dead_code)]` and `#![allow(unused)]` in the
# source cannot hide an item.
#
#   scripts/dead_code_scan.sh RUST_DIR
#
# RUST_DIR holds Cargo.toml and src/lib.rs (rust/ in this repo). Needs bash, tar, python3 and
# cargo. CARGO_TARGET_DIR, when set, is the build folder (a reused one makes the scan fast);
# else the scan builds in its temp folder. Exit 0 with no dead code, 1 with dead code (one line
# per item), 2 when the copy cannot be made, the rewrite of lib.rs finds no `pub mod`, or the
# check fails to compile.
set -uo pipefail

src=${1:?usage: scripts/dead_code_scan.sh RUST_DIR}
tmp=$(mktemp -d "${TMPDIR:-/tmp}/sm-dead-code.XXXXXX") || exit 2
trap 'rm -rf "${tmp:?}"' EXIT

mkdir "$tmp/crate"
tar -C "$src" --exclude=./target -cf - . | tar -C "$tmp/crate" -xf - || { echo "cannot copy $src" >&2; exit 2; }

python3 -I - "$tmp/crate/src/lib.rs" <<'EOF' || exit 2
import re, sys
p = sys.argv[1]
s, n = re.subn(r"^pub mod (\w+)(?=\s*[;{])", r"pub(crate) mod \1", open(p).read(), flags=re.M)
if n == 0:
    sys.exit(f"{p}: no `pub mod` line to make pub(crate)")
s = re.sub(r"^pub use ", "pub(crate) use ", s, flags=re.M)
open(p, "w").write(s + "\npub use cli::main;\n")
EOF

(cd "$tmp/crate" && CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$tmp/target} \
    cargo rustc --lib --locked --profile check --message-format json \
    -- --force-warn dead_code > "$tmp/check.json" 2> "$tmp/stderr")
rc=$?
if [[ $rc != 0 ]]; then
    cat "$tmp/stderr" >&2
    echo "cargo check failed (exit $rc)" >&2
    exit 2
fi

python3 -I - "$tmp/check.json" <<'EOF'
import json, sys
dead = []
for line in open(sys.argv[1]):
    if not line.startswith("{"):
        continue
    m = json.loads(line)
    msg = m.get("message") or {}
    if m.get("reason") != "compiler-message" or (msg.get("code") or {}).get("code") != "dead_code":
        continue
    span = next((s for s in msg["spans"] if s["is_primary"]), msg["spans"][0])
    dead.append(f"{span['file_name']}:{span['line_start']}: {msg['message']}")
for d in dead:
    print(d)
print(f"dead code: {len(dead)}")
sys.exit(1 if dead else 0)
EOF
