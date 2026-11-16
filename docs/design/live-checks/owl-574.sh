#!/usr/bin/env bash
# OWL-574 live check, quoted next to "The GitHub Issues adapter" of docs/design/build-plan.md, "P9 · Open-source
# readiness": whether Linear's GitHub integration creates an issue when a pull request's title or body, or a commit
# message, holds the placeholder `KEY-NEW` (KEY a team's key), bare or after a closing keyword; and whether the
# runner's current output, and the break it would add (`KEY-`, U+200B, `NEW`), create one. Linear's documentation
# names the placeholder; no check had read it. An agent's `OWL-NEW` must not make the tracker create an issue, which
# only the runner or a person does.
#
# One pull request per row, one after the other, so that an issue Linear creates belongs to the row that ran last.
# Each row's branch holds one commit adding a file of its own, whose message is the row's commit form or a neutral
# one; its pull request carries the row's title and body forms. It is read 20 seconds after it opens, then merged
# with a merge commit whose title and message are the probe's own, and read again 40 seconds after the merge. A read
# lists the team's issues created since the run started, the control's issue aside. The control row's body is
# `Fixes KEY-c`, KEY-c an issue the run creates: it must reach In Progress or Done, or Linear did not see the
# pull requests and the other rows prove nothing.
#
#   LINEAR_API_KEY=... bash docs/design/live-checks/owl-574.sh pitchopp/owlshift-live-sandbox SBX   (--help)
#
# Needs bash (3.2 is enough), jq, curl, gh signed in (gh auth status) as an account that writes the sandbox's
# contents and merges its pull requests with a merge commit, and LINEAR_API_KEY, a key that creates, reads and
# deletes issues in the team. About 15 minutes. It creates on the sandbox one branch, one file and one pull request
# per row, merges each into the default branch, then deletes the branches and removes the files in commits of its
# own; in Linear it creates the control's issue and a headroom issue (deleted at once: it shows the plan's issue cap
# leaves room for an issue Linear's integration would create), and deletes, at the end, the control's issue and every
# issue the rows made Linear create. It refuses pitchopp/owlshift and the team OWL. The report, on standard output,
# holds no token and no account identifier: paste it on the ticket.

set -u -o pipefail
umask 077

