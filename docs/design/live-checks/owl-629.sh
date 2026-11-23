#!/bin/bash
# OWL-629 (runtime and operations, "More stacks"), on macOS directly and on Linux through owl-629-linux.sh: for pnpm's
# majors and for yarn 2 and later, a fixture repository whose dependencies are in no cache of the machine, its
# `[stack]` written by `owlshift init` and left unedited, then the real `owlshift gate`, confined: once as `init` wrote
# it, then once per `run_caches` line, that line alone removed. The harness is owl-628.sh's (OWL-628), with a tool
# chain per fixture first on `PATH`.
#
# Every fixture is one Node project, `is-odd` 3.0.1 and `is-number` 7.0.0 (three packages with `is-odd`'s own
# `is-number` 6.0.0), its `test` script requiring `is-odd`, its `packageManager` naming the version the fixture runs:
#   pnpm9, pnpm11, pnpm12   pnpm 9.15.9, 11.28.13 or 12.21.0, that pnpm first on `PATH`.
#   pnpm10                  pnpm 10.33.2, the machine's own.
#   pnpm12-nopin            pnpm 12.21.0, no `packageManager`.
#   pnpm9-on10              `packageManager` pnpm 9.15.9, the machine's pnpm 10.33.2, which switches to it.
#   pnpm12-switch           `packageManager` pnpm 12.20.0, pnpm 12.21.0 first on `PATH`, which switches to it.
#   pnpm9-corepack          pnpm 9.15.9 through corepack's `pnpm`, first on `PATH`.
#   yarn4                   yarn 4.19.0, `.yarnrc.yml` with `nodeLinker: node-modules`, yarn's global cache.
#   yarn4-pnp               the same with yarn's default linker, Plug'n'Play.
#   yarn4-norc              yarn 4.19.0 without `.yarnrc.yml`: Plug'n'Play, yarn's global cache.
#   yarn4-local             yarn 4.19.0, `.yarnrc.yml` with `enableGlobalCache: false`: the cache in the project.
#   yarn3                   yarn 3.8.7, `.yarnrc.yml` with `nodeLinker: node-modules`: the cache in the project.
#   yarn2                   yarn 2.4.2, the same.
#   yarn4-corepack          `yarn4` through corepack's `yarn`, first on `PATH`.
# Each yarn fixture ignores `.yarn/*` but for its committed folders and `.pnp.*`, as yarn's own `.gitignore` does.
#
# Each fixture is a git repository whose `origin`, `git@github.com:owl629/<fixture>.git`, is served by a local bare
# remote through `core.sshCommand`, so nothing reaches GitHub. Owlshift's folders are moved into a scratch folder
# (`OWLSHIFT_CONFIG_DIR`, `OWLSHIFT_DATA_DIR`, `OWLSHIFT_CACHE_DIR`), and git's global configuration with
# `GIT_CONFIG_GLOBAL`: the person's own are left as they are. The lock files are made first, unconfined, with a home
# of the fixture's own in that folder.
#
# Usage: TOOLS=DIR [STACK_EDIT=SED] bash owl-629.sh OWLSHIFT_BINARY [FIXTURE...]   (network needed)
#   TOOLS: a folder holding the tool chains, each installed with `npm install -g --prefix "$TOOLS/<name>"`: `pnpm9`
#   (pnpm@9.15.9), `pnpm11` (pnpm@11.28.13), `pnpm12` (pnpm@12.21.0), `yarn4` (@yarnpkg/cli-dist@4.19.0), `yarn3`
#   (@yarnpkg/cli-dist@3.8.7), `yarn2` (@yarnpkg/cli-dist@2.4.2), `corepack` (corepack@0.36.0); never under `/tmp`,
#   which the sandbox closes. The machine's own pnpm 10 is the one on `PATH` without it.
#   STACK_EDIT: a sed script applied to the `owlshift.toml` `init` wrote, for an experiment the results say.
# It writes only under a temporary folder, removed at the end, unless KEEP=1. It prints results only, the home shown
# as `~`.
set -u
bin=$(cd "$(dirname "$1")" && pwd -P)/$(basename "$1"); shift
tools=$(cd "${TOOLS:?TOOLS names the tool chains}" && pwd -P)
home=$(cd "$HOME" && pwd -P)
work=$(mktemp -d "${TMPDIR:-/tmp}/owl629.XXXXXX"); work=$(cd "$work" && pwd -P)
export OWLSHIFT_CONFIG_DIR=$work/config OWLSHIFT_DATA_DIR=$work/data OWLSHIFT_CACHE_DIR=$work/cache
export GIT_CONFIG_GLOBAL=$work/gitconfig
say() { printf '%s %s\n' "$(date -u +%H:%M:%SZ)" "$*"; }
clean() { sed "s|$home|~|g; s|$work|<work>|g; s|$tools|<tools>|g; s/^/  | /"; }
if [ "$(uname -s)" = Darwin ]; then
  base_path=/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin
  system="macOS $(sw_vers -productVersion) ($(sw_vers -buildVersion)) $(uname -m)"
