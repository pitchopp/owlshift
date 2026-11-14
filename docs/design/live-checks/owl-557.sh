#!/usr/bin/env bash
# OWL-557 live check, quoted next to "The GitHub Issues adapter" of docs/design/build-plan.md, "P9 · Open-source
# readiness": where an issue's last edit can be read, so that the adapter holds an issue whose text changed after
# the admitter's label. Which times move, and which do not, on each change a person makes to an issue:
#   - REST's `updated_at` and GraphQL's `updatedAt`, `lastEditedAt` and `userContentEdits` of the issue;
#   - the issue's REST events (a `renamed` event for a title edit, anything for a body edit).
# The changes, a few seconds apart so that whole-second times tell them apart: a label added, a comment posted, the
# title edited, the body edited. A second issue is opened with its body and its label in one call, as a person
# filing a ready issue does, to read what an issue never edited shows.
#
#   bash docs/design/live-checks/owl-557.sh pitchopp/owlshift-live-sandbox      (--help: what it does)
#
# Needs bash (3.2 is enough), jq, and gh signed in (gh auth status) as an account that can write the sandbox's
# issues. It runs alone in about half a minute. It creates on the sandbox two issues and a label `owl-557-probe`,
# and at the end closes the issues as not planned and deletes the label. It refuses pitchopp/owlshift. The report,
# on standard output, holds no token and no account identifier: an editor is compared with the signed-in account.

set -u -o pipefail
umask 077

PROGRAM=${0##*/}
LABEL=owl-557-probe
BODY="A probe of OWL-557 (docs/design/live-checks/owl-557.sh), closed at the end of its run."
# The repository this check must never write to: Owlshift's own.
FORBIDDEN=pitchopp/owlshift
PAUSE=${OWL557_PAUSE:-3}

usage() {
    cat <<EOF
Usage: bash $PROGRAM OWNER/REPO

  OWNER/REPO  the sandbox: pitchopp/owlshift-live-sandbox. Never $FORBIDDEN.

Opens two issues and a label on OWNER/REPO, changes the first one step by step and reads, after each step, every
time that could tell the issue's text changed. Closes the issues as not planned and deletes the label at the end.
EOF
}

case ${1:-} in
    -h | --help) usage; exit 0 ;;
    */*) REPO=$1 ;;
    *) usage >&2; exit 2 ;;
esac
lower=$(printf '%s' "$REPO" | tr '[:upper:]' '[:lower:]')
if [ "$lower" = "$FORBIDDEN" ]; then
    echo "$PROGRAM: refused: $REPO is Owlshift's own repository" >&2
    exit 2
fi
OWNER=${REPO%%/*}
NAME=${REPO#*/}
command -v jq >/dev/null || { echo "$PROGRAM: jq is missing" >&2; exit 2; }
gh auth status >/dev/null 2>&1 || { echo "$PROGRAM: gh is not signed in" >&2; exit 2; }

VIEWER=$(gh api graphql -f query='query { viewer { login } }' --jq .data.viewer.login) || exit 1

made=()
cleanup() {
    for n in "${made[@]:-}"; do
        [ -n "$n" ] || continue
        gh api -X PATCH "repos/$REPO/issues/$n" -f state=closed -f state_reason=not_planned >/dev/null 2>&1 \
            || echo "cleanup: could not close #$n" >&2
    done
    gh api -X DELETE "repos/$REPO/labels/$LABEL" >/dev/null 2>&1 || true
}
trap cleanup EXIT

QUERY='query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    issue(number: $number) {
      createdAt updatedAt lastEditedAt includesCreatedEdit
      editor { login }
      userContentEdits(first: 20) { totalCount nodes { createdAt editedAt editor { login } } }
    }
  }
}'

# One line per read: the step, then each time, an editor named only as "viewer" or "other".
read_issue() {
    local step=$1 n=$2 rest gql events
    rest=$(gh api "repos/$REPO/issues/$n" --jq '{updated_at}') || return 1
    gql=$(gh api graphql -f query="$QUERY" -f owner="$OWNER" -f name="$NAME" -F number="$n" \
        --jq '.data.repository.issue') || return 1
    events=$(gh api "repos/$REPO/issues/$n/events?per_page=100" \
        --jq '[.[] | {event, created_at, rename: (.rename // null)}]') || return 1
    jq -n --arg step "$step" --arg viewer "$VIEWER" --argjson rest "$rest" --argjson gql "$gql" \
        --argjson events "$events" '
        def who: if . == null then null elif .login == $viewer then "viewer" else "other" end;
        {
          step: $step,
          rest_updated_at: $rest.updated_at,
          updatedAt: $gql.updatedAt,
          lastEditedAt: $gql.lastEditedAt,
          includesCreatedEdit: $gql.includesCreatedEdit,
          editor: ($gql.editor | who),
          userContentEdits: ($gql.userContentEdits | {totalCount,
            nodes: [.nodes[] | {createdAt, editedAt, editor: (.editor | who)}]}),
          events: $events
        }' -c
}

stamp() { date -u +%Y-%m-%dT%H:%M:%SZ; }

echo "OWL-557 live check on $REPO, $(stamp), gh $(gh --version | head -1 | awk '{print $3}'), $(jq --version)"

gh api -X POST "repos/$REPO/labels" -f name="$LABEL" -f color=ededed >/dev/null 2>&1 || true

A=$(gh api -X POST "repos/$REPO/issues" -f title="OWL-557 probe A" -f body="$BODY" --jq .number) || exit 1
made+=("$A")
echo "issue A #$A opened at $(stamp), no label"
read_issue "A opened" "$A" || exit 1

sleep "$PAUSE"
gh api -X POST "repos/$REPO/issues/$A/labels" -f "labels[]=$LABEL" >/dev/null || exit 1
echo "label added at $(stamp)"
sleep 1
read_issue "A labelled" "$A" || exit 1

sleep "$PAUSE"
gh api -X POST "repos/$REPO/issues/$A/comments" -f body="A comment of the OWL-557 probe." >/dev/null || exit 1
echo "comment posted at $(stamp)"
sleep 1
read_issue "A commented" "$A" || exit 1

sleep "$PAUSE"
gh api -X PATCH "repos/$REPO/issues/$A" -f title="OWL-557 probe A, title edited" >/dev/null || exit 1
echo "title edited at $(stamp)"
sleep 1
read_issue "A title edited" "$A" || exit 1

sleep "$PAUSE"
gh api -X PATCH "repos/$REPO/issues/$A" -f body="$BODY (body edited)" >/dev/null || exit 1
echo "body edited at $(stamp)"
sleep 1
read_issue "A body edited" "$A" || exit 1

sleep "$PAUSE"
gh api -X PATCH "repos/$REPO/issues/$A" -f title="OWL-557 probe A, title edited twice" >/dev/null || exit 1
echo "title edited again at $(stamp), after the body"
sleep 1
read_issue "A title edited after the body" "$A" || exit 1

B=$(gh api -X POST "repos/$REPO/issues" -f title="OWL-557 probe B" -f body="$BODY" -f "labels[]=$LABEL" \
    --jq .number) || exit 1
made+=("$B")
echo "issue B #$B opened with its label at $(stamp)"
sleep 2
read_issue "B opened with its label" "$B" || exit 1

echo "done at $(stamp); closing #$A and #$B as not planned, deleting the label"
