#!/bin/bash
# OWL-628 (runtime and operations, "More stacks"), on macOS directly and on Linux through owl-628-linux.sh: for each
# ecosystem `owlshift init` writes lines for since OWL-628 (Elixir; Deno, with and without its lock file), a fixture
# repository whose dependencies are in no cache of the machine, its `[stack]` written by `owlshift init` and left
# unedited, then the real `owlshift gate`, confined: once as `init` wrote it, then once per `run_caches` line, that
# line alone removed. The `dart` and `swift` fixtures, for which `init` writes a note only, serve the experiments
# with `STACK_EDIT` the record gives. The harness is owl-627.sh's (OWL-627), with this slice's fixtures; the Markdown
# tracker's example ticket is kept out of the commits.
#
# Each fixture is a git repository whose `origin`, `git@github.com:owl628/<ecosystem>.git`, is served by a local bare
# remote through `core.sshCommand`, so nothing reaches GitHub. Owlshift's folders are moved into a scratch folder
# (`OWLSHIFT_CONFIG_DIR`, `OWLSHIFT_DATA_DIR`, `OWLSHIFT_CACHE_DIR`), and git's global configuration with
# `GIT_CONFIG_GLOBAL`: the person's own are left as they are. The lock files are made first, unconfined, with their
# tools' caches in that folder.
#
# Usage: [EXTRA_PATH=DIRS] [MIX_SEED=DIR] [CACHE_SEED=DIR] [STACK_EDIT=SED] bash owl-628.sh OWLSHIFT_BINARY
#   [ECOSYSTEM...]   (network needed)
#   EXTRA_PATH: folders put first on `PATH`, such as tool chains unpacked for the check, never under `/tmp`, which
#   the sandbox closes.
#   MIX_SEED: a Mix home holding Hex, used to lock the Elixir fixture unconfined when the network is too slow for
#   `mix local.hex`'s deadline.
#   CACHE_SEED: a folder holding, per ecosystem, run cache folders an earlier run filled, copied in before the first
#   gate (a retry when the network stopped a setup at its deadline).
#   STACK_EDIT: a sed script applied to the `owlshift.toml` `init` wrote, for an experiment the results say.
# It writes only under a temporary folder, removed at the end, unless KEEP=1. It prints results only, the home shown
# as `~`.
set -u
bin=$(cd "$(dirname "$1")" && pwd -P)/$(basename "$1"); shift
home=$(cd "$HOME" && pwd -P)
work=$(mktemp -d "${TMPDIR:-/tmp}/owl628.XXXXXX"); work=$(cd "$work" && pwd -P)
export OWLSHIFT_CONFIG_DIR=$work/config OWLSHIFT_DATA_DIR=$work/data OWLSHIFT_CACHE_DIR=$work/cache
export GIT_CONFIG_GLOBAL=$work/gitconfig
say() { printf '%s %s\n' "$(date -u +%H:%M:%SZ)" "$*"; }
clean() { sed "s|$home|~|g; s|$work|<work>|g; s/^/  | /"; }
if [ "$(uname -s)" = Darwin ]; then
  base_path=${EXTRA_PATH:+$EXTRA_PATH:}/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin
  system="macOS $(sw_vers -productVersion) ($(sw_vers -buildVersion)) $(uname -m)"
else
  base_path=${EXTRA_PATH:+$EXTRA_PATH:}/usr/local/bin:/usr/bin:/bin
  system="$(. /etc/os-release && echo "$PRETTY_NAME") $(uname -m), $(bwrap --version), uid $(id -u)"
fi
ecosystems=${*:-elixir deno deno-nolock}
export PATH=$base_path

mkdir -p "$work/config" "$work/ssh/owl628" "$work/seed"
printf '#!/bin/sh\n# The git command an ssh host would run, run here.\ncd %s && exec sh -c "$2"\n' "'$work/ssh'" >"$work/local-ssh"
chmod +x "$work/local-ssh"
for kv in "core.sshCommand $work/local-ssh" "ssh.variant simple" "user.name Probe" "user.email probe@example.com" \
  "init.defaultBranch main" "commit.gpgsign false"; do
  git config --global "${kv%% *}" "${kv#* }"
done

