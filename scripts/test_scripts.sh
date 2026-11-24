#!/usr/bin/env bash
# Tests of scripts/wait_free.sh, scripts/dead_code_scan.sh and scripts/install.sh.
# Needs bash, lsof, cargo and the release build (rust/target/release/sm).
# The dead-code case builds in rust/target (CARGO_TARGET_DIR when set), so the
# dependencies it checks are reused.
#
#   scripts/test_scripts.sh
#
# Prints one line per case and exits 1 when any case fails.
set -uo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/sm-test-scripts.XXXXXX")
tmp=$(cd "$tmp" && pwd -P)
trap 'rm -rf "${tmp:?}"' EXIT
failed=0
n=0

ok() { n=$((n + 1)); printf 'ok   %s\n' "$1"; }
bad() { n=$((n + 1)); failed=$((failed + 1)); printf 'FAIL %s: %s\n' "$1" "$2"; }

sm_bin=$repo/rust/target/release/sm
[[ -x $sm_bin ]] || { echo "no release build at $sm_bin: run cargo build --release --locked in rust/" >&2; exit 2; }

# wait_free.sh: a free folder passes at once, a short holder is waited for, a long one fails
wf=$repo/scripts/wait_free.sh
mkdir -p "$tmp/wf"
(cd / && bash "$wf" "$tmp/wf" 5 2>/dev/null)
rc=$?
[[ $rc == 0 ]] && ok "wait_free passes on a free folder" || bad "wait_free passes on a free folder" "rc=$rc"

(cd "$tmp/wf" && exec sleep 2) &
start=$(date +%s)
(cd / && bash "$wf" "$tmp/wf" 10 2>/dev/null)
rc=$?
took=$(($(date +%s) - start))
wait
if [[ $rc == 0 && $took -ge 1 ]]; then
    ok "wait_free waits for a process whose cwd is in the folder"
else
    bad "wait_free waits for a process whose cwd is in the folder" "rc=$rc took=${took}s"
fi

(cd "$tmp/wf" && exec sleep 30) &
holder=$!
(cd / && bash "$wf" "$tmp/wf" 1 2>/dev/null)
rc=$?
kill "$holder" 2>/dev/null
wait "$holder" 2>/dev/null
[[ $rc == 1 ]] && ok "wait_free exits 1 when a process holds the folder past the limit" \
    || bad "wait_free exits 1 when a process holds the folder past the limit" "rc=$rc"

