#!/bin/bash
# OWL-637 (architecture section 8; runtime and operations, "More stacks"), macOS: a Maven project with its wrapper
# `mvnw`, its `[stack]` written by `owlshift init` and left unedited, then the real `owlshift gate`, confined: once by a
# binary without OWL-637's change to the sandbox, expected red on the wrapper's `mkdtemp failed`, then once by this
# branch's binary, expected green. The harness is a reduced copy of owl-627.sh's (OWL-627), which it leaves as it is: a
# fixture repository whose `origin`, `git@github.com:owl637/maven-wrapper.git`, is served by a local bare remote
# through `core.sshCommand`, Owlshift's folders and git's global configuration moved into a scratch folder.
#
# Before each gate the run cache `maven` holds a Maven repository filled first, unconfined, by the machine's `mvn` (the
# wrapper's plugin and the build's dependencies), and never the wrapper's `wrapper/dists`: each gate's wrapper must
# download the Maven it pins, making its download folder with `mktemp -d`, which is what is checked. Only that
# download is left to the confined gates, so a slow network does not stop them at their deadline.
#
# Usage: [REPOSITORY_SEED=DIR] bash owl-637.sh OWLSHIFT_BINARY OWLSHIFT_BINARY_WITHOUT   (network needed)
#   REPOSITORY_SEED: a Maven repository an earlier run filled, copied in before the unconfined fill (a retry).
# It writes only under a temporary folder, removed at the end, unless KEEP=1. It prints results only, the home shown
# as `~`.
set -u
[ "$(uname -s)" = Darwin ] || { echo "macOS only"; exit 2; }
bin=$(cd "$(dirname "$1")" && pwd -P)/$(basename "$1")
without=$(cd "$(dirname "$2")" && pwd -P)/$(basename "$2")
home=$(cd "$HOME" && pwd -P)
work=$(mktemp -d "${TMPDIR:-/tmp}/owl637.XXXXXX"); work=$(cd "$work" && pwd -P)
export OWLSHIFT_CONFIG_DIR=$work/config OWLSHIFT_DATA_DIR=$work/data OWLSHIFT_CACHE_DIR=$work/cache
export GIT_CONFIG_GLOBAL=$work/gitconfig
export PATH=/opt/homebrew/opt/openjdk/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin
say() { printf '%s %s\n' "$(date -u +%H:%M:%SZ)" "$*"; }
clean() { sed "s|$home|~|g; s|$work|<work>|g; s/^/  | /"; }

mkdir -p "$work/config" "$work/ssh/owl637" "$work/seed"
printf '#!/bin/sh\n# The git command an ssh host would run, run here.\ncd %s && exec sh -c "$2"\n' "'$work/ssh'" >"$work/local-ssh"
chmod +x "$work/local-ssh"
for kv in "core.sshCommand $work/local-ssh" "ssh.variant simple" "user.name Probe" "user.email probe@example.com" \
  "init.defaultBranch main" "commit.gpgsign false"; do
  git config --global "${kv%% *}" "${kv#* }"
done
say "macOS $(sw_vers -productVersion) ($(sw_vers -buildVersion)) $(uname -m); with: $("$bin" --version | head -1); \
without: $("$without" --version | head -1); $(mvn --version 2>/dev/null | head -1); $(java -version 2>&1 | head -1)"

