#!/usr/bin/env bash
# OWL-556 live check, quoted next to "The GitHub Issues adapter" of docs/design/build-plan.md, "P9 · Open-source
# readiness": which rewriting of a closing keyword ("Fixes #5") GitHub ignores when the pull request whose body holds
# it merges, so that the runner can neutralise the agent's text and leave its own `Closes #n` line as the only one
# that closes an issue.
#
# One pull request carries every form, each naming an issue of its own (a victim), and ends with `Closes #T`, T the
# run's target. It is squash-merged with a commit message of the probe's own, which names no issue, so that only the
# body can close one. Then each issue is read: a victim still open is a form GitHub ignores. Controls: the plain forms
# (`Fixes #n`, `Fixes owner/repo#n`, `Fixes <issue URL>`, `Closes #n`, `Resolves #n`) must close their victims, and T must close, or the run
# proves nothing. The "shipped" rows are the form the runner writes (U+200B after the keyword's second letter),
# for each of GitHub's nine keywords. Who opens and merges the pull request does not change how GitHub reads its body: the run goes
# through the person's own `gh` login, not the app's token (G5 settled that a pull request the token opens closes
# its issue alike).
#
#   bash docs/design/live-checks/owl-556.sh pitchopp/owlshift-live-sandbox      (--help: what it does)
#
# Needs bash (3.2 is enough), jq, and gh signed in (gh auth status) as an account that can write the sandbox's issues
# and contents and merge its pull requests. It runs alone in about 2 minutes. It creates on the sandbox 30 issues, a
# branch `owl-556-probe-<time>` with one file, and a pull request, which it squash-merges into the default branch;
# then it deletes the branch, removes the file from the default branch in a commit of its own, and closes as not
# planned every issue the merge left open. It refuses pitchopp/owlshift. The report, on standard output, holds no
# token and no account identifier: paste it on OWL-556.

set -u -o pipefail
umask 077

PROGRAM=${0##*/}
BODY="A probe of OWL-556 (docs/design/live-checks/owl-556.sh). The run ends it as not planned unless its pull request ends it first."
# The repository this check must never write to: Owlshift's own.
FORBIDDEN=pitchopp/owlshift

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
)
CONTROLS=(0 10 13 16 17)

