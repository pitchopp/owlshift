#!/bin/sh
# OWL-655 (architecture section 5, "Conflicts and the rebase run", *A clean refresh*): what the runner's merge of a
# clean refresh rests on, with the machine's git. A scratch repository with a bare remote: a delivered branch `b` that
# changes line 1 of a shared file, and a base `main` that then changes its line 3 (another line, so no conflict).
#
# 1. `git merge-tree --write-tree -z main b` exits 0 and writes the merged tree's id alone, then a NUL.
# 2. `git -c user.name=Owlshift -c user.email=owlshift@localhost commit-tree --no-gpg-sign <tree> -p <b> -p <main>`
#    writes a commit with the two parents, in that order, and that tree.
# 3. `git push --porcelain --force-with-lease=refs/heads/b:<b> origin <merge>:refs/heads/b` lands as a fast-forward,
#    porcelain flag ' '; a push leased on a commit the remote no longer holds is rejected, flag '!'.
#
# Usage: sh owl-655.sh   (no network). It writes only under a temporary folder, removed at the end, unless KEEP=1.
# It exits 0 when every result is the one expected.
set -u
work=$(mktemp -d "${TMPDIR:-/tmp}/owl655.XXXXXX")
export GIT_CONFIG_GLOBAL="$work/gitconfig" GIT_CONFIG_NOSYSTEM=1
git config --global user.name Probe
git config --global user.email probe@example.com
git config --global init.defaultBranch main
git config --global commit.gpgsign false
fail=0
check() {
  if [ "$1" = "$2" ]; then echo "  ok: $3"; else
    echo "  UNEXPECTED: $3: got '$1', expected '$2'"
    fail=1
  fi
}

echo "$(git --version); $(uname -s) $(uname -m)"
git init -q --bare "$work/remote.git"
git init -q "$work/repo"
cd "$work/repo" || exit 1
git remote add origin "$work/remote.git"
printf 'one\ntwo\nthree\n' >shared.txt
git add shared.txt && git commit -q -m base && git push -q origin main
git switch -q -c b
printf 'ONE\ntwo\nthree\n' >shared.txt
git commit -q -am delivery && git push -q origin b
head=$(git rev-parse b)
git switch -q main
printf 'one\ntwo\nTHREE\n' >shared.txt
git commit -q -am sibling && git push -q origin main
onto=$(git rev-parse main)

echo "1. merge-tree"
git merge-tree --write-tree -z --end-of-options main b >"$work/out"
check "$?" 0 "merge-tree exits 0 on a clean merge"
bytes=$(wc -c <"$work/out" | tr -d ' ')
tree=$(tr -d '\000' <"$work/out")
check "$bytes" "$((${#tree} + 1))" "its output is one field and a NUL ($bytes bytes)"
check "$(git cat-file -t "$tree")" tree "the field names a tree: $tree"

echo "2. commit-tree"
merge=$(git -c user.name=Owlshift -c user.email=owlshift@localhost commit-tree --no-gpg-sign "$tree" -p "$head" \
  -p "$onto" -m "Merge main into b")
check "$?" 0 "commit-tree exits 0 with a commit of ${#merge} characters"
check "$(git rev-parse "$merge^1") $(git rev-parse "$merge^2")" "$head $onto" "its parents are the head, then the base"
check "$(git rev-parse "$merge^{tree}")" "$tree" "its tree is merge-tree's"
git cat-file -p "$merge" | sed -n '/^parent/p; /^author/s/ [0-9]* [-+][0-9]*$//p' | sed 's/^/  | /'
check "$(git show "$merge:shared.txt" | tr '\n' ' ')" "ONE two THREE " "the merged file holds both changes"

echo "3. leased push"
out=$(git push --porcelain --no-verify "--force-with-lease=refs/heads/b:$head" origin "$merge:refs/heads/b" 2>/dev/null)
check "$?" 0 "the push exits 0"
check "$(printf '%s\n' "$out" | grep 'refs/heads/b' | cut -c1)" " " "the porcelain flag is ' ', a fast-forward"
printf '%s\n' "$out" | sed "s|$work|<work>|g; s/^/  | /"
check "$(git --git-dir="$work/remote.git" rev-parse b)" "$merge" "the remote branch is the merge"
out=$(git push --porcelain --no-verify "--force-with-lease=refs/heads/b:$head" origin "$head:refs/heads/b" 2>/dev/null)
code=$?
check "$(printf '%s\n' "$out" | grep 'refs/heads/b' | cut -c1) $code" "! 1" \
  "a push leased on the old head is rejected, flag '!'"

cd / || exit 1
if [ -n "${KEEP:-}" ]; then echo "kept the scratch folder $work"; else rm -rf "$work"; fi
exit $fail
