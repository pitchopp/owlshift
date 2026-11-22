#!/bin/bash
# OWL-627 (runtime and operations, "More stacks"), on macOS directly and on Linux through owl-627-linux.sh: for each
# ecosystem `owlshift init` detects since OWL-627 (pdm, Maven with and without its wrapper, Composer, .NET), a fixture
# repository whose dependencies are in no cache of the machine, its `[stack]` written by `owlshift init` and left
# unedited, then the real `owlshift gate`, confined: once as `init` wrote it, then once per `run_caches` line, that line
# alone removed. The harness is owl-624.sh's (OWL-624), without its other ecosystems' helpers.
#
# Each fixture is a git repository whose `origin`, `git@github.com:owl627/<ecosystem>.git`, is served by a local bare
# remote through `core.sshCommand`, so nothing reaches GitHub. Owlshift's folders are moved into a scratch folder
# (`OWLSHIFT_CONFIG_DIR`, `OWLSHIFT_DATA_DIR`, `OWLSHIFT_CACHE_DIR`), and git's global configuration with
# `GIT_CONFIG_GLOBAL`: the person's own are left as they are. The lock files and the wrapper are made first,
# unconfined, with their tools' caches in that folder. `pdm-pyenv` is `pdm` with `caches = [".pyenv"]` declared and
# allowed, as `owlshift gate` proposes on a machine whose pyenv, in the home, pdm lists while it looks for a Python.
#
# Usage: [CACHE_SEED=DIR] bash owl-627.sh OWLSHIFT_BINARY [ECOSYSTEM...]   (network needed)
#   CACHE_SEED: a folder holding, per ecosystem, run cache folders an earlier run filled, copied in before the first
#   gate (a retry when the network stopped a setup at its deadline).
# It writes only under a temporary folder, removed at the end, unless KEEP=1. It prints results only, the home shown
# as `~`.
set -u
bin=$(cd "$(dirname "$1")" && pwd -P)/$(basename "$1"); shift
home=$(cd "$HOME" && pwd -P)
work=$(mktemp -d "${TMPDIR:-/tmp}/owl627.XXXXXX"); work=$(cd "$work" && pwd -P)
export OWLSHIFT_CONFIG_DIR=$work/config OWLSHIFT_DATA_DIR=$work/data OWLSHIFT_CACHE_DIR=$work/cache
export GIT_CONFIG_GLOBAL=$work/gitconfig
say() { printf '%s %s\n' "$(date -u +%H:%M:%SZ)" "$*"; }
clean() { sed "s|$home|~|g; s|$work|<work>|g; s/^/  | /"; }
if [ "$(uname -s)" = Darwin ]; then
  base_path=${EXTRA_PATH:+$EXTRA_PATH:}/opt/homebrew/opt/openjdk/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin
  ecosystems=${*:-pdm maven maven-wrapper composer dotnet}
  system="macOS $(sw_vers -productVersion) ($(sw_vers -buildVersion)) $(uname -m)"
else
  base_path=/usr/local/bin:/usr/bin:/bin:/usr/share/dotnet
  ecosystems=${*:-pdm maven maven-wrapper composer dotnet}
  system="$(. /etc/os-release && echo "$PRETTY_NAME") $(uname -m), $(bwrap --version), uid $(id -u)"
fi
export PATH=$base_path

mkdir -p "$work/config" "$work/ssh/owl627" "$work/seed"
printf '#!/bin/sh\n# The git command an ssh host would run, run here.\ncd %s && exec sh -c "$2"\n' "'$work/ssh'" >"$work/local-ssh"
chmod +x "$work/local-ssh"
for kv in "core.sshCommand $work/local-ssh" "ssh.variant simple" "user.name Probe" "user.email probe@example.com" \
  "init.defaultBranch main" "commit.gpgsign false"; do
  git config --global "${kv%% *}" "${kv#* }"
done

