#!/usr/bin/env bash
# Stage, pack, verify, and publish npm packages for simple-memory without writing
# build artifacts or credentials into tracked directories.
set -euo pipefail

usage() {
    cat >&2 <<'EOF'
Usage: scripts/stage_npm.sh [options]
  --repo DIR          Repository root (default: parent of scripts/)
  --out DIR           Staging output directory (default: <repo>/npm/.stage)
  --bin TARGET=PATH   Stage binary for TARGET (darwin-arm64, darwin-x64, linux-x64, linux-arm64)
  --pack              Pack staged packages into <out>/tarballs/
  --verify-pack       Verify all tarballs in <out>/tarballs/
  --publish           Verify, preflight, and publish tarballs in <out>/tarballs/
  --dry-run           With --publish, stop after whoami + npm publish --dry-run preflight
  -h, --help          Show this help
EOF
}

script_dir=$(cd "$(dirname "$0")" && pwd -P)
repo=$(cd "$script_dir/.." && pwd -P)
out=""
do_pack=0
do_verify=0
do_publish=0
do_dry_run=0
bin_targets=()
bin_paths=()
npmrc_dir=""

cleanup() {
    if [[ -n "${npmrc_dir:-}" ]]; then
        rm -rf "$npmrc_dir"
        npmrc_dir=""
    fi
}
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM

