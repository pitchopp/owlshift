#!/bin/sh
# OWL-339 live checks (a) and (d), quoted by the "OWL-339 live checks" bullet of docs/design/build-plan.md.
# Usage: sh docs/design/live-checks/owl-339.sh   (no argument; scratch repositories under ${TMPDIR:-/tmp})
# Every command prints `status=<exit status>` after it. A root user is not bound by chmod: (d) is then not run.
set -u
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@example.com GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@example.com
date -u +"date=%Y-%m-%dT%H:%M:%SZ"; git --version; uname -sr; echo "uid=$(id -u)"
root=$(mktemp -d "${TMPDIR:-/tmp}/owl339.XXXXXX")
trap 'chmod -R u+w "$root" 2>/dev/null; rm -rf "$root"' EXIT
st() { echo "status=$?"; }

# watch <entry> <seconds> <logfile> [<ref file>]: polls the folder in a tight loop and prints each change of the
# files present (and of the head-name and onto files of a rebase), of the kind of HEAD (none, symbolic, detached)
# and, when a ref file is given, of its presence. Times are seconds from the start of the poll.
watch() {
  perl -e '
    use Time::HiRes qw(time sleep); my ($dir, $secs, $ref) = @ARGV; my $t0 = time; my $last = "";
    while (time - $t0 < $secs) {
      my @f; if (opendir(my $d, $dir)) { @f = sort grep { $_ ne "." && $_ ne ".." } readdir $d; closedir $d }
      my $kind = "none"; if (open(my $h, "<", "$dir/HEAD")) { my $l = <$h>; $kind = defined $l && $l =~ /^ref:/ ? "symbolic" : "detached" }
      my @sub; for my $s ("rebase-merge", "rebase-apply") { if (opendir(my $d, "$dir/$s")) { push @sub, map { "$s/$_" } grep { /^(head-name|onto)$/ } readdir $d; closedir $d } }
      my $now = join(",", @f, @sub) . " HEAD=$kind" . (length $ref ? (-e $ref ? " ref=yes" : " ref=no") : "");
      if ($now ne $last) { printf "%.3f %s\n", time - $t0, $now; $last = $now } sleep 0.0005 }' "$1" "$2" "${4:-}" > "$3"
}

# repo <dir>: `base` (5 commits and a file c), then `topic` (3 commits), `clash` (1 commit editing c) and 20 more
# commits on main, one of them editing c, so that rebasing topic onto main has work to do and clash stops.
repo() {
  git init -q -b main "$1" && cd "$1" || exit 1
  i=0; while [ $i -lt 5 ]; do echo $i > m$i; git add m$i; git commit -q -m "main $i"; i=$((i+1)); done
  echo base > c; git add c; git commit -q -m base
  git checkout -q -b topic; for n in 1 2 3; do echo $n > t$n; git add t$n; git commit -q -m "topic $n"; done
  git checkout -q -b clash main; echo clash > c; git commit -q -am clash
  git checkout -q main; i=5; while [ $i -lt 25 ]; do echo $i > m$i; git add m$i; git commit -q -m "main $i"; i=$((i+1)); done
  echo main > c; git commit -q -am "main c"
}

echo "== (a) git worktree add -b x ../x, entry worktrees/x polled with refs/heads/x"
repo "$root/a1"; mkdir -p .git/worktrees
watch .git/worktrees/x 3 "$root/a1.log" "$root/a1/.git/refs/heads/x" & w=$!; sleep 0.3
git worktree add -q -b x ../x; st; wait $w; cat "$root/a1.log"

# The three rebases run in a linked worktree `w` of a repository, on topic (clash for the last), its entry polled.
rebase() { # rebase <label> <branch> <command...>
  l=$1; b=$2; shift 2
  repo "$root/$l"; git worktree add -q "../$l-w" "$b"; cd "../$l-w" || exit 1
  e=$(git rev-parse --absolute-git-dir)
  watch "$e" 4 "$root/$l.log" & w=$!; sleep 0.3
  "$@" >/dev/null 2>&1; st; wait $w; cat "$root/$l.log"
}
echo "== (a) merge backend: git rebase main"
rebase a2 topic git rebase main
echo "== (a) merge backend, interactive: GIT_SEQUENCE_EDITOR=true git rebase -i main"
rebase a3 topic env GIT_SEQUENCE_EDITOR=true git rebase -i main
echo "== (a) --apply backend, no stop: git rebase --apply main"
rebase a4 topic git rebase --apply main
echo "== (a) --apply backend, stops on a conflict: git rebase --apply main (on clash)"
rebase a5 clash git rebase --apply main

