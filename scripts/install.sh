#!/usr/bin/env bash
# Build the release `sm` binary from rust/ and atomically install `sm` (plus a `simple-memory`
# symlink) into SM_INSTALL_DIR (default: ~/.local/bin).
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
dest_dir=${SM_INSTALL_DIR:-$HOME/.local/bin}
mkdir -p "$dest_dir"
dest_dir=$(cd "$dest_dir" && pwd -P)

target_dir=${CARGO_TARGET_DIR:-$repo/rust/target}
case $target_dir in /*) ;; *) target_dir=$PWD/$target_dir ;; esac
# Build from inside rust/: rustup reads rust/rust-toolchain.toml from the working directory only.
(cd "$repo/rust" && CARGO_TARGET_DIR=$target_dir cargo build --release --locked --manifest-path "$repo/rust/Cargo.toml")

built="$target_dir/release/sm"
[[ -x $built ]] || { echo "install.sh: expected executable at $built" >&2; exit 1; }

tmp_bin=$(mktemp "$dest_dir/.sm.install.XXXXXX")
tmp_link="$dest_dir/.simple-memory.install.$$"
trap 'rm -f "${tmp_bin:?}" "${tmp_link:-}"' EXIT
cp "$built" "$tmp_bin"
chmod 755 "$tmp_bin"
mv -f "$tmp_bin" "$dest_dir/sm"
rm -f "$tmp_link"
ln -s "sm" "$tmp_link"
mv -f "$tmp_link" "$dest_dir/simple-memory"

"$dest_dir/sm" --version
case ":${PATH:-}:" in
    *":$dest_dir:"*) ;;
    *)
        printf 'Add %s to your PATH:\n  export PATH="%s:$PATH"\n' "$dest_dir" "$dest_dir"
        ;;
esac
