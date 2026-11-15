#!/usr/bin/env bash
# OWL-569 live check, quoted next to "The GitHub Issues adapter" of docs/design/build-plan.md, "P9 · Open-source
# readiness": which shapes of "closing keyword, then the text after it" close an issue, on GitHub and through Linear's
# GitHub integration, in a pull request's body and in a commit message that reaches the default branch; so that the
# runner can break a keyword only when an issue reference follows it, and leave whole an agent's "fix: handle empty
# input" or "Fix the parser", which a commit-message linter reads.
#
# One pull request. Its body carries one line per body form, each naming an issue of its own (a victim), and ends
# with `Closes #T`, T the run's target. Its branch carries one commit per commit form, each message that form alone,
# naming a victim of its own, then a last commit whose message carries the controls. It is merged with a merge
# commit whose title and message are the probe's own and name no issue, so that the branch's commits reach the
# default branch as they are and nothing else names a victim. Then each issue is read: a victim closed is a form its
# platform closes on. The "rule" column says what the narrowed rule does to the form's keyword, "breaks" it or
# "keeps" it whole: a kept form that closes its issue means the rule is unsafe, and the report counts them. Controls
# (`Fixes #n` and `Fixes KEY-n`, in the body and in a commit) must close, or the rows of their platform and place
# prove nothing. The run goes through the person's own `gh` login, as owl-556.sh's did.
#
#   bash docs/design/live-checks/owl-569.sh pitchopp/owlshift-live-sandbox [SANDBOX_TEAM_KEY]   (--help: what it does)
#
# Needs bash (3.2 is enough), jq, and gh signed in (gh auth status) as an account that can write the sandbox's issues
# and contents and merge its pull requests with a merge commit; with a team key, also curl and LINEAR_API_KEY, a key
# that creates and deletes issues in that team. It runs alone in about 3 minutes. It creates on the sandbox 22 issues,
# a branch `keyword-probe-<time>` with one file and 9 commits, and a pull request, which it merges into the default
# branch; then it deletes the branch, removes the file from the default branch in a commit of its own, and closes as
# not planned every issue the merge left open. With a team key it creates 17 Linear issues in that team and deletes
# them at the end. It refuses pitchopp/owlshift and the team OWL. The report, on standard output, holds no token and
# no account identifier: paste it on the ticket.

set -u -o pipefail
umask 077

