#!/bin/bash
# OWL-624 (runtime and operations, "Stack detection (P9)" and "Caches a run may write"), on macOS directly and on
# Linux through owl-624-linux.sh: for each ecosystem `owlshift init` detects since OWL-624 (Go, poetry, pip, bundler,
# Gradle), a fixture repository whose dependencies are in no cache of the machine, its `[stack]` written by
# `owlshift init`, then the real `owlshift gate`, confined: once as `init` wrote it, then once per `run_caches` line,
# that line alone removed; for Go, once more with a file `gofmt` would change.
#
# Each fixture is a git repository whose `origin`, `git@github.com:owl624/<ecosystem>.git`, is served by a local bare
# remote through `core.sshCommand` (as the bench's `step_child::SERVED_URL`), so nothing reaches GitHub. Owlshift's
# folders are moved into a scratch folder (`OWLSHIFT_CONFIG_DIR`, `OWLSHIFT_DATA_DIR`, `OWLSHIFT_CACHE_DIR`), and git's
# global configuration with `GIT_CONFIG_GLOBAL`: the person's own are left as they are. The lock files are made first,
# unconfined, into throwaway caches in that folder. `bundler-pinned` is `bundler` with a lock file naming a newer
# bundler than the machine's (`BUNDLED WITH 2.5.23`), which bundler 2.3 and later install before anything else.
# `poetry-home`, on macOS, runs `poetry`'s fixture with the person's pipx poetry, `~/.local/bin`, first on `PATH`.
# `poetry-pyenv`, on macOS, is `poetry` with `caches = [".pyenv"]` declared and allowed, as `owlshift gate` proposes
# on a machine whose pyenv, in the home, poetry 2 lists while it looks for a Python.
#
# Usage: [CACHE_SEED=DIR] [CONCURRENT=1] bash owl-624.sh OWLSHIFT_BINARY [hand|init] [ECOSYSTEM...]   (network needed)
#   CACHE_SEED: a folder holding, per ecosystem, run cache folders an earlier run filled, copied in before the first
#   gate (a retry when the network stopped a setup at its deadline).
#   hand: the `[stack]` this script holds for each ecosystem replaces the detected one (the check before the code);
#   init: `init`'s own lines, unedited (the default).
#   CONCURRENT=1: after each ecosystem's runs, two gates at once on its emptied run caches, then a third (`pair`).
# It writes only under a temporary folder, removed at the end. It prints results only, the home shown as `~`.
set -u
bin=$(cd "$(dirname "$1")" && pwd -P)/$(basename "$1"); shift
mode=${1:-init}; [ $# -gt 0 ] && shift
home=$(cd "$HOME" && pwd -P)
work=$(mktemp -d "${TMPDIR:-/tmp}/owl624.XXXXXX"); work=$(cd "$work" && pwd -P)
export OWLSHIFT_CONFIG_DIR=$work/config OWLSHIFT_DATA_DIR=$work/data OWLSHIFT_CACHE_DIR=$work/cache
export GIT_CONFIG_GLOBAL=$work/gitconfig
say() { printf '%s %s\n' "$(date -u +%H:%M:%SZ)" "$*"; }
clean() { sed "s|$home|~|g; s|$work|<work>|g; s/^/  | /"; }
if [ "$(uname -s)" = Darwin ]; then
  base_path=${EXTRA_PATH:+$EXTRA_PATH:}/opt/homebrew/opt/openjdk/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin
  ecosystems=${*:-go poetry poetry-pyenv poetry-home pip bundler gradle}
  system="macOS $(sw_vers -productVersion) ($(sw_vers -buildVersion)) $(uname -m)"
else
  base_path=/usr/local/go/bin:/opt/gradle/bin:/usr/local/bin:/usr/bin:/bin
  ecosystems=${*:-go poetry pip bundler bundler-pinned gradle}
  system="$(. /etc/os-release && echo "$PRETTY_NAME") $(uname -m), $(bwrap --version), uid $(id -u)"
fi
export PATH=$base_path

mkdir -p "$work/config" "$work/ssh/owl624" "$work/seed"
printf '#!/bin/sh\n# The git command an ssh host would run, run here.\ncd %s && exec sh -c "$2"\n' "'$work/ssh'" >"$work/local-ssh"
chmod +x "$work/local-ssh"
for kv in "core.sshCommand $work/local-ssh" "ssh.variant simple" "user.name Probe" "user.email probe@example.com" \
  "init.defaultBranch main" "commit.gpgsign false"; do
  git config --global "${kv%% *}" "${kv#* }"
done

say "$system; $("$bin" --version | head -1); $(go version); $(poetry --version 2>/dev/null); $(python3 --version); \
pip $(python3 -m pip --version | cut -d' ' -f2); $(ruby --version | cut -d' ' -f1-2); \
bundler $(bundle --version | awk '{print $NF}'); $(java -version 2>&1 | head -1); $(gradle --version 2>/dev/null | grep '^Gradle')"

# fixture ECOSYSTEM DIR: the project's files, its lock made unconfined into a throwaway cache.
fixture() {
  local dir=$2 seed=$work/seed/$1
  mkdir -p "$dir"
  case $1 in
  go)
    printf 'module example.com/probe\n\ngo 1.22\n\nrequire github.com/dustin/go-humanize v1.0.1\n' >"$dir/go.mod"
    printf 'package main\n\nimport (\n\t"fmt"\n\n\t"github.com/dustin/go-humanize"\n)\n\nfunc size() string { return humanize.Bytes(1024) }\n\nfunc main() { fmt.Println(size()) }\n' >"$dir/main.go"
    printf 'package main\n\nimport "testing"\n\nfunc TestSize(t *testing.T) {\n\tif size() != "1.0 kB" {\n\t\tt.Fatal(size())\n\t}\n}\n' >"$dir/main_test.go"
    (cd "$dir" && GOMODCACHE=$seed GOCACHE=$seed-build GOFLAGS=-modcacherw go mod tidy >/dev/null 2>&1) ;;
  poetry | poetry-home | poetry-pyenv)
    printf '[tool.poetry]\nname = "probe"\nversion = "0.1.0"\ndescription = ""\nauthors = ["Probe <probe@example.com>"]\n\n[tool.poetry.dependencies]\npython = "^3.10"\nhumanize = "4.11.0"\n\n[tool.poetry.group.dev.dependencies]\npytest = "8.3.4"\nruff = "0.8.4"\n\n[tool.ruff]\nline-length = 100\n\n[tool.pytest.ini_options]\ntestpaths = ["tests"]\n\n[build-system]\nrequires = ["poetry-core>=1.0"]\nbuild-backend = "poetry.core.masonry.api"\n' >"$dir/pyproject.toml"
    mkdir -p "$dir/probe" "$dir/tests"
    printf 'import humanize\n\n\ndef size() -> str:\n    return humanize.naturalsize(1024)\n' >"$dir/probe/__init__.py"
    printf 'from probe import size\n\n\ndef test_size():\n    assert size() == "1.0 kB"\n' >"$dir/tests/test_probe.py"
    printf '__pycache__/\n' >"$dir/.gitignore"
    pytest_only "$dir/pyproject.toml"
    (cd "$dir" && POETRY_CACHE_DIR=$seed poetry lock -q >/dev/null 2>&1) ;;
  pip)
    printf 'humanize==4.11.0\n' >"$dir/requirements.txt"
    printf 'pytest==8.3.4\nruff==0.8.4\n' >"$dir/requirements-dev.txt"
    printf '[tool.ruff]\nline-length = 100\n\n[tool.pytest.ini_options]\ntestpaths = ["tests"]\npythonpath = ["."]\n' >"$dir/pyproject.toml"
    mkdir -p "$dir/tests"
    printf 'import humanize\n\n\ndef size() -> str:\n    return humanize.naturalsize(1024)\n' >"$dir/probe.py"
    printf 'from probe import size\n\n\ndef test_size():\n    assert size() == "1.0 kB"\n' >"$dir/tests/test_probe.py"
    printf '__pycache__/\n' >"$dir/.gitignore"
    pytest_only "$dir/pyproject.toml" "$dir/requirements-dev.txt" ;;
  bundler | bundler-pinned)
    gem_mirror
    printf 'source "%s"\n\ngem "rainbow", "3.1.1"\ngem "rspec", "3.13.0"\n' "$gem_source" >"$dir/Gemfile"
    printf -- '--require spec_helper\n' >"$dir/.rspec"
    mkdir -p "$dir/lib" "$dir/spec"
    printf 'require "rainbow"\n\nmodule Probe\n  def self.word\n    Rainbow("probe").to_s\n  end\nend\n' >"$dir/lib/probe.rb"
    printf '$LOAD_PATH.unshift(File.expand_path("../lib", __dir__))\n' >"$dir/spec/spec_helper.rb"
    printf 'require "probe"\n\nRSpec.describe Probe do\n  it("says probe") { expect(Probe.word).to eq("probe") }\nend\n' >"$dir/spec/probe_spec.rb"
    (cd "$dir" && BUNDLE_PATH=$seed BUNDLE_USER_HOME=$seed-home bundle lock >/dev/null 2>&1)
    if [ "$1" = bundler-pinned ]; then
      awk 'pinned { print "   2.5.23"; pinned = 0; next } /^BUNDLED WITH/ { pinned = 1 } { print }' "$dir/Gemfile.lock" \
        >"$dir/Gemfile.lock.new" && mv "$dir/Gemfile.lock.new" "$dir/Gemfile.lock"
    fi ;;
  gradle)
    printf 'rootProject.name = "probe"\n' >"$dir/settings.gradle.kts"
    printf 'plugins {\n    java\n}\n\nrepositories {\n    mavenCentral()\n}\n\ndependencies {\n    implementation("org.apache.commons:commons-lang3:3.17.0")\n    testImplementation("junit:junit:4.13.2")\n}\n' >"$dir/build.gradle.kts"
    mkdir -p "$dir/src/main/java/probe" "$dir/src/test/java/probe"
    printf 'package probe;\n\nimport org.apache.commons.lang3.StringUtils;\n\npublic class Probe {\n    public static String word() {\n        return StringUtils.capitalize("probe");\n    }\n}\n' >"$dir/src/main/java/probe/Probe.java"
    printf 'package probe;\n\nimport static org.junit.Assert.assertEquals;\n\nimport org.junit.Test;\n\npublic class ProbeTest {\n    @Test\n    public void word() {\n        assertEquals("Probe", Probe.word());\n    }\n}\n' >"$dir/src/test/java/probe/ProbeTest.java"
    printf '.gradle/\nbuild/\n' >"$dir/.gitignore"
    (cd "$dir" && GRADLE_USER_HOME=$seed gradle wrapper -q --no-daemon >/dev/null 2>&1)
    gradle_dist "$dir" ;;
  esac
}