PROGRAM=${0##*/}
# The probe's name in everything GitHub or Linear stores: no Linear identifier and no closing word.
PROBE="New-placeholder probe"
INTRO="A probe of Linear's new-issue placeholder (Owlshift's live check)."
NEUTRAL_COMMIT="Add the probe's file"
FORBIDDEN=pitchopp/owlshift
FORBIDDEN_TEAM=OWL
ZWSP=$(printf '\342\200\213')

# The rows, in parallel arrays: a name, the pull request's title, the body line after the intro, the commit's
# message. In each, `KEY` stands for the team's key, `LKEY` for it in lower case and `CTRL` for the control's
# identifier; `Z` is U+200B. The first row is the control. OWL574_ROWS, a list of row numbers ("1 10 11"), runs those
# rows alone; the control's issue is created either way, and read only when row 1 runs.
ROW_NAME=(
    "control: body Fixes KEY-c"
    "body: KEY-NEW alone on its line"
    "body: Fixes KEY-NEW"
    "title: KEY-NEW"
    "commit message: KEY-NEW alone"
    "commit message: Fixes KEY-NEW"
    "body, the runner's output today: Fi<U+200B>xes KEY-NEW"
    "body, the break: KEY-<U+200B>NEW"
    "commit message, the break: KEY-<U+200B>NEW"
    "body: key-new, both in lower case"
    "body: KEY-new, the placeholder in lower case"
    "body: key-NEW, the key in lower case"
)
ROW_TITLE=("PROBE" "PROBE" "PROBE" "PROBE KEY-NEW" "PROBE" "PROBE" "PROBE" "PROBE" "PROBE" "PROBE" "PROBE" "PROBE")
ROW_BODY=("Fixes CTRL" "KEY-NEW" "Fixes KEY-NEW" "" "" "" "FiZxes KEY-NEW" "KEY-ZNEW" "" "LKEY-new" "KEY-new" "LKEY-NEW")
ROW_COMMIT=("" "" "" "" "KEY-NEW" "Fixes KEY-NEW" "" "" "KEY-ZNEW" "" "" "")

usage() {
    cat <<EOF
Usage: LINEAR_API_KEY=... bash $PROGRAM OWNER/REPO TEAM_KEY

  OWNER/REPO  the sandbox: pitchopp/owlshift-live-sandbox, whose repository Linear's GitHub
              integration sees. Never $FORBIDDEN.
  TEAM_KEY    a Linear team whose issues the run may create and delete. Never $FORBIDDEN_TEAM.

About 15 minutes: ${#ROW_NAME[@]} pull requests, one after the other, each merged into the default
branch; then the branches and files are removed and the Linear issues deleted. Paste everything
it prints on standard output on the ticket.
EOF
}

say() { printf '%s\n' "$*" >&2; }
report() { printf '%s\n' "$*"; }
section() { printf '\n== %s ==\n' "$1"; }
fail() {
    FAILED=$((FAILED + 1))
    report "$1"
}
field() { jq -r "$1 // empty" <"$2" 2>/dev/null; }

# rest NAME PATH [METHOD] [BODY_FILE]: `gh api`; the answer in $WORK/NAME.body, the status printed.
rest() {
    local name=$1 path=$2 method=${3:-GET} body=${4:-}
    local args=(api -i -X "$method" -H 'X-GitHub-Api-Version: 2022-11-28' "$path")
    if [ -n "$body" ]; then args+=(--input "$body"); fi
    gh "${args[@]}" </dev/null >"$WORK/$name.raw" 2>"$WORK/$name.err" || true
    awk '{ sub(/\r$/, "") } body { print; next } /^$/ { body = 1 }' "$WORK/$name.raw" >"$WORK/$name.body"
    awk 'NR == 1 { print ($2 == "" ? "000" : $2); exit } END { if (NR == 0) print "000" }' "$WORK/$name.raw"
}

# linear NAME QUERY [VARIABLES]: Linear's GraphQL API, the key read from a header file so that it shows in no
# process list; the answer in $WORK/NAME.body, the status printed.
linear() {
    local name=$1 vars=${3:-}
    if [ -z "$vars" ]; then vars='{}'; fi
    jq -n -c --arg q "$2" --argjson v "$vars" '{query: $q, variables: $v}' >"$WORK/$name.json"
    curl -s -o "$WORK/$name.body" -w '%{http_code}' -H "@$WORK/linear-auth" -H 'Content-Type: application/json' \
        --data-binary "@$WORK/$name.json" https://api.linear.app/graphql 2>"$WORK/$name.err" || true
}

script_sha() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$0" | cut -c1-16
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$0" | cut -c1-16
    else
        echo unknown
    fi
}

# fill TEXT: TEXT with PROBE, KEY, CTRL and Z replaced.
fill() {
    local t=$1
    t=${t//PROBE/$PROBE}
    t=${t//CTRL/$CONTROL}
    t=${t//LKEY/$(printf '%s' "$TEAM" | tr '[:upper:]' '[:lower:]')}
    t=${t//KEY/$TEAM}
    t=${t//Z/$ZWSP}
    printf '%s' "$t"
}

start() {
    local status push
    section start
    status=$(rest repo "repos/$REPO")
    push=$(field .permissions.push "$WORK/repo.body")
    BASE=$(field .default_branch "$WORK/repo.body")
    report "start: gh reads REPO: HTTP $status, push: ${push:-unknown}, allows merge commits: $(field .allow_merge_commit "$WORK/repo.body")"
    if [ "$status" != 200 ] || [ "$push" != true ] || [ -z "$BASE" ]; then
        fail "start: gh's account cannot write REPO; nothing run"
        return 1
    fi
    printf 'Authorization: %s\n' "$LINEAR_API_KEY" >"$WORK/linear-auth"
    # shellcheck disable=SC2016
    status=$(linear team 'query($k: String!) { organization { urlKey } teams(filter: {key: {eq: $k}}) { nodes { id key } } }' \
        "$(jq -n -c --arg k "$TEAM" '{k: $k}')")
    TEAM_ID=$(field '.data.teams.nodes[0].id' "$WORK/team.body")
    report "start: Linear reads workspace $(field .data.organization.urlKey "$WORK/team.body"), team $TEAM: HTTP $status, found: $([ -n "$TEAM_ID" ] && echo yes || echo no)"
    if [ -z "$TEAM_ID" ]; then
        fail "start: the Linear team did not read; nothing run"
        return 1
    fi
    SINCE=$(date -u +%Y-%m-%dT%H:%M:%SZ)
    # The headroom issue: created and deleted at once, so that a refusal by the plan's cap shows here.
    open_linear headroom "$PROBE: headroom, deleted at once"
    if [ -z "$OPEN_ID" ]; then
        fail "start: no headroom issue ($(jq -c '.errors[0].message // empty' <"$WORK/headroom.body" 2>/dev/null)); nothing run"
        return 1
    fi
    delete_linear "$OPEN_ID" >/dev/null
    open_linear control "$PROBE: the control, its pull request says it fixes it"
    if [ -z "$OPEN_ID" ]; then
        fail "start: no control issue; nothing run"
        return 1
    fi
    CONTROL_ID=$OPEN_ID
    CONTROL=$OPEN_IDENT
    report "start: headroom issue created and deleted; control $CONTROL"
}

# open_linear NAME TITLE: sets OPEN_ID and OPEN_IDENT (KEY-n), empty when refused.
open_linear() {
    # shellcheck disable=SC2016
    linear "$1" 'mutation($t: String!, $title: String!, $d: String!) { issueCreate(input: {teamId: $t, title: $title, description: $d}) { issue { id identifier } } }' \
        "$(jq -n -c --arg t "$TEAM_ID" --arg title "$2" --arg d "$INTRO" '{t: $t, title: $title, d: $d}')" >/dev/null
    OPEN_ID=$(field .data.issueCreate.issue.id "$WORK/$1.body")
    OPEN_IDENT=$(field .data.issueCreate.issue.identifier "$WORK/$1.body")
}

delete_linear() {
    # shellcheck disable=SC2016
    linear ldel 'mutation($i: String!) { issueDelete(id: $i) { success } }' "$(jq -n -c --arg i "$1" '{i: $i}')" >/dev/null
    field .data.issueDelete.success "$WORK/ldel.body"
}

# created: sets SEEN to the team's issues created since the run started, the control's aside, as "KEY-n (title;
# links N)" separated by "; ", or "none"; their ids are added to CREATED.
created() {
    local status ids
    # shellcheck disable=SC2016
    status=$(linear new 'query($t: ID, $s: DateTimeOrDuration) { issues(first: 50, filter: {team: {id: {eq: $t}}, createdAt: {gt: $s}}) { nodes { id identifier title attachments(first: 10) { nodes { url } } } } }' \
        "$(jq -n -c --arg t "$TEAM_ID" --arg s "$SINCE" '{t: $t, s: $s}')")
    if [ "$status" != 200 ] || ! jq -e '.data.issues.nodes' <"$WORK/new.body" >/dev/null 2>&1; then
        FAILED=$((FAILED + 1))
        SEEN="UNREAD (HTTP $status)"
        return 0
    fi
    ids=$(jq -r --arg c "$CONTROL_ID" '.data.issues.nodes[] | select(.id != $c) | .id' <"$WORK/new.body")
    for id in $ids; do
        case " ${CREATED[*]:-} " in *" $id "*) ;; *) CREATED+=("$id") ;; esac
    done
    SEEN=$(jq -r --arg c "$CONTROL_ID" '[.data.issues.nodes[] | select(.id != $c) | "\(.identifier) (\(.title); links \(.attachments.nodes | length))"] | if length == 0 then "none" else join("; ") end' <"$WORK/new.body")
}

control_state() {
    # shellcheck disable=SC2016
    linear cst 'query($i: String!) { issue(id: $i) { state { type name } attachments(first: 10) { nodes { id } } } }' \
        "$(jq -n -c --arg i "$CONTROL_ID" '{i: $i}')" >/dev/null
    jq -r '.data.issue | if . == null then "unread" else "\(.state.type)/\(.state.name), links \(.attachments.nodes | length)" end' <"$WORK/cst.body" 2>/dev/null
}

# run_row I: one row's branch, commit, pull request and merge; reports the row.
run_row() {
    local i=$1 status branch file title body message pr sha after_open after_merge try
    branch="new-probe-$(date +%s)-$i"
    jq -n -c --arg r "refs/heads/$branch" --arg s "$BASE_SHA" '{ref: $r, sha: $s}' >"$WORK/mkref.json"
    status=$(rest mkref "repos/$REPO/git/refs" POST "$WORK/mkref.json")
    if [ "$status" != 201 ]; then
        fail "row $((i + 1)): no branch (HTTP $status); stopped"
        return 1
    fi
    BRANCHES+=("$branch")
    file="new-probe/$branch.md"
    message=$(fill "${ROW_COMMIT[$i]}")
    if [ -z "$message" ]; then message=$NEUTRAL_COMMIT; fi
    jq -n -c --arg b "$branch" --arg m "$message" --arg c "$(printf 'A new-placeholder probe, row %s.\n' "$((i + 1))" | base64)" \
        '{message: $m, content: $c, branch: $b}' >"$WORK/file.json"
    status=$(rest file "repos/$REPO/contents/$file" PUT "$WORK/file.json")
    if [ "$status" != 201 ]; then
        fail "row $((i + 1)): no commit (HTTP $status); stopped"
        return 1
    fi
    title=$(fill "${ROW_TITLE[$i]}")
    body="$INTRO"
    if [ -n "${ROW_BODY[$i]}" ]; then body="$body

$(fill "${ROW_BODY[$i]}")"; fi
    jq -n -c --arg h "$branch" --arg b "$BASE" --arg t "$title" --arg body "$body" \
        '{title: $t, head: $h, base: $b, body: $body}' >"$WORK/pr.json"
    status=$(rest pr "repos/$REPO/pulls" POST "$WORK/pr.json")
    pr=$(field .number "$WORK/pr.body")
    if [ -z "$pr" ]; then
        fail "row $((i + 1)): no pull request (HTTP $status); stopped"
        return 1
    fi
    PRS+=("$pr")
    sleep 20
    created
    after_open=$SEEN
    jq -n -c --arg t "$PROBE" '{merge_method: "merge", commit_title: $t, commit_message: "A new-placeholder probe."}' >"$WORK/merge.json"
    try=1
    while :; do
        status=$(rest merge "repos/$REPO/pulls/$pr/merge" PUT "$WORK/merge.json")
        if [ "$status" = 200 ] || [ "$try" -ge 4 ]; then break; fi
        try=$((try + 1))
        sleep 5
    done
    sha=$(field .sha "$WORK/merge.body")
    if [ "$status" != 200 ] || [ -z "$sha" ]; then
        fail "row $((i + 1)): #$pr not merged (HTTP $status); stopped"
        return 1
    fi
    MERGED_FILES+=("$file")
    BASE_SHA=$sha
    sleep 40
    created
    after_merge=$SEEN
    if [ "$i" = 0 ]; then
        CONTROL_STATE=$(control_state)
        case $CONTROL_STATE in
        started/* | completed/*) ;;
        *) fail "row 1: the control stayed $CONTROL_STATE: Linear did not see the pull request, the rows prove nothing" ;;
        esac
        report "| $((i + 1)) | ${ROW_NAME[$i]} | #$pr | $after_open | $after_merge (control: $CONTROL_STATE) |"
    else
        report "| $((i + 1)) | ${ROW_NAME[$i]} | #$pr | $after_open | $after_merge |"
    fi
}

# shellcheck disable=SC2329 # run by the EXIT trap
cleanup() {
    local i sha status
    if [ "${CLEANED:-}" = yes ]; then return 0; fi
    CLEANED=yes
    if [ "${STARTED:-no}" = yes ]; then
        section cleanup
        for b in ${BRANCHES[@]+"${BRANCHES[@]}"}; do
            status=$(rest delref "repos/$REPO/git/refs/heads/$b" DELETE)
            if [ "$status" != 204 ] && [ "$status" != 422 ]; then report "cleanup: branch $b not deleted: HTTP $status"; fi
        done
        for p in ${PRS[@]+"${PRS[@]}"}; do
            if [ "$(rest prget "repos/$REPO/pulls/$p")" = 200 ] && [ "$(field .state "$WORK/prget.body")" = open ]; then
                jq -n -c '{state: "closed"}' >"$WORK/prclose.json"
                report "cleanup: close #$p unmerged: HTTP $(rest prclose "repos/$REPO/pulls/$p" PATCH "$WORK/prclose.json")"
            fi
        done
        for f in ${MERGED_FILES[@]+"${MERGED_FILES[@]}"}; do
            rest getfile "repos/$REPO/contents/$f?ref=$BASE" >/dev/null
            sha=$(field .sha "$WORK/getfile.body")
            jq -n -c --arg s "$sha" --arg b "$BASE" --arg m "$PROBE: remove its file" '{message: $m, sha: $s, branch: $b}' >"$WORK/delfile.json"
            status=$(rest delfile "repos/$REPO/contents/$f" DELETE "$WORK/delfile.json")
            if [ "$status" != 200 ]; then report "cleanup: $f not removed from $BASE: HTTP $status"; fi
        done
        report "cleanup: ${#BRANCHES[@]} branches deleted, ${#MERGED_FILES[@]} files removed from $BASE"
        if [ -n "${CONTROL_ID:-}" ]; then
            report "cleanup: delete the control $CONTROL: $(delete_linear "$CONTROL_ID")"
        fi
        i=0
        for id in ${CREATED[@]+"${CREATED[@]}"}; do
            i=$((i + 1))
            report "cleanup: delete created issue $i of ${#CREATED[@]}: $(delete_linear "$id")"
        done
        report ""
        report "done, failures: $FAILED, issues Linear created: ${#CREATED[@]}."
    fi
    rm -rf "${WORK:-/nonexistent}"
}

for arg in "$@"; do
    case $arg in
    -h | --help)
        usage
        exit 0
        ;;
    esac
done
if [ $# -ne 2 ]; then
    usage >&2
    exit 2
fi
REPO=$1
TEAM=$(printf '%s' "$2" | tr '[:lower:]' '[:upper:]')
case $REPO in
*/*) ;;
*)
    say "$PROGRAM: $REPO must have the form owner/name"
    exit 2
    ;;
esac
if [ "$(printf '%s' "$REPO" | tr '[:upper:]' '[:lower:]')" = "$FORBIDDEN" ]; then
    say "$PROGRAM: never run against $FORBIDDEN: it merges pull requests"
    exit 2
fi
if [ "$TEAM" = "$FORBIDDEN_TEAM" ]; then
    say "$PROGRAM: never run against the Linear team $FORBIDDEN_TEAM: it creates and deletes issues"
    exit 2
fi
if [ -z "${LINEAR_API_KEY:-}" ]; then
    say "$PROGRAM: LINEAR_API_KEY is needed"
    exit 2
fi
for tool in gh jq curl base64; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        say "$PROGRAM: $tool is needed"
        exit 2
    fi
done

WORK=$(mktemp -d "${TMPDIR:-/tmp}/owl574.XXXXXX") || exit 2
FAILED=0
TEAM_ID=
CONTROL_ID=
CONTROL=
CONTROL_STATE=
SEEN=
BASE=
BASE_SHA=
SINCE=
BRANCHES=()
PRS=()
MERGED_FILES=()
CREATED=()
trap cleanup EXIT
trap 'exit 130' INT TERM

report "OWL-574 check, $(date -u +%Y-%m-%dT%H:%M:%SZ), $(gh --version | head -n 1 | cut -d' ' -f1-3), $(jq --version), $(uname -s), script sha256 $(script_sha), Linear team: $TEAM"
if ! gh auth status >/dev/null 2>&1 </dev/null; then
    say "$PROGRAM: gh is not signed in (gh auth status)"
    exit 2
fi
STARTED=yes
start || exit 1
rest ref "repos/$REPO/git/ref/heads/$BASE" >/dev/null
BASE_SHA=$(field .object.sha "$WORK/ref.body")
if [ -z "$BASE_SHA" ]; then
    fail "start: the default branch's head did not read; nothing run"
    exit 1
fi
section rows
report "Each row: the team's issues created since the run started (the control's aside), 20 s after the pull request opened, then 40 s after its merge."
report ""
report "| # | row | pull request | after it opened | after its merge |"
report "|---|-----|--------------|-----------------|-----------------|"
i=0
while [ "$i" -lt "${#ROW_NAME[@]}" ]; do
    case " ${OWL574_ROWS:-$((i + 1))} " in
    *" $((i + 1)) "*) run_row "$i" || exit 1 ;;
    esac
    i=$((i + 1))
done
exit "$FAILED"
