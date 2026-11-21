#!/bin/bash
# OWL-617 on macOS (architecture section 8, "Caches a run may write"; build plan, "P5 · Quality pipeline", part W):
# R1, the variable each package manager reads for its cache, and a setup and a build with the cache moved by it, inside
# a Seatbelt profile shaped as `owlshift_platform::sandbox::seatbelt_profile` shapes a gate command's; R2, two confined
# runs of one project writing one cache folder at once, then a third reading it offline; R3, what a first setup costs on
# an empty run cache against one on a filled cache, for Owlshift (`cargo fetch --locked` of this repository's HEAD, as
# its gate's first cargo command fetches) and Locary (its setup's two installs, `pnpm install --frozen-lockfile` in
# `frontend/` and `uv sync --frozen ... --python 3.12` in `backend/`, on copies of their manifests and lock files, read
# from `LOCARY` (default `~/Projects/locary`), which is left as it is).
#
# The profile: everything allowed, no write but the devices, the home and the shared temporary folders closed to reads
# and writes, then opened again: read-only, `~/.gitconfig`, `~/.config/git` and the tool chains `AgentEnv::policy`
# opens (`~/.rustup`, `~/.cargo/bin`, `~/.cargo/registry`, `~/.cargo/git`); read and written, the worktree, the run's
# temporary folder and the run cache folders (`~/Library/Caches/owlshift/github.com/owl617-probe/<case>/<name>`); the
# folders above an opened one listable; the credential paths and the Keychain closed. The environment is the agent's:
# `env -i` with PATH, HOME, USER, LOGNAME, SHELL, LANG and TMPDIR (the run's folder), plus the case's variables. rustup's
# proxies and the tool chain stay where Homebrew and rustup put them.
#
# Usage: bash owl-617-mac.sh [r1|r2|r3|all]   (network needed: the installs are online, into the run caches)
# It writes only under a temporary folder and the probe's cache tree, removed at the end. It prints results only, the
# home shown as `~`.
set -u
which=${1:-all}
home=$(cd "$HOME" && pwd -P)
shared_tmp=$(cd "${TMPDIR:-/tmp}" && pwd -P)
probe=$(mktemp -d "$shared_tmp/owl617.XXXXXX")
tree="$home/Library/Caches/owlshift/github.com/owl617-probe"
path="/opt/homebrew/opt/rustup/bin:$home/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"
say() { printf '%s %s\n' "$(date -u +%H:%M:%SZ)" "$*"; }
clean() { sed "s|$home|~|g; s|$probe|<probe>|g; s/^/  | /"; }