else
  base_path=/opt/node/bin:/usr/local/bin:/usr/bin:/bin
  system="$(. /etc/os-release && echo "$PRETTY_NAME") $(uname -m), $(bwrap --version), uid $(id -u)"
fi
fixtures=${*:-pnpm9 pnpm10 pnpm11 pnpm12 pnpm12-nopin pnpm9-on10 pnpm12-switch pnpm9-corepack yarn4 yarn4-pnp yarn4-norc \
yarn4-local yarn3 yarn2 yarn4-corepack}

mkdir -p "$work/config" "$work/ssh/owl629" "$work/seed"
printf '#!/bin/sh\n# The git command an ssh host would run, run here.\ncd %s && exec sh -c "$2"\n' "'$work/ssh'" >"$work/local-ssh"
chmod +x "$work/local-ssh"
# From the scratch folder: in the Linux rig the checkout's `.git` names a folder of the host, and git refuses to start
# there, even for `--global`.
for kv in "core.sshCommand $work/local-ssh" "ssh.variant simple" "user.name Probe" "user.email probe@example.com" \
  "init.defaultBranch main" "commit.gpgsign false"; do
  (cd "$work" && PATH=$base_path git config --global "${kv%% *}" "${kv#* }")
done

say "$system; $("$bin" --version | head -1); node $(PATH=$base_path node --version); \
pnpm $(PATH=$base_path pnpm --version 2>/dev/null) on PATH without TOOLS"

# The tool chain folder first on `PATH` for a fixture (`-`: the machine's), and the manager its `packageManager` names
# (`-`: none).
tool() {
  case $1 in
  pnpm9) echo "pnpm9 pnpm@9.15.9" ;;
  pnpm11) echo "pnpm11 pnpm@11.28.13" ;;
  pnpm12) echo "pnpm12 pnpm@12.21.0" ;;
  pnpm12-nopin) echo "pnpm12 -" ;;
  pnpm12-switch) echo "pnpm12 pnpm@12.20.0" ;;
  pnpm10) echo "- pnpm@10.33.2" ;;
  pnpm9-on10) echo "- pnpm@9.15.9" ;;
  pnpm9-corepack) echo "corepack pnpm@9.15.9" ;;
  yarn4 | yarn4-pnp | yarn4-norc | yarn4-local) echo "yarn4 yarn@4.19.0" ;;
  yarn3) echo "yarn3 yarn@3.8.7" ;;
  yarn2) echo "yarn2 yarn@2.4.2" ;;
  yarn4-corepack) echo "corepack yarn@4.19.0" ;;
  esac
}

# unconfined COMMAND...: a tool run outside `owlshift gate`, with a home of the fixture's own, so that the person's own
# home gets nothing: pnpm switching to the version a project names, corepack and yarn all write there.
unconfined() {
  mkdir -p "$work/seed/$fx-home/run"
  env HOME="$work/seed/$fx-home" XDG_RUNTIME_DIR="$work/seed/$fx-home/run" "$@"
}

# fixture FIXTURE DIR: the project's files, its lock made unconfined into a throwaway cache.
fixture() {
  local dir=$2 folder manager
  read -r folder manager <<<"$(tool "$1")"
  mkdir -p "$dir"
  local pin=''
  [ "$manager" = - ] || pin=$(printf '\n  "packageManager": "%s",' "$manager")
  printf '{\n  "name": "probe",\n  "version": "1.0.0",\n  "private": true,%s\n  "scripts": {\n    "test": "node test.js"\n  },\n  "dependencies": {\n    "is-number": "7.0.0",\n    "is-odd": "3.0.1"\n  }\n}\n' \
    "$pin" >"$dir/package.json"
  printf "if (!require('is-odd')(3)) process.exit(1);\nconsole.log('3 is odd');\n" >"$dir/test.js"
  case $1 in
  pnpm*)
    printf 'node_modules/\n' >"$dir/.gitignore"
    (cd "$dir" && unconfined pnpm install --lockfile-only >/dev/null 2>&1) || say "pnpm could not lock" ;;
  yarn*)
    printf '.yarn/*\n!.yarn/patches\n!.yarn/plugins\n!.yarn/releases\n!.yarn/sdks\n!.yarn/versions\n.pnp.*\nnode_modules/\n' \
      >"$dir/.gitignore"
    case $1 in
    yarn4-pnp) printf 'nodeLinker: pnp\n' >"$dir/.yarnrc.yml" ;;
    yarn4-norc) ;;
    yarn4-local) printf 'enableGlobalCache: false\nnodeLinker: node-modules\n' >"$dir/.yarnrc.yml" ;;
    *) printf 'nodeLinker: node-modules\n' >"$dir/.yarnrc.yml" ;;
    esac
    # yarn 2 has no `--mode`: it installs, and what the install leaves is removed below.
    local mode=--mode=update-lockfile
    [ "$1" = yarn2 ] && mode=
    (cd "$dir" && unconfined yarn install $mode >/dev/null 2>&1 \
      && rm -rf node_modules .yarn/cache .yarn/install-state.gz .pnp.cjs .pnp.loader.mjs) || say "yarn could not lock" ;;
  esac
}

