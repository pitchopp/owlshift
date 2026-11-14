#!/usr/bin/env bash
# OWL-558 live check, quoted next to "The GitHub Issues adapter" of docs/design/build-plan.md, "P9 · Open-source
# readiness": whether a closing keyword ("Fixes #5") in a commit message of a pull request closes that issue when the
# pull request merges, under each merge method `[forge] merge_method` allows, and whether the form the runner writes
# (U+200B after the keyword's second letter, OWL-556) leaves it open in a commit message as it does in a body.
#
# One pull request per case, each with commits whose messages name issues of their own (victims), and a body that
# ends with `Closes #T`, T the case's target, as `with_closing_line` writes it. Each is merged by its case's method,
# with GitHub's default commit message (no `commit_title` or `commit_message`, as Owlshift's merge on green calls
# the endpoint today) or, in the "own message" case, with a title and message of the probe's own naming no issue.
# Then each issue is read: a victim closed is a keyword GitHub read in a commit message. Controls: every target must
# close, and the plain `Fixes #n` the shipped case carries must close, or the rows around them prove nothing. The
# commits are made through the contents API, so they are the person's; GitHub reads a commit's message when the
# commit reaches the default branch, whoever pushed it.
#
#   bash docs/design/live-checks/owl-558.sh pitchopp/owlshift-live-sandbox      (--help: what it does)
#
# Needs bash (3.2 is enough), jq, and gh signed in (gh auth status) as an account that can write the sandbox's issues
# and contents and merge its pull requests by squash, merge commit and rebase. It runs alone in about 3 minutes. It
# creates on the sandbox 26 issues, six branches `owl-558-probe-<time>-<case>` with one or two files each, and six
# pull requests, which it merges into the default branch; then it deletes the branches, removes the files from the
# default branch in commits of its own, and closes as not planned every issue the merges left open. It refuses
# pitchopp/owlshift. The report, on standard output, holds no token and no account identifier: paste it on OWL-558.

set -u -o pipefail
umask 077