# pytest_only FILE...: with PY_TOOLS=pytest, the Python fixture without ruff, whose 10 MB wheel a slow network can hold
# past the setup's deadline: its requirement and its `[tool.ruff]` table removed, so `init` writes the pytest line alone.
pytest_only() {
  [ "${PY_TOOLS:-}" = pytest ] || return 0
  local f
  for f in "$@"; do
    sed -i.bak -e '/^ruff/d' -e '/^\[tool\.ruff\]/,/^$/d' "$f" && rm -f "$f.bak"
  done
}

# gem_mirror: the fixture's gems, each fetched once from rubygems.org by name, served on the loopback with the index
# `gem generate_index` builds (its `builder` gem installed in the scratch folder), so that rubygems.org's 25 MB index
# is not fetched by every install on a slow network: bundler installs them over HTTP into BUNDLE_PATH all the same.
# GEM_SOURCE=remote keeps rubygems.org.
gem_mirror() {
  if [ "${GEM_SOURCE:-local}" = remote ]; then gem_source=https://rubygems.org; return; fi
  [ -n "${gem_source:-}" ] && return
  local m=$work/gems g port
  mkdir -p "$m/gems"
  for g in builder-3.3.0 rainbow-3.1.1 rspec-3.13.0 rspec-core-3.13.0 rspec-expectations-3.13.0 rspec-mocks-3.13.0 \
    rspec-support-3.13.0 diff-lcs-1.5.1 bundler-2.5.23; do
    curl -sSfL --retry 3 -o "$m/gems/$g.gem" "https://rubygems.org/gems/$g.gem" || say "could not fetch $g.gem"
  done
  GEM_HOME=$work/builder GEM_PATH=$work/builder gem install --local --no-document "$m/gems/builder-3.3.0.gem" >/dev/null
  rm "$m/gems/builder-3.3.0.gem"
  GEM_HOME=$work/builder GEM_PATH=$work/builder gem generate_index --directory "$m" >/dev/null 2>&1 \
    || say "gem generate_index failed"
  port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
  python3 -m http.server --bind 127.0.0.1 --directory "$m" "$port" >/dev/null 2>&1 &
  gem_served=$!
  gem_source=http://127.0.0.1:$port
  say "the fixture's gems served at $gem_source/"
}

