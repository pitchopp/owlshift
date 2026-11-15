#!/usr/bin/env bash
# OWL-556 and OWL-559 live check, quoted next to "The GitHub Issues adapter" of docs/design/build-plan.md, "P9 ·
# Open-source readiness": which rewriting of a closing keyword ("Fixes #5") GitHub ignores when the pull request whose
# body holds it merges, so that the runner can neutralise the agent's text and leave its own `Closes #n` line as the
# only one that closes an issue; and (OWL-559) whether Linear's GitHub integration, whose magic words outnumber
# GitHub's keywords, closes a Linear issue from the same pull request's body.
#
# One pull request carries every form, each naming an issue of its own (a victim), and ends with `Closes #T`, T the
# run's target. It is squash-merged with a commit message of the probe's own, which names no issue, so that only the
# body can close one. Then each issue is read: a victim still open is a form GitHub ignores. Controls: the plain forms
# (`Fixes #n`, `Fixes owner/repo#n`, `Fixes <issue URL>`, `Closes #n`, `Resolves #n`) must close their victims, and T must close, or the run
# proves nothing. The "shipped" rows are the form the runner writes (U+200B after the keyword's second letter),
# for each of GitHub's nine keywords. The "lookalike" and "glued" rows (OWL-559) are keywords the runner's scanner
# does not see: a letter replaced by a non-ASCII one, or a non-ASCII letter or a digit stuck to the keyword, which
# makes one word of both. Who opens and merges the pull request does not change how GitHub reads its body: the run goes
# through the person's own `gh` login, not the app's token (G5 settled that a pull request the token opens closes
# its issue alike).
#
# With a Linear team's key as second argument (OWL-559), the body also carries one line per Linear form, each naming
# an issue the run opens in that team: Linear's closing magic words, the runner's broken form of some, and a
# non-closing word. A victim whose state turns `completed` is a form Linear closes on; the control `Fixes KEY-n` must,
# or the Linear rows prove nothing (the repository is not connected to the workspace's GitHub integration, or the
# team's merge status is not a completed one). The branch, the pull request's title and every commit message name
# no Linear issue, so the integration links the pull request only through its body. OWL-570 adds each non-closing
# word, a control that must move its victim to a `started` state, and the runner's broken form of each, which must
# not move its victim (a failure if it does), and a relation word, which must not move its victim either.
#
#   bash docs/design/live-checks/owl-556.sh pitchopp/owlshift-live-sandbox [SANDBOX_TEAM_KEY]   (--help: what it does)
#
# Needs bash (3.2 is enough), jq, and gh signed in (gh auth status) as an account that can write the sandbox's issues
# and contents and merge its pull requests; with a team key, also curl and LINEAR_API_KEY, a key that creates and
# deletes issues in that team. It runs alone in about 3 minutes. It creates on the sandbox 41 issues, a branch
# `closing-probe-<time>` with one file, and a pull request, which it squash-merges into the default branch; then it
# deletes the branch, removes the file from the default branch in a commit of its own, and closes as not planned
# every issue the merge left open. With a team key it creates 35 Linear issues in that team and deletes them at the
# end. It refuses pitchopp/owlshift and the team OWL. The report, on standard output, holds no token and no account
# identifier: paste it on the ticket.

set -u -o pipefail
umask 077

