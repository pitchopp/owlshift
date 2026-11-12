#!/usr/bin/env bash
# OWL-547 live checks, quoted next to G3, G4 and G6 of docs/design/build-plan.md, "P9 · Open-source readiness": the
# three points the run of 2026-11-12 (OWL-532) left unobserved, through a user token of Owlshift's GitHub App,
# `owl-shift`, signed in with the device flow and its client ID, as that run was.
#   G3  An issue opened with the ready label: does a `labeled` event appear later, or never.
#   G4  A "blocked by" relation a person writes to an issue the token cannot read: listed, or omitted.
#   G6  Whether a conditional request answered 304 counts against the token's hourly limit.
# Each check has a control, so that a lag is not read as "never": G3 a label added to an issue already open, and
# OWL-532's #12, opened with its label hours before; G4 a relation to an issue the token reads, written at the same
# moment, and the person's own read of the list; G6 plain reads before and after the conditional ones, and what other
# clients spend while the token is idle (they can only add to a count, never take from it).
#
#   bash docs/design/live-checks/owl-547.sh OWNER/REPO OTHER_OWNER/OTHER_REPO      (--help: what to prepare)
#
# Needs bash (3.2 is enough), curl, jq, and gh signed in as the person who writes G4's relations. The access token
# reaches curl only through a file of mode 600 in a private scratch folder, removed at the end; nothing is stored.
# The report, on standard output, holds no token and no account identifier: accounts are compared, never printed.

set -u -o pipefail
umask 077