say "$system; $("$bin" --version | head -1); $(pdm --version 2>/dev/null); $(python3 --version); \
$(mvn --version 2>/dev/null | head -1); $(java -version 2>&1 | head -1); $(php --version 2>/dev/null | head -1); \
$(composer --version 2>/dev/null | head -1); dotnet $(dotnet --version 2>/dev/null)"

# fixture ECOSYSTEM DIR: the project's files, its lock made unconfined into a throwaway cache.
fixture() {
  local dir=$2 seed=$work/seed/$1
  mkdir -p "$dir"
  case $1 in
  pdm | pdm-pyenv)
    printf '[project]\nname = "probe"\nversion = "0.1.0"\nrequires-python = ">=3.10"\ndependencies = ["humanize==4.11.0"]\n\n[dependency-groups]\ndev = ["pytest==8.3.4"]\n\n[tool.pdm]\ndistribution = false\n\n[tool.pytest.ini_options]\ntestpaths = ["tests"]\npythonpath = ["."]\n' >"$dir/pyproject.toml"
    mkdir -p "$dir/tests"
    printf 'import humanize\n\n\ndef size() -> str:\n    return humanize.naturalsize(1024)\n' >"$dir/probe.py"
    printf 'from probe import size\n\n\ndef test_size():\n    assert size() == "1.0 kB"\n' >"$dir/tests/test_probe.py"
    # `.pdm-python`, the interpreter pdm saved, ignored as the `.gitignore` of `pdm init` does; PDM_PYTHON_SEEN=1
    # leaves it out, and the lock's own `.pdm-python` is not committed either way.
    if [ -n "${PDM_PYTHON_SEEN:-}" ]; then printf '__pycache__/\n' >"$dir/.gitignore"
    else printf '__pycache__/\n.pdm-python\n' >"$dir/.gitignore"; fi
    (cd "$dir" && PDM_CACHE_DIR=$seed PDM_CHECK_UPDATE=false pdm lock -q >/dev/null 2>&1 \
      && rm -rf .pdm-python .venv) ;;
  maven | maven-wrapper)
    printf '<?xml version="1.0" encoding="UTF-8"?>\n<project xmlns="http://maven.apache.org/POM/4.0.0">\n  <modelVersion>4.0.0</modelVersion>\n  <groupId>probe</groupId>\n  <artifactId>probe</artifactId>\n  <version>0.1.0</version>\n  <properties>\n    <maven.compiler.release>17</maven.compiler.release>\n    <project.build.sourceEncoding>UTF-8</project.build.sourceEncoding>\n  </properties>\n  <dependencies>\n    <dependency>\n      <groupId>org.apache.commons</groupId>\n      <artifactId>commons-lang3</artifactId>\n      <version>3.17.0</version>\n    </dependency>\n    <dependency>\n      <groupId>junit</groupId>\n      <artifactId>junit</artifactId>\n      <version>4.13.2</version>\n      <scope>test</scope>\n    </dependency>\n  </dependencies>\n</project>\n' >"$dir/pom.xml"
    mkdir -p "$dir/src/main/java/probe" "$dir/src/test/java/probe"
    printf 'package probe;\n\nimport org.apache.commons.lang3.StringUtils;\n\npublic class Probe {\n    public static String word() {\n        return StringUtils.capitalize("probe");\n    }\n}\n' >"$dir/src/main/java/probe/Probe.java"
    printf 'package probe;\n\nimport static org.junit.Assert.assertEquals;\n\nimport org.junit.Test;\n\npublic class ProbeTest {\n    @Test\n    public void word() {\n        assertEquals("Probe", Probe.word());\n    }\n}\n' >"$dir/src/test/java/probe/ProbeTest.java"
    printf 'target/\n' >"$dir/.gitignore"
    if [ "$1" = maven-wrapper ]; then
      (cd "$dir" && mvn -q -B -Dmaven.repo.local="$seed/repository" wrapper:wrapper >/dev/null 2>&1) \
        || say "the wrapper could not be made"
    fi ;;
  composer)
    printf '{\n    "name": "probe/probe",\n    "require": {\n        "php": ">=8.2",\n        "psr/log": "3.0.2"\n    },\n    "require-dev": {\n        "phpunit/phpunit": "^11.5"\n    },\n    "autoload": {\n        "psr-4": { "Probe\\\\": "src/" }\n    }\n}\n' >"$dir/composer.json"
    printf '<?xml version="1.0" encoding="UTF-8"?>\n<phpunit bootstrap="vendor/autoload.php" colors="false">\n  <testsuites>\n    <testsuite name="probe">\n      <directory>tests</directory>\n    </testsuite>\n  </testsuites>\n</phpunit>\n' >"$dir/phpunit.xml.dist"
    mkdir -p "$dir/src" "$dir/tests"
    printf '<?php\n\nnamespace Probe;\n\nuse Psr\\Log\\NullLogger;\n\nfinal class Probe\n{\n    public static function word(): string\n    {\n        (new NullLogger())->info("probe");\n        return ucfirst("probe");\n    }\n}\n' >"$dir/src/Probe.php"
    printf '<?php\n\nuse PHPUnit\\Framework\\TestCase;\nuse Probe\\Probe;\n\nfinal class ProbeTest extends TestCase\n{\n    public function testWord(): void\n    {\n        $this->assertSame("Probe", Probe::word());\n    }\n}\n' >"$dir/tests/ProbeTest.php"
    printf 'vendor/\n.phpunit.cache/\n.phpunit.result.cache\n' >"$dir/.gitignore"
    (cd "$dir" && COMPOSER_HOME=$seed-home COMPOSER_CACHE_DIR=$seed composer update -q --no-interaction --no-install \
      >/dev/null 2>&1) || say "composer could not lock" ;;
  dotnet)
    # The SDK's own templates, no restore: the solution in the SDK's default format (`.slnx` since .NET 10).
    (cd "$dir" && DOTNET_CLI_HOME=$seed-home NUGET_PACKAGES=$seed DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_NOLOGO=1 \
      sh -c 'dotnet new sln -n Probe && dotnet new classlib -n Probe.Lib -o src/Probe.Lib --no-restore \
        && dotnet new xunit -n Probe.Tests -o tests/Probe.Tests --no-restore \
        && dotnet add tests/Probe.Tests reference src/Probe.Lib \
        && dotnet sln add tests/Probe.Tests' >/dev/null 2>&1) || say "dotnet new failed"
    # `dotnet sln add` adds the projects a project references with it (.NET 10 and later).
    grep -q Probe.Lib.csproj "$dir"/Probe.sln* || say "the solution lacks Probe.Lib"
    printf 'namespace Probe.Lib;\n\npublic static class Probe\n{\n    public static string Word() => "Probe";\n}\n' \
      >"$dir/src/Probe.Lib/Class1.cs"
    printf 'namespace Probe.Tests;\n\npublic class UnitTest1\n{\n    [Fact]\n    public void Word() => Assert.Equal("Probe", Probe.Lib.Probe.Word());\n}\n' \
      >"$dir/tests/Probe.Tests/UnitTest1.cs"
    printf 'bin/\nobj/\nTestResults/\n' >"$dir/.gitignore" ;;
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
  git init -q --bare "$work/ssh/owl627/$eco.git"
  (cd "$dir" && git init -q && git remote add origin "git@github.com:owl627/$eco.git" && git add -A \
    && git commit -q -m fixture && git push -q origin main) >/dev/null 2>&1 || say "the fixture could not be pushed"
  (cd "$dir" && "$bin" init --tracker markdown --skip-secrets </dev/null 2>&1 | grep -E '^(Detected|  Note|Skipped)') | clean
  if [ -n "${STACK_EDIT:-}" ]; then
    # An experiment: a sed expression applied to the `owlshift.toml` `init` wrote, said in the results.
    sed "$STACK_EDIT" "$dir/owlshift.toml" >"$dir/owlshift.toml.new" && mv "$dir/owlshift.toml.new" "$dir/owlshift.toml"
    say "edited with: $STACK_EDIT"
  fi
  say "the [stack] written:"
  sed -n '/^\[stack\]/,/^# The files agents receive/p' "$dir/owlshift.toml" \
    | grep -E '^(gate|setup|caches|run_caches|\]|    |# read from|# note:)' | clean
  cp "$dir/owlshift.toml" "$work/owlshift.toml.$eco"
  if [ "$eco" = pdm-pyenv ]; then
    # What `owlshift gate` proposes when pdm's search for a Python is refused `~/.pyenv`: the project declares it
    # read-only, and this machine allows it.
    awk '/^# caches = / { print "caches = [\".pyenv\"]"; next } { print }' "$work/owlshift.toml.$eco" >"$dir/owlshift.toml"
    printf '[repositories."github.com/owl627/%s"]\nallow_caches = [".pyenv"]\n' "$eco" >>"$work/config/config.toml"
    cp "$dir/owlshift.toml" "$work/owlshift.toml.$eco"
    say "with caches = [\".pyenv\"], allowed in the personal file"
  fi
  if [ -d "${CACHE_SEED:-/nonexistent}/$eco" ]; then
    # A retry on a slow network: the run caches start with what an earlier run of this script downloaded into them.
    mkdir -p "$work/cache/github.com/owl627" && cp -R "$CACHE_SEED/$eco" "$work/cache/github.com/owl627/$eco"
    say "the run caches seeded from an earlier run: $(du -sk "$work/cache/github.com/owl627/$eco" | awk '{print $1}') kB"
  fi
  gate "$eco, as written" "$dir"
  say "the run caches: $(cd "$work/cache/github.com/owl627/$eco" 2>/dev/null && du -sk -- * 2>/dev/null \
    | awk '{ printf "%s %s, ", $2, ($1 < 1024 ? $1 " kB" : sprintf("%.0f MB", $1 / 1024)) }')"
  [ -n "${SAVE_CACHES:-}" ] && mkdir -p "$SAVE_CACHES" && rm -rf "${SAVE_CACHES:?}/$eco" \
    && cp -R "$work/cache/github.com/owl627/$eco" "$SAVE_CACHES/$eco" 2>/dev/null
  # Each run cache removed alone: the others stay, filled by the run above.
  for variable in $(sed -n 's/^run_caches\.\([A-Za-z_0-9]*\) = .*/\1/p' "$work/owlshift.toml.$eco"); do
    awk -v v="$variable" 'index($0, "run_caches." v " = ") != 1' "$work/owlshift.toml.$eco" >"$dir/owlshift.toml"
    gate "$eco, without run_caches.$variable" "$dir"
  done
  if [ "$eco" = maven ] || [ "$eco" = maven-wrapper ]; then
    # Without the variable the flag names `/repository`: the line's own doing. Maven's own default, the flag removed
    # and `MAVEN_USER_HOME` kept, is what shows the repository must move.
    sed 's| -Dmaven.repo.local=\\"$MAVEN_USER_HOME/repository\\"||' "$work/owlshift.toml.$eco" >"$dir/owlshift.toml"
    sed 's| -Dmaven.repo.local="$MAVEN_USER_HOME/repository"||' "$dir/owlshift.toml" >"$dir/owlshift.toml.new" \
      && mv "$dir/owlshift.toml.new" "$dir/owlshift.toml"
    grep -q 'maven.repo.local' "$dir/owlshift.toml" && say "the flag could not be removed"
    gate "$eco, without -Dmaven.repo.local" "$dir"
  fi
  cp "$work/owlshift.toml.$eco" "$dir/owlshift.toml"
done

if [ -n "${KEEP:-}" ]; then say "kept the scratch folder $work"; exit 0; fi
chmod -R u+w "$work" 2>/dev/null
rm -rf "$work"
say "removed the scratch folder"