say "$system; $("$bin" --version | head -1); $(elixir --version 2>/dev/null | tail -1); \
$(command -v dart >/dev/null && dart --version 2>&1 | head -1);$(swift --version 2>/dev/null | grep -o 'Swift version [^ ]*'); \
$(deno --version 2>/dev/null | head -1)"

# fixture ECOSYSTEM DIR: the project's files, its lock made unconfined into a throwaway cache.
fixture() {
  local dir=$2 seed=$work/seed/$1
  mkdir -p "$dir"
  case $1 in
  elixir)
    printf 'defmodule Probe.MixProject do\n  use Mix.Project\n\n  def project do\n    [app: :probe, version: "0.1.0", elixir: "~> 1.15", deps: deps()]\n  end\n\n  def application, do: [extra_applications: [:logger]]\n\n  defp deps, do: [{:decimal, "2.3.0"}]\nend\n' >"$dir/mix.exs"
    printf '[\n  inputs: ["{mix,.formatter}.exs", "{config,lib,test}/**/*.{ex,exs}"]\n]\n' >"$dir/.formatter.exs"
    mkdir -p "$dir/lib" "$dir/test"
    printf 'defmodule Probe do\n  def word, do: Decimal.to_string(Decimal.new("1.50"))\nend\n' >"$dir/lib/probe.ex"
    printf 'ExUnit.start()\n' >"$dir/test/test_helper.exs"
    printf 'defmodule ProbeTest do\n  use ExUnit.Case\n\n  test "word" do\n    assert Probe.word() == "1.50"\n  end\nend\n' >"$dir/test/probe_test.exs"
    printf '/_build/\n/deps/\n' >"$dir/.gitignore"
    # Mix fetches Hex with a 60 s deadline, past which a slow network leaves it: MIX_SEED, a Mix home holding Hex,
    # serves the lock.
    [ -d "${MIX_SEED:-/nonexistent}" ] && cp -R "$MIX_SEED" "$seed-mix"
    (cd "$dir" && MIX_HOME=$seed-mix HEX_HOME=$seed mix local.hex --force --if-missing >/dev/null 2>&1 \
      && MIX_HOME=$seed-mix HEX_HOME=$seed mix deps.get >/dev/null 2>&1 && rm -rf deps _build) \
      || say "mix could not lock" ;;
  dart)
    printf 'name: probe\nenvironment:\n  sdk: ^3.5.0\ndependencies:\n  path: 1.9.1\ndev_dependencies:\n  test: ^1.25.0\n' \
      >"$dir/pubspec.yaml"
    mkdir -p "$dir/lib" "$dir/test"
    printf "import 'package:path/path.dart' as p;\n\nString word() => p.basename('/probe/Probe');\n" >"$dir/lib/probe.dart"
    printf "import 'package:probe/probe.dart';\nimport 'package:test/test.dart';\n\nvoid main() {\n  test('word', () => expect(word(), 'Probe'));\n}\n" \
      >"$dir/test/probe_test.dart"
    printf '.dart_tool/\n' >"$dir/.gitignore"
    (cd "$dir" && PUB_CACHE=$seed dart --disable-analytics >/dev/null 2>&1; PUB_CACHE=$seed dart pub get >/dev/null 2>&1) \
      || say "dart could not lock" ;;
  swift)
    printf '// swift-tools-version:6.0\nimport PackageDescription\n\nlet package = Package(\n    name: "Probe",\n    dependencies: [\n        .package(url: "https://github.com/JohnSundell/Files", exact: "4.2.0"),\n    ],\n    targets: [\n        .target(name: "Probe", dependencies: ["Files"]),\n        .testTarget(name: "ProbeTests", dependencies: ["Probe"]),\n    ]\n)\n' \
      >"$dir/Package.swift"
    mkdir -p "$dir/Sources/Probe" "$dir/Tests/ProbeTests"
    printf 'import Files\n\npublic func word() -> String {\n    _ = Folder.self\n    return "Probe"\n}\n' \
      >"$dir/Sources/Probe/Probe.swift"
    printf 'import Testing\n@testable import Probe\n\n@Test func probeWord() {\n    #expect(word() == "Probe")\n}\n' \
      >"$dir/Tests/ProbeTests/ProbeTests.swift"
    printf '.build/\n.swiftpm/\n' >"$dir/.gitignore"
    (cd "$dir" && swift package --cache-path "$seed/cache" --config-path "$seed/config" \
      --security-path "$seed/security" resolve >/dev/null 2>&1 && rm -rf .build) || say "swift could not resolve" ;;
  deno | deno-nolock)
    printf '{\n  "imports": {\n    "@std/assert": "jsr:@std/assert@1.0.13"\n  }\n}\n' >"$dir/deno.json"
    printf 'export function word(): string {\n  return "Probe";\n}\n' >"$dir/main.ts"
    printf 'import { assertEquals } from "@std/assert";\nimport { word } from "./main.ts";\n\nDeno.test("word", () => {\n  assertEquals(word(), "Probe");\n});\n' \
      >"$dir/main_test.ts"
    # `deno-nolock`: the same project without its lock file, which the tests must not write.
    if [ "$1" = deno ]; then
      (cd "$dir" && DENO_DIR=$seed deno install >/dev/null 2>&1) || say "deno could not lock"
    fi ;;
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

