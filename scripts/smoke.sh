#!/usr/bin/env bash
# Smoke test for a stranger's install of the Rust binary: run the main commands in a fresh temp
# HOME and SIMPLE_MEMORY_HOME, with no ~/.claude and no `claude` on PATH.
#
#   scripts/smoke.sh [SM]
#
# SM is the binary to test (default: rust/target/release/sm in this checkout; build it first with
# `cargo build --release --locked` in rust/). Exit 0 when every step passes; on a failure the temp
# folder is kept for a look.
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
bin=${1:-$repo/rust/target/release/sm}
[[ -x $bin ]] || { echo "no sm binary at $bin" >&2; exit 2; }
bin=$(cd "$(dirname "$bin")" && pwd -P)/$(basename "$bin")
tmp=$(mktemp -d "${TMPDIR:-/tmp}/sm-smoke.XXXXXX")
tmp=$(cd "$tmp" && pwd -P)

step() { printf '== %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; printf 'kept for a look: %s\n' "$tmp" >&2; exit 1; }

# `sm` and `simple-memory` link to the binary, as the README install does
mkdir -p "$tmp/bin" "$tmp/home/work"
ln -s "$bin" "$tmp/bin/sm"
ln -s "$bin" "$tmp/bin/simple-memory"
export HOME="$tmp/home"
export SIMPLE_MEMORY_HOME="$HOME/.simple-memory"
export PATH="$tmp/bin:/usr/bin:/bin"
unset SM_PROMPT_HOOK SM_PARITY_NOW SM_PARITY_SEED SM_PARITY_TRACE
cd "$HOME/work"
command -v claude >/dev/null && fail "claude is on PATH"
[[ -e $HOME/.claude ]] && fail "$HOME/.claude exists"

step "sm --version, simple-memory --version"
sm --version | grep -qE '^sm [0-9]+\.[0-9]+\.[0-9]+$' || fail "sm --version printed: $(sm --version)"
[[ $(simple-memory --version) == "$(sm --version)" ]] || fail "simple-memory --version differs"

step "sm init (twice: the second run keeps the config)"
sm init
[[ -f $SIMPLE_MEMORY_HOME/config.toml && -f $SIMPLE_MEMORY_HOME/memory.db ]] || fail "init left no config or database"
cfg_sum=$(cksum < "$SIMPLE_MEMORY_HOME/config.toml")
sm init | grep -q "kept config" || fail "second init did not keep the config"
[[ $(cksum < "$SIMPLE_MEMORY_HOME/config.toml") == "$cfg_sum" ]] || fail "second init changed the config"

step "sm put, query, show"
id=$(sm put --repo demo --tag decided --evidence "smoke test" "Release builds run on the arm64 runners only" | grep -oE 'm_[0-9A-Z]{26}' | head -1)
[[ -n $id ]] || fail "put printed no id"
echo "id $id"
sm query "arm64 runners" --repo demo | grep -q "$id" || fail "query did not find $id"
sm show "$id" | grep -q "arm64 runners only" || fail "show did not print the body"

step "sm doctor"
sm doctor | grep -q "^integrity=ok" || fail "doctor integrity not ok"

step "sm import catch-up"
sm import catch-up || fail "catch-up exit $?"
rows() { [[ -f $SIMPLE_MEMORY_HOME/catchup.log ]] && wc -l < "$SIMPLE_MEMORY_HOME/catchup.log" | tr -d ' ' || echo 0; }
[[ $(rows) == 1 ]] || fail "catch-up wrote $(rows) log rows"
tail -1 "$SIMPLE_MEMORY_HOME/catchup.log" | grep -q '"ok": true' || fail "catch-up row not ok"

step "sm hook prompt with no claude on PATH: local keywords stand in"
payload='{"prompt": "what did we decide about the arm64 runners for release builds", "session_id": "00000000-0000-4000-8000-000000000001", "cwd": "'"$HOME/work"'"}'
out=$(printf '%s' "$payload" | sm hook prompt) || fail "hook exit $?"
# a one-item store gives every word idf 0, under the local floor, so nothing is injected
[[ -z $out ]] || fail "hook injected: $out"
tail -1 "$SIMPLE_MEMORY_HOME/hook.log" | grep -q '"path": "local".*"fallback": "error"' \
  || fail "hook.log has no local fallback row"
# the hook starts a detached catch-up (no stamp): wait for its row, at most 30 s
for _ in $(seq 60); do [[ $(rows) == 2 ]] && break; sleep 0.5; done
[[ $(rows) == 2 ]] || fail "the hook's background catch-up wrote no row"
tail -1 "$SIMPLE_MEMORY_HOME/catchup.log" | grep -q '"ok": true' || fail "background catch-up row not ok"
# the row comes before the child exits, and the child keeps the cwd it got from this shell
# ($HOME/work) until it exits: wait until no process holds a file under $tmp, at most 30 s, so
# backup and the cleanup never race it (scripts/wait_free.sh; scripts/test_scripts.sh tests it)
cd /
bash "$repo/scripts/wait_free.sh" "$tmp" 30 || fail "the hook's catch-up still holds $tmp (wait_free exit $?)"
cd "$HOME/work"

step "sm backup"
sm backup || fail "backup exit $?"
ls "$SIMPLE_MEMORY_HOME"/backups/* >/dev/null 2>&1 || fail "backup wrote no file"

step "PASS"
cd /
rm -rf -- "${tmp:?}"
