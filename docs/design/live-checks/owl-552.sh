#!/usr/bin/env bash
# OWL-552 live check G7, quoted next to G1 of docs/design/build-plan.md, "P9 · Open-source readiness": the account a
# user token of Owlshift's GitHub App, `owl-shift`, signs in as, read by `GET /user` and `GET /user/{id}`, against the
# account its comments come from and the account an issue's assignee is. The local web UI replies on a GitHub Issues
# project as that account, and the Writer's floor compares its numeric id with the open ask's decider (the assignee's
# numeric id as text).
#   G7a `GET /user` answers 200 with `login`, a numeric `id` and `type` "User".
#   G7b `GET /user/{id}` of that id answers 200 with the same login, id and type; of an id no account holds, 404.
#       Of another account (the repository's owner, or OWL552_OTHER_ID), 200 with a login: the read the UI makes of the decider.
#   G7c An issue opened through the token with the signed-in account as assignee names that id as its assignee; a
#       comment the token posts on it is that account's: `user.id` the same id, `user.type` "User", and the list of
#       the issue's comments shows it with that author. The comment's `performed_via_github_app` is reported.
#
#   bash docs/design/live-checks/owl-552.sh OWNER/REPO                (--help: what to prepare)
#
# Needs bash (3.2 is enough), curl and jq. The access token reaches curl only through a file of mode 600 in a private
# scratch folder, removed at the end; nothing is stored. The report, on standard output, holds no token and no account
# identifier: ids are compared, never printed.

set -u -o pipefail
umask 077

PROGRAM=${0##*/}
API=https://api.github.com
LOGIN=https://github.com
# owl-shift's client ID, as crates/owlshift-adapters/src/forge/github_app.rs ships it (not a secret).
CLIENT_ID=${OWL552_CLIENT_ID:-Iv23li5EhBqiuljIPbiK}
# An id no account holds: GitHub's ids do not reach it yet.
UNKNOWN_ID=${OWL552_UNKNOWN_ID:-999999999999}
TITLE="A probe of OWL-552 (docs/design/live-checks/owl-552.sh), closed at the end of its run."
COMMENT="Probe comment of OWL-552, deleted at the end of its run."

usage() {
    cat <<EOF
Usage: bash $PROGRAM OWNER/REPO

  OWNER/REPO  the sandbox the owl-shift app is installed on, Issues (read and write)
              accepted, where you may be assigned: pitchopp/owlshift-live-sandbox

It signs in through the device flow (enter the code it shows), reads GET /user and
GET /user/{id}, opens one issue assigned to you, posts one comment on it and compares the
ids. At the end it deletes its comment and closes its issue as not planned. Paste everything
it prints on standard output on OWL-552.

OWL552_OTHER_ID=<id>  the account GET /user/{id} reads besides your own (default: the repository's
              owner; set it when you own the sandbox, so the owner is you)
EOF
}

say() { printf '%s\n' "$*" >&2; }
report() { printf '%s\n' "$*"; }
section() { printf '\n== %s ==\n' "$1"; }
fail() {
    FAILED=$((FAILED + 1))
    report "$1"
}
yes_no() { if [ -n "$1" ] && [ "$1" = "$2" ]; then echo yes; else echo no; fi; }
is_number() { case $1 in '' | *[!0-9]*) return 1 ;; *) return 0 ;; esac }

# The one place a secret is printed: into a file of mode 600, read by curl.
write_secret_file() {
    rm -f "$1"
    (umask 077 && printf '%s\n' "$2" >"$1")
    chmod 600 "$1"
}

field() { jq -r "$1 // empty" <"$2" 2>/dev/null; }
# One header's value from a dump of response headers, the last one kept.
header() { grep -i "^$1:" "$2" 2>/dev/null | tail -n 1 | cut -d: -f2- | tr -d '\r' | sed 's/^ *//'; }
perms() { header x-accepted-github-permissions "$WORK/$1.head"; }