# dead_code_scan.sh: one unused pub fn planted in a copy of rust/ is the only item it reports
dc=$repo/scripts/dead_code_scan.sh
if command -v cargo > /dev/null; then
    mkdir -p "$tmp/dc" "$tmp/dctmp"
    tar -C "$repo/rust" --exclude=./target -cf - . | tar -C "$tmp/dc" -xf -
    printf '\npub fn planted_unused() {}\n' >> "$tmp/dc/src/core/fold.rs"
    out=$(TMPDIR="$tmp/dctmp" CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$repo/rust/target} bash "$dc" "$tmp/dc" 2>&1)
    rc=$?
    if [[ $rc == 1 && $out == *"function \`planted_unused\` is never used"* && $out == *"dead code: 1" ]]; then
        ok "dead_code_scan exits 1 on one planted unused pub fn and names only it"
    else
        bad "dead_code_scan exits 1 on one planted unused pub fn and names only it" "rc=$rc out='$(tail -n 3 <<< "$out")'"
    fi
    # the scan removes its temp copy on every exit
    if compgen -G "$tmp/dctmp/sm-dead-code.*" > /dev/null; then
        bad "dead_code_scan leaves no temp folder in TMPDIR" "$(ls "$tmp/dctmp")"
    else
        ok "dead_code_scan leaves no temp folder in TMPDIR"
    fi
    # nothing hides an item from the scan: one unused pub fn under each of #[allow(dead_code)],
    # #[expect(dead_code)] and a crate-level #![allow(unused)], one in an inline `pub mod`, and one
    # that a `pub use` in lib.rs re-exports
    cp "$repo/rust/src/core/fold.rs" "$tmp/dc/src/core/fold.rs"
    printf '\n#[allow(dead_code)]\npub fn planted_allow() {}\n#[expect(dead_code)]\npub fn planted_expect() {}\npub fn planted_crate_allow() {}\npub fn planted_reexport() {}\n' >> "$tmp/dc/src/core/fold.rs"
    { printf '#![allow(unused)]\n'; cat "$repo/rust/src/lib.rs"
      printf 'pub mod planted_mod {\n    pub fn planted_inline() {}\n}\npub use core::fold::planted_reexport;\n'; } > "$tmp/dc/src/lib.rs"
    out=$(TMPDIR="$tmp/dctmp" CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$repo/rust/target} bash "$dc" "$tmp/dc" 2>&1)
    rc=$?
    hidden=ok
    for name in planted_allow planted_expect planted_crate_allow planted_inline planted_reexport; do
        [[ $out == *"function \`$name\` is never used"* ]] || hidden="$hidden missing:$name"
    done
    if [[ $rc == 1 && $hidden == ok && $out == *"dead code: 5" ]]; then
        ok "dead_code_scan names items under lint attributes, in an inline pub mod and behind a pub use"
    else
        bad "dead_code_scan names items under lint attributes, in an inline pub mod and behind a pub use" "rc=$rc $hidden out='$(tail -n 4 <<< "$out")'"
    fi
    # a warning that is not dead_code does not fail the scan: an unused variable in a live function
    cp "$repo/rust/src/lib.rs" "$tmp/dc/src/lib.rs"
    awk '{ print } index($0, "pub fn fold(text: Option<&str>) -> String {") == 1 { print "    let planted_var = 1;" }' \
        "$repo/rust/src/core/fold.rs" > "$tmp/dc/src/core/fold.rs"
    out=$(TMPDIR="$tmp/dctmp" CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$repo/rust/target} bash "$dc" "$tmp/dc" 2>&1)
    rc=$?
    [[ $rc == 0 && $out == "dead code: 0" ]] && ok "dead_code_scan exits 0 when the only warning is not dead_code" \
        || bad "dead_code_scan exits 0 when the only warning is not dead_code" "rc=$rc out='$(tail -n 3 <<< "$out")'"
    cp "$repo/rust/src/core/fold.rs" "$tmp/dc/src/core/fold.rs"
    # a copy that does not compile is an error, not a clean scan
    printf '\npub fn broken( {}\n' >> "$tmp/dc/src/core/fold.rs"
    CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$repo/rust/target} bash "$dc" "$tmp/dc" > /dev/null 2>&1
    rc=$?
    [[ $rc == 2 ]] && ok "dead_code_scan exits 2 when the crate does not compile" \
        || bad "dead_code_scan exits 2 when the crate does not compile" "rc=$rc"
    # a lib.rs with no `pub mod` to make pub(crate) would scan nothing, so it is an error
    cp "$repo/rust/src/core/fold.rs" "$tmp/dc/src/core/fold.rs"
    sed 's/^pub mod /pub(crate) mod /' "$repo/rust/src/lib.rs" > "$tmp/dc/src/lib.rs"
    CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$repo/rust/target} bash "$dc" "$tmp/dc" > /dev/null 2>&1
    rc=$?
    [[ $rc == 2 ]] && ok "dead_code_scan exits 2 on a lib.rs with no pub mod" \
        || bad "dead_code_scan exits 2 on a lib.rs with no pub mod" "rc=$rc"
else
    bad "dead_code_scan cases" "no cargo on PATH"
fi

# install.sh with a fake cargo (reuses the already-built release binary at $sm_bin)
mkdir -p "$tmp/inst-bin" "$tmp/inst-dest"
cat > "$tmp/inst-bin/cargo" <<EOF
#!/bin/sh
echo "\$*" >> "$tmp/inst-cargo.log"
EOF
chmod 755 "$tmp/inst-bin/cargo"
inst_out=$(env -i PATH="$tmp/inst-bin:/usr/bin:/bin" HOME="$tmp" SM_INSTALL_DIR="$tmp/inst-dest" bash "$repo/scripts/install.sh" 2>&1)
rc=$?
want_inst="build --release --locked --manifest-path $repo/rust/Cargo.toml"
if [[ $rc == 0 && -x "$tmp/inst-dest/sm" && -L "$tmp/inst-dest/simple-memory" && "$(cat "$tmp/inst-cargo.log")" == "$want_inst" && "$inst_out" == *"Add $tmp/inst-dest to your PATH:"* ]]; then
    ok "install.sh builds, atomically installs binary + symlink, prints version and PATH hint"
