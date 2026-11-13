#!/bin/sh
# Commits a release's Homebrew formula as Formula/owlshift.rb on main
# (OWL-549), once formula-verify.sh has verified the release and rendered it.
# `.github/workflows/formula.yml` runs it with the workflow's token, allowed
# to write the repository's contents; nothing else in it writes.
#
# One commit through GitHub's contents API, which GitHub makes on top of
# main's head: never a force push, and no other file changes. It writes
# nothing when main's formula is already this one, or is of a newer version,
# so an older release published late or a run re-run later never takes the
# tap back. The commit's author and its sign-off are the Actions bot, as
# CONTRIBUTING.md's sign-off rule asks of every commit. The formula's blob as
# read guards the write: when main's formula changed in between, GitHub
# refuses it, and the script reads main again and tries once more.
#
#   sh scripts/release/formula-commit.sh VERSION FORMULA REPOSITORY
#
# REPOSITORY is <owner>/<name>; `gh` reads its token from GH_TOKEN.

set -eu

if [ "$#" -ne 3 ]; then
    echo "usage: formula-commit.sh VERSION FORMULA REPOSITORY" >&2
    exit 2
fi
version=$1
formula=$2
repository=$3
path=Formula/owlshift.rb
bot='github-actions[bot]'
bot_email='41898282+github-actions[bot]@users.noreply.github.com'

fail() {
    printf 'formula-commit: error: %s\n' "$*" >&2
    exit 1
}

# A line GitHub Actions shows on the run's summary, plain text elsewhere.
notice() {
    printf '::notice::%s\n' "$*"
}

# Whether version $1 is older than version $2, both <major>.<minor>.<patch>.
older() {
    awk -v a="$1" -v b="$2" 'BEGIN {
        split(a, x, "."); split(b, y, ".")
        for (i = 1; i <= 3; i++) {
            if (x[i] + 0 < y[i] + 0) exit 0
            if (x[i] + 0 > y[i] + 0) exit 1
        }
        exit 1
    }'
}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

message=$(printf 'Update the Homebrew formula to %s\n\nRendered by .github/workflows/formula.yml from the signed SHA256SUMS of\nrelease v%s.\n\nSigned-off-by: %s <%s>' \
    "$version" "$version" "$bot" "$bot_email")
content=$(base64 < "$formula" | tr -d '\n')

# Returns 0 once main holds this formula or a newer one, 3 when GitHub
# refused the commit because main's formula changed since it was read.
attempt() {
    if gh api "repos/$repository/contents/$path?ref=main" --jq '.sha, .content' \
        > "$work/main" 2> "$work/error"; then
        blob=$(head -n 1 "$work/main")
        tail -n +2 "$work/main" | tr -d '\n' | base64 -d > "$work/main.rb"
        if cmp -s "$formula" "$work/main.rb"; then
            notice "main's formula is already the one of $version"
            return 0
        fi
        current=$(awk -F '"' '/^  version "/ { print $2; exit }' "$work/main.rb")
        if [ -n "$current" ] && older "$version" "$current"; then
            notice "main's formula is of $current, newer than $version: left as it is"
            return 0
        fi
    elif grep -q 'HTTP 404' "$work/error"; then
        blob=
    else
        fail "could not read main's $path: $(cat "$work/error")"
    fi
    set -- --method PUT "repos/$repository/contents/$path" \
        -f "message=$message" -f "content=$content" -f branch=main \
        -f "author[name]=$bot" -f "author[email]=$bot_email"
    if [ -n "$blob" ]; then
        set -- "$@" -f "sha=$blob"
    fi
    if gh api "$@" --jq .commit.sha > "$work/commit" 2> "$work/error"; then
        echo "committed $path for $version on main: $(cat "$work/commit")"
        return 0
    fi
    if grep -Eq 'HTTP (409|422)' "$work/error"; then
        return 3
    fi
    fail "GitHub refused the commit on main: $(cat "$work/error")"
}

status=0
attempt || status=$?
if [ "$status" -eq 3 ]; then
    echo "main's $path changed while committing: reading it again"
    status=0
    attempt || status=$?
fi
if [ "$status" -ne 0 ]; then
    fail "GitHub refused the commit on main twice, the last time with: $(cat "$work/error")"
fi