usage() {
    cat <<EOF
Usage: bash $PROGRAM OWNER/REPO

  OWNER/REPO  the sandbox: pitchopp/owlshift-live-sandbox. Never $FORBIDDEN.

Needs jq and gh signed in as an account that writes the sandbox. About 2 minutes, alone:
it opens $((${#FORM_NAME[@]} + 1)) issues, a
branch with one file, and a pull request, squash-merges it into the default branch, reads
the issues, then deletes the file and the branch and ends the issues still open as not
planned. Paste everything it prints on standard output on OWL-556.
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

VICTIM=()
TARGET=
BRANCH=
PR=
MERGED=no
FILE_PATH=

probe() {
    local i status line sha
    section issues
    open_issue target "OWL-556: the target, its pull request ends with Closes"
    TARGET=$OPEN_NUMBER
    i=0
    while [ "$i" -lt "${#FORM_NAME[@]}" ]; do
        open_issue "v$i" "OWL-556: victim of form $((i + 1)), ${FORM_NAME[$i]}"
        VICTIM+=("$OPEN_NUMBER")
        i=$((i + 1))
    done
    if [ -z "$TARGET" ] || [ "${#OPENED[@]}" -ne $((${#FORM_NAME[@]} + 1)) ]; then
        fail "issues: not all opened; not run"
        return 1
    fi
    report "issues: target #$TARGET, victims #${VICTIM[0]} to #${VICTIM[$((${#VICTIM[@]} - 1))]}"

    # The body: one paragraph per form, then the runner's closing line, as `with_closing_line` writes it.
    : >"$WORK/lines"
    i=0
    while [ "$i" -lt "${#FORM_NAME[@]}" ]; do
        line=$(jq -n -r --arg n "${VICTIM[$i]}" --arg r "$REPO" --arg u "https://github.com/$REPO/issues/${VICTIM[$i]}" \
            "${FORM_JQ[$i]}")
        printf 'Form %s:\n\n%s\n\n' "$((i + 1))" "$line" >>"$WORK/lines"
        i=$((i + 1))
    done
    printf 'Closes #%s\n' "$TARGET" >>"$WORK/lines"

    section "pull request"
    BRANCH="owl-556-probe-$(date +%s)"
    jq -n -c --arg r "refs/heads/$BRANCH" --arg s "$BASE_SHA" '{ref: $r, sha: $s}' >"$WORK/mkref.json"
    status=$(rest mkref "repos/$REPO/git/refs" POST "$WORK/mkref.json")
    report "pull request: branch created from $BASE: HTTP $status"
    if [ "$status" != 201 ]; then
        BRANCH=
        fail "pull request: no branch; not run"
        return 1
    fi
    FILE_PATH="owl-556-probe/$BRANCH.md"
    jq -n -c --arg b "$BRANCH" --arg c "$(printf 'A probe of OWL-556.\n' | base64)" \
        '{message: "OWL-556 probe: one file", content: $c, branch: $b}' >"$WORK/file.json"
    status=$(rest file "repos/$REPO/contents/$FILE_PATH" PUT "$WORK/file.json")
    report "pull request: one file committed on the branch: HTTP $status"
    jq -n -c --arg h "$BRANCH" --arg b "$BASE" --rawfile body "$WORK/lines" \
        '{title: "OWL-556 probe", head: $h, base: $b, body: $body}' >"$WORK/pr.json"
    status=$(rest pr "repos/$REPO/pulls" POST "$WORK/pr.json")
    PR=$(field .number "$WORK/pr.body")
    report "pull request: opened #${PR:-none}: HTTP $status; the body GitHub stored equals the one sent: $(jq -r --rawfile b "$WORK/lines" '.body == $b' <"$WORK/pr.body" 2>/dev/null)"
    if [ -z "$PR" ]; then
        fail "pull request: not opened; not run"
        return 1
    fi
    sleep 10
    closing_refs "before the merge (+10 s)"

    # A squash merge with a message of the probe's own: only the body may close an issue.
    jq -n -c '{merge_method: "squash", commit_title: "OWL-556 probe", commit_message: "A probe of OWL-556."}' >"$WORK/merge.json"
    status=$(rest merge "repos/$REPO/pulls/$PR/merge" PUT "$WORK/merge.json")
    sha=$(field .sha "$WORK/merge.body")
    report "pull request: squash-merged: HTTP $status, merged: $(field .merged "$WORK/merge.body")"
    if [ "$status" != 200 ] || [ -z "$sha" ]; then
        fail "pull request: not merged: $(field .message "$WORK/merge.body"); nothing to read"
        return 1
    fi
    MERGED=yes
    status=$(rest commit "repos/$REPO/commits/$sha")
    report "pull request: the merge commit's message names an issue: $(jq -r '.commit.message | test("#[0-9]|/issues/[0-9]")' <"$WORK/commit.body" 2>/dev/null) (HTTP $status)"
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
                jq -n -c --arg s "$sha" --arg b "$BASE" '{message: "OWL-556 probe: remove its file", sha: $s, branch: $b}' >"$WORK/delfile.json"
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
    say "$PROGRAM: never run against $FORBIDDEN: it merges a pull request and closes issues"
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

WORK=$(mktemp -d "${TMPDIR:-/tmp}/owl556.XXXXXX") || exit 2
FAILED=0
trap cleanup EXIT
trap 'exit 130' INT TERM

report "OWL-556 check, $(date -u +%Y-%m-%dT%H:%M:%SZ), $(gh --version | head -n 1 | cut -d' ' -f1-3), $(jq --version), $(uname -s), script sha256 $(script_sha)"
if ! gh auth status >/dev/null 2>&1 </dev/null; then
    say "$PROGRAM: gh is not signed in (gh auth status)"
    exit 2
fi
STARTED=yes
start || exit 1
probe || exit 1
read_back
exit "$FAILED"
