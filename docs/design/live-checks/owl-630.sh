#!/bin/bash
# OWL-630 (architecture section 8, "Several projects in one repository"), on macOS directly and on Linux through
# owl-630-linux.sh: repositories whose projects sit one folder down, their `[stack]` written by `owlshift init` and
# left unedited, then the real `owlshift gate`, confined. The harness is owl-629.sh's (OWL-629).
#
# Fixtures:
#   mono        Locary's shape: `backend/`, a uv project (`humanize` 4.11.0, `pytest` 8.3.4 in its `dev` group, its
#               pytest settings in `pyproject.toml`), and `frontend/`, a pnpm 10.33.2 project (`is-odd` 3.0.1 and
#               `is-number` 7.0.0, its `test` script requiring `is-odd`). Then, with SWEEP=1 (the default on macOS),
#               once per `run_caches` line, that line alone removed.
#   shared      `admin/` and `web/`, the same pnpm project twice: one `PNPM_HOME` for both. The gate log's pnpm
#               progress lines show what each install fetched and what it reused from the run cache.
#   workspace   a pnpm workspace whose members, `admin/` and `web/`, sit one folder down, with the root's lock alone:
#               `init` writes the root's lines only, whose `pnpm -r test` runs both members' tests.
#
# Each fixture is a git repository whose `origin`, `git@github.com:owl630/<fixture>.git`, is served by a local bare
# remote through `core.sshCommand`, so nothing reaches GitHub. Owlshift's folders are moved into a scratch folder
# (`OWLSHIFT_CONFIG_DIR`, `OWLSHIFT_DATA_DIR`, `OWLSHIFT_CACHE_DIR`), and git's global configuration with
# `GIT_CONFIG_GLOBAL`: the person's own are left as they are. The lock files are made first, unconfined, with a home
# of the fixture's own in that folder.
#
# Usage: [SWEEP=0|1] bash owl-630.sh OWLSHIFT_BINARY [FIXTURE...]   (network needed; pnpm 10, uv and python3 on PATH)
# It writes only under a temporary folder, removed at the end, unless KEEP=1. It prints results only, the home shown
# as `~`.
set -u
bin=$(cd "$(dirname "$1")" && pwd -P)/$(basename "$1"); shift
home=$(cd "$HOME" && pwd -P)
work=$(mktemp -d "${TMPDIR:-/tmp}/owl630.XXXXXX"); work=$(cd "$work" && pwd -P)
export OWLSHIFT_CONFIG_DIR=$work/config OWLSHIFT_DATA_DIR=$work/data OWLSHIFT_CACHE_DIR=$work/cache
export GIT_CONFIG_GLOBAL=$work/gitconfig
say() { printf '%s %s\n' "$(date -u +%H:%M:%SZ)" "$*"; }
clean() { sed "s|$home|~|g; s|$work|<work>|g; s/^/  | /"; }
if [ "$(uname -s)" = Darwin ]; then
  export PATH=/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin
  system="macOS $(sw_vers -productVersion) ($(sw_vers -buildVersion)) $(uname -m)"
  sweep=${SWEEP:-1}
else
  export PATH=/opt/node/bin:/usr/local/bin:/usr/bin:/bin
  system="$(. /etc/os-release && echo "$PRETTY_NAME") $(uname -m), $(bwrap --version), uid $(id -u)"
  sweep=${SWEEP:-0}
fi
fixtures=${*:-mono shared workspace}

mkdir -p "$work/config" "$work/ssh/owl630" "$work/seed"
printf '#!/bin/sh\n# The git command an ssh host would run, run here.\ncd %s && exec sh -c "$2"\n' "'$work/ssh'" >"$work/local-ssh"
chmod +x "$work/local-ssh"
for kv in "core.sshCommand $work/local-ssh" "ssh.variant simple" "user.name Probe" "user.email probe@example.com" \
  "init.defaultBranch main" "commit.gpgsign false"; do
  (cd "$work" && git config --global "${kv%% *}" "${kv#* }")
done

say "$system; $("$bin" --version | head -1); node $(node --version); pnpm $(pnpm --version); $(uv --version); \
$(python3 --version)"

# unconfined COMMAND...: a tool run outside `owlshift gate`, with a home of the fixture's own.
unconfined() {
  mkdir -p "$work/seed/$fx-home"
  env HOME="$work/seed/$fx-home" "$@"
}

# node_project DIR NAME: a pnpm project, its lock made unless it is a workspace member (MEMBER=1).
node_project() {
  mkdir -p "$1"
  printf '{\n  "name": "%s",\n  "version": "1.0.0",\n  "private": true,\n  "packageManager": "pnpm@10.33.2",\n  "scripts": {\n    "test": "node test.js"\n  },\n  "dependencies": {\n    "is-number": "7.0.0",\n    "is-odd": "3.0.1"\n  }\n}\n' \
    "$2" >"$1/package.json"
  printf "if (!require('is-odd')(3)) process.exit(1);\nconsole.log('%s: 3 is odd');\n" "$2" >"$1/test.js"
  printf 'node_modules/\n' >"$1/.gitignore"
  [ "${MEMBER:-0}" = 1 ] || (cd "$1" && unconfined pnpm install --lockfile-only >/dev/null 2>&1) || say "pnpm could not lock $2"
}

