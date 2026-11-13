#!/usr/bin/env bash
# OWL-554 live check, quoted next to G6 of docs/design/build-plan.md ("P9 · Open-source readiness") and in
# architecture section 5, "Rate limits": which budget each read of the GitHub Issues tracker counts against, whether
# a conditional read answered 304 counts, and whether GraphQL offers a conditional read, so that the drain's
# transport and its rule for a spent budget rest on measures.
#
# Read-only: it makes no write. For each read kind the tracker makes in a pass or an intake (the list by label, an
# issue's events, comments, `blocked_by`, `blocking`, the issue, a comment's reactions) it reads the path twice
# plainly, then twice with `If-None-Match` and the `ETag` the second read gave, and prints each answer's status and
# rate-limit headers. `/rate_limit`, which reading does not move (OWL-532), is read before and after the REST reads
# and around two GraphQL queries. Other clients of the same budgets (the person's other sessions) move the counters
# meanwhile, so each answer's own `x-ratelimit-used` is the figure, `/rate_limit` the cross-check.
#
#   bash docs/design/live-checks/owl-554.sh OWNER/REPO ISSUE COMMENT
#
# ISSUE: an issue of OWNER/REPO with comments and a `blocked_by` list; COMMENT: a comment id with a reaction. Needs
# bash (3.2 is enough), jq, and gh signed in (gh auth status) as an account that reads the repository's issues. It
# runs in about 30 seconds and spends 10 requests of `core`, 4 of the counter `blocked_by` and reactions count on
# (build plan, G6) and 2 points of `graphql`. The report, on standard output, holds no token and no account
# identifier.

set -u -o pipefail

if [ $# -ne 3 ] || [ "$1" = --help ]; then
    sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
fi
REPO=$1 ISSUE=$2 COMMENT=$3
R=repos/$REPO

# One read of `$1`, conditional on the entity tag `$2` when given: "status used= limit= reset= resource= etag=".
read_once() {
    local out
    if [ -n "${2:-}" ]; then
        out=$(gh api -i -H "If-None-Match: $2" "$1" 2>/dev/null)
    else
        out=$(gh api -i "$1" 2>/dev/null)
    fi
    printf '%s\n' "$out" | tr -d '\r' | awk '
        NR == 1 { status = $2 }
        tolower($1) == "x-ratelimit-used:" { used = $2 }
        tolower($1) == "x-ratelimit-limit:" { limit = $2 }
        tolower($1) == "x-ratelimit-reset:" { reset = $2 }
        tolower($1) == "x-ratelimit-resource:" { resource = $2 }
        tolower($1) == "etag:" { etag = $2 ($3 == "" ? "" : " " $3) }
        /^$/ { exit }
        END { printf "%s used=%s limit=%s reset=%s resource=%s etag=%s\n", status, used, limit, reset, resource, etag }'
}

budgets() {
    gh api rate_limit --jq '"/rate_limit: core used=\(.resources.core.used) reset=\(.resources.core.reset); graphql used=\(.resources.graphql.used) reset=\(.resources.graphql.reset)"'
}

echo "OWL-554 live check, $(date -u +%FT%TZ), gh $(gh --version | head -1 | awk '{print $3}')"
budgets
for path in \
    "issues?state=open&per_page=100&page=1" \
    "issues/$ISSUE/events?per_page=100&page=1" \
    "issues/$ISSUE/comments?per_page=100&page=1" \
    "issues/$ISSUE/dependencies/blocked_by?per_page=100&page=1" \
    "issues/$ISSUE/dependencies/blocking?per_page=100&page=1" \
    "issues/$ISSUE" \
    "issues/comments/$COMMENT/reactions?per_page=100&page=1"; do
    echo "== $path"
    echo "  plain       $(read_once "$R/$path")"
    second=$(read_once "$R/$path")
    echo "  plain       $second"
    etag=${second#*etag=}
    if [ -z "$etag" ]; then
        echo "  no ETag: no conditional read"
        continue
    fi
    echo "  conditional $(read_once "$R/$path" "$etag")"
    echo "  conditional $(read_once "$R/$path" "$etag")"
done
budgets
echo "== GraphQL, twice: a one-issue query"
for _ in 1 2; do
    gh api -i graphql -f query="{ repository(owner: \"${REPO%/*}\", name: \"${REPO#*/}\") { issue(number: $ISSUE) { number } } rateLimit { cost } }" 2>/dev/null |
        tr -d '\r' | awk '
            NR == 1 { status = $2 }
            tolower($1) == "x-ratelimit-used:" { used = $2 }
            tolower($1) == "x-ratelimit-reset:" { reset = $2 }
            tolower($1) == "x-ratelimit-resource:" { resource = $2 }
            tolower($1) == "etag:" { etag = $2 }
            /"cost":/ { match($0, /"cost":[0-9]+/); cost = substr($0, RSTART + 7, RLENGTH - 7) }
            END { printf "  %s used=%s reset=%s resource=%s cost=%s etag=%s\n", status, used, reset, resource, cost, (etag == "" ? "none" : etag) }'
done
budgets