PROGRAM=${0##*/}
# The probe's name in everything GitHub or Linear stores: no Linear identifier, which Linear's integration would link.
PROBE="Closing-keyword probe"
BODY="A probe of the closing-keyword break (Owlshift's live check). The run ends it as not planned unless its pull request ends it first."
# The repository and the Linear team this check must never write to: Owlshift's own.
FORBIDDEN=pitchopp/owlshift
FORBIDDEN_TEAM=OWL

# The forms, in parallel arrays: a name, and a jq expression of the line the body carries, given $n (the victim's
# number), $r (OWNER/REPO) and $u (the victim's URL). A control is a plain form, which must close its victim.
FORM_NAME=(
    "control: Fixes #n"
    "code span: \`Fixes #n\`"
    "zero-width space (U+200B) inside the keyword"
    "zero-width space (U+200B) after the #"
    "word joiner (U+2060) inside the keyword"
    "HTML entity for the # (&#35;)"
    "backslash before the #"
    "a word between: Fixes issue #n"
    "fenced code block"
    "HTML comment"
    "control: Fixes owner/repo#n"
    "code span: \`Fixes owner/repo#n\`"
    "zero-width space inside the keyword, owner/repo#n"
    "control: Fixes <issue URL>"
    "code span: \`Fixes <issue URL>\`"
    "zero-width space inside the keyword, <issue URL>"
    "control: Closes #n"
    "control: Resolves #n"
    "shipped: fi<U+200B>x #n"
    "shipped: fi<U+200B>xes #n"
    "shipped: fi<U+200B>xed #n"
    "shipped: cl<U+200B>ose #n"
    "shipped: cl<U+200B>oses #n"
    "shipped: cl<U+200B>osed #n"
    "shipped: re<U+200B>solve #n"
    "shipped: re<U+200B>solves #n"
    "shipped: re<U+200B>solved #n"
    "zero-width space (U+200B) before the keyword"
    "zero-width space (U+200B) between the keyword and the space"
    "lookalike: clo<U+017F>es #n (long s)"
    "lookalike: <U+FB01>xes #n (fi ligature)"
    "lookalike: clos<U+0435>s #n (Cyrillic e)"
    "lookalike: f<U+0131>xes #n (dotless i)"
    "lookalike: F<U+0130>XES #n (dotted capital I)"
    "lookalike: fullwidth FIXES #n"
    "lookalike: mathematical bold Fixes #n"
    "glued: <U+65E5>Fixes #n (a CJK letter before)"
    "glued: <U+00E9>Fixes #n (a Latin letter before)"
    "glued: 1Fixes #n (an ASCII digit before)"
    "glued: Fixes<U+65E5> #n (a CJK letter after)"
)
FORM_JQ=(
    '"Fixes #\($n)"'
    '"`Fixes #\($n)`"'
    '"Fix\u200bes #\($n)"'
    '"Fixes #\u200b\($n)"'
    '"Fix\u2060es #\($n)"'
    '"Fixes &#35;\($n)"'
    '"Fixes \\#\($n)"'
    '"Fixes issue #\($n)"'
    '"```\nFixes #\($n)\n```"'
    '"<!-- Fixes #\($n) -->"'
    '"Fixes \($r)#\($n)"'
    '"`Fixes \($r)#\($n)`"'
    '"Fix\u200bes \($r)#\($n)"'
    '"Fixes \($u)"'
    '"`Fixes \($u)`"'
    '"Fix\u200bes \($u)"'
    '"Closes #\($n)"'
    '"Resolves #\($n)"'
    '"Fi\u200bx #\($n)"'
    '"Fi\u200bxes #\($n)"'
    '"Fi\u200bxed #\($n)"'
    '"Cl\u200bose #\($n)"'
    '"Cl\u200boses #\($n)"'
    '"Cl\u200bosed #\($n)"'
    '"Re\u200bsolve #\($n)"'
    '"Re\u200bsolves #\($n)"'
    '"Re\u200bsolved #\($n)"'
    '"\u200bFixes #\($n)"'
    '"Fixes\u200b #\($n)"'
    '"Clo\u017fes #\($n)"'
    '"\ufb01xes #\($n)"'
    '"Clos\u0435s #\($n)"'
    '"F\u0131xes #\($n)"'
    '"F\u0130XES #\($n)"'
    '"\uff26\uff49\uff58\uff45\uff53 #\($n)"'
    '"\ud835\udc05\ud835\udc22\ud835\udc31\ud835\udc1e\ud835\udc2c #\($n)"'
    '"\u65e5Fixes #\($n)"'
    '"\u00e9Fixes #\($n)"'
    '"1Fixes #\($n)"'
    '"Fixes\u65e5 #\($n)"'
)
CONTROLS=(0 10 13 16 17)

# The Linear forms (OWL-559), given $k (the victim's identifier, KEY-n). Linear's closing magic words, as
# https://linear.app/docs/github listed them on 2026-11-14: close, closes, closed, closing, fix, fixes, fixed,
# fixing, resolve, resolves, resolved, resolving, complete, completes, completed, completing, implement, implements,
# implemented, implementing, linear issue; non-closing: ref, refs, references, part of, contributes to, toward,
# towards. The nine GitHub shares are probed through one form per verb; the twelve others each. OWL-570: the
# non-closing words, as https://linear.app/docs/github listed them on 2026-11-15 (the same seven), each plain and
# broken, and the relation word `relates to`, which the page says links with no status change.
LFORM_NAME=(
    "control: Fixes KEY-n"
    "shipped: Fi<U+200B>xes KEY-n"
    "shipped: Cl<U+200B>oses KEY-n"
    "shipped: Re<U+200B>solves KEY-n"
    "Closing KEY-n"
    "Fixing KEY-n"
    "Resolving KEY-n"
    "Complete KEY-n"
    "Completes KEY-n"
    "Completed KEY-n"
    "Completing KEY-n"
    "Implement KEY-n"
    "Implements KEY-n"
    "Implemented KEY-n"
    "Implementing KEY-n"
    "Linear issue KEY-n"
    "broken: Cl<U+200B>osing KEY-n"
    "broken: Co<U+200B>mpletes KEY-n"
    "broken: Im<U+200B>plements KEY-n"
    "broken: Li<U+200B>near issue KEY-n"
    "control (non-closing): Refs KEY-n"
    "control (non-closing): Ref KEY-n"
    "control (non-closing): References KEY-n"
    "control (non-closing): Part of KEY-n"
    "control (non-closing): Contributes to KEY-n"
    "control (non-closing): Toward KEY-n"
    "control (non-closing): Towards KEY-n"
    "broken: Re<U+200B>fs KEY-n"
    "broken: Re<U+200B>f KEY-n"
    "broken: Re<U+200B>ferences KEY-n"
    "broken: Pa<U+200B>rt of KEY-n"
    "broken: Co<U+200B>ntributes to KEY-n"
    "broken: To<U+200B>ward KEY-n"
    "broken: To<U+200B>wards KEY-n"
    "relation: Relates to KEY-n"
)
LFORM_JQ=(
    '"Fixes \($k)"'
    '"Fi\u200bxes \($k)"'
    '"Cl\u200boses \($k)"'
    '"Re\u200bsolves \($k)"'
    '"Closing \($k)"'
    '"Fixing \($k)"'
    '"Resolving \($k)"'
    '"Complete \($k)"'
    '"Completes \($k)"'
    '"Completed \($k)"'
    '"Completing \($k)"'
    '"Implement \($k)"'
    '"Implements \($k)"'
    '"Implemented \($k)"'
    '"Implementing \($k)"'
    '"Linear issue \($k)"'
    '"Cl\u200bosing \($k)"'
    '"Co\u200bmpletes \($k)"'
    '"Im\u200bplements \($k)"'
    '"Li\u200bnear issue \($k)"'
    '"Refs \($k)"'
    '"Ref \($k)"'
    '"References \($k)"'
    '"Part of \($k)"'
    '"Contributes to \($k)"'
    '"Toward \($k)"'
    '"Towards \($k)"'
    '"Re\u200bfs \($k)"'
    '"Re\u200bf \($k)"'
    '"Re\u200bferences \($k)"'
    '"Pa\u200brt of \($k)"'
    '"Co\u200bntributes to \($k)"'
    '"To\u200bward \($k)"'
    '"To\u200bwards \($k)"'
    '"Relates to \($k)"'
)
LCONTROLS=(0)
# OWL-570: the non-closing controls, which must move their victim to a `started` state, and the forms that must not
# move theirs: the runner's broken non-closing words and the relation word.
LMOVES=(20 21 22 23 24 25 26)
LSTILL=(27 28 29 30 31 32 33 34)

usage() {
    cat <<EOF
Usage: bash $PROGRAM OWNER/REPO [SANDBOX_TEAM_KEY]

  OWNER/REPO        the sandbox: pitchopp/owlshift-live-sandbox. Never $FORBIDDEN.
  SANDBOX_TEAM_KEY  optional: a Linear team whose issues the run may create and delete, in a
                    workspace whose GitHub integration is connected to OWNER/REPO. Never $FORBIDDEN_TEAM.
                    Needs LINEAR_API_KEY.

Needs jq and gh signed in as an account that writes the sandbox. About 3 minutes, alone:
it opens $((${#FORM_NAME[@]} + 1)) issues, a
branch with one file, and a pull request, squash-merges it into the default branch, reads
the issues, then deletes the file and the branch and ends the issues still open as not
planned. With a team key it also opens ${#LFORM_NAME[@]} Linear issues there, reads them, and
deletes them. Paste everything it prints on standard output on the ticket.
EOF
}

say() { printf '%s\n' "$*" >&2; }
report() { printf '%s\n' "$*"; }
section() { printf '\n== %s ==\n' "$1"; }
fail() {
    FAILED=$((FAILED + 1))
    report "$1"
}
is_number() { case $1 in '' | *[!0-9]*) return 1 ;; *) return 0 ;; esac }

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
    report "start: gh reads REPO: HTTP $status, push: ${push:-unknown}, private: $(field .private "$WORK/repo.body")"
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
LVICTIM=()
# open_linear NAME TITLE: sets LOPEN_ID (the identifier, KEY-n).
open_linear() {
    local status id
    # shellcheck disable=SC2016
    status=$(linear "$1" 'mutation($t: String!, $title: String!, $d: String!) { issueCreate(input: {teamId: $t, title: $title, description: $d}) { issue { id identifier } } }' \
        "$(jq -n -c --arg t "$TEAM_ID" --arg title "$2" --arg d "$BODY" '{t: $t, title: $title, d: $d}')")
    id=$(field .data.issueCreate.issue.id "$WORK/$1.body")
    LOPEN_ID=$(field .data.issueCreate.issue.identifier "$WORK/$1.body")
    if [ -n "$id" ]; then
        LOPENED+=("$id")
    else
        fail "open: Linear '$2': HTTP $status"
    fi
}

VICTIM=()
TARGET=
BRANCH=
PR=
MERGED=no
FILE_PATH=

probe() {
    local i status line sha
    section issues
    open_issue target "$PROBE: the target, its pull request ends with Closes"
    TARGET=$OPEN_NUMBER
    i=0
    while [ "$i" -lt "${#FORM_NAME[@]}" ]; do
        open_issue "v$i" "$PROBE: victim of form $((i + 1)), ${FORM_NAME[$i]}"
        VICTIM+=("$OPEN_NUMBER")
        i=$((i + 1))
    done
    if [ -z "$TARGET" ] || [ "${#OPENED[@]}" -ne $((${#FORM_NAME[@]} + 1)) ]; then
        fail "issues: not all opened; not run"
        return 1
    fi
    report "issues: target #$TARGET, victims #${VICTIM[0]} to #${VICTIM[$((${#VICTIM[@]} - 1))]}"
    if [ -n "$TEAM" ]; then
        i=0
        while [ "$i" -lt "${#LFORM_NAME[@]}" ]; do
            open_linear "l$i" "$PROBE: Linear form $((i + 1)), ${LFORM_NAME[$i]}"
            LVICTIM+=("$LOPEN_ID")
            i=$((i + 1))
        done
        if [ "${#LOPENED[@]}" -ne "${#LFORM_NAME[@]}" ]; then
            fail "issues: not all Linear issues opened; not run"
            return 1
        fi
        report "issues: Linear victims ${LVICTIM[0]} to ${LVICTIM[$((${#LVICTIM[@]} - 1))]}"
    fi

    # The body: one paragraph per form, then the runner's closing line, as `with_closing_line` writes it.
    : >"$WORK/lines"
    i=0
    while [ "$i" -lt "${#FORM_NAME[@]}" ]; do
        line=$(jq -n -r --arg n "${VICTIM[$i]}" --arg r "$REPO" --arg u "https://github.com/$REPO/issues/${VICTIM[$i]}" \
            "${FORM_JQ[$i]}")
        printf 'Form %s:\n\n%s\n\n' "$((i + 1))" "$line" >>"$WORK/lines"
        i=$((i + 1))
    done
    i=0
    while [ "$i" -lt "${#LVICTIM[@]}" ]; do
        line=$(jq -n -r --arg k "${LVICTIM[$i]}" "${LFORM_JQ[$i]}")
        printf 'Linear form %s:\n\n%s\n\n' "$((i + 1))" "$line" >>"$WORK/lines"
        i=$((i + 1))
    done
    printf 'Closes #%s\n' "$TARGET" >>"$WORK/lines"

    section "pull request"
    BRANCH="closing-probe-$(date +%s)"
    jq -n -c --arg r "refs/heads/$BRANCH" --arg s "$BASE_SHA" '{ref: $r, sha: $s}' >"$WORK/mkref.json"
    status=$(rest mkref "repos/$REPO/git/refs" POST "$WORK/mkref.json")
    report "pull request: branch created from $BASE: HTTP $status"
    if [ "$status" != 201 ]; then
        BRANCH=
        fail "pull request: no branch; not run"
        return 1
    fi
    FILE_PATH="closing-probe/$BRANCH.md"
    jq -n -c --arg b "$BRANCH" --arg c "$(printf 'A closing-keyword probe.\n' | base64)" --arg m "$PROBE: one file" \
        '{message: $m, content: $c, branch: $b}' >"$WORK/file.json"
    status=$(rest file "repos/$REPO/contents/$FILE_PATH" PUT "$WORK/file.json")
    report "pull request: one file committed on the branch: HTTP $status"
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

    # A squash merge with a message of the probe's own: only the body may close an issue.
    jq -n -c --arg t "$PROBE" '{merge_method: "squash", commit_title: $t, commit_message: "A closing-keyword probe."}' >"$WORK/merge.json"
    status=$(rest merge "repos/$REPO/pulls/$PR/merge" PUT "$WORK/merge.json")
    sha=$(field .sha "$WORK/merge.body")
    report "pull request: squash-merged: HTTP $status, merged: $(field .merged "$WORK/merge.body")"
    if [ "$status" != 200 ] || [ -z "$sha" ]; then
        fail "pull request: not merged: $(field .message "$WORK/merge.body"); nothing to read"
        return 1
    fi
    MERGED=yes
    status=$(rest commit "repos/$REPO/commits/$sha")
    report "pull request: the merge commit's message names an issue: $(jq -r '.commit.message | test("#[0-9]|/issues/[0-9]|[A-Za-z]+-[0-9]")' <"$WORK/commit.body" 2>/dev/null) (HTTP $status)"
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

# in_list N ITEM...: whether N is one of the items.
in_list() {
    local n=$1 c
    shift
    for c in "$@"; do if [ "$c" = "$n" ]; then return 0; fi; done
    return 1
}

is_control() {
    local c
    for c in "${CONTROLS[@]}"; do if [ "$c" = "$1" ]; then return 0; fi; done
    return 1
}

read_back() {
    local i s verdict
    section result
    sleep 60
    closing_refs "after the merge (+60 s)"
    s=$(state "$TARGET")
    report "result: target #$TARGET (Closes #$TARGET, last line): $s"
    if [ "${s%%/*}" != closed ]; then fail "result: the target did not close: the run proves nothing"; fi
    report ""
    report "| # | form | issue | state at +60 s | verdict |"
    report "|---|------|-------|----------------|---------|"
    i=0
    while [ "$i" -lt "${#FORM_NAME[@]}" ]; do
        s=$(state "${VICTIM[$i]}")
        if [ "${s%%/*}" = closed ]; then verdict=closes; else verdict=ignored; fi
        if is_control "$i"; then
            if [ "$verdict" = closes ]; then verdict="closes (control, as expected)"; else
                verdict="ignored (CONTROL FAILED: the forms after it prove nothing)"
                FAILED=$((FAILED + 1))
            fi
        fi
        report "| $((i + 1)) | ${FORM_NAME[$i]} | #${VICTIM[$i]} | $s | $verdict |"
        i=$((i + 1))
    done
    if [ -n "$TEAM" ]; then
        report ""
        report "| # | Linear form | issue | before the merge (+10 s) | after the merge (+60 s) | verdict |"
        report "|---|-------------|-------|--------------------------|-------------------------|---------|"
        i=0
        while [ "$i" -lt "${#LOPENED[@]}" ]; do
            s=$(linear_state "${LOPENED[$i]}")
            # OWL-570: a victim in a `started` state was moved; one with a link and no move, linked.
            case $s in
            completed/*) verdict=closes ;;
            started/*) verdict=moves ;;
            *", links 0") verdict=ignored ;;
            unread) verdict=unread ;;
            *) verdict=links ;;
            esac
            if [ "$i" = "${LCONTROLS[0]}" ]; then
                if [ "$verdict" = closes ]; then verdict="closes (control, as expected)"; else
                    verdict="$verdict (CONTROL FAILED: the Linear forms prove nothing)"
                    FAILED=$((FAILED + 1))
                fi
            elif in_list "$i" "${LMOVES[@]}"; then
                if [ "$verdict" = moves ]; then verdict="moves (control, as expected)"; else
                    verdict="$verdict (CONTROL FAILED: the non-closing word moved nothing)"
                    FAILED=$((FAILED + 1))
                fi
            elif in_list "$i" "${LSTILL[@]}"; then
                case $verdict in
                closes | moves | unread)
                    verdict="$verdict (FAILED: this form must not move its issue)"
                    FAILED=$((FAILED + 1))
                    ;;
                esac
            fi
            report "| $((i + 1)) | ${LFORM_NAME[$i]} | ${LVICTIM[$i]} | ${LBEFORE[$i]:-unread} | $s | $verdict |"
            i=$((i + 1))
        done
    fi
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
            report "cleanup: delete Linear issue ${LVICTIM[$i]:-$i}: HTTP $status, $(field .data.issueDelete.success "$WORK/ldel.body")"
            i=$((i + 1))
        done
        report ""
        report "done, failures: $FAILED."
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

WORK=$(mktemp -d "${TMPDIR:-/tmp}/owl556.XXXXXX") || exit 2
FAILED=0
trap cleanup EXIT
trap 'exit 130' INT TERM

report "OWL-556/OWL-559/OWL-570 check,$(date -u +%Y-%m-%dT%H:%M:%SZ), $(gh --version | head -n 1 | cut -d' ' -f1-3), $(jq --version), $(uname -s), script sha256 $(script_sha), Linear team: ${TEAM:-none}"
if ! gh auth status >/dev/null 2>&1 </dev/null; then
    say "$PROGRAM: gh is not signed in (gh auth status)"
    exit 2
fi
STARTED=yes
start || exit 1
linear_start || exit 1
probe || exit 1
read_back
exit "$FAILED"