# call NAME METHOD URL [BODY_FILE] [AUTH_FILE]: the answer in $WORK/NAME.body, its headers in $WORK/NAME.head, the
# status printed.
call() {
    local name=$1 method=$2 url=$3 body=${4:-} auth=${5:-} accept status
    case $url in
    "$LOGIN"/*) accept='Accept: application/json' ;;
    *) accept='Accept: application/vnd.github+json' ;;
    esac
    local args=(-sS -X "$method" -o "$WORK/$name.body" -D "$WORK/$name.head" -w '%{http_code}'
        -H "$accept" -H 'X-GitHub-Api-Version: 2022-11-28')
    if [ -n "$body" ]; then args+=(--data "@$body"); fi
    if [ -n "$auth" ]; then args+=(-H "@$auth"); fi
    : >"$WORK/$name.body"
    : >"$WORK/$name.head"
    status=$(curl "${args[@]}" "$url" 2>"$WORK/$name.err") || true
    printf '%s\n' "${status:-000}"
}

# rest NAME PATH [METHOD] [BODY_FILE]: a call with the signed-in token.
rest() { call "$1" "${3:-GET}" "$API/$2" "${4:-}" "$WORK/auth"; }

script_sha() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$0" | cut -c1-16
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$0" | cut -c1-16
    else
        echo unknown
    fi
}

sign_in() {
    local status interval life device polls=0 err token
    printf 'client_id=%s\n' "$CLIENT_ID" >"$WORK/device.form"
    status=$(call device POST "$LOGIN/login/device/code" "$WORK/device.form")
    device=$(field .device_code "$WORK/device.body")
    if [ -z "$device" ]; then
        fail "sign-in: no device code: HTTP $status $(field .error "$WORK/device.body")"
        return 1
    fi
    interval=$(field .interval "$WORK/device.body")
    interval=${interval:-5}
    life=$(field .expires_in "$WORK/device.body")
    life=${life:-900}
    write_secret_file "$WORK/poll.form" \
        "client_id=$CLIENT_ID&device_code=$device&grant_type=urn:ietf:params:oauth:grant-type:device_code"
    say ""
    say "Open $(field .verification_uri "$WORK/device.body") and enter the code $(field .user_code "$WORK/device.body")"
    say "Waiting for the code to be entered..."
    while [ $((polls * interval)) -lt "$life" ]; do
        sleep "$interval"
        polls=$((polls + 1))
        call poll POST "$LOGIN/login/oauth/access_token" "$WORK/poll.form" >/dev/null
        err=$(field .error "$WORK/poll.body")
        case $err in
        authorization_pending) ;;
        slow_down)
            interval=$(field .interval "$WORK/poll.body")
            interval=${interval:-10}
            ;;
        '')
            # A token, or a garbled answer (no body, curl failed), which is polled again.
            if [ -n "$(field .access_token "$WORK/poll.body")" ]; then break; fi
            ;;
        *)
            fail "sign-in: $err after $polls polls"
            return 1
            ;;
        esac
    done
    token=$(field .access_token "$WORK/poll.body")
    rm -f "$WORK/poll.body" "$WORK/poll.form" "$WORK/device.body"
    if [ -z "$token" ]; then
        fail "sign-in: no token after $polls polls"
        return 1
    fi
    write_secret_file "$WORK/auth" "Authorization: Bearer $token"
    token=
    report "sign-in: signed in after $polls polls; the token is kept in a file of this run only"
}

g7a() {
    local status
    section "G7a · GET /user"
    status=$(rest me user)
    ME_ID=$(field .id "$WORK/me.body")
    ME_LOGIN=$(field .login "$WORK/me.body")
    ME_TYPE=$(field .type "$WORK/me.body")
    report "G7a: GET user: HTTP $status; x-accepted-github-permissions: $(perms me); \`id\` a number: $(is_number "$ME_ID" && echo yes || echo no); \`login\` present: $(yes_no "$([ -n "$ME_LOGIN" ] && echo y)" y); type: ${ME_TYPE:-none}"
    if [ "$status" != 200 ] || ! is_number "$ME_ID" || [ -z "$ME_LOGIN" ] || [ "$ME_TYPE" != User ]; then
        fail "G7a: FAILED: GET user did not answer a User with an id and a login: $(field .message "$WORK/me.body")"
        return 1
    fi
}

g7b() {
    local status other
    section "G7b · GET /user/{id}"
    status=$(rest byid "user/$ME_ID")
    report "G7b: GET user/<own id>: HTTP $status; same id: $(yes_no "$(field .id "$WORK/byid.body")" "$ME_ID"); same login: $(yes_no "$(field .login "$WORK/byid.body")" "$ME_LOGIN"); type: $(field .type "$WORK/byid.body")"
    if [ "$status" != 200 ]; then fail "G7b: FAILED: GET user/<own id> did not answer 200: $(field .message "$WORK/byid.body")"; fi
    status=$(rest unknown "user/$UNKNOWN_ID")
    report "G7b: GET user/<an id no account holds>: HTTP $status $(field .message "$WORK/unknown.body")"
    if [ "$status" != 404 ]; then fail "G7b: FAILED: an unknown id did not answer 404"; fi
    # The UI reads another account's login by its id (the decider's, when the key is not theirs): by default the
    # repository's owner, when it is not the signed-in account; OWL552_OTHER_ID names another one.
    other=${OWL552_OTHER_ID:-}
    if [ -z "$other" ]; then
        status=$(rest repo "repos/$REPO")
        other=$(field .owner.id "$WORK/repo.body")
        if [ "$other" = "$ME_ID" ]; then other=; fi
    fi
    if is_number "$other"; then
        status=$(rest other "user/$other")
        report "G7b: GET user/<another account>: HTTP $status; type: $(field .type "$WORK/other.body"); login present: $(yes_no "$([ -n "$(field .login "$WORK/other.body")" ] && echo y)" y); a login other than the signed-in one: $(yes_no "$([ "$(field .login "$WORK/other.body")" != "$ME_LOGIN" ] && echo y)" y)"
        if [ "$status" != 200 ]; then fail "G7b: FAILED: GET user/<another account> did not answer 200: $(field .message "$WORK/other.body")"; fi
    else
        report "G7b: GET user/<another account>: NOT CHECKED, the repository's owner is the signed-in account; run again with OWL552_OTHER_ID set to the id of another account"
    fi
}

g7c() {
    local status n cid
    section "G7c · the account of a comment and of an assignee"
    jq -n -c --arg t "$TITLE" --arg a "$ME_LOGIN" '{title: $t, assignees: [$a]}' >"$WORK/issue.json"
    status=$(rest issue "repos/$REPO/issues" POST "$WORK/issue.json")
    n=$(field .number "$WORK/issue.body")
    if [ "$status" != 201 ] || ! is_number "$n"; then
        fail "G7c: FAILED: the issue was not opened: HTTP $status $(field .message "$WORK/issue.body")"
        return 1
    fi
    ISSUE=$n
    report "G7c: open an issue assigned to the signed-in login: HTTP $status; x-accepted-github-permissions: $(perms issue); assignees: $(jq -r '.assignees | length' <"$WORK/issue.body"); the assignee's id is the signed-in id: $(yes_no "$(jq -r '.assignees[0].id // empty' <"$WORK/issue.body")" "$ME_ID"); the issue's user id is the signed-in id: $(yes_no "$(field .user.id "$WORK/issue.body")" "$ME_ID")"
    jq -n -c --arg b "$COMMENT" '{body: $b}' >"$WORK/comment.json"
    status=$(rest comment "repos/$REPO/issues/$ISSUE/comments" POST "$WORK/comment.json")
    cid=$(field .id "$WORK/comment.body")
    if [ "$status" != 201 ] || ! is_number "$cid"; then
        fail "G7c: FAILED: the comment was not posted: HTTP $status $(field .message "$WORK/comment.body")"
        return 1
    fi
    COMMENT_ID=$cid
    report "G7c: post a comment: HTTP $status; x-accepted-github-permissions: $(perms comment); user.id is the signed-in id: $(yes_no "$(field .user.id "$WORK/comment.body")" "$ME_ID"); user.login is the signed-in login: $(yes_no "$(field .user.login "$WORK/comment.body")" "$ME_LOGIN"); user.type: $(field .user.type "$WORK/comment.body"); performed_via_github_app.slug: $(field .performed_via_github_app.slug "$WORK/comment.body"); author_association: $(field .author_association "$WORK/comment.body")"
    status=$(rest list "repos/$REPO/issues/$ISSUE/comments?per_page=100")
    report "G7c: list the issue's comments: HTTP $status; $(jq -r --argjson c "$COMMENT_ID" --arg me "$ME_ID" '[.[] | select(.id == $c)] | "found: \(length); its user.id is the signed-in id: \(if length == 1 and (.[0].user.id | tostring) == $me then "yes" else "no" end)"' <"$WORK/list.body" 2>/dev/null)"
    if [ "$(field .user.id "$WORK/comment.body")" != "$ME_ID" ] || [ "$(jq -r '.assignees[0].id // empty' <"$WORK/issue.body")" != "$ME_ID" ]; then
        fail "G7c: FAILED: the ids of the comment, the assignee and GET user are not one account"
    fi
}

# shellcheck disable=SC2329 # run by the EXIT trap
cleanup() {
    local status
    if [ "${CLEANED:-}" = yes ]; then return 0; fi
    CLEANED=yes
    if [ -f "${WORK:-}/auth" ]; then
        section cleanup
        if [ -n "${COMMENT_ID:-}" ]; then
            status=$(rest delcomment "repos/$REPO/issues/comments/$COMMENT_ID" DELETE)
            report "cleanup: delete the comment: HTTP $status"
        fi
        if [ -n "${ISSUE:-}" ]; then
            jq -n -c '{state: "closed", state_reason: "not_planned"}' >"$WORK/close.json"
            status=$(rest close "repos/$REPO/issues/$ISSUE" PATCH "$WORK/close.json")
            report "cleanup: close #$ISSUE as not planned: HTTP $status"
        fi
        report ""
        report "done, failures: $FAILED. The access token expires on its own (8 h); nothing was stored."
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
for tool in curl jq; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        say "$PROGRAM: $tool is needed"
        exit 2
    fi
done

WORK=$(mktemp -d "${TMPDIR:-/tmp}/owl552.XXXXXX") || exit 2
FAILED=0
ME_ID=0
ME_LOGIN=
ME_TYPE=
ISSUE=
COMMENT_ID=
trap cleanup EXIT
trap 'exit 130' INT TERM

report "OWL-552 check G7, $(date -u +%Y-%m-%dT%H:%M:%SZ), $(curl --version | head -n 1 | cut -d' ' -f1-2), $(jq --version), $(uname -s), script sha256 $(script_sha)"
section sign-in
sign_in || exit 1
g7a || exit 1
g7b
g7c
exit "$FAILED"
