#!/usr/bin/env bash
# OWL-665 live check, quoted in docs/design/records/OWL-665.md: whether a token can set the commit status
# `owlshift/gate` on a commit, and what GitHub then says of who set it. The status's `creator` is what a project's CI
# trusts (architecture section 5, "The gate's status"), so the check reads it back through
# `GET /repos/{owner}/{repo}/commits/{sha}/statuses` and compares it with the token's own account.
#
#   OWL665_TOKEN=... bash docs/design/live-checks/owl-665.sh pitchopp/owlshift-live-sandbox app            (--help)
#   OWL665_TOKEN=... bash docs/design/live-checks/owl-665.sh pitchopp/owlshift-live-sandbox fine-grained
#
# The second argument names the kind of token, for the report only: `app` (the GitHub App's user token, once the app
# asks Commit statuses (read and write) and the installation on the repository accepted it), `fine-grained` (a
# fine-grained personal token with Commit statuses (write) and Contents (read) on the repository: the check reads the
# default branch's head), or `oauth`. The App's user token is
# the `access_token` of the keychain entry `owlshift init` wrote, under service `owlshift`, account
# `github.com/<owner>/<repo>/github-app` of any project signed in through the app:
#
#   OWL665_TOKEN=$(security find-generic-password -s owlshift -a github.com/<owner>/<repo>/github-app -w \
#     | jq -r .access_token) bash docs/design/live-checks/owl-665.sh pitchopp/owlshift-live-sandbox app
#
# It lives 8 hours: a 401 means it expired, and the next refresh of a running `owlshift watch` renews the entry.
#
# Needs bash (3.2 is enough), curl and jq. A few seconds. It sets one commit status, of context `owlshift/gate`, on
# the head of the repository's default branch, and reads that commit's statuses back; nothing else is written, and a
# status cannot be deleted, so run it on the sandbox only. It refuses pitchopp/owlshift. The report, on standard
# output, holds no token and no account identifier: it says whether the creator is the token's own account, and of
# which type. Paste it on the ticket.

set -u -o pipefail
umask 077

PROGRAM=${0##*/}
FORBIDDEN=pitchopp/owlshift
API=https://api.github.com
CONTEXT=owlshift/gate

usage() {
    sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'
}

case "${1:-}" in
    -h | --help | "")
        usage
        exit 0
        ;;
esac
REPO=$1
KIND=${2:-unnamed}
if [ "$REPO" = "$FORBIDDEN" ]; then
    echo "$PROGRAM: refusing $FORBIDDEN: run it on the sandbox" >&2
    exit 2
fi
if [ -z "${OWL665_TOKEN:-}" ]; then
    echo "$PROGRAM: set OWL665_TOKEN to the token to check" >&2
    exit 2
fi
for tool in curl jq; do
    command -v "$tool" >/dev/null || {
        echo "$PROGRAM: $tool is needed" >&2
        exit 2
    }
done

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# One call: the token goes to curl on its standard input, never on a command line. Writes the body to $WORK/body,
# the headers to $WORK/headers, and prints the HTTP status.
call() {
    local method=$1 path=$2 data=${3:-}
    local args=(-s -o "$WORK/body" -D "$WORK/headers" -w '%{http_code}' -X "$method"
        -H "Accept: application/vnd.github+json" -H "X-GitHub-Api-Version: 2022-11-28" -K -)
    if [ -n "$data" ]; then
        args+=(-H "Content-Type: application/json" --data "$data")
    fi
    printf 'header = "Authorization: Bearer %s"\n' "$OWL665_TOKEN" | curl "${args[@]}" "$API/$path"
}

header() {
    tr -d '\r' <"$WORK/headers" | awk -v name="$1" 'tolower($0) ~ "^" tolower(name) ":" { sub(/^[^:]*: */, ""); print }'
}

echo "OWL-665 live check, $(date -u +%Y-%m-%dT%H:%M:%SZ), token kind: $KIND"

status=$(call GET user)
if [ "$status" != 200 ]; then
    echo "GET /user: HTTP $status $(jq -r '.message // empty' "$WORK/body")"
    exit 1
fi
me=$(jq -r .login "$WORK/body")

status=$(call GET "repos/$REPO")
if [ "$status" != 200 ]; then
    echo "GET /repos/{repo}: HTTP $status $(jq -r '.message // empty' "$WORK/body")"
    exit 1
fi
branch=$(jq -r .default_branch "$WORK/body")
status=$(call GET "repos/$REPO/commits/$branch")
[ "$status" = 200 ] || {
    echo "GET the default branch's head: HTTP $status"
    exit 1
}
sha=$(jq -r .sha "$WORK/body")
echo "commit: the default branch's head ($branch)"

description="OWL-665 live check, $KIND token"
status=$(call POST "repos/$REPO/statuses/$sha" \
    "$(jq -cn --arg c "$CONTEXT" --arg d "$description" '{state: "success", context: $c, description: $d}')")
echo "POST /repos/{repo}/statuses/{sha}: HTTP $status $(jq -r '.message // empty' "$WORK/body")"
echo "  x-accepted-github-permissions: $(header x-accepted-github-permissions)"
[ "$status" = 201 ] || exit 1
id=$(jq -r .id "$WORK/body")

# GitHub's read can lag its write: a status just set was missing from the first read-back on 2026-11-27 (R3).
for attempt in 1 2 3 4 5; do
    status=$(call GET "repos/$REPO/commits/$sha/statuses?per_page=100")
    [ "$status" = 200 ] && jq -e --argjson id "$id" 'any(.[]; .id == $id)' "$WORK/body" >/dev/null && break
    [ "$attempt" = 5 ] || sleep 2
done
echo "GET /repos/{repo}/commits/{sha}/statuses: HTTP $status"
jq -r --argjson id "$id" --arg me "$me" '
    [.[] | select(.id == $id)] | first
    | if . == null then "  the status set is not listed"
      else
        "  state: \(.state), context: \(.context), description: \(.description)",
        "  creator: type \(.creator.type), the token'"'"'s own account: \(if .creator.login == $me then "yes" else "no" end)",
        "  creator is a bot (login ends with [bot]): \(.creator.login | endswith("[bot]"))",
        "  fields: \(keys | join(", "))"
      end' "$WORK/body"