# gate LABEL DIR: commits and pushes the checkout, then runs `owlshift gate`, printing its verdict and its end.
gate() {
  local label=$1 dir=$2 out rc start
  (cd "$dir" && git add -A && git commit -q -m "$label" && git push -q origin main) >/dev/null 2>&1
  start=$(date +%s)
  out=$(cd "$dir" && "$bin" gate </dev/null 2>&1); rc=$?
  say "$label: owlshift gate exit $rc in $(($(date +%s) - start)) s"
  out=$(printf '%s\n' "$out" | grep -v '^Its whole output goes to\|^Gate log: ')
  if printf '%s\n' "$out" | grep -q '^Gate red'; then
    # The failing command's end and the verdict, and the folder the sandbox refused when it names one.
    printf '%s\n' "$out" | grep -B"${TAIL:-6}" '^Gate red' | clean
    printf '%s\n' "$out" | grep -A1 '^The sandbox closes your home' | tail -1 | clean
  else
    printf '%s\n' "$out" | tail -"${TAIL:-8}" | clean
  fi
}

for fx in $fixtures; do
  say "--- $fx"
  read -r folder _ <<<"$(tool "$fx")"
  if [ "$folder" = - ]; then export PATH=$base_path; else export PATH=$tools/$folder/bin:$base_path; fi
  dir=$work/src/$fx
  fixture "$fx" "$dir"
  manager=${fx%%[0-9]*}
  say "$manager $(cd "$dir" && unconfined "$manager" --version 2>&1 | tail -1) from $(command -v "$manager" | clean | sed 's/^  | //')"
  git init -q --bare "$work/ssh/owl629/$fx.git"
  (cd "$dir" && git init -q && git remote add origin "git@github.com:owl629/$fx.git" && git add -A \
    && git commit -q -m fixture && git push -q origin main) >/dev/null 2>&1 || say "the fixture could not be pushed"
  (cd "$dir" && "$bin" init --tracker markdown --skip-secrets </dev/null 2>&1 | grep -E '^(Detected|  Note|Skipped)') | clean
  # The Markdown tracker's example ticket stays out of the commits, as Owlshift commits none of its files.
  printf 'tickets/\n' >>"$dir/.git/info/exclude"
  if [ -n "${STACK_EDIT:-}" ]; then
    # An experiment: a sed script applied to the `owlshift.toml` `init` wrote, said in the results.
    sed "$STACK_EDIT" "$dir/owlshift.toml" >"$dir/owlshift.toml.new" && mv "$dir/owlshift.toml.new" "$dir/owlshift.toml"
    say "edited with: $STACK_EDIT"
  fi
  say "the [stack] written:"
  sed -n '/^\[stack\]/,/^# The files agents receive/p' "$dir/owlshift.toml" \
    | grep -E '^(gate|setup|caches|run_caches|\]|    |# read from|# note:)' | clean
  cp "$dir/owlshift.toml" "$work/owlshift.toml.$fx"
  gate "$fx, as written" "$dir"
  say "the run caches: $(cd "$work/cache/github.com/owl629/$fx" 2>/dev/null && du -sk -- * 2>/dev/null \
    | awk '{ printf "%s %s, ", $2, ($1 < 1024 ? $1 " kB" : sprintf("%.0f MB", $1 / 1024)) }')"
  # Each run cache removed alone: the others stay, filled by the run above.
  for variable in $(sed -n 's/^run_caches\.\([A-Za-z_0-9]*\) = .*/\1/p' "$work/owlshift.toml.$fx"); do
    awk -v v="$variable" 'index($0, "run_caches." v " = ") != 1' "$work/owlshift.toml.$fx" >"$dir/owlshift.toml"
    gate "$fx, without run_caches.$variable" "$dir"
  done
  cp "$work/owlshift.toml.$fx" "$dir/owlshift.toml"
done

if [ -n "${KEEP:-}" ]; then say "kept the scratch folder $work"; exit 0; fi
chmod -R u+w "$work" 2>/dev/null
rm -rf "$work"
say "removed the scratch folder"