else
    bad "install.sh builds, atomically installs binary + symlink, prints version and PATH hint" "rc=$rc out='$inst_out'"
fi
# the build starts inside rust/, so rust/rust-toolchain.toml applies (rustup reads it from the working directory only), and a relative CARGO_TARGET_DIR still names the folder it was given in
mkdir -p "$tmp/inst-bin2" "$tmp/inst-dest2" "$tmp/inst-cwd/rel-target/release"
cat > "$tmp/inst-bin2/cargo" <<EOF
#!/bin/sh
{ pwd -P; echo "\$CARGO_TARGET_DIR"; } > "$tmp/inst-cargo2.log"
EOF
chmod 755 "$tmp/inst-bin2/cargo"
cp "$sm_bin" "$tmp/inst-cwd/rel-target/release/sm"
chmod 755 "$tmp/inst-cwd/rel-target/release/sm"
inst_cwd=$(cd "$tmp/inst-cwd" && env -i PATH="$tmp/inst-bin2:/usr/bin:/bin" HOME="$tmp" CARGO_TARGET_DIR=rel-target SM_INSTALL_DIR="$tmp/inst-dest2" bash "$repo/scripts/install.sh" 2>&1)
rc=$?
want_cwd=$(cd "$repo/rust" && pwd -P)
want_target=$(cd "$tmp/inst-cwd" && pwd -P)/rel-target
if [[ $rc == 0 && "$(sed -n 1p "$tmp/inst-cargo2.log")" == "$want_cwd" && "$(sed -n 2p "$tmp/inst-cargo2.log")" == "$want_target" ]]; then
    ok "install.sh builds from inside rust/ and keeps a relative CARGO_TARGET_DIR"
else
    bad "install.sh builds from inside rust/ and keeps a relative CARGO_TARGET_DIR" "rc=$rc log='$(cat "$tmp/inst-cargo2.log" 2>/dev/null)' out='$inst_cwd'"
fi
inst_on_path=$(env -i PATH="$tmp/inst-dest:$tmp/inst-bin:/usr/bin:/bin" HOME="$tmp" SM_INSTALL_DIR="$tmp/inst-dest" bash "$repo/scripts/install.sh" 2>&1)
rc2=$?
if [[ $rc2 == 0 && "$inst_on_path" != *"Add "* ]]; then
    ok "install.sh omits PATH hint when SM_INSTALL_DIR is already on PATH"
else
    bad "install.sh omits PATH hint when SM_INSTALL_DIR is already on PATH" "rc=$rc2 out='$inst_on_path'"
fi
mkdir -p "$tmp/custom-target/release" "$tmp/inst-custom"
cp "$sm_bin" "$tmp/custom-target/release/sm"
chmod 755 "$tmp/custom-target/release/sm"
inst_ct=$(env -i PATH="$tmp/inst-custom:$tmp/inst-bin:/usr/bin:/bin" HOME="$tmp" CARGO_TARGET_DIR="$tmp/custom-target" SM_INSTALL_DIR="$tmp/inst-custom" bash "$repo/scripts/install.sh" 2>&1)
rc3=$?
if [[ $rc3 == 0 && -x "$tmp/inst-custom/sm" && -L "$tmp/inst-custom/simple-memory" && "$inst_ct" == *"sm "* ]]; then
    ok "install.sh installs from CARGO_TARGET_DIR when set"
else
    bad "install.sh installs from CARGO_TARGET_DIR when set" "rc=$rc3 out='$inst_ct'"
fi

# skills/memory-maintenance/scripts/test_audit.py
if command -v python3 > /dev/null; then
    audit_out=$(python3 -B -m unittest discover -s "$repo/skills/memory-maintenance/scripts" -p 'test_*.py' 2>&1)
    rc_audit=$?
    if [[ $rc_audit == 0 ]]; then
        ok "memory-maintenance audit unit tests pass"
    else
        bad "memory-maintenance audit unit tests pass" "rc=$rc_audit out='$(tail -n 5 <<< "$audit_out")'"
    fi
else
    bad "memory-maintenance audit unit tests" "no python3 on PATH"
fi

echo "$((n - failed)) passed, $failed failed"
[[ $failed == 0 ]]
