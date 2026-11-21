#!/bin/sh
# OWL-617 on Linux (architecture section 8, "Caches a run may write"; build plan, "P5 · Quality pipeline", part W):
# R1 and R2 of owl-617-mac.sh under bwrap, with the mounts `owlshift_platform::sandbox::bwrap_args` lays out for a gate
# command: `/` read-only, a private `/tmp`, the home a private tmpfs, `~/.gitconfig`, `~/.config/git` and the tool
# chains (`~/.rustup`, `~/.cargo/bin`, `~/.cargo/registry`, `~/.cargo/git`) bound back read-only, the worktree, the
# run's temporary folder and the run cache folders (`~/.cache/owlshift/github.com/owl617-probe/<case>/<name>`, the
# `$XDG_CACHE_HOME` default) bound read and written. The agent's environment: `env -i` with PATH, HOME, USER, LANG
# and TMPDIR, plus the case's variables.
#
# Ubuntu 24.04, bubblewrap, a non-root user `owl` (uid 1001) whose rustup and tool chain are installed in its home by
# rustup's own installer, node from nodejs.org in /opt/node with pnpm installed by npm there, uv copied from its
# official image, Python from the distribution; and the two options bwrap needs inside Docker
# (`--security-opt seccomp=unconfined --security-opt systempaths=unconfined`, not --privileged).
#
# Usage, Docker running: sh docs/design/live-checks/owl-617-linux.sh   (network needed). It prints results only.
set -eu
image=owlshift-owl617-rig
docker build -q -t "$image" - <<'EOF' >/dev/null
FROM ghcr.io/astral-sh/uv:0.11.7 AS uv
FROM ubuntu:24.04
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends \
      bubblewrap ca-certificates curl xz-utils build-essential git python3 >/dev/null \
    && rm -rf /var/lib/apt/lists/*
RUN arch=$(dpkg --print-architecture | sed 's/amd64/x64/') \
    && curl -fsSL "https://nodejs.org/dist/v22.11.0/node-v22.11.0-linux-$arch.tar.xz" | tar -xJ -C /opt \
    && mv /opt/node-v22.11.0-linux-$arch /opt/node \
    && PATH=/opt/node/bin:$PATH npm install -g --silent pnpm@10.33.2
COPY --from=uv /uv /usr/local/bin/uv
RUN useradd -m -u 1001 owl
USER owl
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y -q --profile minimal --component clippy,rustfmt >/dev/null 2>&1 \
    && /home/owl/.cargo/bin/cargo install -q cargo-nextest@0.9.146 --locked >/dev/null 2>&1 \
    && rm -rf /home/owl/.cargo/registry/cache /home/owl/.cargo/registry/src
EOF
docker run --rm -i --security-opt seccomp=unconfined --security-opt systempaths=unconfined "$image" bash -s <<'EOF'
set -u
H=/home/owl
tree=$H/.cache/owlshift/github.com/owl617-probe
path=$H/.cargo/bin:/opt/node/bin:/usr/local/bin:/usr/bin:/bin
probe=$H/probe
mkdir -p "$probe" "$H/run-tmp"
say() { printf '%s %s\n' "$(date -u +%H:%M:%SZ)" "$*"; }
clean() { sed "s|$H|~|g; s|$probe|<probe>|g; s/^/  | /"; }
confined() { # WORKDIR RUN_TMP "VARS" COMMAND...
  local wd=$1 tmp=$2 vars=$3; shift 3
  local binds=() v
  for v in $vars; do case $v in *=/*) binds+=(--bind-try "${v#*=}" "${v#*=}") ;; esac; done
  env -i PATH="$path" HOME="$H" USER=owl LOGNAME=owl SHELL=/bin/sh LANG=C.UTF-8 TMPDIR="$tmp" $vars \
    bwrap --die-with-parent --unshare-pid --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp --tmpfs "$H" \
      --ro-bind-try "$H/.gitconfig" "$H/.gitconfig" --ro-bind-try "$H/.config/git" "$H/.config/git" \
      --ro-bind-try "$H/.rustup" "$H/.rustup" --ro-bind-try "$H/.cargo/bin" "$H/.cargo/bin" \
      --ro-bind-try "$H/.cargo/registry" "$H/.cargo/registry" --ro-bind-try "$H/.cargo/git" "$H/.cargo/git" \
      --bind-try "$tmp" "$tmp" --bind-try "$wd" "$wd" "${binds[@]}" --chdir "$wd" -- "$@"
}
check() { # LABEL WORKDIR "VARS" "LINE"
  local label=$1 wd=$2 vars=$3 line=$4 tmp out rc start
  tmp=$(mktemp -d "$H/run-tmp/t.XXXXXX")
  start=$(date +%s)
  out=$(confined "$wd" "$tmp" "$vars" /bin/sh -c "$line" 2>&1); rc=$?
  say "$label: exit $rc in $(($(date +%s) - start)) s — \$ ${vars:+$(echo "$vars" | sed "s|$tree|<tree>|g") }$line"
  printf '%s\n' "$out" | tail -${TAIL:-4} | clean
}
project() { # KIND DIR
  mkdir -p "$2"
  case $1 in
  cargo)
    printf '[package]\nname = "probe"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\nhumansize = "=2.1.3"\noorandom = "=11.1.4"\n' >"$2/Cargo.toml"
    mkdir -p "$2/src" && printf 'fn main() {\n    let size = humansize::format_size(1024u64, humansize::DECIMAL);\n    println!("{size} {}", oorandom::Rand32::new(1).rand_u32());\n}\n' >"$2/src/main.rs" ;;
  node) printf '{"name":"probe","version":"1.0.0","private":true,"dependencies":{"humanize-duration":"3.32.1","is-number":"7.0.0"}}\n' >"$2/package.json" ;;
  python) printf '[project]\nname = "probe"\nversion = "0.1.0"\nrequires-python = ">=3.12"\ndependencies = ["six==1.16.0", "humanize==4.11.0"]\n\n[tool.uv]\npackage = false\n' >"$2/pyproject.toml" ;;
  esac
}
say "$(. /etc/os-release && echo "$PRETTY_NAME") $(uname -m), $(bwrap --version), uid $(id -u); $(PATH=$path cargo --version); $(PATH=$path rustup --version 2>/dev/null); pnpm $(PATH=$path pnpm --version); npm $(PATH=$path npm --version); node $(PATH=$path node --version); $(uv --version); $(python3 --version)"
rc=$tree/r1; mkdir -p "$rc/cargo" "$rc/pnpm-store" "$rc/npm" "$rc/uv" "$rc/uv-python"
say "--- R1 cargo"
project cargo "$probe/cargo"
check "cargo, default CARGO_HOME (the person's, read-only)" "$probe/cargo" "" "cargo fetch"
check "cargo, CARGO_HOME moved" "$probe/cargo" "CARGO_HOME=$rc/cargo" "cargo build && ./target/debug/probe"
check "cargo clippy, CARGO_HOME moved" "$probe/cargo" "CARGO_HOME=$rc/cargo" "cargo clippy --locked -- -D warnings"
check "cargo fmt, CARGO_HOME moved" "$probe/cargo" "CARGO_HOME=$rc/cargo" "cargo fmt -- --check && echo formatted"
check "cargo nextest, CARGO_HOME moved" "$probe/cargo" "CARGO_HOME=$rc/cargo" "cargo nextest --version | head -1"
check "rustup's choice, CARGO_HOME moved" "$probe/cargo" "CARGO_HOME=$rc/cargo" "rustup show active-toolchain; command -v cargo"
say "what the moved CARGO_HOME holds: $(ls -A "$rc/cargo" | tr '\n' ' ')"
say "--- R1 pnpm"
project node "$probe/pnpm"
check "pnpm, store not moved" "$probe/pnpm" "" "pnpm install"
rm -rf "$probe/pnpm/node_modules" "$probe/pnpm/pnpm-lock.yaml" "$probe/pnpm/.pnpm-store"
check "pnpm, npm_config_store_dir moved" "$probe/pnpm" "npm_config_store_dir=$rc/pnpm-store" "pnpm install && pnpm store path"
rm -rf "$probe/pnpm/node_modules"
check "pnpm --frozen-lockfile, npm_config_store_dir moved" "$probe/pnpm" "npm_config_store_dir=$rc/pnpm-store" "pnpm install --frozen-lockfile && node -e \"console.log(require('humanize-duration')(3000))\""
say "--- R1 npm"
project node "$probe/npm"
check "npm, cache not moved" "$probe/npm" "" "npm install --no-audit --no-fund"
rm -rf "$probe/npm/node_modules" "$probe/npm/package-lock.json"
check "npm, npm_config_cache moved" "$probe/npm" "npm_config_cache=$rc/npm" "npm install --no-audit --no-fund && rm -rf node_modules && npm ci --no-audit --no-fund && node -e \"require('is-number')\" && echo required"
say "--- R1 uv"
project python "$probe/uv"
check "uv, cache not moved" "$probe/uv" "" "uv sync"
rm -rf "$probe/uv/.venv" "$probe/uv/uv.lock"
check "uv, UV_CACHE_DIR moved" "$probe/uv" "UV_CACHE_DIR=$rc/uv" "uv sync && uv run python -c 'import six, humanize; print(humanize.naturalsize(1024))'"
rm -rf "$probe/uv/.venv"
check "uv --python 3.13, UV_CACHE_DIR and UV_PYTHON_INSTALL_DIR moved" "$probe/uv" "UV_CACHE_DIR=$rc/uv UV_PYTHON_INSTALL_DIR=$rc/uv-python" "uv sync --python 3.13 && .venv/bin/python --version"
rc=$tree/r2; mkdir -p "$rc/cargo" "$rc/pnpm-store" "$rc/npm" "$rc/uv" "$rc/uv-python"
seed=$probe/seed-cache
for kind in cargo node python; do project $kind "$probe/r2-$kind-a"; done
(cd "$probe/r2-cargo-a" && CARGO_HOME="$seed/cargo" PATH=$path cargo generate-lockfile -q)
(cd "$probe/r2-node-a" && PATH=$path pnpm install --lockfile-only --store-dir "$seed/pnpm" >/dev/null 2>&1 \
  && PATH=$path npm install --package-lock-only --no-audit --no-fund --cache "$seed/npm" >/dev/null 2>&1)
(cd "$probe/r2-python-a" && UV_CACHE_DIR="$seed/uv" uv lock -q)
for kind in cargo node python; do for copy in b c; do cp -R "$probe/r2-$kind-a" "$probe/r2-$kind-$copy"; done; done
pair() { # KIND "VARS" "LINE" "THIRD LINE"
  say "--- R2 $1: two runs at once on one empty folder"
  TAIL=2 check "$1 a" "$probe/r2-$1-a" "$2" "$3" >"$probe/out-a" 2>&1 &
  TAIL=2 check "$1 b" "$probe/r2-$1-b" "$2" "$3" >"$probe/out-b" 2>&1 &
  wait
  cat "$probe/out-a" "$probe/out-b"
  TAIL=3 check "$1 c, third run, offline" "$probe/r2-$1-c" "$2" "$4"
}
pair cargo "CARGO_HOME=$rc/cargo" "cargo build --locked" "cargo build --locked --offline"
pair node "npm_config_store_dir=$rc/pnpm-store" "pnpm install --frozen-lockfile" "pnpm install --frozen-lockfile --offline"
for copy in a b c; do rm -rf "$probe/r2-node-$copy/node_modules"; done
pair node "npm_config_cache=$rc/npm" "npm ci --no-audit --no-fund" "npm ci --no-audit --no-fund --offline"
pair python "UV_CACHE_DIR=$rc/uv UV_PYTHON_INSTALL_DIR=$rc/uv-python" "uv sync --locked" "uv sync --locked --offline"
EOF