PROGRAM=${0##*/}
# The probe's name in everything GitHub or Linear stores: no Linear identifier, which Linear's integration would link,
# and no closing word.
PROBE="Keyword-shape probe"
BODY="A probe of the shapes a closing keyword takes (Owlshift's live check). The run ends it as not planned unless its pull request ends it first."
# The repository and the Linear team this check must never write to: Owlshift's own.
FORBIDDEN=pitchopp/owlshift
FORBIDDEN_TEAM=OWL

# The GitHub body forms, in parallel arrays: a name, a jq expression of the line the body carries, given $n (the
# victim's number), $u (the victim's URL) and $c (the control's number), and what the narrowed rule does to its
# keyword. The first is the control.
FORM_NAME=(
    "control: Fixes #n"
    "a colon: Fixes: #n"
    "the reference on the next line: Fix<LF>#n"
    "the reference in the next paragraph: Fix<LF><LF>#n"
    "parentheses: Fixes (#n)"
    "bold: Fixes **#n**"
    "a link whose text is the reference: Fixes [#n](<issue URL>)"
    "a link whose text is a phrase: Fixes [the bug](<issue URL>)"
    "an autolink: Fixes <<issue URL>>"
    "Fixes GH-n"
    "a second reference: Fixes #c, #n (#c the control's issue)"
    "Conventional Commits, the reference as scope: fix(#n): handle empty input"
    "Conventional Commits, a scope between: fix(parser): #n"
    "Conventional Commits, a word between: fix: handle #n"
    "a phrase between: Fix the parser (#n)"
    "a phrase between: Fixes the bug in #n"
)
FORM_JQ=(
    '"Fixes #\($n)"'
    '"Fixes: #\($n)"'
    '"Fix\n#\($n)"'
    '"Fix\n\n#\($n)"'
    '"Fixes (#\($n))"'
    '"Fixes **#\($n)**"'
    '"Fixes [#\($n)](\($u))"'
    '"Fixes [the bug](\($u))"'
    '"Fixes <\($u)>"'
    '"Fixes GH-\($n)"'
    '"Fixes #\($c), #\($n)"'
    '"fix(#\($n)): handle empty input"'
    '"fix(parser): #\($n)"'
    '"fix: handle #\($n)"'
    '"Fix the parser (#\($n))"'
    '"Fixes the bug in #\($n)"'
)
FORM_RULE=(control breaks breaks breaks breaks breaks breaks breaks breaks breaks breaks breaks keeps keeps keeps keeps)

# The GitHub commit forms: each is the whole message of a commit of its own, given $n. The last commit's message
# carries the commit control, `Fixes #n`, and, with a team key, Linear's.
CFORM_NAME=(
    "Conventional Commits subject, a word between: fix: handle #n"
    "Conventional Commits subject, a scope between: fix(parser): #n"
    "subject, a phrase between: Fix the parser (#n)"
    "subject, a phrase between: Fixes the bug in #n"
)
CFORM_JQ=(
    '"fix: handle #\($n)"'
    '"fix(parser): #\($n)"'
    '"Fix the parser (#\($n))"'
    '"Fixes the bug in #\($n)"'
)

# The Linear body forms, given $k (the victim's identifier, KEY-n), $l (its URL) and $c (the control's identifier).
# Linear's documentation (https://linear.app/docs/github, read on 2026-11-15) names a magic word followed by an issue's
# ID or URL, several IDs after one word, in a pull request's title or description and in a commit message.
LFORM_NAME=(
    "control: Fixes KEY-n"
    "a colon: Fixes: KEY-n"
    "the reference on the next line: Fix<LF>KEY-n"
    "parentheses: Fixes (KEY-n)"
    "the issue's URL: Fixes <issue URL>"
    "lower case: Fixes key-n"
    "a second reference: Fixes KEY-c, KEY-n (KEY-c the control's issue)"
    "Conventional Commits, the reference as scope: fix(KEY-n): handle empty input"
    "Conventional Commits, a scope between: fix(parser): KEY-n"
    "Conventional Commits, a word between: fix: handle KEY-n"
    "a phrase between: Fix the parser (KEY-n)"
    "a word between: Fixes issue KEY-n"
)
LFORM_JQ=(
    '"Fixes \($k)"'
    '"Fixes: \($k)"'
    '"Fix\n\($k)"'
    '"Fixes (\($k))"'
    '"Fixes \($l)"'
    '"Fixes \($k | ascii_downcase)"'
    '"Fixes \($c), \($k)"'
    '"fix(\($k)): handle empty input"'
    '"fix(parser): \($k)"'
    '"fix: handle \($k)"'
    '"Fix the parser (\($k))"'
    '"Fixes issue \($k)"'
)
LFORM_RULE=(control breaks breaks breaks breaks breaks breaks breaks keeps keeps keeps keeps)

# The Linear commit forms, given $k, as the GitHub ones.
LCFORM_NAME=(
    "Conventional Commits subject, a word between: fix: handle KEY-n"
    "Conventional Commits subject, a scope between: fix(parser): KEY-n"
    "subject, a phrase between: Fix the parser (KEY-n)"
    "subject, a word between: Fixes issue KEY-n"
)
LCFORM_JQ=(
    '"fix: handle \($k)"'
    '"fix(parser): \($k)"'
    '"Fix the parser (\($k))"'
    '"Fixes issue \($k)"'
)

usage() {
    cat <<EOF
Usage: bash $PROGRAM OWNER/REPO [SANDBOX_TEAM_KEY]

  OWNER/REPO        the sandbox: pitchopp/owlshift-live-sandbox. Never $FORBIDDEN.
  SANDBOX_TEAM_KEY  optional: a Linear team whose issues the run may create and delete, in a
                    workspace whose GitHub integration is connected to OWNER/REPO. Never $FORBIDDEN_TEAM.
                    Needs LINEAR_API_KEY.

Needs jq and gh signed in as an account that writes the sandbox and may merge with a merge
commit. About 3 minutes, alone: it opens $((${#FORM_NAME[@]} + ${#CFORM_NAME[@]} + 2)) issues, a branch with one file and
$((${#CFORM_NAME[@]} + ${#LCFORM_NAME[@]} + 1)) commits, and a pull request, merges it into the default branch, reads the issues,
then deletes the file and the branch and ends the issues still open as not planned. With a
team key it also opens $((${#LFORM_NAME[@]} + ${#LCFORM_NAME[@]} + 1)) Linear issues there, reads them, and deletes them.
Paste everything it prints on standard output on the ticket.
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

# rest NAME PATH [METHOD] [BODY_FILE]: `gh api` as the signed-in person; the answer in $WORK/NAME.body, the status
# printed. PATH is relative to the API's root, or `graphql`.
rest() {
    local name=$1 path=$2 method=${3:-GET} body=${4:-}
    local args=(api -i -X "$method" -H 'X-GitHub-Api-Version: 2022-11-28' "$path")
    if [ -n "$body" ]; then args+=(--input "$body"); fi
    gh "${args[@]}" </dev/null >"$WORK/$name.raw" 2>"$WORK/$name.err" || true
    # The status line, the headers, a blank line, then the body.
    awk '{ sub(/\r$/, "") } body { print; next } /^$/ { body = 1 }' "$WORK/$name.raw" >"$WORK/$name.body"
    awk 'NR == 1 { print ($2 == "" ? "000" : $2); exit } END { if (NR == 0) print "000" }' "$WORK/$name.raw"
}

# linear NAME QUERY [VARIABLES]: Linear's GraphQL API with LINEAR_API_KEY, read from a header file so that it shows
# in no process list; the answer in $WORK/NAME.body, the status printed.
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

# Sets BASE (the default branch) and BASE_SHA.
start() {
    local status push
    section start
    status=$(rest repo "repos/$REPO")
    push=$(field .permissions.push "$WORK/repo.body")
    BASE=$(field .default_branch "$WORK/repo.body")
    report "start: gh reads REPO: HTTP $status, push: ${push:-unknown}, private: $(field .private "$WORK/repo.body"), allows merge commits: $(field .allow_merge_commit "$WORK/repo.body")"
    if [ "$status" != 200 ] || [ "$push" != true ] || [ -z "$BASE" ]; then
        fail "start: gh's account cannot write REPO ($(head -n 1 "$WORK/repo.err")); nothing run"
        return 1
    fi
    status=$(rest ref "repos/$REPO/git/ref/heads/$BASE")
    BASE_SHA=$(field .object.sha "$WORK/ref.body")
    if [ -z "$BASE_SHA" ]; then
        fail "start: the default branch's head did not read: HTTP $status; nothing run"
        return 1
    fi
}

# Sets TEAM_ID from TEAM, when a team key was given.
linear_start() {
    local status
    if [ -z "$TEAM" ]; then return 0; fi
    printf 'Authorization: %s\n' "$LINEAR_API_KEY" >"$WORK/linear-auth"
    # shellcheck disable=SC2016
    status=$(linear team 'query($k: String!) { organization { urlKey } teams(filter: {key: {eq: $k}}) { nodes { id key } } }' \
        "$(jq -n -c --arg k "$TEAM" '{k: $k}')")
    TEAM_ID=$(field '.data.teams.nodes[0].id' "$WORK/team.body")
    report "start: Linear reads workspace $(field .data.organization.urlKey "$WORK/team.body"), team $TEAM: HTTP $status, found: $([ -n "$TEAM_ID" ] && echo yes || echo no)"
    if [ -z "$TEAM_ID" ]; then
        fail "start: the Linear team $TEAM did not read; nothing run"
        return 1
    fi
}

OPENED=()
# open_issue NAME TITLE: sets OPEN_NUMBER and OPEN_URL.
open_issue() {
    local status
    jq -n -c --arg t "$2" --arg b "$BODY" '{title: $t, body: $b}' >"$WORK/$1.json"
    status=$(rest "$1" "repos/$REPO/issues" POST "$WORK/$1.json")
    OPEN_NUMBER=$(field .number "$WORK/$1.body")
    OPEN_URL=$(field .html_url "$WORK/$1.body")
    if [ -n "$OPEN_NUMBER" ]; then
        OPENED+=("$OPEN_NUMBER")
    else
        fail "open: '$2': HTTP $status"
    fi
}

LOPENED=()
# open_linear NAME TITLE: sets LOPEN_ID (the identifier, KEY-n) and LOPEN_URL.
open_linear() {
    local status id
    # shellcheck disable=SC2016
    status=$(linear "$1" 'mutation($t: String!, $title: String!, $d: String!) { issueCreate(input: {teamId: $t, title: $title, description: $d}) { issue { id identifier url } } }' \
        "$(jq -n -c --arg t "$TEAM_ID" --arg title "$2" --arg d "$BODY" '{t: $t, title: $title, d: $d}')")
    id=$(field .data.issueCreate.issue.id "$WORK/$1.body")
    LOPEN_ID=$(field .data.issueCreate.issue.identifier "$WORK/$1.body")
    LOPEN_URL=$(field .data.issueCreate.issue.url "$WORK/$1.body")
    if [ -n "$id" ]; then
        LOPENED+=("$id")
    else
        fail "open: Linear '$2': HTTP $status"
    fi
}

# The victims: the GitHub body forms', the GitHub commit forms', the commit control; the Linear body forms' (with
# their URLs and Linear ids), the Linear commit forms', Linear's commit control.
VICTIM=()
VICTIM_URL=()
CVICTIM=()
CCONTROL=
LVICTIM=()
LVICTIM_URL=()
LCVICTIM=()
LCCONTROL=
TARGET=
BRANCH=
PR=
MERGED=no
FILE_PATH=
FILE_SHA=
COMMITS=0

issues() {
    local i
    section issues
    open_issue target "$PROBE: the target, its pull request ends with Closes"
    TARGET=$OPEN_NUMBER
    i=0
    while [ "$i" -lt "${#FORM_NAME[@]}" ]; do
        open_issue "v$i" "$PROBE: victim of body form $((i + 1)), ${FORM_NAME[$i]}"
        VICTIM+=("$OPEN_NUMBER")
        VICTIM_URL+=("$OPEN_URL")
        i=$((i + 1))
    done
    i=0
    while [ "$i" -lt "${#CFORM_NAME[@]}" ]; do
        open_issue "c$i" "$PROBE: victim of commit form $((i + 1)), ${CFORM_NAME[$i]}"
        CVICTIM+=("$OPEN_NUMBER")
        i=$((i + 1))
    done
    open_issue ccontrol "$PROBE: victim of the commit control, a body line Fixes #n"
    CCONTROL=$OPEN_NUMBER
    if [ -z "$TARGET" ] || [ "${#OPENED[@]}" -ne $((${#FORM_NAME[@]} + ${#CFORM_NAME[@]} + 2)) ]; then
        fail "issues: not all opened; not run"
        return 1
    fi
    report "issues: target #$TARGET, victims #${VICTIM[0]} to #$CCONTROL"
    if [ -z "$TEAM" ]; then return 0; fi
    i=0
    while [ "$i" -lt "${#LFORM_NAME[@]}" ]; do
        open_linear "l$i" "$PROBE: Linear body form $((i + 1)), ${LFORM_NAME[$i]}"
        LVICTIM+=("$LOPEN_ID")
        LVICTIM_URL+=("$LOPEN_URL")
        i=$((i + 1))
    done
    i=0
    while [ "$i" -lt "${#LCFORM_NAME[@]}" ]; do
        open_linear "lc$i" "$PROBE: Linear commit form $((i + 1)), ${LCFORM_NAME[$i]}"
        LCVICTIM+=("$LOPEN_ID")
        i=$((i + 1))
    done
    open_linear lccontrol "$PROBE: Linear commit control, a body line Fixes KEY-n"
    LCCONTROL=$LOPEN_ID
    if [ "${#LOPENED[@]}" -ne $((${#LFORM_NAME[@]} + ${#LCFORM_NAME[@]} + 1)) ]; then
        fail "issues: not all Linear issues opened; not run"
        return 1
    fi
    report "issues: Linear victims ${LVICTIM[0]} to $LCCONTROL"
}

# commit MESSAGE: one commit on the branch, rewriting the probe's one file, whose message is MESSAGE.
commit() {
    local status
    COMMITS=$((COMMITS + 1))
    if [ -z "$FILE_SHA" ]; then
        jq -n -c --arg b "$BRANCH" --arg m "$1" --arg c "$(printf 'A keyword-shape probe, commit %s.\n' "$COMMITS" | base64)" \
            '{message: $m, content: $c, branch: $b}' >"$WORK/file.json"
    else
        jq -n -c --arg b "$BRANCH" --arg m "$1" --arg c "$(printf 'A keyword-shape probe, commit %s.\n' "$COMMITS" | base64)" \
            --arg s "$FILE_SHA" '{message: $m, content: $c, branch: $b, sha: $s}' >"$WORK/file.json"
    fi
    status=$(rest file "repos/$REPO/contents/$FILE_PATH" PUT "$WORK/file.json")
    FILE_SHA=$(field .content.sha "$WORK/file.body")
    if [ "$status" != 200 ] && [ "$status" != 201 ]; then
        fail "pull request: commit $COMMITS not made (HTTP $status); not run"
        return 1
    fi
}

probe() {
    local i status line sha try controls
    # The body: one paragraph per form, then the runner's closing line, as `with_closing_line` writes it.
    : >"$WORK/lines"
    i=0
    while [ "$i" -lt "${#FORM_NAME[@]}" ]; do
        line=$(jq -n -r --arg n "${VICTIM[$i]}" --arg u "${VICTIM_URL[$i]}" --arg c "${VICTIM[0]}" "${FORM_JQ[$i]}")
        printf 'Form %s:\n\n%s\n\n' "$((i + 1))" "$line" >>"$WORK/lines"
        i=$((i + 1))
    done
    i=0
    while [ "$i" -lt "${#LVICTIM[@]}" ]; do
        line=$(jq -n -r --arg k "${LVICTIM[$i]}" --arg l "${LVICTIM_URL[$i]}" --arg c "${LVICTIM[0]}" "${LFORM_JQ[$i]}")
        printf 'Linear form %s:\n\n%s\n\n' "$((i + 1))" "$line" >>"$WORK/lines"
        i=$((i + 1))
    done
    printf 'Closes #%s\n' "$TARGET" >>"$WORK/lines"

    section "pull request"
    BRANCH="keyword-probe-$(date +%s)"
    jq -n -c --arg r "refs/heads/$BRANCH" --arg s "$BASE_SHA" '{ref: $r, sha: $s}' >"$WORK/mkref.json"
    status=$(rest mkref "repos/$REPO/git/refs" POST "$WORK/mkref.json")
    report "pull request: branch created from $BASE: HTTP $status"
    if [ "$status" != 201 ]; then
        BRANCH=
        fail "pull request: no branch; not run"
        return 1
    fi
    FILE_PATH="keyword-probe/$BRANCH.md"
    i=0
    while [ "$i" -lt "${#CFORM_NAME[@]}" ]; do
        commit "$(jq -n -r --arg n "${CVICTIM[$i]}" "${CFORM_JQ[$i]}")" || return 1
        i=$((i + 1))
    done
    i=0
    while [ "$i" -lt "${#LCVICTIM[@]}" ]; do
        commit "$(jq -n -r --arg k "${LCVICTIM[$i]}" "${LCFORM_JQ[$i]}")" || return 1
        i=$((i + 1))
    done
    controls="Fixes #$CCONTROL"
    if [ -n "$LCCONTROL" ]; then controls="$controls
Fixes $LCCONTROL"; fi
    commit "Add the probe's last line

$controls" || return 1
    report "pull request: $COMMITS commits on the branch"
    jq -n -c --arg h "$BRANCH" --arg b "$BASE" --arg t "$PROBE" --rawfile body "$WORK/lines" \
        '{title: $t, head: $h, base: $b, body: $body}' >"$WORK/pr.json"
    status=$(rest pr "repos/$REPO/pulls" POST "$WORK/pr.json")
    PR=$(field .number "$WORK/pr.body")
    report "pull request: opened #${PR:-none}: HTTP $status; the body GitHub stored equals the one sent: $(jq -r --rawfile b "$WORK/lines" '.body == $b' <"$WORK/pr.body" 2>/dev/null)"
    if [ -z "$PR" ]; then
        fail "pull request: not opened; not run"
        return 1
    fi
    sleep 10
    closing_refs "before the merge (+10 s)"
    i=0
    while [ "$i" -lt "${#LOPENED[@]}" ]; do
        LBEFORE[i]=$(linear_state "${LOPENED[$i]}")
        i=$((i + 1))
    done

    # A merge commit with a title and message of the probe's own: the branch's commits reach the default branch as
    # they are, and the merge commit names no issue.
    jq -n -c --arg t "$PROBE" '{merge_method: "merge", commit_title: $t, commit_message: "A keyword-shape probe."}' >"$WORK/merge.json"
    try=1
    while :; do
        status=$(rest merge "repos/$REPO/pulls/$PR/merge" PUT "$WORK/merge.json")
        if [ "$status" = 200 ] || [ "$try" -ge 4 ]; then break; fi
        try=$((try + 1))
        sleep 5
    done
    sha=$(field .sha "$WORK/merge.body")
    report "pull request: merged with a merge commit: HTTP $status (try $try), merged: $(field .merged "$WORK/merge.body")"
    if [ "$status" != 200 ] || [ -z "$sha" ]; then
        fail "pull request: not merged: $(field .message "$WORK/merge.body"); nothing to read"
        return 1
    fi
    MERGED=yes
    status=$(rest commit "repos/$REPO/commits/$sha")
    report "pull request: the merge commit's message names an issue: $(jq -r '.commit.message | test("#[0-9]|/issues/[0-9]|[A-Za-z]+-[0-9]")' <"$WORK/commit.body" 2>/dev/null), parents: $(jq -r '.parents | length' <"$WORK/commit.body" 2>/dev/null) (HTTP $status)"
}

# The pull request's closingIssuesReferences, as numbers.
closing_refs() {
    local q status
    # shellcheck disable=SC2016
    q='query($o: String!, $n: String!, $p: Int!) { repository(owner: $o, name: $n) { pullRequest(number: $p) { closingIssuesReferences(first: 50) { nodes { number } } } } }'
    jq -n -c --arg q "$q" --arg o "$OWNER" --arg n "$NAME" --argjson p "$PR" '{query: $q, variables: {o: $o, n: $n, p: $p}}' >"$WORK/cir.json"
    status=$(rest cir graphql POST "$WORK/cir.json")
    report "pull request: closingIssuesReferences $1: HTTP $status, $(jq -c '[.data.repository.pullRequest.closingIssuesReferences.nodes[]?.number]' <"$WORK/cir.body" 2>/dev/null)"
}

# state NUMBER: "open", or "closed/<state_reason>".
state() {
    rest st "repos/$REPO/issues/$1" >/dev/null
    jq -r 'if .state == "closed" then "closed/\(.state_reason // "none")" else (.state // "unread") end' <"$WORK/st.body" 2>/dev/null
}

# linear_state ID: "<state type>/<state name>, links N", N the issue's attachments, a linked pull request among them.
linear_state() {
    # shellcheck disable=SC2016
    linear lst 'query($i: String!) { issue(id: $i) { state { type name } attachments(first: 10) { nodes { id } } } }' \
        "$(jq -n -c --arg i "$1" '{i: $i}')" >/dev/null
    jq -r '.data.issue | if . == null then "unread" else "\(.state.type)/\(.state.name), links \(.attachments.nodes | length)" end' \
        <"$WORK/lst.body" 2>/dev/null
}

# verdict CLOSED RULE: sets VERDICT, the verdict of a row, and counts a failed control and a kept form that closed.
verdict() {
    case $2 in
    control)
        if [ "$1" = yes ]; then VERDICT="closes (control, as expected)"; else
            FAILED=$((FAILED + 1))
            VERDICT="ignored (CONTROL FAILED: the rows of its platform and place prove nothing)"
        fi
        ;;
    keeps)
        if [ "$1" = yes ]; then
            UNSAFE=$((UNSAFE + 1))
            VERDICT="closes (A KEPT FORM CLOSES: the narrowed rule is unsafe)"
        else VERDICT=ignored; fi
        ;;
    *) if [ "$1" = yes ]; then VERDICT=closes; else VERDICT=ignored; fi ;;
    esac
}

# github_row PLACE NAME NUMBER RULE: one row of the GitHub table.
github_row() {
    local s closed
    s=$(state "$3")
    if [ "${s%%/*}" = closed ]; then closed=yes; else closed=no; fi
    verdict "$closed" "$4"
    ROW=$((ROW + 1))
    report "| $ROW | $1 | $2 | #$3 | $4 | $s | $VERDICT |"
}

# linear_row PLACE NAME UUID IDENTIFIER BEFORE RULE: one row of the Linear table.
linear_row() {
    local s closed
    s=$(linear_state "$3")
    case $s in completed/*) closed=yes ;; *) closed=no ;; esac
    verdict "$closed" "$6"
    ROW=$((ROW + 1))
    report "| $ROW | $1 | $2 | $4 | $6 | $5 | $s | $VERDICT |"
}

read_back() {
    local i s
    section result
    sleep 60
    closing_refs "after the merge (+60 s)"
    s=$(state "$TARGET")
    report "result: target #$TARGET (Closes #$TARGET, last line): $s"
    if [ "${s%%/*}" != closed ]; then fail "result: the target did not close: the run proves nothing"; fi
    report ""
    report "| # | place | form | issue | rule | state at +60 s | verdict |"
    report "|---|-------|------|-------|------|----------------|---------|"
    ROW=0
    i=0
    while [ "$i" -lt "${#FORM_NAME[@]}" ]; do
        github_row body "${FORM_NAME[$i]}" "${VICTIM[$i]}" "${FORM_RULE[$i]}"
        i=$((i + 1))
    done
    github_row commit "control: a body line Fixes #n" "$CCONTROL" control
    i=0
    while [ "$i" -lt "${#CFORM_NAME[@]}" ]; do
        github_row commit "${CFORM_NAME[$i]}" "${CVICTIM[$i]}" keeps
        i=$((i + 1))
    done
    if [ -n "$TEAM" ]; then
        report ""
        report "| # | place | Linear form | issue | rule | before the merge (+10 s) | after the merge (+60 s) | verdict |"
        report "|---|-------|-------------|-------|------|--------------------------|-------------------------|---------|"
        ROW=0
        i=0
        while [ "$i" -lt "${#LFORM_NAME[@]}" ]; do
            linear_row body "${LFORM_NAME[$i]}" "${LOPENED[$i]}" "${LVICTIM[$i]}" "${LBEFORE[$i]:-unread}" "${LFORM_RULE[$i]}"
            i=$((i + 1))
        done
        # LOPENED holds the body forms' issues, then the commit forms', then the commit control's.
        s=$((${#LFORM_NAME[@]} + ${#LCFORM_NAME[@]}))
        linear_row commit "control: a body line Fixes KEY-n" "${LOPENED[$s]}" "$LCCONTROL" "${LBEFORE[$s]:-unread}" control
        i=0
        while [ "$i" -lt "${#LCFORM_NAME[@]}" ]; do
            s=$((${#LFORM_NAME[@]} + i))
            linear_row commit "${LCFORM_NAME[$i]}" "${LOPENED[$s]}" "${LCVICTIM[$i]}" "${LBEFORE[$s]:-unread}" keeps
            i=$((i + 1))
        done
    fi
    report ""
    report "result: kept forms that closed an issue: $UNSAFE"
}

# shellcheck disable=SC2329 # run by the EXIT trap
cleanup() {
    local i n status sha
    if [ "${CLEANED:-}" = yes ]; then return 0; fi
    CLEANED=yes
    if [ "${STARTED:-no}" = yes ]; then
        section cleanup
        if [ -n "$PR" ] && [ "$MERGED" = no ]; then
            jq -n -c '{state: "closed"}' >"$WORK/prclose.json"
            report "cleanup: close pull request #$PR unmerged: HTTP $(rest prclose "repos/$REPO/pulls/$PR" PATCH "$WORK/prclose.json")"
        fi
        if [ -n "$BRANCH" ]; then
            report "cleanup: delete the branch: HTTP $(rest delref "repos/$REPO/git/refs/heads/$BRANCH" DELETE)"
        fi
        if [ "$MERGED" = yes ]; then
            rest getfile "repos/$REPO/contents/$FILE_PATH?ref=$BASE" >/dev/null
            sha=$(field .sha "$WORK/getfile.body")
            if [ -n "$sha" ]; then
                jq -n -c --arg s "$sha" --arg b "$BASE" --arg m "$PROBE: remove its file" '{message: $m, sha: $s, branch: $b}' >"$WORK/delfile.json"
                report "cleanup: remove the probe's file from $BASE: HTTP $(rest delfile "repos/$REPO/contents/$FILE_PATH" DELETE "$WORK/delfile.json")"
            else
                report "cleanup: the probe's file did not read on $BASE; remove $FILE_PATH by hand"
            fi
        fi
        jq -n -c '{state: "closed", state_reason: "not_planned"}' >"$WORK/close.json"
        i=0
        while [ "$i" -lt "${#OPENED[@]}" ]; do
            n=${OPENED[$i]}
            if [ "$(state "$n")" = open ]; then
                status=$(rest close "repos/$REPO/issues/$n" PATCH "$WORK/close.json")
                report "cleanup: close #$n as not planned: HTTP $status"
            fi
            i=$((i + 1))
        done
        i=0
        while [ "$i" -lt "${#LOPENED[@]}" ]; do
            # shellcheck disable=SC2016
            status=$(linear ldel 'mutation($i: String!) { issueDelete(id: $i) { success } }' \
                "$(jq -n -c --arg i "${LOPENED[$i]}" '{i: $i}')")
            report "cleanup: delete Linear issue $((i + 1)) of ${#LOPENED[@]}: HTTP $status, $(field .data.issueDelete.success "$WORK/ldel.body")"
            i=$((i + 1))
        done
        report ""
        report "done, failures: $FAILED, kept forms that closed: $UNSAFE."
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
if [ $# -lt 1 ] || [ $# -gt 2 ]; then
    usage >&2
    exit 2
fi
REPO=$1
TEAM=${2:-}
TEAM_ID=
LBEFORE=()
ROW=0
VERDICT=
case $REPO in
*/*) ;;
*)
    say "$PROGRAM: $REPO must have the form owner/name"
    exit 2
    ;;
esac
if [ "$(printf '%s' "$REPO" | tr '[:upper:]' '[:lower:]')" = "$FORBIDDEN" ]; then
    say "$PROGRAM: never run against $FORBIDDEN: it merges a pull request and closes issues"
    exit 2
fi
if [ "$(printf '%s' "$TEAM" | tr '[:lower:]' '[:upper:]')" = "$FORBIDDEN_TEAM" ]; then
    say "$PROGRAM: never run against the Linear team $FORBIDDEN_TEAM: it closes and deletes issues"
    exit 2
fi
OWNER=${REPO%%/*}
NAME=${REPO#*/}
TOOLS="gh jq base64"
if [ -n "$TEAM" ]; then
    TOOLS="$TOOLS curl"
    if [ -z "${LINEAR_API_KEY:-}" ]; then
        say "$PROGRAM: a team key needs LINEAR_API_KEY"
        exit 2
    fi
fi
for tool in $TOOLS; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        say "$PROGRAM: $tool is needed"
        exit 2
    fi
done

WORK=$(mktemp -d "${TMPDIR:-/tmp}/owl569.XXXXXX") || exit 2
FAILED=0
UNSAFE=0
trap cleanup EXIT
trap 'exit 130' INT TERM

report "OWL-569 check, $(date -u +%Y-%m-%dT%H:%M:%SZ), $(gh --version | head -n 1 | cut -d' ' -f1-3), $(jq --version), $(uname -s), script sha256 $(script_sha), Linear team: ${TEAM:-none}"
if ! gh auth status >/dev/null 2>&1 </dev/null; then
    say "$PROGRAM: gh is not signed in (gh auth status)"
    exit 2
fi
STARTED=yes
start || exit 1
linear_start || exit 1
issues || exit 1
probe || exit 1
read_back
exit "$FAILED"