# confined WORKDIR RUN_TMP "VAR=value ..." COMMAND...: one command, confined, with the agent's environment.
confined() {
  local wd=$1 tmp=$2 vars=$3; shift 3
  local params=() n=0 profile
  profile="(version 1)
(allow default)
(deny file-write*)
(allow file-write* (subpath \"/dev\"))"
  p() { params+=(-D "$1$n=$2"); profile="$profile
($3 (subpath (param \"$1$n\")))"; n=$((n + 1)); }
  p HOME "$home" "deny file-read-data file-read-xattr file-write*"
  for c in "$shared_tmp" /private/tmp /private/var/tmp; do p C "$c" "deny file-read-data file-read-xattr file-write*"; done
  for r in .gitconfig .config/git .rustup .cargo/bin .cargo/registry .cargo/git; do
    p R "$home/$r" "allow file-read-data file-read-xattr"
  done
  local writable=("$wd")
  for v in $vars; do
    case $v in *=/*) writable+=("${v#*=}") ;; esac
  done
  for w in "${writable[@]}"; do p W "$w" "allow file-read-data file-read-xattr file-write*"; done
  p T "$tmp" "allow file-read-data file-read-xattr file-write*"
  # The folders above an opened one, listable (`above_opened`).
  local seen=" " a
  for o in "${writable[@]}" "$tmp" "$home/.rustup" "$home/.cargo/bin" "$home/.config/git"; do
    a=$(dirname "$o")
    while [ "$a" != / ]; do
      case $a in "$home"* | "$shared_tmp"*)
        case $seen in *" $a "*) ;; *) seen="$seen$a "; params+=(-D "A$n=$a")
          profile="$profile
(allow file-read-data (literal (param \"A$n\")))"; n=$((n + 1)) ;; esac ;;
      esac
      a=$(dirname "$a")
    done
  done
  for h in .ssh .aws .netrc .npmrc .pypirc .config/gh .cargo/credentials .cargo/credentials.toml Library/Keychains; do
    p H "$home/$h" "deny file-read-data file-read-xattr file-read-metadata file-write*"
  done
  profile="$profile
(deny mach-lookup (global-name \"com.apple.SecurityServer\") (global-name \"com.apple.securityd.xpc\"))"
  (cd "$wd" && /usr/bin/env -i PATH="$path" HOME="$home" USER="$USER" LOGNAME="$USER" SHELL=/bin/sh LANG=en_US.UTF-8 \
    TMPDIR="$tmp" $vars /usr/bin/sandbox-exec "${params[@]}" -p "$profile" -- "$@")
}

# check LABEL WORKDIR "VARS" "SHELL LINE": runs the line confined, prints its exit status, its time and its output's end.
check() {
  local label=$1 wd=$2 vars=$3 line=$4 tmp out rc start
  tmp=$(mktemp -d "$probe/tmp.XXXXXX")
  start=$(date +%s)
  out=$(confined "$wd" "$tmp" "$vars" /bin/sh -c "$line" 2>&1); rc=$?
  say "$label: exit $rc in $(($(date +%s) - start)) s — \$ ${vars:+$(echo "$vars" | sed "s|$tree|<tree>|g") }$line"
  printf '%s\n' "$out" | tail -${TAIL:-4} | clean
}

project() { # project KIND DIR
  mkdir -p "$2"
  case $1 in
  cargo)
    printf '[package]\nname = "probe"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\nhumansize = "=2.1.3"\noorandom = "=11.1.4"\n' >"$2/Cargo.toml"
    mkdir -p "$2/src" && printf 'fn main() {\n    let size = humansize::format_size(1024u64, humansize::DECIMAL);\n    println!("{size} {}", oorandom::Rand32::new(1).rand_u32());\n}\n' >"$2/src/main.rs" ;;
  node) printf '{"name":"probe","version":"1.0.0","private":true,"dependencies":{"humanize-duration":"3.32.1","is-number":"7.0.0"}}\n' >"$2/package.json" ;;
  python) printf '[project]\nname = "probe"\nversion = "0.1.0"\nrequires-python = ">=3.12"\ndependencies = ["six==1.16.0", "humanize==4.11.0"]\n\n[tool.uv]\npackage = false\n' >"$2/pyproject.toml" ;;
  esac
}

say "macOS $(sw_vers -productVersion) ($(sw_vers -buildVersion)); $(PATH=$path cargo --version); $(PATH=$path rustup --version 2>/dev/null); pnpm $(PATH=$path pnpm --version); npm $(PATH=$path npm --version); node $(PATH=$path node --version); $(PATH=$path uv --version)"
say "person's ~/.cargo/registry holds humansize: $(ls "$home"/.cargo/registry/cache/*/ 2>/dev/null | grep -c '^humansize-'), oorandom: $(ls "$home"/.cargo/registry/cache/*/ 2>/dev/null | grep -c '^oorandom-')"

if [ "$which" = r1 ] || [ "$which" = all ]; then
  rc="$tree/r1"; mkdir -p "$rc/cargo" "$rc/pnpm-store" "$rc/npm" "$rc/uv" "$rc/uv-python"
  say "--- R1 cargo"
  project cargo "$probe/cargo"
  check "cargo, default CARGO_HOME (the person's, read-only)" "$probe/cargo" "" "cargo fetch"
  check "cargo, CARGO_HOME moved" "$probe/cargo" "CARGO_HOME=$rc/cargo" "cargo build && ./target/debug/probe"
  check "cargo clippy, CARGO_HOME moved" "$probe/cargo" "CARGO_HOME=$rc/cargo" "cargo clippy --locked -- -D warnings"
  check "cargo fmt, CARGO_HOME moved" "$probe/cargo" "CARGO_HOME=$rc/cargo" "cargo fmt -- --check && echo formatted"
  check "cargo nextest, CARGO_HOME moved" "$probe/cargo" "CARGO_HOME=$rc/cargo" "cargo nextest --version"
  check "rustup's choice, CARGO_HOME moved" "$probe/cargo" "CARGO_HOME=$rc/cargo" "rustup show active-toolchain; command -v cargo"
  say "what the moved CARGO_HOME holds: $(ls -A "$rc/cargo" | tr '\n' ' ')"
  say "--- R1 pnpm"
  project node "$probe/pnpm"
  check "pnpm, store not moved" "$probe/pnpm" "" "pnpm install"
  rm -rf "$probe/pnpm/node_modules" "$probe/pnpm/pnpm-lock.yaml" "$probe/pnpm/.pnpm-store"
  check "pnpm, npm_config_store_dir moved" "$probe/pnpm" "npm_config_store_dir=$rc/pnpm-store" "pnpm install && pnpm store path"
  rm -rf "$probe/pnpm/node_modules"
  check "pnpm --frozen-lockfile, npm_config_store_dir moved" "$probe/pnpm" "npm_config_store_dir=$rc/pnpm-store" "pnpm install --frozen-lockfile && node -e \"console.log(require('humanize-duration')(3000))\""
  rm -rf "$probe/pnpm/node_modules"
  check "pnpm --frozen-lockfile, NPM_CONFIG_STORE_DIR (upper case) moved" "$probe/pnpm" "NPM_CONFIG_STORE_DIR=$rc/pnpm-store" "pnpm install --frozen-lockfile && pnpm store path"
  say "what the moved store holds: $(ls -A "$rc/pnpm-store" | tr '\n' ' '); in the worktree: $(ls -A "$probe/pnpm" | tr '\n' ' ')"
  say "--- R1 npm"
  project node "$probe/npm"
  check "npm, cache not moved" "$probe/npm" "" "npm install --no-audit --no-fund"
  rm -rf "$probe/npm/node_modules" "$probe/npm/package-lock.json"
  check "npm, npm_config_cache moved" "$probe/npm" "npm_config_cache=$rc/npm" "npm install --no-audit --no-fund && rm -rf node_modules && npm ci --no-audit --no-fund && node -e \"require('is-number')\" && echo required"
  rm -rf "$probe/npm/node_modules"
  check "npm ci, NPM_CONFIG_CACHE (upper case) moved" "$probe/npm" "NPM_CONFIG_CACHE=$rc/npm" "npm ci --no-audit --no-fund --offline && echo offline-ok"
  say "what the moved npm cache holds: $(ls -A "$rc/npm" | tr '\n' ' ')"
  say "--- R1 uv"
  project python "$probe/uv"
  check "uv, cache not moved" "$probe/uv" "" "uv sync"
  rm -rf "$probe/uv/.venv" "$probe/uv/uv.lock"
  check "uv, UV_CACHE_DIR moved" "$probe/uv" "UV_CACHE_DIR=$rc/uv" "uv sync && uv run python -c 'import six, humanize; print(humanize.naturalsize(1024))'"
  rm -rf "$probe/uv/.venv"
  check "uv --python 3.12, UV_CACHE_DIR and UV_PYTHON_INSTALL_DIR moved" "$probe/uv" "UV_CACHE_DIR=$rc/uv UV_PYTHON_INSTALL_DIR=$rc/uv-python" "uv sync --python 3.12 && .venv/bin/python --version"
  say "what the moved uv folders hold: $(ls -A "$rc/uv" | tr '\n' ' ') / $(ls -A "$rc/uv-python" | tr '\n' ' ')"
fi

if [ "$which" = r2 ] || [ "$which" = all ]; then
  rc="$tree/r2"; mkdir -p "$rc/cargo" "$rc/pnpm-store" "$rc/npm" "$rc/uv" "$rc/uv-python"
  # The lock files are made first, unconfined into a throwaway cache, so the two runs install one locked set.
  seed="$probe/seed-cache"
  for kind in cargo node python; do project $kind "$probe/r2-$kind-a"; done
  (cd "$probe/r2-cargo-a" && CARGO_HOME="$seed/cargo" PATH=$path cargo generate-lockfile -q)
  (cd "$probe/r2-node-a" && PATH=$path pnpm install --lockfile-only --store-dir "$seed/pnpm" >/dev/null 2>&1 \
    && PATH=$path npm install --package-lock-only --no-audit --no-fund --cache "$seed/npm" >/dev/null 2>&1)
  (cd "$probe/r2-python-a" && UV_CACHE_DIR="$seed/uv" PATH=$path uv lock -q)
  for kind in cargo node python; do
    for copy in b c; do cp -R "$probe/r2-$kind-a" "$probe/r2-$kind-$copy"; done
  done
  # pair KIND "VARS" "LINE" "THIRD LINE": two runs at once in a and b, then the third in c.
  pair() {
    say "--- R2 $1: two runs at once on one empty folder"
    TAIL=2 check "$1 a" "$probe/r2-$1-a" "$2" "$3" >"$probe/out-a" 2>&1 &
    TAIL=2 check "$1 b" "$probe/r2-$1-b" "$2" "$3" >"$probe/out-b" 2>&1 &
    wait
    cat "$probe/out-a" "$probe/out-b"
    TAIL=3 check "$1 c, third run, offline" "$probe/r2-$1-c" "$2" "$4"
  }
  pair cargo "CARGO_HOME=$rc/cargo" "cargo build --locked" "cargo build --locked --offline"
  pair node "npm_config_store_dir=$rc/pnpm-store" "pnpm install --frozen-lockfile" \
    "pnpm install --frozen-lockfile --offline"
  for copy in a b c; do rm -rf "$probe/r2-node-$copy/node_modules"; done
  pair node "npm_config_cache=$rc/npm" "npm ci --no-audit --no-fund" "npm ci --no-audit --no-fund --offline"
  pair python "UV_CACHE_DIR=$rc/uv UV_PYTHON_INSTALL_DIR=$rc/uv-python" "uv sync --locked" "uv sync --locked --offline"
fi

if [ "$which" = r3 ] || [ "$which" = all ]; then
  rc="$tree/r3"; mkdir -p "$rc/cargo" "$rc/pnpm-store" "$rc/uv" "$rc/uv-python"
  size() { du -sk "$1" | awk '{printf "%.0f MB", $1 / 1024}'; }
  repo=$(cd "$(dirname "$0")" && git rev-parse --show-toplevel)
  mkdir -p "$probe/owlshift" && git -C "$repo" archive HEAD | tar -x -C "$probe/owlshift"
  say "--- R3 Owlshift at $(git -C "$repo" rev-parse --short HEAD): $(grep -c '^name = ' "$probe/owlshift/Cargo.lock") packages in Cargo.lock"
  TAIL=1 check "owlshift, empty CARGO_HOME" "$probe/owlshift" "CARGO_HOME=$rc/cargo" "cargo fetch --locked"
  say "the run cache: $(size "$rc/cargo")"
  TAIL=1 check "owlshift, filled CARGO_HOME" "$probe/owlshift" "CARGO_HOME=$rc/cargo" "cargo fetch --locked"
  locary=${LOCARY:-$home/Projects/locary}
  mkdir -p "$probe/locary/frontend" "$probe/locary/backend"
  cp "$locary/frontend/package.json" "$locary/frontend/pnpm-lock.yaml" "$probe/locary/frontend/"
  cp "$locary/backend/pyproject.toml" "$locary/backend/uv.lock" "$probe/locary/backend/"
  say "--- R3 Locary at $(git -C "$locary" rev-parse --short HEAD): $(grep -c 'resolution:' "$probe/locary/frontend/pnpm-lock.yaml") packages in pnpm-lock.yaml, $(grep -c '^\[\[package\]\]' "$probe/locary/backend/uv.lock") in uv.lock"
  pnpm_line="CYPRESS_INSTALL_BINARY=0 pnpm install --frozen-lockfile --config.confirm-modules-purge=false"
  TAIL=2 check "locary pnpm, empty store" "$probe/locary/frontend" "npm_config_store_dir=$rc/pnpm-store" "$pnpm_line"
  say "the run cache: $(size "$rc/pnpm-store")"
  rm -rf "$probe/locary/frontend/node_modules"
  TAIL=2 check "locary pnpm, filled store" "$probe/locary/frontend" "npm_config_store_dir=$rc/pnpm-store" "$pnpm_line"
  uv_line="uv sync --frozen --no-install-project --group lint --group test --python 3.12"
  TAIL=8 check "locary uv, empty cache and no Python" "$probe/locary/backend" "UV_CACHE_DIR=$rc/uv UV_PYTHON_INSTALL_DIR=$rc/uv-python" "$uv_line"
  say "the run caches: uv $(size "$rc/uv"), uv-python $(size "$rc/uv-python")"
  rm -rf "$probe/locary/backend/.venv" "$rc/uv" && mkdir -p "$rc/uv"
  TAIL=2 check "locary uv, empty cache, Python kept (today's setup, its cache in the run's temporary folder)" "$probe/locary/backend" "UV_CACHE_DIR=$rc/uv UV_PYTHON_INSTALL_DIR=$rc/uv-python" "$uv_line"
  rm -rf "$probe/locary/backend/.venv"
  TAIL=2 check "locary uv, filled cache" "$probe/locary/backend" "UV_CACHE_DIR=$rc/uv UV_PYTHON_INSTALL_DIR=$rc/uv-python" "$uv_line"
fi

chmod -R u+w "$probe" "$tree" 2>/dev/null
rm -rf "$probe" "$tree"
rmdir "$home/Library/Caches/owlshift/github.com" "$home/Library/Caches/owlshift" 2>/dev/null
say "removed the probe folder and the probe's cache tree"