while [[ $# -gt 0 ]]; do
    case "$1" in
        --repo)
            [[ $# -ge 2 ]] || { echo "stage_npm.sh: --repo requires a value" >&2; exit 2; }
            repo=$2
            shift 2
            ;;
        --out)
            [[ $# -ge 2 ]] || { echo "stage_npm.sh: --out requires a value" >&2; exit 2; }
            out=$2
            shift 2
            ;;
        --bin)
            [[ $# -ge 2 ]] || { echo "stage_npm.sh: --bin requires TARGET=PATH" >&2; exit 2; }
            spec=$2
            if [[ "$spec" != *=* ]]; then
                echo "stage_npm.sh: --bin must have form TARGET=PATH, got: $spec" >&2
                exit 2
            fi
            target=${spec%%=*}
            bin_path=${spec#*=}
            case "$target" in
                darwin-arm64|darwin-x64|linux-x64|linux-arm64) ;;
                *)
                    echo "stage_npm.sh: unsupported target: $target" >&2
                    exit 2
                    ;;
            esac
            if [[ -z "$bin_path" ]]; then
                echo "stage_npm.sh: empty binary path for target: $target" >&2
                exit 2
            fi
            if [[ ${#bin_targets[@]} -gt 0 ]]; then
                for existing in "${bin_targets[@]}"; do
                    if [[ "$existing" == "$target" ]]; then
                        echo "stage_npm.sh: duplicate --bin target: $target" >&2
                        exit 2
                    fi
                done
            fi
            bin_targets+=("$target")
            bin_paths+=("$bin_path")
            shift 2
            ;;
        --pack)
            do_pack=1
            shift
            ;;
        --verify-pack)
            do_verify=1
            shift
            ;;
        --publish)
            do_publish=1
            shift
            ;;
        --dry-run)
            do_dry_run=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "stage_npm.sh: unknown option: $1" >&2
            usage
            exit 2
            ;;
    esac
done

if [[ ! -d "$repo" ]]; then
    echo "stage_npm.sh: repository directory not found: $repo" >&2
    exit 1
fi
repo=$(cd "$repo" && pwd -P)

if [[ -z "$out" ]]; then
    out="$repo/npm/.stage"
fi

if [[ ${#bin_targets[@]} -eq 0 && $do_verify -eq 0 && $do_publish -eq 0 ]]; then
    echo "stage_npm.sh: specify at least one --bin TARGET=PATH, --verify-pack, or --publish" >&2
    usage
    exit 2
fi

if [[ $do_pack -eq 1 && ${#bin_targets[@]} -eq 0 ]]; then
    echo "stage_npm.sh: --pack requires at least one --bin TARGET=PATH" >&2
    exit 2
fi

if [[ $do_publish -eq 1 && ${#bin_targets[@]} -gt 0 && $do_pack -eq 0 ]]; then
    echo "stage_npm.sh: --publish with --bin requires --pack" >&2
    exit 2
fi

if [[ $do_dry_run -eq 1 && $do_publish -eq 0 ]]; then
    echo "stage_npm.sh: --dry-run requires --publish" >&2
    exit 2
fi

# Guard --out against destructive paths before any cleanup.
out=$(python3 - "$out" "$repo" "${HOME:-}" <<'PY'
import os
import sys

raw_out, raw_repo, raw_home = sys.argv[1], sys.argv[2], sys.argv[3]
if not raw_out or raw_out.strip() == "":
    sys.exit("stage_npm.sh: --out must not be empty")

canon_out = os.path.realpath(os.path.abspath(raw_out))
canon_repo = os.path.realpath(os.path.abspath(raw_repo))
forbidden = {"/", "/tmp", "/private/tmp", "/var", "/Users", "/home", canon_repo}
if raw_home:
    forbidden.add(os.path.realpath(os.path.abspath(raw_home)))

if canon_out in forbidden:
    sys.exit(f"stage_npm.sh: refusing unsafe --out directory: {canon_out}")

print(canon_out)
PY
)

validate_binary() {
    local target=$1
    local bin_path=$2
    local file_desc pattern

    if [[ ! -f "$bin_path" ]]; then
        echo "stage_npm.sh: binary not found for $target: $bin_path" >&2
        exit 1
    fi

    file_desc=$(file -b "$bin_path")
    if [[ "$file_desc" == *"universal binary"* ]]; then
        echo "stage_npm.sh: expected single-arch binary for $target, got universal binary: $file_desc" >&2
        exit 1
    fi

    case "$target" in
        darwin-arm64) pattern='Mach-O.*arm64' ;;
        darwin-x64)   pattern='Mach-O.*x86_64' ;;
        linux-x64)    pattern='ELF.*x86-64' ;;
        linux-arm64)  pattern='ELF.*aarch64' ;;
    esac

    if ! printf '%s\n' "$file_desc" | grep -Eq "$pattern"; then
        echo "stage_npm.sh: binary arch/OS mismatch for $target ($bin_path): $file_desc" >&2
        exit 1
    fi

    if [[ "$target" == darwin-* && "$(uname -s)" == "Darwin" ]]; then
        local otool_out
        if ! otool_out=$(otool -L "$bin_path"); then
            echo "stage_npm.sh: otool -L failed for $bin_path" >&2
            exit 1
        fi
        python3 - "$bin_path" "$otool_out" <<'PY'
import sys

bin_path, text = sys.argv[1], sys.argv[2]
lines = text.splitlines()
bad = []
for line in lines[1:]:
    stripped = line.strip()
    if not stripped:
        continue
    lib = stripped.split()[0]
    if not (lib.startswith("/usr/lib/") or lib.startswith("/System/Library/")):
        bad.append(lib)

if bad:
    joined = ", ".join(bad)
    sys.exit(f"stage_npm.sh: non-system dylib dependency in {bin_path}: {joined}")
PY
    fi
}

stage_packages() {
    local version
    version=$(python3 - "$repo" "${bin_targets[@]}" <<'PY'
import json
import os
import re
import sys

repo = sys.argv[1]
targets = sys.argv[2:]

cargo_path = os.path.join(repo, "rust", "Cargo.toml")
if not os.path.isfile(cargo_path):
    sys.exit(f"stage_npm.sh: missing {cargo_path}")

in_pkg = False
cargo_ver = None
with open(cargo_path, encoding="utf-8") as f:
    for raw in f:
        line = raw.strip()
        if line.startswith("["):
            in_pkg = (line == "[package]")
            continue
        if in_pkg:
            m = re.match(r'^version\s*=\s*"([^"]+)"', line)
            if m:
                cargo_ver = m.group(1)
                break

if not cargo_ver:
    sys.exit(f"stage_npm.sh: could not read [package] version from {cargo_path}")

main_pkg_path = os.path.join(repo, "npm", "simple-memory", "package.json")
with open(main_pkg_path, encoding="utf-8") as f:
    main_pkg = json.load(f)

if main_pkg.get("name") != "simple-memory":
    sys.exit(f"stage_npm.sh: unexpected main package name: {main_pkg.get('name')}")
if main_pkg.get("version") != cargo_ver:
    sys.exit(
        f"stage_npm.sh: version mismatch: rust/Cargo.toml={cargo_ver} vs "
        f"npm/simple-memory/package.json={main_pkg.get('version')}"
    )

for t in targets:
    plat_path = os.path.join(repo, "npm", "platforms", t, "package.json")
    if not os.path.isfile(plat_path):
        sys.exit(f"stage_npm.sh: missing platform manifest: {plat_path}")
    with open(plat_path, encoding="utf-8") as f:
        plat_pkg = json.load(f)
    expected_name = f"simple-memory-{t}"
    if plat_pkg.get("name") != expected_name:
        sys.exit(
            f"stage_npm.sh: platform package name mismatch in {plat_path}: "
            f"expected {expected_name}, got {plat_pkg.get('name')}"
        )
    if plat_pkg.get("version") != cargo_ver:
        sys.exit(
            f"stage_npm.sh: version mismatch: rust/Cargo.toml={cargo_ver} vs "
            f"{plat_path}={plat_pkg.get('version')}"
        )

print(cargo_ver)
PY
)

    local i
    for ((i = 0; i < ${#bin_targets[@]}; i++)); do
        validate_binary "${bin_targets[$i]}" "${bin_paths[$i]}"
    done

    for required_file in \
        "$repo/LICENSE" \
        "$repo/README.md" \
        "$repo/npm/simple-memory/bin/sm.js" \
        "$repo/npm/simple-memory/lib/platform.js" \
        "$repo/npm/simple-memory/scripts/postinstall.js"
    do
        if [[ ! -f "$required_file" ]]; then
            echo "stage_npm.sh: missing required file: $required_file" >&2
            exit 1
        fi
    done

    rm -rf "$out/packages" "$out/tarballs"
    mkdir -p "$out/packages" "$out/tarballs"

    for ((i = 0; i < ${#bin_targets[@]}; i++)); do
        local t="${bin_targets[$i]}"
        local b="${bin_paths[$i]}"
        local pdir="$out/packages/simple-memory-$t"
        mkdir -p "$pdir/bin"
        cp "$repo/npm/platforms/$t/package.json" "$pdir/package.json"
        cp "$b" "$pdir/bin/sm"
        chmod 0755 "$pdir/bin/sm"
        cp "$repo/LICENSE" "$pdir/LICENSE"
        cat > "$pdir/README.md" <<EOF
# simple-memory-$t

Prebuilt \`sm\` binary for \`$t\`, installed automatically as an optional dependency of [\`simple-memory\`](https://www.npmjs.com/package/simple-memory).

See https://github.com/tuanhung303/simple-memory for full documentation.
EOF
    done

    local main_dir="$out/packages/simple-memory"
    mkdir -p "$main_dir/bin" "$main_dir/lib" "$main_dir/scripts"
    cp "$repo/npm/simple-memory/bin/sm.js" "$main_dir/bin/sm.js"
    chmod 0755 "$main_dir/bin/sm.js"
    cp "$repo/npm/simple-memory/lib/platform.js" "$main_dir/lib/platform.js"
    cp "$repo/npm/simple-memory/scripts/postinstall.js" "$main_dir/scripts/postinstall.js"
    cp "$repo/LICENSE" "$main_dir/LICENSE"
    cp "$repo/README.md" "$main_dir/README.md"

    python3 - "$repo/npm/simple-memory/package.json" "$main_dir/package.json" "$version" "${bin_targets[@]}" <<'PY'
import json
import sys

src, dst, version = sys.argv[1], sys.argv[2], sys.argv[3]
targets = sorted(sys.argv[4:])
with open(src, encoding="utf-8") as f:
    pkg = json.load(f)

pkg["optionalDependencies"] = {f"simple-memory-{t}": version for t in targets}
with open(dst, "w", encoding="utf-8") as f:
    json.dump(pkg, f, indent=2)
    f.write("\n")
PY

    echo "staged simple-memory@$version (${bin_targets[*]}) into $out/packages"

    if [[ $do_pack -eq 1 ]]; then
        for ((i = 0; i < ${#bin_targets[@]}; i++)); do
            local t="${bin_targets[$i]}"
            npm pack --pack-destination "$out/tarballs" "$out/packages/simple-memory-$t"
        done
        npm pack --pack-destination "$out/tarballs" "$main_dir"
    fi
}

verified_packs=0

verify_packs() {
    if [[ $verified_packs -eq 1 ]]; then
        return 0
    fi
    local tarball_dir="$out/tarballs"
    if [[ ! -d "$tarball_dir" ]]; then
        echo "stage_npm.sh: tarballs directory not found: $tarball_dir" >&2
        exit 1
    fi

    python3 - "$tarball_dir" <<'PY'
import glob
import json
import os
import posixpath
import sys
import tarfile

tarball_dir = sys.argv[1]
tgz_files = sorted(glob.glob(os.path.join(tarball_dir, "*.tgz")))
if not tgz_files:
    sys.exit(f"stage_npm.sh: no .tgz files found in {tarball_dir}")

PLATFORM_PKGS = {
    "simple-memory-darwin-arm64",
    "simple-memory-darwin-x64",
    "simple-memory-linux-x64",
    "simple-memory-linux-arm64",
}

MAIN_REQUIRED = {
    "package/package.json",
    "package/bin/sm.js",
    "package/lib/platform.js",
    "package/scripts/postinstall.js",
    "package/LICENSE",
    "package/README.md",
}

PLATFORM_REQUIRED = {
    "package/package.json",
    "package/bin/sm",
    "package/LICENSE",
    "package/README.md",
}

FORBIDDEN_PARTS = {"node_modules", "vendor", ".git"}

packages_seen = {}
main_opt_deps = None

for tgz in tgz_files:
    base = os.path.basename(tgz)
    with tarfile.open(tgz, "r:gz") as tf:
        file_members = {}
        for m in tf.getmembers():
            raw_name = m.name
            if raw_name.startswith("/") or ".." in raw_name.split("/"):
                sys.exit(f"stage_npm.sh: unsafe member path in {base}: {raw_name}")
            if m.issym() or m.islnk():
                sys.exit(f"stage_npm.sh: forbidden link entry in {base}: {raw_name}")

            norm = posixpath.normpath(raw_name)
            parts = norm.split("/")
            for part in parts:
                if part in FORBIDDEN_PARTS or part == ".env" or part.startswith(".env."):
                    sys.exit(f"stage_npm.sh: forbidden member in {base}: {raw_name}")

            if m.isdir():
                continue
            if not m.isfile():
                sys.exit(f"stage_npm.sh: non-regular file in {base}: {raw_name}")

            if norm in file_members:
                sys.exit(f"stage_npm.sh: duplicate member in {base}: {norm}")
            file_members[norm] = m

            data = tf.extractfile(m).read()
            # Reproducible-build check: ensure no host home directory paths leaked into artifacts.
            needles = [b"/Users/", b"/home/"]
            home_dir = os.environ.get("HOME", "")
            if home_dir:
                real_home = os.path.realpath(home_dir)
                if real_home and real_home != "/":
                    home_bytes = real_home.encode("utf-8", errors="ignore")
                    if home_bytes not in needles:
                        needles.append(home_bytes)
            for needle in needles:
                hits = data.count(needle)
                if hits > 0:
                    sys.exit(
                        f"stage_npm.sh: hardcoded host path {needle.decode('utf-8', 'replace')!r} "
                        f"found in {base} member {norm} ({hits} hit(s))"
                    )

        if "package/package.json" not in file_members:
            sys.exit(f"stage_npm.sh: missing package/package.json in {base}")
        pkg = json.loads(tf.extractfile(file_members["package/package.json"]).read().decode("utf-8"))
        name = pkg.get("name")
        version = pkg.get("version")
        if not name or not version:
            sys.exit(f"stage_npm.sh: invalid package.json metadata in {base}")

        actual_files = set(file_members.keys())
        if name == "simple-memory":
            if actual_files != MAIN_REQUIRED:
                missing = sorted(MAIN_REQUIRED - actual_files)
                extra = sorted(actual_files - MAIN_REQUIRED)
                sys.exit(
                    f"stage_npm.sh: unexpected file set in {base}: missing={missing}, extra={extra}"
                )
            bin_mode = file_members["package/bin/sm.js"].mode & 0o777
            if bin_mode != 0o755:
                sys.exit(
                    f"stage_npm.sh: expected mode 0755 for package/bin/sm.js in {base}, got {oct(bin_mode)}"
                )
            main_opt_deps = pkg.get("optionalDependencies") or {}
        elif name in PLATFORM_PKGS:
            if actual_files != PLATFORM_REQUIRED:
                missing = sorted(PLATFORM_REQUIRED - actual_files)
                extra = sorted(actual_files - PLATFORM_REQUIRED)
                sys.exit(
                    f"stage_npm.sh: unexpected file set in {base}: missing={missing}, extra={extra}"
                )
            bin_mode = file_members["package/bin/sm"].mode & 0o777
            if bin_mode != 0o755:
                sys.exit(
                    f"stage_npm.sh: expected mode 0755 for package/bin/sm in {base}, got {oct(bin_mode)}"
                )
        else:
            sys.exit(f"stage_npm.sh: unexpected package name '{name}' in {base}")

        if name in packages_seen:
            sys.exit(f"stage_npm.sh: duplicate tarball for package {name} in {tarball_dir}")
        packages_seen[name] = version

if "simple-memory" not in packages_seen:
    sys.exit(f"stage_npm.sh: simple-memory tarball missing from {tarball_dir}")

main_version = packages_seen["simple-memory"]
if not main_opt_deps:
    sys.exit(f"stage_npm.sh: simple-memory optionalDependencies is empty in {tarball_dir}")
for dep_name, dep_ver in main_opt_deps.items():
    if dep_name not in packages_seen:
        sys.exit(
            f"stage_npm.sh: simple-memory optionalDependency {dep_name} has no matching tarball in {tarball_dir}"
        )
    if packages_seen[dep_name] != dep_ver or dep_ver != main_version:
        sys.exit(
            f"stage_npm.sh: version mismatch for optionalDependency {dep_name}: "
            f"declared={dep_ver}, tarball={packages_seen[dep_name]}, main={main_version}"
        )
extra_platforms = sorted((set(packages_seen.keys()) - {"simple-memory"}) - set(main_opt_deps.keys()))
if extra_platforms:
    sys.exit(
        f"stage_npm.sh: orphan platform tarball(s) not declared in simple-memory optionalDependencies: {extra_platforms}"
    )

print(f"verified {len(tgz_files)} tarball(s) in {tarball_dir}")
PY
    verified_packs=1
}

read_npm_token() {
    local raw_token="${NPM_TOKEN:-}"
    if [[ -n "$raw_token" ]]; then
        python3 - <<'PY'
import os
import sys

tok = os.environ.get("NPM_TOKEN", "").rstrip("\r")
if not tok or any(ch.isspace() or ord(ch) < 32 for ch in tok):
    sys.exit("stage_npm.sh: NPM_TOKEN in environment is empty or contains whitespace/control characters")
sys.stdout.write(tok)
PY
        return 0
    fi

    local env_file="$repo/.env"
    if [[ ! -f "$env_file" ]]; then
        echo "stage_npm.sh: NPM_TOKEN not set and $env_file not found" >&2
        exit 1
    fi

    python3 - "$env_file" <<'PY'
import re
import sys

env_file = sys.argv[1]
token = None
with open(env_file, encoding="utf-8") as f:
    for raw in f:
        line = raw.rstrip("\r\n")
        m = re.match(r"^NPM_TOKEN=(.*)$", line)
        if not m:
            continue
        val = m.group(1)
        if len(val) >= 2 and ((val[0] == '"' and val[-1] == '"') or (val[0] == "'" and val[-1] == "'")):
            val = val[1:-1]
        token = val

if not token:
    sys.exit("stage_npm.sh: NPM_TOKEN not found or empty in .env")
if any(ch.isspace() or ord(ch) < 32 for ch in token):
    sys.exit("stage_npm.sh: NPM_TOKEN in .env contains whitespace or control characters")
sys.stdout.write(token)
PY
}

publish_packs() {
    verify_packs

    local token
    token=$(read_npm_token)

    local npmrc
    npmrc_dir=$(mktemp -d "${TMPDIR:-/tmp}/sm-npmrc.XXXXXX")
    chmod 0700 "$npmrc_dir"
    npmrc="$npmrc_dir/.npmrc"

    (
        umask 077
        printf 'registry=https://registry.npmjs.org/\n//registry.npmjs.org/:_authToken=%s\n' "$token" > "$npmrc"
    )
    chmod 0600 "$npmrc"
    unset token

    local ordered_tgz=()
    while IFS= read -r tgz_line; do
        [[ -n "$tgz_line" ]] && ordered_tgz+=("$tgz_line")
    done < <(python3 - "$out/tarballs" <<'PY'
import glob
import json
import os
import sys
import tarfile

tarball_dir = sys.argv[1]
platforms = []
main_tgz = None
for tgz in sorted(glob.glob(os.path.join(tarball_dir, "*.tgz"))):
    with tarfile.open(tgz, "r:gz") as tf:
        pkg = json.loads(tf.extractfile("package/package.json").read().decode("utf-8"))
    if pkg.get("name") == "simple-memory":
        main_tgz = tgz
    else:
        platforms.append(tgz)

if not main_tgz:
    sys.exit(f"stage_npm.sh: main simple-memory tarball missing from {tarball_dir}")

for p in platforms:
    print(p)
print(main_tgz)
PY
)

    echo "preflight: npm whoami"
    npm whoami --userconfig "$npmrc"

    local tgz
    for tgz in "${ordered_tgz[@]}"; do
        echo "preflight: npm publish --dry-run $(basename "$tgz")"
        npm publish --dry-run --access public --userconfig "$npmrc" "$tgz"
    done

    if [[ $do_dry_run -eq 1 ]]; then
        echo "dry-run preflight passed for ${#ordered_tgz[@]} package(s)"
        return 0
    fi

    for tgz in "${ordered_tgz[@]}"; do
        echo "publishing: $(basename "$tgz")"
        npm publish --access public --userconfig "$npmrc" "$tgz"
    done
}

if [[ ${#bin_targets[@]} -gt 0 ]]; then
    stage_packages
fi

if [[ $do_verify -eq 1 ]]; then
    verify_packs
fi

if [[ $do_publish -eq 1 ]]; then
    publish_packs
fi