PROGRAM=${0##*/}
API=https://api.github.com
LOGIN=https://github.com
# owl-shift's client ID, as crates/owlshift-adapters/src/forge/github_app.rs ships it (not a secret).
CLIENT_ID=${OWL547_CLIENT_ID:-Iv23li5EhBqiuljIPbiK}
SLUG=owl-shift
LABEL=owlshift-547-ready
BODY="A probe of OWL-547 (docs/design/live-checks/owl-547.sh), closed at the end of its run."
G3_WINDOW=${OWL547_G3_WINDOW:-600}
OLD_ISSUE=${OWL547_OLD_ISSUE:-12}

usage() {
    cat <<EOF
Usage: bash $PROGRAM OWNER/REPO OTHER_OWNER/OTHER_REPO

  OWNER/REPO  the sandbox the owl-shift app is installed on, Issues (read and write)
              accepted: pitchopp/owlshift-live-sandbox
  OTHER       a private repository you can write to, with issues on, that the app's
              installation does not hold, so that the token cannot read its issues:
              pitchopp/owlshift-merge-sandbox. If the installation holds it, the run
              asks you to remove it, and at the end says to add it back.

Before the run (about 15 minutes):
  - gh signed in as you (gh auth status): G4's relations are written by you, through gh.
  - Stop what you can of your account's other GitHub clients (Owlshift, Claude Code
    sessions, an editor's GitHub extension) until the run ends: G6 reads the hourly
    budget. The run measures what the others still spend; its verdict holds either way.

It signs in through the device flow (enter the code it shows), asks you once to open an
issue in the browser (optional), then runs alone. At the end it removes its relations
and its label and closes its issues as not planned. Paste everything it prints on
standard output on OWL-547.
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

# call NAME METHOD URL [BODY_FILE] [AUTH_FILE] [EXTRA_HEADER]: the answer in $WORK/NAME.body, its headers in
# $WORK/NAME.head, the status printed.
call() {
    local name=$1 method=$2 url=$3 body=${4:-} auth=${5:-} extra=${6:-} accept status
    case $url in
    "$LOGIN"/*) accept='Accept: application/json' ;;
    *) accept='Accept: application/vnd.github+json' ;;
    esac
    local args=(-sS -X "$method" -o "$WORK/$name.body" -D "$WORK/$name.head" -w '%{http_code}'
        -H "$accept" -H 'X-GitHub-Api-Version: 2022-11-28')
    if [ -n "$body" ]; then args+=(--data "@$body"); fi
    if [ -n "$auth" ]; then args+=(-H "@$auth"); fi
    if [ -n "$extra" ]; then args+=(-H "$extra"); fi
    : >"$WORK/$name.body"
    : >"$WORK/$name.head"
    status=$(curl "${args[@]}" "$url" 2>"$WORK/$name.err") || true
    printf '%s\n' "${status:-000}"
}

# rest NAME PATH [METHOD] [BODY_FILE] [EXTRA_HEADER]: a call with the signed-in token.
rest() { call "$1" "${3:-GET}" "$API/$2" "${4:-}" "$WORK/auth" "${5:-}"; }

# graphql NAME QUERY VARIABLES_JSON
graphql() {
    jq -n -c --arg q "$2" --argjson v "$3" '{query: $q, variables: $v}' >"$WORK/$1.json"
    call "$1" POST "$API/graphql" "$WORK/$1.json" "$WORK/auth"
}
# A GraphQL answer's errors, OTHER's name replaced by "OTHER".
gql_errors() { jq -c --arg o "$OTHER" '[.errors[]? | {type, message: (.message | split($o) | join("OTHER"))}]' <"$1" 2>/dev/null; }

# gh_status NAME ARGS...: `gh api -i ARGS` as the person, the HTTP status printed.
gh_status() {
    local name=$1
    shift
    gh api -i "$@" >"$WORK/$name.gh" 2>"$WORK/$name.gherr" </dev/null || true
    awk 'NR == 1 { print $2; exit }' "$WORK/$name.gh"
}

pause() {
    local answer
    say ""
    say "$1"
    say "Press Enter when done (or type 'skip' then Enter to leave it undone)."
    read -r answer || answer=skip
    [ "$answer" != skip ]
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
        '') break ;;
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
    report "sign-in: signed in after $polls polls; the token is kept in memory and a file of this run only"
}

# The checks every part needs; sets ME_ID, G4_SKIP and OTHER_REMOVED.
start() {
    local status issues gh_id private
    section start
    status=$(rest user user)
    ME_ID=$(field .id "$WORK/user.body")
    ME_ID=${ME_ID:-0}
    report "start: GET /user: HTTP $status, numeric id present: $(is_number "$ME_ID" && [ "$ME_ID" != 0 ] && echo yes || echo no)"
    status=$(rest inst user/installations)
    issues=$(jq -r --arg s "$SLUG" '[.installations[]? | select(.app_slug == $s)][0].permissions.issues // "none"' \
        <"$WORK/inst.body" 2>/dev/null)
    report "start: GET /user/installations: HTTP $status, the app's installation holds issues: ${issues:-none}"
    status=$(rest repo "repos/$REPO")
    report "start: the token reads REPO: HTTP $status"
    if [ "$issues" != write ] || [ "$status" != 200 ]; then
        fail "start: the token cannot write REPO's issues; nothing run"
        return 1
    fi

    G4_SKIP=
    OTHER_REMOVED=no
    if [ "$GH_OK" = no ]; then
        G4_SKIP="gh is not installed"
    elif ! gh auth status >/dev/null 2>&1 </dev/null; then
        G4_SKIP="gh is not signed in"
        GH_OK=no
    else
        gh_id=$(gh api user --jq .id </dev/null 2>/dev/null)
        report "start: gh's account equals the signed-in user: $(yes_no "$gh_id" "$ME_ID")"
        private=$(gh api "repos/$OTHER" --jq .private </dev/null 2>/dev/null)
        report "start: gh reads OTHER: $([ -n "$private" ] && echo yes || echo no), private=${private:-unknown}"
        if [ -z "$private" ]; then G4_SKIP="gh cannot read OTHER"; fi
    fi
    status=$(rest other "repos/$OTHER")
    report "start: the token reads OTHER: HTTP $status"
    if [ "$status" = 200 ] && [ -z "$G4_SKIP" ]; then
        if pause "Remove $OTHER from the owl-shift app's installation: github.com/settings/installations > owl-shift > Configure > Repository access > Only select repositories: remove it > Save. The run says at the end to add it back."; then
            OTHER_REMOVED=yes
            status=$(rest other "repos/$OTHER")
            report "start: after the removal, the token reads OTHER: HTTP $status"
        fi
        if [ "$status" = 200 ]; then G4_SKIP="the token still reads OTHER"; fi
    fi

    jq -n -c --arg l "$LABEL" '{name: $l, color: "0e8a16"}' >"$WORK/label.json"
    status=$(rest label "repos/$REPO/labels" POST "$WORK/label.json")
    report "start: create label $LABEL: HTTP $status"
}

OPENED=()
# open_issue NAME TITLE [LABEL]: opens an issue through the token; sets OPEN_STATUS, OPEN_NUMBER and OPEN_ID.
open_issue() {
    local name=$1 title=$2 label=${3:-}
    if [ -n "$label" ]; then
        jq -n -c --arg t "$title" --arg b "$BODY" --arg l "$label" '{title: $t, body: $b, labels: [$l]}' >"$WORK/$name.json"
    else
        jq -n -c --arg t "$title" --arg b "$BODY" '{title: $t, body: $b}' >"$WORK/$name.json"
    fi
    OPEN_STATUS=$(rest "$name" "repos/$REPO/issues" POST "$WORK/$name.json")
    OPEN_NUMBER=$(field .number "$WORK/$name.body")
    OPEN_ID=$(field .id "$WORK/$name.body")
    if [ -n "$OPEN_NUMBER" ]; then OPENED+=("$OPEN_NUMBER"); fi
}

has_label() { jq -r --arg l "$LABEL" '[.labels[]? | (.name // .)] | index($l) != null' <"$1" 2>/dev/null; }

# G3's issues, in parallel arrays: number, path, local time counted from, GitHub's creation time, and the elapsed
# seconds at which a labeled event of LABEL was first read in the events, the timeline and GraphQL.
G3_NUM=()
G3_PATH=()
G3_T0=()
G3_CREATED=()
G3_EV=()
G3_TL=()
G3_GQL=()
g3_add() {
    G3_NUM+=("$1")
    G3_PATH+=("$2")
    G3_T0+=("$3")
    G3_CREATED+=("$4")
    G3_EV+=("")
    G3_TL+=("")
    G3_GQL+=("")
}

g3_open() {
    local status number created q vars url repo_id label_id
    section G3

    # The web UI, as a person opens an issue there.
    if pause "In your browser, open a new issue on $REPO titled 'OWL-547 G3: opened in the web UI with the label'. Before you press Create, pick the label $LABEL in the form's Labels."; then
        say "Type the new issue's number, then Enter:"
        read -r number || number=
        if is_number "$number"; then
            OPENED+=("$number")
            status=$(rest g3web "repos/$REPO/issues/$number")
            created=$(field .created_at "$WORK/g3web.body")
            report "G3: #$number opened in the web UI: read HTTP $status, the label on it: $(has_label "$WORK/g3web.body"), its author equals the signed-in user: $(yes_no "$(field .user.id "$WORK/g3web.body")" "$ME_ID") (its times below count from the number typed)"
            g3_add "$number" "web UI, the person" "$(date +%s)" "$created"
        else
            report "G3: the web UI path: no number typed, skipped"
        fi
    else
        report "G3: the web UI path: skipped"
    fi

    # REST, through the token.
    open_issue g3rest "OWL-547 G3: opened with the label (REST, the app's token)" "$LABEL"
    report "G3: open #$OPEN_NUMBER with the label through REST, the token: HTTP $OPEN_STATUS; $(perms g3rest); the label on it: $(has_label "$WORK/g3rest.body")"
    if [ -n "$OPEN_NUMBER" ]; then g3_add "$OPEN_NUMBER" "REST, the token" "$(date +%s)" "$(field .created_at "$WORK/g3rest.body")"; fi

    # GraphQL's createIssue with labelIds, through the token.
    vars=$(jq -n -c --arg o "$OWNER" --arg n "$NAME" --arg l "$LABEL" '{o: $o, n: $n, l: $l}')
    # shellcheck disable=SC2016 # GraphQL's variables
    q='query($o: String!, $n: String!, $l: String!) { repository(owner: $o, name: $n) { id label(name: $l) { id } } }'
    status=$(graphql g3ids "$q" "$vars")
    repo_id=$(field .data.repository.id "$WORK/g3ids.body")
    label_id=$(field .data.repository.label.id "$WORK/g3ids.body")
    if [ -n "$repo_id" ] && [ -n "$label_id" ]; then
        # shellcheck disable=SC2016
        q='mutation($i: CreateIssueInput!) { createIssue(input: $i) { issue { number createdAt labels(first: 10) { nodes { name } } } } }'
        status=$(graphql g3gql "$q" "$(jq -n -c --arg r "$repo_id" --arg l "$label_id" --arg b "$BODY" \
            '{i: {repositoryId: $r, title: "OWL-547 G3: opened with the label (GraphQL createIssue, the token)", body: $b, labelIds: [$l]}}')")
        number=$(field .data.createIssue.issue.number "$WORK/g3gql.body")
        report "G3: open #$number with the label through GraphQL's createIssue, the token: HTTP $status, errors=$(gql_errors "$WORK/g3gql.body"), the label on it: $(jq -r --arg l "$LABEL" '[.data.createIssue.issue.labels.nodes[]?.name] | index($l) != null' <"$WORK/g3gql.body" 2>/dev/null)"
        if [ -n "$number" ]; then
            OPENED+=("$number")
            g3_add "$number" "GraphQL createIssue, the token" "$(date +%s)" "$(field .data.createIssue.issue.createdAt "$WORK/g3gql.body")"
        fi
    else
        fail "G3: GraphQL path not run: repository or label id not read (HTTP $status, errors=$(gql_errors "$WORK/g3ids.body"))"
    fi

    # gh issue create --label, the person's own login.
    if [ "$GH_OK" = yes ]; then
        url=$(gh issue create -R "$REPO" --title "OWL-547 G3: opened with the label (gh issue create, the person)" \
            --body "$BODY" --label "$LABEL" </dev/null 2>"$WORK/g3gh.err")
        number=${url##*/}
        if is_number "$number"; then
            OPENED+=("$number")
            status=$(rest g3gh "repos/$REPO/issues/$number")
            report "G3: open #$number with the label through gh issue create, the person: done; read HTTP $status, the label on it: $(has_label "$WORK/g3gh.body")"
            g3_add "$number" "gh issue create, the person" "$(date +%s)" "$(field .created_at "$WORK/g3gh.body")"
        else
            fail "G3: gh issue create failed: $(head -n 1 "$WORK/g3gh.err")"
        fi
    else
        report "G3: the gh path: not run, gh is not signed in"
    fi

    # The control: the label added to an issue already open, through the token.
    open_issue g3ctl "OWL-547 G3: the label added once open (control)"
    number=$OPEN_NUMBER
    created=$(field .created_at "$WORK/g3ctl.body")
    sleep 2
    rest g3ctl0 "repos/$REPO/issues/$number" >/dev/null
    jq -n -c --arg l "$LABEL" '{labels: [$l]}' >"$WORK/g3add.json"
    status=$(rest g3add "repos/$REPO/issues/$number/labels" POST "$WORK/g3add.json")
    G3_BASE=$(date +%s)
    G3_CONTROL=$number
    G3_CONTROL_UPDATED=$(field .updated_at "$WORK/g3ctl0.body")
    sleep 1
    rest g3ctl1 "repos/$REPO/issues/$number" >/dev/null
    report "G3: control #$number, opened without the label, the label added 2 s later through the token: HTTP $status; the issue's updated_at moved with it: $(if [ "$(field .updated_at "$WORK/g3ctl0.body")" != "$(field .updated_at "$WORK/g3ctl1.body")" ]; then echo yes; else echo no; fi)"
    g3_add "$number" "control, label added once open" "$G3_BASE" "$created"

    # OWL-532's issue opened with its label, hours ago.
    status=$(rest g3old "repos/$REPO/issues/$OLD_ISSUE/events?per_page=100")
    rest g3oldi "repos/$REPO/issues/$OLD_ISSUE" >/dev/null
    report "G3: #$OLD_ISSUE, opened by OWL-532 with owlshift-check:ready, that label deleted since, opened $(jq -r '((now - (.created_at | fromdateiso8601)) / 3600 | floor | tostring) + " h ago"' <"$WORK/g3oldi.body" 2>/dev/null): events HTTP $status, labeled events $(jq '[.[]? | select(.event == "labeled")] | length' <"$WORK/g3old.body" 2>/dev/null), of owlshift-check:ready $(jq '[.[]? | select(.event == "labeled" and .label.name == "owlshift-check:ready")] | length' <"$WORK/g3old.body" 2>/dev/null), event types $(jq -c '[.[]?.event]' <"$WORK/g3old.body" 2>/dev/null)"
}

# One read of every G3 issue: REST events and timeline, and one GraphQL query for all.
g3_poll() {
    local i n now el ev tl gq q count
    count=${#G3_NUM[@]}
    if [ "$count" -eq 0 ]; then return 0; fi
    # shellcheck disable=SC2016
    q='query($o: String!, $n: String!) { repository(owner: $o, name: $n) {'
    i=0
    while [ "$i" -lt "$count" ]; do
        n=${G3_NUM[$i]}
        q="$q i$n: issue(number: $n) { timelineItems(first: 50, itemTypes: [LABELED_EVENT]) { nodes { ... on LabeledEvent { createdAt label { name } } } } }"
        i=$((i + 1))
    done
    q="$q } }"
    graphql g3q "$q" "$(jq -n -c --arg o "$OWNER" --arg n "$NAME" '{o: $o, n: $n}')" >/dev/null
    now=$(date +%s)
    i=0
    while [ "$i" -lt "$count" ]; do
        n=${G3_NUM[$i]}
        el=$((now - ${G3_T0[$i]}))
        rest g3e "repos/$REPO/issues/$n/events?per_page=100" >/dev/null
        ev=$(jq --arg l "$LABEL" '[.[]? | select(.event == "labeled" and .label.name == $l)] | length' <"$WORK/g3e.body" 2>/dev/null)
        rest g3t "repos/$REPO/issues/$n/timeline?per_page=100" >/dev/null
        tl=$(jq --arg l "$LABEL" '[.[]? | select(.event == "labeled" and .label.name == $l)] | length' <"$WORK/g3t.body" 2>/dev/null)
        gq=$(jq --arg l "$LABEL" --arg k "i$n" '[.data.repository[$k].timelineItems.nodes[]? | select(.label.name == $l)] | length' <"$WORK/g3q.body" 2>/dev/null)
        report "G3 +${el}s, #$n (${G3_PATH[$i]}): labeled in the events ${ev:-?}, in the timeline ${tl:-?}, LabeledEvent in GraphQL ${gq:-?}"
        if [ "${ev:-0}" -gt 0 ] && [ -z "${G3_EV[$i]}" ]; then
            G3_EV[i]=$el
            report "G3 #$n: its labeled event: $(jq -r --arg l "$LABEL" --argjson me "$ME_ID" --arg c "${G3_CREATED[$i]}" '
                [.[] | select(.event == "labeled" and .label.name == $l)][0]
                | "actor equals the signed-in user: \(.actor.id == $me), performed_via_github_app=\(.performed_via_github_app.slug // "none"), created \((.created_at | fromdateiso8601) - ($c | fromdateiso8601)) s after the issue"' \
                <"$WORK/g3e.body" 2>/dev/null)"
        fi
        if [ "${tl:-0}" -gt 0 ] && [ -z "${G3_TL[$i]}" ]; then G3_TL[i]=$el; fi
        if [ "${gq:-0}" -gt 0 ] && [ -z "${G3_GQL[$i]}" ]; then G3_GQL[i]=$el; fi
        i=$((i + 1))
    done
    if [ "$(jq '.errors // [] | length' <"$WORK/g3q.body" 2>/dev/null)" != 0 ]; then
        report "G3: GraphQL errors: $(gql_errors "$WORK/g3q.body")"
    fi
}

g3_all_seen() {
    local i=0
    while [ "$i" -lt "${#G3_NUM[@]}" ]; do
        if [ -z "${G3_EV[$i]}" ] || [ -z "${G3_TL[$i]}" ] || [ -z "${G3_GQL[$i]}" ]; then return 1; fi
        i=$((i + 1))
    done
    return 0
}

# g3_poll_at SECONDS: waits until SECONDS after the control's label, then reads.
g3_poll_at() {
    local at=$((G3_BASE + $1)) now
    if g3_all_seen; then return 0; fi
    now=$(date +%s)
    if [ "$now" -lt "$at" ]; then sleep $((at - now)); fi
    g3_poll
}

g3_seen() { if [ -n "$1" ]; then echo "first read at +${1}s"; else echo "not read by +${2}s"; fi; }

g3_summary() {
    local i=0 last
    section "G3 result"
    while [ "$i" -lt "${#G3_NUM[@]}" ]; do
        last=$(($(date +%s) - ${G3_T0[$i]}))
        report "G3 result, #${G3_NUM[$i]} (${G3_PATH[$i]}): a labeled event in the events $(g3_seen "${G3_EV[$i]}" "$last"); in the timeline $(g3_seen "${G3_TL[$i]}" "$last"); in GraphQL $(g3_seen "${G3_GQL[$i]}" "$last")"
        i=$((i + 1))
    done
    if [ -n "${G3_CONTROL:-}" ]; then
        rest g3ctl2 "repos/$REPO/issues/$G3_CONTROL" >/dev/null
        report "G3 result: the control's updated_at, read now, moved since before its label: $(if [ "$(field .updated_at "$WORK/g3ctl2.body")" != "$G3_CONTROL_UPDATED" ]; then echo yes; else echo no; fi)"
    fi
}

REL_N=
REL_IDS=()
OTHER_ISSUE=
# The token's view of #REL_N's blockers: REST list, the issue's summary, GraphQL's blockedBy.
g4_read() {
    local status gstatus q
    status=$(rest g4l "repos/$REPO/issues/$REL_N/dependencies/blocked_by")
    rest g4i "repos/$REPO/issues/$REL_N" >/dev/null
    # shellcheck disable=SC2016
    q='query($o: String!, $n: String!, $i: Int!) { repository(owner: $o, name: $n) { issue(number: $i) { blockedBy(first: 20) { totalCount nodes { number repository { nameWithOwner } } } issueDependenciesSummary { blockedBy totalBlockedBy } } } }'
    gstatus=$(graphql g4q "$q" "$(jq -n -c --arg o "$OWNER" --arg n "$NAME" --argjson i "$REL_N" '{o: $o, n: $n, i: $i}')")
    report "G4 $1: the token's REST blocked_by: HTTP $status, $(jq -r --arg r "$REPO" --arg o "$OTHER" --argjson k "$G4_CONTROL" '
        def of($x): (.repository_url // "" | ascii_downcase | endswith("/repos/" + ($x | ascii_downcase)));
        if type == "array" then
          "entries \(length), the control listed: \(any(.[]; .number == $k and of($r))), of OTHER: \([.[] | select(of($o))] | length), of neither: \([.[] | select((of($r) or of($o)) | not)] | length), the keys of those: \([.[] | select(of($r) | not) | keys] | unique)"
        else "message=\(.message // "none")" end' <"$WORK/g4l.body" 2>/dev/null)"
    report "G4 $1: issue_dependencies_summary=$(jq -c '.issue_dependencies_summary // null' <"$WORK/g4i.body" 2>/dev/null)"
    report "G4 $1: GraphQL blockedBy: HTTP $gstatus, errors=$(gql_errors "$WORK/g4q.body"), $(jq -r --arg o "$OTHER" '
        .data.repository.issue as $i
        | if $i == null then "issue null"
          else ($i.blockedBy.nodes // []) as $nodes
            | "totalCount \($i.blockedBy.totalCount), nodes \($nodes | length), null nodes \([$nodes[] | select(. == null)] | length), of OTHER \([$nodes[] | select(. != null and ((.repository.nameWithOwner // "") | ascii_downcase) == ($o | ascii_downcase))] | length), issueDependenciesSummary=\($i.issueDependenciesSummary | tojson)" end' \
        <"$WORK/g4q.body" 2>/dev/null)"
}

g4() {
    local status url other_id t off now control_id
    section G4
    if [ -n "$G4_SKIP" ]; then
        report "G4: not run: $G4_SKIP"
        return 0
    fi
    open_issue g4n "OWL-547 G4: blocked by an issue the token reads and one it cannot read"
    REL_N=$OPEN_NUMBER
    open_issue g4k "OWL-547 G4: a blocker the token reads (control)"
    G4_CONTROL=${OPEN_NUMBER:-0}
    control_id=$OPEN_ID
    url=$(gh issue create -R "$OTHER" --title "OWL-547 G4: a blocker the owl-shift token cannot read" --body "$BODY" \
        </dev/null 2>"$WORK/g4m.err")
    OTHER_ISSUE=${url##*/}
    if ! is_number "$OTHER_ISSUE"; then
        OTHER_ISSUE=
        fail "G4: gh could not open OTHER's issue: $(head -n 1 "$WORK/g4m.err")"
        return 0
    fi
    other_id=$(gh api "repos/$OTHER/issues/$OTHER_ISSUE" --jq .id </dev/null 2>/dev/null)
    if [ -z "$REL_N" ] || [ -z "$control_id" ] || ! is_number "$other_id"; then
        fail "G4: an issue was not opened or read; not run"
        return 0
    fi
    report "G4: #$REL_N, its control blocker #$G4_CONTROL (both through the token) and OTHER's issue (through gh) opened"

    status=$(gh_status g4wk -X POST "repos/$REPO/issues/$REL_N/dependencies/blocked_by" -F "issue_id=$control_id")
    REL_IDS+=("$control_id")
    report "G4: the person writes #$REL_N blocked by #$G4_CONTROL through gh: HTTP $status"
    status=$(gh_status g4wm -X POST "repos/$REPO/issues/$REL_N/dependencies/blocked_by" -F "issue_id=$other_id")
    REL_IDS+=("$other_id")
    report "G4: the person writes #$REL_N blocked by OTHER's issue through gh: HTTP $status"
    t=$(date +%s)
    gh api "repos/$REPO/issues/$REL_N/dependencies/blocked_by" </dev/null >"$WORK/g4gh.json" 2>/dev/null
    report "G4: the person's read of #$REL_N's blocked_by through gh: $(jq -r --arg o "$OTHER" '
        "entries \(length), of OTHER \([.[] | select(.repository_url // "" | ascii_downcase | endswith("/repos/" + ($o | ascii_downcase)))] | length)"' \
        <"$WORK/g4gh.json" 2>/dev/null)"
    status=$(rest g4m "repos/$OTHER/issues/$OTHER_ISSUE")
    report "G4: the token reads OTHER's issue: HTTP $status"
    for off in 2 10 30 60; do
        now=$(date +%s)
        if [ "$now" -lt $((t + off)) ]; then sleep $((t + off - now)); fi
        g4_read "+${off}s"
    done
}

# core.used from /rate_limit, which reading does not move (OWL-532), read 2 s after the last request so that the
# counter has settled.
rate_used() {
    sleep 2
    rest rl rate_limit >/dev/null
    field .resources.core.used "$WORK/rl.body"
}

# g6_phase plain|cond COUNT: COUNT reads of G6_URL; sets PH_200, PH_304, PH_OTHER and PH_U (the x-ratelimit-used
# of the first and the last answer).
g6_phase() {
    local kind=$1 count=$2 i=0 status first='' last=''
    PH_200=0
    PH_304=0
    PH_OTHER=0
    while [ "$i" -lt "$count" ]; do
        if [ "$kind" = cond ]; then
            status=$(rest g6 "$G6_URL" GET "" "If-None-Match: $G6_ETAG")
        else
            status=$(rest g6 "$G6_URL")
        fi
        case $status in
        200)
            PH_200=$((PH_200 + 1))
            if [ "$kind" = cond ]; then G6_ETAG=$(header etag "$WORK/g6.head"); fi
            ;;
        304) PH_304=$((PH_304 + 1)) ;;
        *) PH_OTHER=$((PH_OTHER + 1)) ;;
        esac
        last=$(header x-ratelimit-used "$WORK/g6.head")
        if [ -z "$first" ]; then first=${last:-absent}; fi
        i=$((i + 1))
    done
    PH_U="$first to ${last:-absent}"
}

g6() {
    local q status tries=0 limit a b r0 r1 r2 r3 c t0 d_u1 d_c d_u2 min_c=999999 min_u=999999 line idle sum_u=0 sum_c=0 gap
    section G6
    open_issue g6i "OWL-547 G6: an issue whose comments are read again"
    q=$OPEN_NUMBER
    if [ -z "$q" ]; then
        fail "G6: no issue opened; not run"
        return 0
    fi
    jq -n -c '{body: "One comment, read again by G6."}' >"$WORK/g6c.json"
    rest g6c "repos/$REPO/issues/$q/comments" POST "$WORK/g6c.json" >/dev/null
    G6_URL="repos/$REPO/issues/$q/comments"
    sleep 3
    status=
    while [ "$tries" -lt 5 ] && [ "$status" != 304 ]; do
        tries=$((tries + 1))
        rest g6 "$G6_URL" >/dev/null
        G6_ETAG=$(header etag "$WORK/g6.head")
        sleep 2
        status=$(rest g6 "$G6_URL" GET "" "If-None-Match: $G6_ETAG")
        if [ "$status" != 304 ]; then sleep 3; fi
    done
    report "G6: #$q's comments: an ETag present: $([ -n "$G6_ETAG" ] && echo yes || echo no); read again with If-None-Match: HTTP $status after $tries tries; the 304 carries x-ratelimit-used: $([ -n "$(header x-ratelimit-used "$WORK/g6.head")" ] && echo yes || echo no), x-ratelimit-resource=$(header x-ratelimit-resource "$WORK/g6.head")"
    if [ "$status" != 304 ]; then
        fail "G6: no 304 to count; not run"
        return 0
    fi
    rest rl rate_limit >/dev/null
    limit=$(field .resources.core.limit "$WORK/rl.body")
    a=$(rate_used)
    sleep 30
    b=$(rate_used)
    idle=$((b - a))
    report "G6: core limit=$limit; /rate_limit's core.used moved by $idle while the token was idle for 30 s (other clients)"
    for c in 1 2 3; do
        t0=$(date +%s)
        r0=$(rate_used)
        g6_phase plain 30
        line="30 plain reads (200: $PH_200, other: $PH_OTHER, x-ratelimit-used $PH_U)"
        r1=$(rate_used)
        g6_phase cond 30
        line="$line; 30 conditional reads (304: $PH_304, 200: $PH_200, other: $PH_OTHER, x-ratelimit-used $PH_U)"
        r2=$(rate_used)
        g6_phase plain 30
        line="$line; 30 plain reads (200: $PH_200, other: $PH_OTHER, x-ratelimit-used $PH_U)"
        r3=$(rate_used)
        d_u1=$((r1 - r0))
        d_c=$((r2 - r1))
        d_u2=$((r3 - r2))
        report "G6 cycle $c, $(($(date +%s) - t0)) s: /rate_limit's core.used moved by $d_u1, then $d_c over the conditional reads, then $d_u2; $line"
        if [ "$d_c" -lt "$min_c" ]; then min_c=$d_c; fi
        if [ "$d_u1" -lt "$min_u" ]; then min_u=$d_u1; fi
        if [ "$d_u2" -lt "$min_u" ]; then min_u=$d_u2; fi
        sum_u=$((sum_u + d_u1 + d_u2))
        sum_c=$((sum_c + d_c))
    done
    # Per cycle, the plain phases' mean minus the conditional phase: about 30 if a 304 is free, about 0 if it counts,
    # whatever the others spend at a steady rate. Times 6 to stay in integers: 6 plain phases, 3 conditional ones.
    gap=$(((sum_u - 2 * sum_c) / 6))
    report "G6: over the 3 cycles, a plain phase moved core.used by $((sum_u / 6)) on average, a conditional one by $((sum_c / 3)): a gap of $gap per 30 reads"
    if [ "$min_c" -le 3 ] && [ "$min_u" -ge 25 ]; then
        report "G6 result: a 304 is not counted: 30 of them moved core.used by $min_c in one cycle, while 30 plain reads never moved it by less than $min_u (other clients can only add)"
    elif [ "$min_c" -ge 25 ] && [ "$idle" -le 2 ]; then
        report "G6 result: a 304 is counted: 30 of them never moved core.used by less than $min_c, the budget otherwise quiet"
    elif [ "$gap" -ge 20 ]; then
        report "G6 result: a 304 is not counted, read from the gap: the others spent meanwhile"
    elif [ "$gap" -le 8 ]; then
        report "G6 result: a 304 is counted, read from the gap: the others spent meanwhile"
    else
        report "G6 result: unclear: see the cycles"
    fi

    if [ "$GH_OK" = yes ]; then
        a=$(rate_used)
        sleep 15
        b=$(rate_used)
        c=0
        while [ "$c" -lt 10 ]; do
            gh api "repos/$REPO" --silent </dev/null >/dev/null 2>&1
            c=$((c + 1))
        done
        r0=$(rate_used)
        report "G6: idle 15 s, others moved core.used by $((b - a)); then 10 reads through gh, the person's own login: the token's core.used moved by $((r0 - b))"
    fi
}

# shellcheck disable=SC2329 # run by the EXIT trap
cleanup() {
    local i n status
    if [ "${CLEANED:-}" = yes ]; then return 0; fi
    CLEANED=yes
    if [ -f "${WORK:-}/auth" ]; then
        section cleanup
        i=0
        while [ "$i" -lt "${#REL_IDS[@]}" ]; do
            status=$(gh_status rel -X DELETE "repos/$REPO/issues/$REL_N/dependencies/blocked_by/${REL_IDS[$i]}")
            report "cleanup: the person removes a relation of #$REL_N: HTTP $status"
            i=$((i + 1))
        done
        if [ -n "$OTHER_ISSUE" ]; then
            if gh issue close -R "$OTHER" "$OTHER_ISSUE" --reason "not planned" </dev/null >/dev/null 2>&1; then
                report "cleanup: OTHER's issue closed as not planned through gh: yes"
            else
                report "cleanup: OTHER's issue closed as not planned through gh: no"
            fi
        fi
        jq -n -c '{state: "closed", state_reason: "not_planned"}' >"$WORK/close.json"
        i=0
        while [ "$i" -lt "${#OPENED[@]}" ]; do
            n=${OPENED[$i]}
            status=$(rest close "repos/$REPO/issues/$n" PATCH "$WORK/close.json")
            report "cleanup: close #$n as not planned: HTTP $status"
            i=$((i + 1))
        done
        status=$(rest dellabel "repos/$REPO/labels/$LABEL" DELETE)
        report "cleanup: delete label $LABEL: HTTP $status"
        if [ "${OTHER_REMOVED:-no}" = yes ]; then
            say ""
            say "If another check needs it, add $OTHER back to the owl-shift app's installation (github.com/settings/installations > owl-shift > Configure > Repository access)."
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
if [ $# -ne 2 ]; then
    usage >&2
    exit 2
fi
REPO=$1
OTHER=$2
for r in "$REPO" "$OTHER"; do
    case $r in
    */*) ;;
    *)
        say "$PROGRAM: $r must have the form owner/name"
        exit 2
        ;;
    esac
done
OWNER=${REPO%%/*}
NAME=${REPO#*/}
for tool in curl jq; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        say "$PROGRAM: $tool is needed"
        exit 2
    fi
done
GH_OK=no
if command -v gh >/dev/null 2>&1; then GH_OK=yes; fi

WORK=$(mktemp -d "${TMPDIR:-/tmp}/owl547.XXXXXX") || exit 2
FAILED=0
ME_ID=0
G4_SKIP="gh is not installed"
G4_CONTROL=0
OTHER_REMOVED=no
trap cleanup EXIT
trap 'exit 130' INT TERM

report "OWL-547 checks, $(date -u +%Y-%m-%dT%H:%M:%SZ), $(curl --version | head -n 1 | cut -d' ' -f1-2), $(jq --version), $(if [ "$GH_OK" = yes ]; then gh --version | head -n 1 | cut -d' ' -f1-3; else echo "no gh"; fi), $(uname -s), script sha256 $(script_sha)"
section sign-in
sign_in || exit 1
if [ "$GH_OK" = no ]; then
    report "start: gh not installed: G4 and G3's gh path are not run"
fi
start || exit 1
g3_open
for off in 2 5 10 20; do g3_poll_at "$off"; done
g4
off=60
while [ "$off" -lt "$G3_WINDOW" ]; do
    g3_poll_at "$off"
    off=$((off * 2))
done
g3_poll_at "$G3_WINDOW"
g3_summary
g6
exit "$FAILED"