PROGRAM=${0##*/}
BODY="A probe of OWL-558 (docs/design/live-checks/owl-558.sh). The run ends it as not planned unless a merge ends it first."
# The repository this check must never write to: Owlshift's own.
FORBIDDEN=pitchopp/owlshift

# The cases, in parallel arrays: a name; the merge method; whether the merge gives a title and message of its own
# (yes) or leaves GitHub's default (no); a `|`-separated label per victim, the first `control:` one a control that
# must close; and a jq expression of the commits' messages, an array, given $v (the victims' numbers, as strings).
CASE_NAME=(
    "squash, GitHub's default message, one commit"
    "squash, GitHub's default message, two commits"
    "squash, the merger's own title and message, two commits"
    "merge commit, GitHub's default message, two commits"
    "rebase, two commits"
    "merge commit, the shipped form in a commit message"
)
CASE_METHOD=(squash squash squash merge rebase merge)
CASE_OWN=(no no yes no no no)
CASE_LABELS=(
    "subject: Fixes #n|body: Resolves #n"
    "first commit's body: Fixes #n|second commit's subject: Closes #n"
    "first commit's subject: Fixes #n|second commit's body: Resolves #n"
    "first commit's subject: Fixes #n|second commit's body: Resolves #n"
    "first commit's subject: Fixes #n|second commit's body: Resolves #n"
    "control: body: Fixes #n|shipped, subject: fi<U+200B>xes #n|shipped, body: fi<U+200B>x #n|shipped, body: fi<U+200B>xed #n|shipped, body: cl<U+200B>ose #n|shipped, body: cl<U+200B>oses #n|shipped, body: cl<U+200B>osed #n|shipped, body: re<U+200B>solve #n|shipped, body: re<U+200B>solves #n|shipped, body: re<U+200B>solved #n"
)
# shellcheck disable=SC2016 # jq expressions, expanded by jq
CASE_COMMITS=(
    '["Fixes #\($v[0])\n\nResolves #\($v[1])"]'
    '["Add the first probe file\n\nFixes #\($v[0])", "Closes #\($v[1])"]'
    '["Fixes #\($v[0])", "Add the second probe file\n\nResolves #\($v[1])"]'
    '["Fixes #\($v[0])", "Add the second probe file\n\nResolves #\($v[1])"]'
    '["Fixes #\($v[0])", "Add the second probe file\n\nResolves #\($v[1])"]'
    '["Fi\u200bxes #\($v[1])\n\nFi\u200bx #\($v[2])\nFi\u200bxed #\($v[3])\nCl\u200bose #\($v[4])\nCl\u200boses #\($v[5])\nCl\u200bosed #\($v[6])\nRe\u200bsolve #\($v[7])\nRe\u200bsolves #\($v[8])\nRe\u200bsolved #\($v[9])\n\nFixes #\($v[0])"]'
)

usage() {
    cat <<EOF
Usage: bash $PROGRAM OWNER/REPO

  OWNER/REPO  the sandbox: pitchopp/owlshift-live-sandbox. Never $FORBIDDEN.

Needs jq and gh signed in as an account that writes the sandbox and may merge by squash,
merge commit and rebase. About 3 minutes, alone: it opens 26 issues, ${#CASE_NAME[@]} branches with one
or two files each, and ${#CASE_NAME[@]} pull requests, merges them into the default branch, reads the
issues, then deletes the files and the branches and ends the issues still open as not
planned. Paste everything it prints on standard output on OWL-558.
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
    report "start: allows squash $(field .allow_squash_merge "$WORK/repo.body"), merge commit $(field .allow_merge_commit "$WORK/repo.body"), rebase $(field .allow_rebase_merge "$WORK/repo.body"); squash defaults: title $(field .squash_merge_commit_title "$WORK/repo.body"), message $(field .squash_merge_commit_message "$WORK/repo.body"); merge commit defaults: title $(field .merge_commit_title "$WORK/repo.body"), message $(field .merge_commit_message "$WORK/repo.body")"
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

OPENED=()
# open_issue NAME TITLE: sets OPEN_NUMBER.
open_issue() {
    local status
    jq -n -c --arg t "$2" --arg b "$BODY" '{title: $t, body: $b}' >"$WORK/$1.json"
    status=$(rest "$1" "repos/$REPO/issues" POST "$WORK/$1.json")
    OPEN_NUMBER=$(field .number "$WORK/$1.body")
    if [ -n "$OPEN_NUMBER" ]; then
        OPENED+=("$OPEN_NUMBER")
    else
        fail "open: '$2': HTTP $status"
    fi
}

# Per case: its target, its victims (a JSON array of numbers as strings), its branch, its pull request, whether it
# merged, the files it committed (space-separated paths).
TARGET=()
VICTIMS=()
BRANCH=()
PR=()
MERGED=()
FILES=()
STAMP=

# label CASE INDEX: the INDEX-th label of CASE.
label() { printf '%s\n' "${CASE_LABELS[$1]}" | tr '|' '\n' | sed -n "$(($2 + 1))p"; }
labels() { printf '%s\n' "${CASE_LABELS[$1]}" | tr '|' '\n' | wc -l | tr -d ' '; }

issues() {
    local c k n v
    section issues
    c=0
    while [ "$c" -lt "${#CASE_NAME[@]}" ]; do
        open_issue "t$c" "OWL-558: target of case $((c + 1)), its pull request ends with Closes"
        TARGET[c]=$OPEN_NUMBER
        v='[]'
        n=$(labels "$c")
        k=0
        while [ "$k" -lt "$n" ]; do
            open_issue "v$c-$k" "OWL-558: victim of case $((c + 1)), $(label "$c" "$k")"
            v=$(jq -c --arg n "$OPEN_NUMBER" '. + [$n]' <<<"$v")
            k=$((k + 1))
        done
        VICTIMS[c]=$v
        report "issues: case $((c + 1)): target #${TARGET[$c]:-none}, victims $(jq -r 'map("#" + .) | join(", ")' <<<"$v")"
        c=$((c + 1))
    done
    if [ "${#OPENED[@]}" -ne 26 ]; then
        fail "issues: ${#OPENED[@]} of 26 opened; not run"
        return 1
    fi
}

# open_case CASE: its branch, its commits, its pull request.
open_case() {
    local c=$1 status k n message file
    BRANCH[c]="owl-558-probe-$STAMP-$((c + 1))"
    MERGED[c]=no
    FILES[c]=
    jq -n -c --arg r "refs/heads/${BRANCH[$c]}" --arg s "$BASE_SHA" '{ref: $r, sha: $s}' >"$WORK/mkref.json"
    status=$(rest mkref "repos/$REPO/git/refs" POST "$WORK/mkref.json")
    if [ "$status" != 201 ]; then
        BRANCH[c]=
        fail "case $((c + 1)): no branch (HTTP $status); not run"
        return 1
    fi
    jq -n -c --argjson v "${VICTIMS[$c]}" "${CASE_COMMITS[$c]}" >"$WORK/messages.json"
    n=$(jq length <"$WORK/messages.json")
    k=0
    while [ "$k" -lt "$n" ]; do
        message=$(jq -r --argjson k "$k" '.[$k]' <"$WORK/messages.json")
        file="owl-558-probe/${BRANCH[$c]}/$((k + 1)).md"
        jq -n -c --arg b "${BRANCH[$c]}" --arg m "$message" --arg c "$(printf 'A probe of OWL-558.\n' | base64)" \
            '{message: $m, content: $c, branch: $b}' >"$WORK/file.json"
        status=$(rest file "repos/$REPO/contents/$file" PUT "$WORK/file.json")
        if [ "$status" != 201 ]; then
            fail "case $((c + 1)): commit $((k + 1)) not made (HTTP $status); not run"
            return 1
        fi
        FILES[c]="${FILES[$c]} $file"
        k=$((k + 1))
    done
    jq -n -c --arg h "${BRANCH[$c]}" --arg b "$BASE" --arg t "OWL-558 probe $((c + 1))" \
        --arg body "$(printf 'A probe of OWL-558 (docs/design/live-checks/owl-558.sh): %s.\n\nCloses #%s' "${CASE_NAME[$c]}" "${TARGET[$c]}")" \
        '{title: $t, head: $h, base: $b, body: $body}' >"$WORK/pr.json"
    status=$(rest pr "repos/$REPO/pulls" POST "$WORK/pr.json")
    PR[c]=$(field .number "$WORK/pr.body")
    report "case $((c + 1)): $n commit(s) on ${BRANCH[$c]}, pull request #${PR[$c]:-none} (HTTP $status)"
    if [ -z "${PR[$c]}" ]; then
        fail "case $((c + 1)): no pull request; not run"
        return 1
    fi
}

# The pull request's closingIssuesReferences, as numbers.
closing_refs() {
    local q status
    # shellcheck disable=SC2016
    q='query($o: String!, $n: String!, $p: Int!) { repository(owner: $o, name: $n) { pullRequest(number: $p) { closingIssuesReferences(first: 50) { nodes { number } } } } }'
    jq -n -c --arg q "$q" --arg o "$OWNER" --arg n "$NAME" --argjson p "$1" '{query: $q, variables: {o: $o, n: $n, p: $p}}' >"$WORK/cir.json"
    status=$(rest cir graphql POST "$WORK/cir.json")
    printf 'HTTP %s, %s' "$status" "$(jq -c '[.data.repository.pullRequest.closingIssuesReferences.nodes[]?.number]' <"$WORK/cir.body" 2>/dev/null)"
}

# merge_case CASE: merges it by its method, then prints the message of each commit it brought to the default branch.
merge_case() {
    local c=$1 status sha try
    if [ "${CASE_OWN[$c]}" = yes ]; then
        jq -n -c --arg m "${CASE_METHOD[$c]}" '{merge_method: $m, commit_title: "OWL-558 probe", commit_message: "A probe of OWL-558."}' >"$WORK/merge.json"
    else
        jq -n -c --arg m "${CASE_METHOD[$c]}" '{merge_method: $m}' >"$WORK/merge.json"
    fi
    try=1
    while :; do
        status=$(rest merge "repos/$REPO/pulls/${PR[$c]}/merge" PUT "$WORK/merge.json")
        if [ "$status" = 200 ] || [ "$try" -ge 4 ]; then break; fi
        try=$((try + 1))
        sleep 5
    done
    sha=$(field .sha "$WORK/merge.body")
    report "case $((c + 1)): ${CASE_METHOD[$c]}-merged #${PR[$c]}: HTTP $status (try $try), merged: $(field .merged "$WORK/merge.body")"
    if [ "$status" != 200 ] || [ -z "$sha" ]; then
        fail "case $((c + 1)): not merged: $(field .message "$WORK/merge.body")"
        return 1
    fi
    MERGED[c]=yes
    # The commits the merge brought: the merge or squash commit, or, newest first, each rebased commit, which touches
    # this case's files where the base's commits do not.
    while [ -n "$sha" ]; do
        rest commit "repos/$REPO/commits/$sha" >/dev/null
        if [ "${CASE_METHOD[$c]}" = rebase ] &&
            ! jq -e --arg b "${BRANCH[$c]}" '[.files[]?.filename] | any(contains($b))' <"$WORK/commit.body" >/dev/null 2>&1; then
            break
        fi
        report "case $((c + 1)):   commit on $BASE, message $(jq -c '.commit.message' <"$WORK/commit.body" 2>/dev/null)"
        if [ "${CASE_METHOD[$c]}" != rebase ]; then break; fi
        sha=$(jq -r '.parents[0].sha // empty' <"$WORK/commit.body" 2>/dev/null)
    done
}

probe() {
    local c
    section "pull requests"
    STAMP=$(date +%s)
    c=0
    while [ "$c" -lt "${#CASE_NAME[@]}" ]; do
        open_case "$c" || return 1
        c=$((c + 1))
    done
    sleep 10
    c=0
    while [ "$c" -lt "${#CASE_NAME[@]}" ]; do
        report "case $((c + 1)): closingIssuesReferences before the merge (+10 s): $(closing_refs "${PR[$c]}")"
        c=$((c + 1))
    done
    section merges
    c=0
    while [ "$c" -lt "${#CASE_NAME[@]}" ]; do
        merge_case "$c"
        c=$((c + 1))
    done
}

# state NUMBER: "open", or "closed/<state_reason>".
state() {
    rest st "repos/$REPO/issues/$1" >/dev/null
    jq -r 'if .state == "closed" then "closed/\(.state_reason // "none")" else (.state // "unread") end' <"$WORK/st.body" 2>/dev/null
}

read_back() {
    local c k n s verdict name
    section result
    sleep 60
    report "| case | method, message | row | issue | state at +60 s | verdict |"
    report "|------|-----------------|-----|-------|----------------|---------|"
    c=0
    while [ "$c" -lt "${#CASE_NAME[@]}" ]; do
        if [ "${MERGED[$c]:-no}" != yes ]; then
            report "| $((c + 1)) | ${CASE_NAME[$c]} | not merged | | | |"
            c=$((c + 1))
            continue
        fi
        s=$(state "${TARGET[$c]}")
        if [ "${s%%/*}" = closed ]; then verdict="closes (control, as expected)"; else
            verdict="ignored (CONTROL FAILED: the case proves nothing)"
            FAILED=$((FAILED + 1))
        fi
        report "| $((c + 1)) | ${CASE_NAME[$c]} | target: body ends with Closes #n | #${TARGET[$c]} | $s | $verdict |"
        n=$(labels "$c")
        k=0
        while [ "$k" -lt "$n" ]; do
            name=$(label "$c" "$k")
            s=$(state "$(jq -r --argjson k "$k" '.[$k]' <<<"${VICTIMS[$c]}")")
            if [ "${s%%/*}" = closed ]; then verdict=closes; else verdict=ignored; fi
            case $name in
            control:*)
                if [ "$verdict" = closes ]; then verdict="closes (control, as expected)"; else
                    verdict="ignored (CONTROL FAILED: the shipped rows prove nothing)"
                    FAILED=$((FAILED + 1))
                fi
                ;;
            esac
            report "| $((c + 1)) | ${CASE_NAME[$c]} | $name | #$(jq -r --argjson k "$k" '.[$k]' <<<"${VICTIMS[$c]}") | $s | $verdict |"
            k=$((k + 1))
        done
        c=$((c + 1))
    done
}

# shellcheck disable=SC2329 # run by the EXIT trap
cleanup() {
    local c i n status sha file
    if [ "${CLEANED:-}" = yes ]; then return 0; fi
    CLEANED=yes
    if [ "${STARTED:-no}" = yes ]; then
        section cleanup
        c=0
        while [ "$c" -lt "${#CASE_NAME[@]}" ]; do
            if [ -n "${PR[$c]:-}" ] && [ "${MERGED[$c]:-no}" = no ]; then
                jq -n -c '{state: "closed"}' >"$WORK/prclose.json"
                report "cleanup: close pull request #${PR[$c]} unmerged: HTTP $(rest prclose "repos/$REPO/pulls/${PR[$c]}" PATCH "$WORK/prclose.json")"
            fi
            if [ -n "${BRANCH[$c]:-}" ]; then
                report "cleanup: delete ${BRANCH[$c]}: HTTP $(rest delref "repos/$REPO/git/refs/heads/${BRANCH[$c]}" DELETE)"
            fi
            if [ "${MERGED[$c]:-no}" = yes ]; then
                for file in ${FILES[$c]}; do
                    rest getfile "repos/$REPO/contents/$file?ref=$BASE" >/dev/null
                    sha=$(field .sha "$WORK/getfile.body")
                    if [ -n "$sha" ]; then
                        jq -n -c --arg s "$sha" --arg b "$BASE" '{message: "OWL-558 probe: remove its file", sha: $s, branch: $b}' >"$WORK/delfile.json"
                        report "cleanup: remove $file from $BASE: HTTP $(rest delfile "repos/$REPO/contents/$file" DELETE "$WORK/delfile.json")"
                    else
                        report "cleanup: $file did not read on $BASE; remove it by hand"
                    fi
                done
            fi
            c=$((c + 1))
        done
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
if [ $# -ne 1 ]; then
    usage >&2
    exit 2
fi
REPO=$1
case $REPO in
*/*) ;;
*)
    say "$PROGRAM: $REPO must have the form owner/name"
    exit 2
    ;;
esac
if [ "$(printf '%s' "$REPO" | tr '[:upper:]' '[:lower:]')" = "$FORBIDDEN" ]; then
    say "$PROGRAM: never run against $FORBIDDEN: it merges pull requests and closes issues"
    exit 2
fi
OWNER=${REPO%%/*}
NAME=${REPO#*/}
for tool in gh jq base64; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        say "$PROGRAM: $tool is needed"
        exit 2
    fi
done

WORK=$(mktemp -d "${TMPDIR:-/tmp}/owl558.XXXXXX") || exit 2
FAILED=0
trap cleanup EXIT
trap 'exit 130' INT TERM

report "OWL-558 check, $(date -u +%Y-%m-%dT%H:%M:%SZ), $(gh --version | head -n 1 | cut -d' ' -f1-3), $(jq --version), $(uname -s), script sha256 $(script_sha)"
if ! gh auth status >/dev/null 2>&1 </dev/null; then
    say "$PROGRAM: gh is not signed in (gh auth status)"
    exit 2
fi
STARTED=yes
start || exit 1
issues || exit 1
probe || exit 1
read_back
exit "$FAILED"