# gradle_dist DIR: points the wrapper of the fixture in DIR at the machine's own Gradle distribution, zipped as
# gradle.org ships it and served on the loopback, so that a slow network does not hold the 130 MB download past the
# gate's 30 minutes. The wrapper still downloads it over HTTP into GRADLE_USER_HOME; the dependencies come from Maven
# Central. GRADLE_DIST=remote keeps gradle.org's URL.
gradle_dist() {
  [ "${GRADLE_DIST:-local}" = remote ] && return
  local props=$1/gradle/wrapper/gradle-wrapper.properties version gradle_home port
  version=$(sed -n 's/^distributionUrl=.*gradle-\(.*\)-bin\.zip$/\1/p' "$props")
  gradle_home=$(cd "$(dirname "$(readlink -f "$(command -v gradle)")")/.." && pwd -P)
  [ -d "$gradle_home/libexec/lib" ] && gradle_home=$gradle_home/libexec
  mkdir -p "$work/dist/stage"
  if [ ! -f "$work/dist/gradle-$version-bin.zip" ]; then
    ln -s "$gradle_home" "$work/dist/stage/gradle-$version"
    (cd "$work/dist/stage" && zip -qr "../gradle-$version-bin.zip" "gradle-$version")
    port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
    python3 -m http.server --bind 127.0.0.1 --directory "$work/dist" "$port" >/dev/null 2>&1 &
    served=$!
    echo "$port" >"$work/dist/port"
  fi
  port=$(cat "$work/dist/port")
  sed -i.bak "s|^distributionUrl=.*|distributionUrl=http\\\\://127.0.0.1\\\\:$port/gradle-$version-bin.zip|" "$props"
  rm -f "$props.bak"
  say "the wrapper's Gradle $version, from $gradle_home, served at http://127.0.0.1:$port/"
}