# The fixture: owl-627.sh's Maven project, its wrapper made and its repository filled unconfined.
dir=$work/src/maven-wrapper repo=$work/seed/repository
mkdir -p "$dir/src/main/java/probe" "$dir/src/test/java/probe"
[ -d "${REPOSITORY_SEED:-/nonexistent}" ] && cp -R "$REPOSITORY_SEED" "$repo"
printf '<?xml version="1.0" encoding="UTF-8"?>\n<project xmlns="http://maven.apache.org/POM/4.0.0">\n  <modelVersion>4.0.0</modelVersion>\n  <groupId>probe</groupId>\n  <artifactId>probe</artifactId>\n  <version>0.1.0</version>\n  <properties>\n    <maven.compiler.release>17</maven.compiler.release>\n    <project.build.sourceEncoding>UTF-8</project.build.sourceEncoding>\n  </properties>\n  <dependencies>\n    <dependency>\n      <groupId>org.apache.commons</groupId>\n      <artifactId>commons-lang3</artifactId>\n      <version>3.17.0</version>\n    </dependency>\n    <dependency>\n      <groupId>junit</groupId>\n      <artifactId>junit</artifactId>\n      <version>4.13.2</version>\n      <scope>test</scope>\n    </dependency>\n  </dependencies>\n</project>\n' >"$dir/pom.xml"
printf 'package probe;\n\nimport org.apache.commons.lang3.StringUtils;\n\npublic class Probe {\n    public static String word() {\n        return StringUtils.capitalize("probe");\n    }\n}\n' >"$dir/src/main/java/probe/Probe.java"
printf 'package probe;\n\nimport static org.junit.Assert.assertEquals;\n\nimport org.junit.Test;\n\npublic class ProbeTest {\n    @Test\n    public void word() {\n        assertEquals("Probe", Probe.word());\n    }\n}\n' >"$dir/src/test/java/probe/ProbeTest.java"
printf 'target/\n' >"$dir/.gitignore"
start=$(date +%s)
(cd "$dir" && mvn -q -B -Dmaven.repo.local="$repo" wrapper:wrapper >/dev/null 2>&1 \
  && mvn -q -B -Dmaven.repo.local="$repo" verify >/dev/null 2>&1 && rm -rf target) \
  || { say "the fixture could not be made"; exit 1; }
say "fixture made unconfined in $(($(date +%s) - start)) s: $(sed -n 's/^distributionUrl=//p' \
  "$dir/.mvn/wrapper/maven-wrapper.properties"), $(sed -n 's/^wrapperVersion=//p' \
  "$dir/.mvn/wrapper/maven-wrapper.properties" | head -1); mvnw $(stat -f %Sp "$dir/mvnw")"

git init -q --bare "$work/ssh/owl637/maven-wrapper.git"
(cd "$dir" && git init -q && git remote add origin git@github.com:owl637/maven-wrapper.git && git add -A \
  && git commit -q -m fixture && git push -q origin main) >/dev/null 2>&1 || say "the fixture could not be pushed"
(cd "$dir" && "$bin" init --tracker markdown --skip-secrets </dev/null 2>&1 | grep -E '^(Detected|  Note|Skipped)') | clean
say "the [stack] written:"
sed -n '/^\[stack\]/,/^# The files agents receive/p' "$dir/owlshift.toml" \
  | grep -E '^(gate|setup|caches|run_caches|\]|    |# read from|# note:)' | clean
(cd "$dir" && git add -A && git commit -q -m init && git push -q origin main) >/dev/null 2>&1

# gate LABEL BINARY EXPECT: the run cache reset to the filled repository alone, then `owlshift gate` by BINARY; its
# verdict and end printed, and whether it is the one EXPECTed: red on `mkdtemp failed`, or green without it.
gate() {
  local label=$1 binary=$2 expect=$3 cache=$work/cache/github.com/owl637/maven-wrapper out rc start verdict
  rm -rf "$cache" && mkdir -p "$cache/maven" && cp -R "$repo" "$cache/maven/repository"
  start=$(date +%s)
  out=$(cd "$dir" && "$binary" gate </dev/null 2>&1); rc=$?
  if [ "$expect" = red ] && [ $rc -ne 0 ] && printf '%s\n' "$out" | grep -q 'mkdtemp failed'; then verdict="as expected"
  elif [ "$expect" = green ] && [ $rc -eq 0 ] && ! printf '%s\n' "$out" | grep -q 'mkdtemp failed'; then
    verdict="as expected"
  else verdict="UNEXPECTED, $expect expected"; fi
  say "$label: owlshift gate exit $rc in $(($(date +%s) - start)) s, $verdict"
  out=$(printf '%s\n' "$out" | grep -v '^Its whole output goes to\|^Gate log: ')
  if printf '%s\n' "$out" | grep -q '^Gate red'; then
    printf '%s\n' "$out" | grep -B"${TAIL:-6}" '^Gate red' | clean
  else
    printf '%s\n' "$out" | tail -"${TAIL:-8}" | clean
  fi
  say "the run cache after it: $(cd "$cache/maven" && du -sk -- * | awk '{ printf "%s %s kB, ", $2, $1 }')"
}

gate "without OWL-637's sandbox change" "$without" red
gate "with it" "$bin" green

if [ -n "${KEEP:-}" ]; then say "kept the scratch folder $work"; exit 0; fi
chmod -R u+w "$work" 2>/dev/null
rm -rf "$work"
say "removed the scratch folder"