echo "== (d) prune and gc with an expired entry under a read-only worktrees folder"
if [ "$(id -u)" = 0 ]; then echo "not run: root"; exit 0; fi
# fixture <name>: repository <name>/repo with worktrees own, other and stale; the folder of stale removed and
# the gitdir file of its entry dated 2020-01-01, so that prune and gc find it expired.
fixture() {
  mkdir "$root/$1" && git init -q -b main "$root/$1/repo" && cd "$root/$1/repo" || exit 1
  git commit -q --allow-empty -m first
  for n in own other stale; do git worktree add -q -b $n "../$n"; done
  rm -rf ../stale; touch -t 202001010000 .git/worktrees/stale/gitdir
}
sums() { (cd .git/worktrees/stale && find . -type f | sort | xargs shasum -a 256); }
# run <label> <command...>: the status, stdout and stderr of a command (lines of the macOS xcrun shim's cache
# warnings, printed by /usr/bin/git when its sandbox cannot write its cache, are dropped).
run() {
  l=$1; shift; "$@" >"$root/$l.out" 2>"$root/$l.err"; s=$?
  echo "$l: status=$s"; echo "  stdout: $(tr '\n' '|' <"$root/$l.out")"
  echo "  stderr: $(grep -v xcrun_db "$root/$l.err" | tr '\n' '|')"
}
echo "-- chmod -R a-w .git/worktrees"
fixture d1; sums > "$root/d1.before"
chmod -R a-w .git/worktrees
run prune git worktree prune --verbose
run gc git gc --quiet
chmod -R u+w .git/worktrees; sums > "$root/d1.after"; cmp "$root/d1.before" "$root/d1.after"; st; cat "$root/d1.after"
echo "-- no chmod"
fixture d2; run prune git worktree prune --verbose; run gc git gc --quiet
echo "-- folder-only chmod a-w .git/worktrees"
fixture d3; chmod a-w .git/worktrees; run prune git worktree prune --verbose; run gc git gc --quiet; chmod u+w .git/worktrees
echo "-- find .git/worktrees ! -perm -u+w, after chmod -R u+w on d1 (prints nothing when every file is writable)"
cd "$root/d1/repo" && find .git/worktrees ! -perm -u+w

# The reflog contrast: git gc rewrites the logs/HEAD of every entry, even writable ones, when nothing is read only.
echo "== (d) reflog contrast: git gc and the logs/HEAD of a writable entry"
reflog() { # reflog <name> <gc command...>: a repository with one commit and a worktree o3; the command; the change
  n=$1; shift
  mkdir "$root/$n" && git init -q -b main "$root/$n/repo" && cd "$root/$n/repo" || exit 1
  git commit -q --allow-empty -m first; git worktree add -q -b o3 ../o3
  f=.git/worktrees/o3/logs/HEAD; cp $f "$root/$n.before"
  echo "before: $(wc -c <$f) bytes $(shasum -a 256 <$f | cut -c1-8)"
  "$@" >/dev/null 2>&1; echo "$* status=$?"
  echo "after: $(wc -c <$f) bytes $(shasum -a 256 <$f | cut -c1-8)"; cmp "$root/$n.before" $f; st
}
reflog r1 git gc --quiet
reflog r2 git gc --auto --quiet
reflog r3 git maintenance run --auto
cd "$root/r1/repo" && git gc --quiet; echo "second gc status=$?, logs/HEAD $(wc -c <.git/worktrees/o3/logs/HEAD) bytes"
git commit -q --allow-empty -m second
git -c gc.auto=1 gc --auto --quiet; echo "-c gc.auto=1 gc --auto status=$?, $(shasum -a 256 <.git/worktrees/o3/logs/HEAD | cut -c1-8)"
git -c gc.auto=1 maintenance run --auto; echo "-c gc.auto=1 maintenance run --auto status=$?, $(shasum -a 256 <.git/worktrees/o3/logs/HEAD | cut -c1-8)"