# The `[stack]` held for `hand`, one value a line: gate, setup (TOML arrays), then the run caches as `VARIABLE=folder`.
hand_stack() {
  case $1 in
  go) printf '%s\n' '["! gofmt -l . | grep .", "go vet ./...", "go test ./..."]' '["go mod download"]' \
    'GOMODCACHE=go-mod GOCACHE=go-build' ;;
  poetry | poetry-home | poetry-pyenv) printf '%s\n' '["poetry run ruff check .", "poetry run pytest"]' '["poetry install"]' \
    'POETRY_CACHE_DIR=poetry' ;;
  pip) printf '%s\n' '[".venv/bin/ruff check .", ".venv/bin/pytest"]' \
    "[\"python3 -m venv .venv && echo '*' > .venv/.gitignore\", \".venv/bin/pip install -r requirements.txt -r requirements-dev.txt\"]" \
    'PIP_CACHE_DIR=pip' ;;
  bundler | bundler-pinned) printf '%s\n' '["bundle exec rspec"]' '["bundle install"]' \
    'BUNDLE_PATH=bundle GEM_HOME=gems' ;;
  gradle) printf '%s\n' '["./gradlew check --no-daemon"]' '[]' 'GRADLE_USER_HOME=gradle' ;;
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
    printf '%s\n' "$out" | tail -"${TAIL:-14}" | clean
  fi
}

# pair LABEL DIR (CONCURRENT=1, OWL-632): two `owlshift gate` at once on one empty run cache tree, as two steps of one
# project install at once: each with its own data folder, since a gate holds the project's lock in it, both with the
# one cache folder; then a third gate alone, on what the two left. Skipped when the `[stack]` has no run cache.
pair() {
  local label=$1 dir=$2 tree=$work/cache/github.com/owl624/$eco side pids="" rc secs
  grep -q '^run_caches\.' "$dir/owlshift.toml" || return 0
  (cd "$dir" && git add -A && git commit -q -m "$label" && git push -q origin main) >/dev/null 2>&1
  chmod -R u+w "$tree" 2>/dev/null; rm -rf "$tree"
  for side in a b; do
    (cd "$dir" && start=$(date +%s) && OWLSHIFT_DATA_DIR=$work/data-$side "$bin" gate </dev/null >"$work/pair-$side" 2>&1
      echo "$? $(($(date +%s) - start))" >"$work/pair-$side.rc") &
    pids="$pids $!"
  done
  # Those two only: the loopback servers are this shell's children too.
  wait $pids
  for side in a b; do
    read -r rc secs <"$work/pair-$side.rc"
    say "$label, gate $side of two at once: owlshift gate exit $rc in $secs s"
    grep -B"${TAIL:-6}" '^Gate red' "$work/pair-$side" | clean
  done
  gate "$label, a third gate alone after them" "$dir"
}