# uv_project DIR: a uv project, its lock made.
uv_project() {
  mkdir -p "$1"
  printf '[project]\nname = "probe"\nversion = "0.1.0"\nrequires-python = ">=3.12"\ndependencies = ["humanize==4.11.0"]\n\n[dependency-groups]\ndev = ["pytest==8.3.4"]\n\n[tool.uv]\npackage = false\n\n[tool.pytest.ini_options]\naddopts = "-q"\n' \
    >"$1/pyproject.toml"
  printf 'import humanize\n\n\ndef test_humanize():\n    assert humanize.intcomma(1000) == "1,000"\n' >"$1/test_probe.py"
  printf '__pycache__/\n.venv/\n.pytest_cache/\n' >"$1/.gitignore"
  (cd "$1" && unconfined uv lock -q >/dev/null 2>&1) || say "uv could not lock"
}

fixture() {
  case $1 in
  mono) uv_project "$2/backend" && node_project "$2/frontend" frontend ;;
  shared) node_project "$2/admin" admin && node_project "$2/web" web ;;
  workspace)
    mkdir -p "$2"
    printf '{\n  "name": "root",\n  "private": true,\n  "packageManager": "pnpm@10.33.2",\n  "scripts": {\n    "test": "pnpm -r test"\n  }\n}\n' \
      >"$2/package.json"
    printf 'packages:\n  - admin\n  - web\n' >"$2/pnpm-workspace.yaml"
    printf 'node_modules/\n' >"$2/.gitignore"
    MEMBER=1 node_project "$2/admin" admin && MEMBER=1 node_project "$2/web" web
    (cd "$2" && unconfined pnpm install --lockfile-only >/dev/null 2>&1) || say "pnpm could not lock the workspace"
    say "locks: $(cd "$2" && find . -name pnpm-lock.yaml | sort | tr '\n' ' ')" ;;
  esac
}

# gate LABEL DIR: commits and pushes the checkout, then runs `owlshift gate`, printing its verdict and its end, and
# the pnpm progress lines of its log with SHOW_PNPM=1.
gate() {
  local label=$1 dir=$2 out rc start log
  (cd "$dir" && git add -A && git commit -q -m "$label" && git push -q origin main) >/dev/null 2>&1
  start=$(date +%s)
  out=$(cd "$dir" && "$bin" gate </dev/null 2>&1); rc=$?
  say "$label: owlshift gate exit $rc in $(($(date +%s) - start)) s"
  log=$(printf '%s\n' "$out" | sed -n 's/^Gate log: //p' | head -1)
  out=$(printf '%s\n' "$out" | grep -v '^Its whole output goes to\|^Gate log: ')
  if printf '%s\n' "$out" | grep -q '^Gate red'; then
    printf '%s\n' "$out" | grep -B"${TAIL:-6}" '^Gate red' | clean
    printf '%s\n' "$out" | grep -A1 '^The sandbox closes your home' | tail -1 | clean
  else
    printf '%s\n' "$out" | tail -"${TAIL:-8}" | clean
  fi
  if [ "${SHOW_PNPM:-0}" = 1 ] && [ -f "$log" ]; then
    grep -E '^(\$ |Progress: resolved .* done)' "$log" | clean
  fi
}

for fx in $fixtures; do
  say "--- $fx"
  dir=$work/src/$fx
  fixture "$fx" "$dir"
  git init -q --bare "$work/ssh/owl630/$fx.git"
  (cd "$dir" && git init -q && git remote add origin "git@github.com:owl630/$fx.git" && git add -A \
    && git commit -q -m fixture && git push -q origin main) >/dev/null 2>&1 || say "the fixture could not be pushed"
  (cd "$dir" && "$bin" init --tracker markdown --skip-secrets </dev/null 2>&1 | grep -E '^(Detected|  Note|Skipped)') | clean
  # The Markdown tracker's example ticket stays out of the commits, as Owlshift commits none of its files.
  printf 'tickets/\n' >>"$dir/.git/info/exclude"
  say "the [stack] written:"
  sed -n '/^\[stack\]/,/^# The files agents receive/p' "$dir/owlshift.toml" \
    | grep -E '^(gate|setup|caches|run_caches|\]|    |# read from|# note:)' | clean
  cp "$dir/owlshift.toml" "$work/owlshift.toml.$fx"
  SHOW_PNPM=1 gate "$fx, as written" "$dir"
  say "the run caches: $(cd "$work/cache/github.com/owl630/$fx" 2>/dev/null && du -sk -- * 2>/dev/null \
    | awk '{ printf "%s %s, ", $2, ($1 < 1024 ? $1 " kB" : sprintf("%.0f MB", $1 / 1024)) }')"
  [ "$fx" = shared ] && say "pnpm stores under the run caches: $(cd "$work/cache/github.com/owl630/$fx" && \
    find . -maxdepth 4 -type d -name 'v10' | sort | tr '\n' ' ')"
  if [ "$fx" = mono ] && [ "$sweep" = 1 ]; then
    # Each run cache removed alone: the others stay, filled by the run above.
    for variable in $(sed -n 's/^run_caches\.\([A-Za-z_0-9]*\) = .*/\1/p' "$work/owlshift.toml.$fx"); do
      awk -v v="$variable" 'index($0, "run_caches." v " = ") != 1' "$work/owlshift.toml.$fx" >"$dir/owlshift.toml"
      gate "$fx, without run_caches.$variable" "$dir"
    done
    cp "$work/owlshift.toml.$fx" "$dir/owlshift.toml"
  fi
done

if [ -n "${KEEP:-}" ]; then say "kept the scratch folder $work"; exit 0; fi
chmod -R u+w "$work" 2>/dev/null
rm -rf "$work"
say "removed the scratch folder"