for eco in $ecosystems; do
  say "--- $eco"
  dir=$work/src/$eco
  fixture "$eco" "$dir"
  git init -q --bare "$work/ssh/owl628/$eco.git"
  (cd "$dir" && git init -q && git remote add origin "git@github.com:owl628/$eco.git" && git add -A \
    && git commit -q -m fixture && git push -q origin main) >/dev/null 2>&1 || say "the fixture could not be pushed"
  (cd "$dir" && "$bin" init --tracker markdown --skip-secrets </dev/null 2>&1 | grep -E '^(Detected|  Note|Skipped)') | clean
  # The Markdown tracker's example ticket stays out of the commits, as Owlshift commits none of its files: `deno fmt`
  # would otherwise check its Markdown.
  printf 'tickets/\n' >>"$dir/.git/info/exclude"
  if [ -n "${STACK_EDIT:-}" ]; then
    # An experiment: a sed script applied to the `owlshift.toml` `init` wrote, said in the results.
    sed "$STACK_EDIT" "$dir/owlshift.toml" >"$dir/owlshift.toml.new" && mv "$dir/owlshift.toml.new" "$dir/owlshift.toml"
    say "edited with: $STACK_EDIT"
  fi
  say "the [stack] written:"
  sed -n '/^\[stack\]/,/^# The files agents receive/p' "$dir/owlshift.toml" \
    | grep -E '^(gate|setup|caches|run_caches|\]|    |# read from|# note:)' | clean
  cp "$dir/owlshift.toml" "$work/owlshift.toml.$eco"
  if [ -d "${CACHE_SEED:-/nonexistent}/$eco" ]; then
    # A retry on a slow network: the run caches start with what an earlier run of this script downloaded into them.
    mkdir -p "$work/cache/github.com/owl628" && cp -R "$CACHE_SEED/$eco" "$work/cache/github.com/owl628/$eco"
    say "the run caches seeded from an earlier run: $(du -sk "$work/cache/github.com/owl628/$eco" | awk '{print $1}') kB"
  fi
  gate "$eco, as written" "$dir"
  say "the run caches: $(cd "$work/cache/github.com/owl628/$eco" 2>/dev/null && du -sk -- * 2>/dev/null \
    | awk '{ printf "%s %s, ", $2, ($1 < 1024 ? $1 " kB" : sprintf("%.0f MB", $1 / 1024)) }')"
  [ -n "${SAVE_CACHES:-}" ] && mkdir -p "$SAVE_CACHES" && rm -rf "${SAVE_CACHES:?}/$eco" \
    && cp -R "$work/cache/github.com/owl628/$eco" "$SAVE_CACHES/$eco" 2>/dev/null
  # Each run cache removed alone: the others stay, filled by the run above.
  for variable in $(sed -n 's/^run_caches\.\([A-Za-z_0-9]*\) = .*/\1/p' "$work/owlshift.toml.$eco"); do
    awk -v v="$variable" 'index($0, "run_caches." v " = ") != 1' "$work/owlshift.toml.$eco" >"$dir/owlshift.toml"
    gate "$eco, without run_caches.$variable" "$dir"
  done
  cp "$work/owlshift.toml.$eco" "$dir/owlshift.toml"
done

if [ -n "${KEEP:-}" ]; then say "kept the scratch folder $work"; exit 0; fi
chmod -R u+w "$work" 2>/dev/null
rm -rf "$work"
say "removed the scratch folder"