for eco in $ecosystems; do
  say "--- $eco"
  dir=$work/src/$eco
  fixture "$eco" "$dir"
  git init -q --bare "$work/ssh/owl624/$eco.git"
  (cd "$dir" && git init -q && git remote add origin "git@github.com:owl624/$eco.git" && git add -A \
    && git commit -q -m fixture && git push -q origin main) >/dev/null 2>&1 || say "the fixture could not be pushed"
  (cd "$dir" && "$bin" init --tracker markdown --skip-secrets </dev/null 2>&1 | grep -E '^(Detected|  Note|Skipped)') | clean
  if [ "$mode" = hand ]; then
    { read -r g; read -r s; read -r r; } < <(hand_stack "$eco")
    caches=""
    for pair in $r; do caches="$caches${caches:+;}run_caches.${pair%%=*} = \"${pair#*=}\""; done
    awk -v g="$g" -v s="$s" -v r="$caches" '$0 == "gate = []" { print "gate = " g; next }
      /^# setup = / { print "setup = " s; next }
      /^# run_caches = / { n = split(r, l, ";"); for (i = 1; i <= n; i++) print l[i]; next } { print }' \
      "$dir/owlshift.toml" >"$dir/owlshift.toml.new" && mv "$dir/owlshift.toml.new" "$dir/owlshift.toml"
  fi
  say "the [stack] written:"
  sed -n '/^\[stack\]/,/^# The files agents receive/p' "$dir/owlshift.toml" \
    | grep -E '^(gate|setup|caches|run_caches|\]|    |# read from|# note:)' | clean
  cp "$dir/owlshift.toml" "$work/owlshift.toml.$eco"
  if [ "$eco" = poetry-pyenv ]; then
    # What `owlshift gate` proposes when poetry's search for a Python is refused `~/.pyenv`: the project declares it
    # read-only, and this machine allows it.
    awk '/^# caches = / { print "caches = [\".pyenv\"]"; next } { print }' "$work/owlshift.toml.$eco" >"$dir/owlshift.toml"
    printf '[repositories."github.com/owl624/%s"]\nallow_caches = [".pyenv"]\n' "$eco" >>"$work/config/config.toml"
    cp "$dir/owlshift.toml" "$work/owlshift.toml.$eco"
    say "with caches = [\".pyenv\"], allowed in the personal file"
  fi
  if [ "$eco" = poetry-home ]; then
    PATH="$home/.local/bin:$base_path" gate "$eco, as written, ~/.local/bin first on PATH ($(readlink "$home/.local/bin/poetry" | sed "s|$home|~|"))" "$dir"
    continue
  fi
  if [ -d "${CACHE_SEED:-/nonexistent}/$eco" ]; then
    # A retry on a slow network: the run caches start with what an earlier run of this script downloaded into them.
    mkdir -p "$work/cache/github.com/owl624" && cp -R "$CACHE_SEED/$eco" "$work/cache/github.com/owl624/$eco"
    say "the run caches seeded from an earlier run: $(du -sk "$work/cache/github.com/owl624/$eco" | awk '{print $1}') kB"
  fi
  gate "$eco, as written" "$dir"
  say "the run caches: $(cd "$work/cache/github.com/owl624/$eco" 2>/dev/null && du -sk -- * 2>/dev/null \
    | awk '{ printf "%s %s, ", $2, ($1 < 1024 ? $1 " kB" : sprintf("%.0f MB", $1 / 1024)) }')"
  case $eco in bundler*)
    # Where the bundler a lock pins went (OWL-632): in the run cache, or else into the lost home.
    say "bundler in the run cache: $(cd "$work/cache/github.com/owl624/$eco" 2>/dev/null \
      && find . -maxdepth 5 -type d -name 'bundler-[0-9]*' | tr '\n' ' ')" ;;
  esac
  # Each run cache removed alone: the others stay, filled by the run above.
  for variable in $(sed -n 's/^run_caches\.\([A-Za-z_0-9]*\) = .*/\1/p' "$work/owlshift.toml.$eco"); do
    awk -v v="$variable" 'index($0, "run_caches." v " = ") != 1' "$work/owlshift.toml.$eco" >"$dir/owlshift.toml"
    gate "$eco, without run_caches.$variable" "$dir"
  done
  cp "$work/owlshift.toml.$eco" "$dir/owlshift.toml"
  [ -n "${CONCURRENT:-}" ] && pair "$eco, as written" "$dir"
  if [ "$eco" = go ]; then
    # The gate's shell runs `! gofmt -l . | grep .`: red, naming the file, on a file gofmt would change.
    printf 'package main\n\nfunc  unformatted() {}\n' >"$dir/unformatted.go"
    gate "$eco, an unformatted file added" "$dir"
  fi
done

[ -n "${served:-}" ] && kill "$served" 2>/dev/null
[ -n "${gem_served:-}" ] && kill "$gem_served" 2>/dev/null
chmod -R u+w "$work" 2>/dev/null
rm -rf "$work"
say "removed the scratch folder"
