#!/usr/bin/env bash
# OWL-537 live checks, quoted in docs/design/build-plan.md, "P9 · Open-source readiness", part 6: the GitHub calls
# `owlshift init` and `owlshift doctor` rely on that G1 to G6 did not observe.
#   U1  `GET /user/{id}` for a known account and an unknown one: doctor's "admitter is a known account".
#   U2  `GET /users/{id}` with a numeric id, for comparison: a login lookup, not an id lookup.
#   L1  `POST repos/{repo}/labels` with a name that exists, exactly and in another case: what `init` meets when a
#       label appears between its list and its create.
#   L2  `GET repos/{repo}/labels` lists the label created, its name as given.
#
#   bash docs/design/live-checks/owl-537.sh                (sandbox: pitchopp/owlshift-live-sandbox)
#
# Needs bash and gh signed in (gh auth status) with write access to the sandbox; nothing else. It writes only to
# the sandbox: one label, deleted at the end. The report, on standard output, holds no token and no account
# identifier or login: accounts are compared, never printed.

set -u -o pipefail

REPO=pitchopp/owlshift-live-sandbox
LABEL=owlshift-537-probe
UNKNOWN_ID=999999999999

case ${1:-} in
'') ;;
*)
    echo "Usage: bash ${0##*/}   (no argument: it runs on $REPO only)" >&2
    exit 2
    ;;
esac

report() { printf '%s\n' "$*"; }

# The HTTP status of `gh api "$@"`, its body in the file $OUT.
OUT=$(mktemp)
trap 'rm -f "$OUT"' EXIT
call() {
    local dump status
    dump=$(gh api --include "$@" 2>/dev/null)
    status=$(printf '%s\n' "$dump" | head -1 | awk '{print $2}')
    # The body alone: what follows the first empty line of the dump.
    printf '%s\n' "$dump" | tr -d '\r' | awk 'body { print } /^$/ { body = 1 }' >"$OUT"
    printf '%s' "${status:-none}"
}
# The first value of the JSON key $1 in the body, a string or a number: enough for GitHub's flat answers, with no
# jq needed.
field() {
    grep -oE "\"$1\": *(\"[^\"]*\"|[0-9]+)" "$OUT" | head -1 | sed 's/^[^:]*: *//; s/^"//; s/"$//'
}
codes() { grep -o '"code": *"[^"]*"' "$OUT" | sed 's/.*: *"//; s/"$//' | paste -sd, -; }

report "owl-537.sh on $REPO, $(date -u +%Y-%m-%dT%H:%M:%SZ), gh $(gh --version | head -1 | awk '{print $3}')"

status=$(call /user)
me=$(field id)
report "self      GET /user: $status, id numeric: $([[ $me =~ ^[0-9]+$ ]] && echo yes || echo no)"
[[ $me =~ ^[0-9]+$ ]] || { report "stop: no numeric id for the signed-in account"; exit 1; }

status=$(call "/user/$me")
report "U1 known  GET /user/{id}: $status, same account: $([ "$(field id)" = "$me" ] && echo yes || echo no), type: $(field type)"
status=$(call "/user/$UNKNOWN_ID")
report "U1 none   GET /user/{unknown}: $status, message: $(field message)"
status=$(call "/users/$me")
report "U2        GET /users/{id}: $status, same account: $([ "$(field id)" = "$me" ] && echo yes || echo no)"

# A leftover of an earlier run is removed first.
gh api -X DELETE "repos/$REPO/labels/$LABEL" >/dev/null 2>&1
status=$(call -X POST "repos/$REPO/labels" -f name="$LABEL" -f color=ededed)
report "L1 new    POST labels: $status, name: $(field name)"
status=$(call -X POST "repos/$REPO/labels" -f name="$LABEL" -f color=ededed)
report "L1 again  POST labels (same name): $status, message: $(field message), error codes: $(codes)"
upper=$(printf '%s' "$LABEL" | tr '[:lower:]' '[:upper:]')
status=$(call -X POST "repos/$REPO/labels" -f name="$upper" -f color=ededed)
report "L1 case   POST labels (other case): $status, message: $(field message), error codes: $(codes)"
status=$(call "repos/$REPO/labels?per_page=100")
report "L2        GET labels: $status, lists it as given: $(grep -o "\"name\": *\"$LABEL\"" "$OUT" | wc -l | tr -d ' ')"

status=$(call -X DELETE "repos/$REPO/labels/$LABEL")
report "cleanup   DELETE label: $status"
