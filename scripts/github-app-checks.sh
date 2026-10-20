#!/usr/bin/env bash
# Live checks C9 to C17 of docs/design/build-plan.md (OWL-336): what an
# Owlshift GitHub App's user token, signed in with the device flow and no
# client secret, can do. A person runs it at a browser and pastes its
# standard output on the ticket. `--help` is the checklist.
#
# Secrets (access tokens, refresh tokens, the device code) reach curl only
# through files written by write_secret_file, never as an argument, and the
# report shows them as <redacted>. GA5 removes this script once its ignored
# Rust live test lands.

set -u -o pipefail
umask 077

PROGRAM=${0##*/}
LOGIN=https://github.com
API=https://api.github.com
CHECKS="C10-denied C10-expired C13 C14 C15-none C15-elsewhere C15-pending C16-suspended C16-permissions C17"

usage() {
    cat <<EOF
Usage: $PROGRAM CLIENT_ID SLUG OWNER/REPO SHA BRANCH [--check ID] [--token-file FILE | --resume FILE]
       $PROGRAM --help

Live checks C9 to C17 of docs/design/build-plan.md (OWL-336) for a GitHub
App's user token, signed in with the device flow and no client secret.
Needs bash, curl and jq. Each run prints its report on standard output and
its instructions on standard error; paste each report on OWL-336.

  CLIENT_ID   the test app's client ID (not a secret)
  SLUG        the test app's slug, as in https://github.com/apps/SLUG
  OWNER/REPO  the scratch repository
  SHA         a commit of OWNER/REPO with a finished GitHub Actions run
  BRANCH      a branch of OWNER/REPO you pushed, with a commit that is not on
              the default branch and no open pull request

Once, before the first run:
  - Register a test GitHub App: Device Flow enabled; repository permissions
    Pull requests (write), Contents, Checks, Commit statuses, Actions and
    Metadata (read); no webhook. Note its client ID and its slug.
  - C11's prerequisites, which this script cannot create (Contents is read
    only): the app installed on OWNER/REPO; a workflow run on SHA, so that
    its check-runs answer is not empty; BRANCH pushed by you. A run without
    them exits non-zero.

Checklist, in order. "required" runs must all be posted; an "optional" run
needs a state you may not be able to set, and is recorded 'not observed'
when you leave it out.

  1. required  User-token expiration on (the app's default).
               $PROGRAM CLIENT_ID SLUG OWNER/REPO SHA BRANCH
               C9, C10 (pending, slow_down, token), C11, C12. Enter the code
               it shows. It opens a pull request from BRANCH and closes it.
  2. required  ... --check C10-denied
               On the device page, enter the code and press Cancel.
  3. optional  ... --check C10-expired
               Do not enter the code: it polls until the code expires
               (15 minutes), one countdown line a minute.
  4. required  ... --check C13
               Expiration still on. Enter the code. It prints expires_in,
               the interval step 6 must outlast, and whether the refresh
               grant is accepted without the client secret.
  5. required  Turn user-token expiration off in the app's settings
               (Optional features), then:
               ... --check C14 --token-file FILE
               Enter the code. The token is written to FILE, mode 600.
  6. required  After C13's interval (step 4's expires_in, 8 hours by
               default; set C13_INTERVAL to it in seconds otherwise):
               ... --check C14 --resume FILE
               Reads the user with FILE's token, then removes FILE.
  7. required  ... --check C15-none
               It asks you first to uninstall the app from OWNER/REPO's
               account (keep your authorization). It prints the install
               link and asks whether it opened the app's install page.
  8. required  ... --check C15-elsewhere
               It asks you first to install the app on another repository
               or account only, not on OWNER/REPO.
  9. optional  ... --check C15-pending
               OWNER/REPO is an organisation repository where you are not
               an owner: it asks you to request the installation, not yet
               approved.
 10. optional  ... --check C16-suspended
               The app installed on OWNER/REPO again. It signs in, then
               asks you to suspend the installation. Unsuspend it after.
 11. optional  ... --check C16-permissions
               It signs in, then asks you to add Issues (read) to the app's
               permissions without accepting it on the installation.
 12. optional  ... --check C17
               It signs in, then asks you to revoke the app's authorization.

Nothing outlives a run but step 5's FILE. If you never run step 6, delete
FILE and revoke the app's authorization (Settings > Applications >
Authorized GitHub Apps).
EOF
}

# The one place a secret is printed: into a file of mode 600, read by curl.
write_secret_file() {
    local file=$1 secret=$2
    rm -f "$file"
    (umask 077 && printf '%s\n' "$secret" >"$file")
    chmod 600 "$file"
}

say() { printf '%s\n' "$*" >&2; }
report() { printf '%s\n' "$*"; }
# Words about a secret, never the secret.
present() { if [ -n "$1" ]; then printf 'present\n'; else printf 'absent\n'; fi; }
differs() { if [ "$1" != "$2" ]; then printf 'yes\n'; else printf 'no\n'; fi; }
section() { printf '\n== %s ==\n' "$1"; }
fail() {
    FAILED=1
    report "$1"
}

# jq filter: the answer's shape, secrets replaced.
REDACT='def r: if type == "object" then with_entries(
    if (.key == "access_token" or .key == "refresh_token" or .key == "device_code" or .key == "token")
    then .value = "<redacted>" else .value |= r end)
  elif type == "array" then map(r) else . end; r'

shape() { jq -c "$REDACT" <"$1" 2>/dev/null || report "(not JSON)"; }
field() { jq -r "$1 // empty" <"$2" 2>/dev/null; }

# One header's value from a dump of response headers, the last one kept.
header() {
    grep -i "^$1:" "$2" 2>/dev/null | tail -n 1 | cut -d: -f2- | tr -d '\r' | sed 's/^ *//'
}

# call NAME METHOD URL [BODY_FILE] [AUTH_FILE]: the answer in $WORK/NAME.body,
# its headers in $WORK/NAME.head, the status printed.
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

# A REST read with the signed-in token.
rest() { call "$1" "${3:-GET}" "$API/$2" "${4:-}" "$WORK/auth"; }

# A GraphQL request with the signed-in token: NAME QUERY VARIABLES_JSON.
graphql() {
    jq -n -c --arg q "$2" --argjson v "$3" '{query: $q, variables: $v}' >"$WORK/$1.json"
    call "$1" POST "$API/graphql" "$WORK/$1.json" "$WORK/auth"
}

gql_errors() { jq -c '[.errors[]? | {type, message}]' <"$1" 2>/dev/null; }

# Rate and permission headers of an answer: values, never a secret.
headers_line() {
    local h=$WORK/$1.head
    printf 'x-ratelimit-limit=%s x-ratelimit-resource=%s x-accepted-github-permissions=%s\n' \
        "$(header x-ratelimit-limit "$h")" "$(header x-ratelimit-resource "$h")" \
        "$(header x-accepted-github-permissions "$h")"
}

use_token() { write_secret_file "$WORK/auth" "Authorization: Bearer $ACCESS_TOKEN"; }

# One poll of the device flow's token endpoint: POLL_STATUS and POLL_ERROR.
poll() {
    write_secret_file "$WORK/poll.form" \
        "client_id=$CLIENT_ID&device_code=$DEVICE_CODE&grant_type=urn:ietf:params:oauth:grant-type:device_code"
    POLL_STATUS=$(call poll POST "$LOGIN/login/oauth/access_token" "$WORK/poll.form")
    POLL_ERROR=$(field .error "$WORK/poll.body")
    POLL_COUNT=$((POLL_COUNT + 1))
    if [ -z "$POLL_ERROR" ] && [ -z "$(field .access_token "$WORK/poll.body")" ]; then
        POLL_ERROR="no token and no error (HTTP $POLL_STATUS)"
    fi
}

# Asks a device code; ID labels the report's lines.
device_code() {
    local id=$1 status
    printf 'client_id=%s\n' "$CLIENT_ID" >"$WORK/device.form"
    status=$(call device POST "$LOGIN/login/device/code" "$WORK/device.form")
    DEVICE_CODE=$(field .device_code "$WORK/device.body")
    INTERVAL=$(field .interval "$WORK/device.body")
    CODE_LIFE=$(field .expires_in "$WORK/device.body")
    report "$id: device code asked with the client ID alone, no client secret: HTTP $status"
    report "$id: answer: $(shape "$WORK/device.body")"
    if [ -z "$DEVICE_CODE" ]; then
        fail "$id: no device code: $(field .error "$WORK/device.body") $(field .error_description "$WORK/device.body")"
        return 1
    fi
    INTERVAL=${INTERVAL:-5}
    CODE_LIFE=${CODE_LIFE:-900}
    POLL_COUNT=0
    say ""
    say "Device page: $(field .verification_uri "$WORK/device.body")   code: $(field .user_code "$WORK/device.body")"
}

# Polls at the interval until an answer other than authorization_pending
# or slow_down; prints a countdown line a minute when COUNTDOWN is set.
poll_until_answer() {
    local id=$1 max=$((CODE_LIFE / INTERVAL + 5)) waited=0 minute=-1 left
    while [ "$POLL_COUNT" -lt "$max" ]; do
        sleep "$INTERVAL"
        waited=$((waited + INTERVAL))
        poll
        case $POLL_ERROR in
        authorization_pending) ;;
        slow_down)
            INTERVAL=$(field .interval "$WORK/poll.body")
            INTERVAL=${INTERVAL:-10}
            ;;
        *) return 0 ;;
        esac
        if [ -n "${COUNTDOWN:-}" ] && [ $((waited / 60)) -ne "$minute" ]; then
            minute=$((waited / 60))
            left=$((CODE_LIFE - waited))
            if [ "$left" -lt 0 ]; then left=0; fi
            say "$id: $left s left before the code expires"
        fi
    done
    POLL_ERROR="still pending after $POLL_COUNT polls"
}

# Keeps the token answer of the last poll or refresh (file FILE).
keep_tokens() {
    ACCESS_TOKEN=$(field .access_token "$1")
    REFRESH_TOKEN=$(field .refresh_token "$1")
    EXPIRES_IN=$(field .expires_in "$1")
    REFRESH_EXPIRES_IN=$(field .refresh_token_expires_in "$1")
}

token_line() {
    report "$1: token answer keys: $(jq -c 'keys' <"$2" 2>/dev/null)"
    report "$1: token_type=$(field .token_type "$2") scope='$(field .scope "$2")' expires_in=${EXPIRES_IN:-absent} refresh_token=$(present "$REFRESH_TOKEN") refresh_token_expires_in=${REFRESH_EXPIRES_IN:-absent}"
}

# A plain sign-in: a device code, then polls until the person enters it.
sign_in() {
    local id=$1
    device_code "$id" || return 1
    say "Waiting for the code to be entered..."
    poll_until_answer "$id"
    if [ -n "$POLL_ERROR" ]; then
        fail "$id: sign-in failed after $POLL_COUNT polls: $POLL_ERROR"
        return 1
    fi
    keep_tokens "$WORK/poll.body"
    use_token
    report "$id: signed in after $POLL_COUNT polls"
    token_line "$id" "$WORK/poll.body"
}

# The signed-in user's login, kept for comparisons and never printed.
read_user() {
    USER_STATUS=$(rest user user)
    USER_LOGIN=$(field .login "$WORK/user.body")
}

pause() {
    say ""
    say "$1"
    say "Press Enter when done."
    read -r _ || true
}

# The installation lookups of doctor (C12, and C15 to C16).
installations() {
    local id=$1 status count i inst listed rstatus
    status=$(rest inst user/installations)
    count=$(field '.installations | length' "$WORK/inst.body")
    report "$id: GET /user/installations: HTTP $status, total_count=$(field .total_count "$WORK/inst.body"); $(headers_line inst)"
    i=0
    while [ "$i" -lt "${count:-0}" ]; do
        jq -c ".installations[$i]" <"$WORK/inst.body" >"$WORK/one.json"
        inst=$(field .id "$WORK/one.json")
        report "$id: installation $inst: app_slug=$(field .app_slug "$WORK/one.json") (equals SLUG: $([ "$(field .app_slug "$WORK/one.json")" = "$SLUG" ] && echo yes || echo no)), account type=$(field .account.type "$WORK/one.json"), repository_selection=$(field .repository_selection "$WORK/one.json"), suspended_at=$(field .suspended_at "$WORK/one.json"), permissions=$(jq -c '.permissions // {}' <"$WORK/one.json")"
        rstatus=$(rest repos "user/installations/$inst/repositories?per_page=100")
        listed=$(jq -r --arg r "$REPO" '[.repositories[]?.full_name] | index($r) != null' <"$WORK/repos.body" 2>/dev/null)
        report "$id: GET /user/installations/$inst/repositories: HTTP $rstatus, total_count=$(field .total_count "$WORK/repos.body"), OWNER/REPO listed: ${listed:-unknown}"
        i=$((i + 1))
    done
}

# Reads of the repository a missing or suspended installation refuses.
repository_reads() {
    local id=$1 status
    status=$(graphql branch "$DEFAULT_BRANCH_QUERY" "$(jq -n -c --arg o "$OWNER" --arg n "$NAME" '{owner: $o, name: $n}')")
    report "$id: default branch (GraphQL): HTTP $status, name=$(field .data.repository.defaultBranchRef.name "$WORK/branch.body"), errors=$(gql_errors "$WORK/branch.body")"
    status=$(rest repo "repos/$REPO")
    report "$id: GET /repos/OWNER/REPO: HTTP $status, message=$(field .message "$WORK/repo.body"); $(headers_line repo)"
    status=$(rest runs "repos/$REPO/commits/$SHA/check-runs?per_page=1")
    report "$id: GET check-runs of SHA: HTTP $status, message=$(field .message "$WORK/runs.body")"
}

install_link() { printf 'https://github.com/apps/%s/installations/new\n' "$SLUG"; }

# The adapter's queries (crates/owlshift-adapters/src/forge/github.rs), whose
# `$` are GraphQL's.
# shellcheck disable=SC2016
DEFAULT_BRANCH_QUERY='query DefaultBranch($owner: String!, $name: String!) { repository(owner: $owner, name: $name) { defaultBranchRef { name target { oid } } } }'
# shellcheck disable=SC2016
REPOSITORY_ID_QUERY='query RepositoryId($owner: String!, $name: String!) { repository(owner: $owner, name: $name) { id } }'
# shellcheck disable=SC2016
CREATE_MUTATION='mutation OpenPullRequest($input: CreatePullRequestInput!) { createPullRequest(input: $input) { pullRequest { number url state headRefOid } } }'
# shellcheck disable=SC2016
FOLLOW_QUERY='query Follow($owner: String!, $name: String!, $number: Int!, $first: Int!, $after: String) { rateLimit { cost remaining resetAt } repository(owner: $owner, name: $name) { pullRequest(number: $number) { state headRefOid mergeable mergeStateStatus commits(last: 1) { nodes { commit { oid statusCheckRollup { contexts(first: $first, after: $after) { totalCount pageInfo { hasNextPage endCursor } nodes { __typename ... on CheckRun { databaseId name status conclusion isRequired(pullRequestNumber: $number) detailsUrl checkSuite { app { slug } workflowRun { workflow { name } } } } ... on StatusContext { context state description isRequired(pullRequestNumber: $number) targetUrl } } } } } } } } } }'
# shellcheck disable=SC2016
REVIEWS_QUERY='query Reviews($owner: String!, $name: String!, $number: Int!) { rateLimit { cost remaining resetAt } repository(owner: $owner, name: $name) { pullRequest(number: $number) { reviews(last: 100, states: [COMMENTED, APPROVED, CHANGES_REQUESTED, DISMISSED]) { nodes { fullDatabaseId author { login } state submittedAt body commit { oid } comments(first: 100) { nodes { path line body } } } } } } }'

# C9, C10's success path, C11 and C12: the run with no flag.
run_default() {
    local status

    section C9
    device_code C9 || return 0

    section C10
    poll
    report "C10: poll before the code is entered: HTTP $POLL_STATUS, error=$POLL_ERROR"
    poll
    report "C10: second poll inside the interval: HTTP $POLL_STATUS, error=$POLL_ERROR, interval now $(field .interval "$WORK/poll.body")"
    if [ "$POLL_ERROR" = slow_down ]; then
        INTERVAL=$(field .interval "$WORK/poll.body")
        INTERVAL=${INTERVAL:-10}
    fi
    say "Waiting for the code to be entered..."
    poll_until_answer C10
    if [ -n "$POLL_ERROR" ]; then
        fail "C10: no token after $POLL_COUNT polls: $POLL_ERROR"
        return 0
    fi
    keep_tokens "$WORK/poll.body"
    use_token
    report "C10: token after $POLL_COUNT polls"
    token_line C10 "$WORK/poll.body"

    section C11
    c11
    section C12
    installations C12
}

c11() {
    local status count run_id job_id branch repo_id number location vars
    status=$(rest checkruns "repos/$REPO/commits/$SHA/check-runs?per_page=100")
    count=$(field '.check_runs | length' "$WORK/checkruns.body")
    report "C11: GET commits/SHA/check-runs: HTTP $status, total_count=$(field .total_count "$WORK/checkruns.body"); $(headers_line checkruns)"
    if [ "$status" = 200 ] && [ "${count:-0}" -eq 0 ]; then
        fail "C11: no check run on $SHA, not run"
        return 0
    fi
    run_id=$(field '.check_runs[0].id' "$WORK/checkruns.body")
    job_id=$(field '[.check_runs[]? | select(.app.slug == "github-actions")][0].id' "$WORK/checkruns.body")
    if [ "$status" = 200 ] && [ -z "$job_id" ]; then
        fail "C11: no GitHub Actions check run on $SHA: the job log is not read"
    fi

    read_user
    report "C11: GET /user: HTTP $USER_STATUS; $(headers_line user)"

    vars=$(jq -n -c --arg o "$OWNER" --arg n "$NAME" '{owner: $o, name: $n}')
    status=$(graphql branch "$DEFAULT_BRANCH_QUERY" "$vars")
    branch=$(field .data.repository.defaultBranchRef.name "$WORK/branch.body")
    report "C11: default branch (GraphQL, GitHubForge::default_branch): HTTP $status, name found: $([ -n "$branch" ] && echo yes || echo no), errors=$(gql_errors "$WORK/branch.body"); $(headers_line branch)"

    status=$(graphql repoid "$REPOSITORY_ID_QUERY" "$vars")
    repo_id=$(field .data.repository.id "$WORK/repoid.body")
    report "C11: repository id (GraphQL): HTTP $status, errors=$(gql_errors "$WORK/repoid.body")"

    number=
    if [ -n "$repo_id" ] && [ -n "$branch" ]; then
        status=$(graphql create "$CREATE_MUTATION" "$(jq -n -c --arg r "$repo_id" --arg h "$BRANCH" --arg b "$branch" \
            '{input: {repositoryId: $r, headRefName: $h, baseRefName: $b, title: "Owlshift live check C11 (closed by the script)", body: "Opened by scripts/github-app-checks.sh (OWL-336) and closed at once.", draft: false}}')")
        number=$(field .data.createPullRequest.pullRequest.number "$WORK/create.body")
        report "C11: createPullRequest: HTTP $status, opened: $([ -n "$number" ] && echo "yes, #$number" || echo no), errors=$(gql_errors "$WORK/create.body"); $(headers_line create)"
    else
        fail "C11: createPullRequest not run: no repository id or default branch"
    fi
    if [ -n "$number" ]; then
        PR_NUMBER=$number
        status=$(rest issue "repos/$REPO/issues/$number")
        report "C11: GET issues/N: HTTP $status, author is the signed-in user: $([ -n "${USER_LOGIN:-}" ] && [ "$(field .user.login "$WORK/issue.body")" = "$USER_LOGIN" ] && echo yes || echo no), performed_via_github_app=$(field .performed_via_github_app.slug "$WORK/issue.body")"

        status=$(graphql follow "$FOLLOW_QUERY" "$(jq -n -c --arg o "$OWNER" --arg n "$NAME" --argjson p "$number" '{owner: $o, name: $n, number: $p, first: 100}')")
        report "C11: pull request read with statusCheckRollup (GraphQL, watch's Follow): HTTP $status, state=$(field .data.repository.pullRequest.state "$WORK/follow.body"), contexts totalCount=$(field '.data.repository.pullRequest.commits.nodes[0].commit.statusCheckRollup.contexts.totalCount' "$WORK/follow.body"), cost=$(field .data.rateLimit.cost "$WORK/follow.body"), errors=$(gql_errors "$WORK/follow.body"); $(headers_line follow)"

        status=$(graphql reviews "$REVIEWS_QUERY" "$(jq -n -c --arg o "$OWNER" --arg n "$NAME" --argjson p "$number" '{owner: $o, name: $n, number: $p}')")
        report "C11: reviews (GraphQL): HTTP $status, reviews=$(field '.data.repository.pullRequest.reviews.nodes | length' "$WORK/reviews.body"), errors=$(gql_errors "$WORK/reviews.body")"
        status=$(rest rreviews "repos/$REPO/pulls/$number/reviews")
        report "C11: GET pulls/N/reviews: HTTP $status; $(headers_line rreviews)"
        status=$(rest icomments "repos/$REPO/issues/$number/comments")
        report "C11: GET issues/N/comments: HTTP $status; $(headers_line icomments)"
        status=$(rest pcomments "repos/$REPO/pulls/$number/comments")
        report "C11: GET pulls/N/comments: HTTP $status; $(headers_line pcomments)"
    else
        report "C11: pull request reads not run: no pull request"
    fi

    if [ -n "$run_id" ]; then
        status=$(rest checkrun "repos/$REPO/check-runs/$run_id")
        report "C11: GET check-runs/ID (ID from the commit's check-runs): HTTP $status, output keys=$(jq -c '.output // {} | keys' <"$WORK/checkrun.body" 2>/dev/null); $(headers_line checkrun)"
    fi
    if [ -n "$job_id" ]; then
        status=$(rest joblog "repos/$REPO/actions/jobs/$job_id/logs")
        location=$(header location "$WORK/joblog.head")
        report "C11: GET actions/jobs/ID/logs (ID of a github-actions check run): HTTP $status, redirect host=$(printf '%s\n' "$location" | sed -E 's#^https?://([^/]+).*#\1#'); $(headers_line joblog)"
        if [ -n "$location" ]; then
            status=$(call logbody GET "$location")
            report "C11: the log, read without the token: HTTP $status, $(wc -c <"$WORK/logbody.body" | tr -d ' ') bytes"
        fi
    fi

    status=$(rest status "repos/$REPO/commits/$SHA/status")
    report "C11: extra read, not the adapter's: GET commits/SHA/status: HTTP $status, state=$(field .state "$WORK/status.body"); $(headers_line status)"

    close_pull_request
}

close_pull_request() {
    local status
    if [ -z "${PR_NUMBER:-}" ]; then return 0; fi
    printf '{"state":"closed"}\n' >"$WORK/close.json"
    status=$(rest close "repos/$REPO/pulls/$PR_NUMBER" PATCH "$WORK/close.json")
    report "C11: closing the pull request: HTTP $status, state=$(field .state "$WORK/close.body")"
    PR_NUMBER=
}

# A refresh grant with the client ID alone: NAME and the refresh token.
refresh() {
    write_secret_file "$WORK/$1.form" "client_id=$CLIENT_ID&grant_type=refresh_token&refresh_token=$2"
    call "$1" POST "$LOGIN/login/oauth/access_token" "$WORK/$1.form"
}

refresh_answer() {
    if [ -n "$(field .access_token "$WORK/$1.body")" ]; then
        printf 'accepted\n'
    else
        printf 'refused (error=%s: %s)\n' "$(field .error "$WORK/$1.body")" "$(field .error_description "$WORK/$1.body")"
    fi
}

run_c13() {
    local status first_access_token first_refresh_token answer
    section C13
    sign_in C13 || return 0
    report "C13: expires_in=${EXPIRES_IN:-absent}: the interval C14's resume run must outlast"
    if [ -z "$REFRESH_TOKEN" ]; then
        fail "C13: no refresh token: user-token expiration is off on the app; turn it on and run again"
        return 0
    fi
    first_access_token=$ACCESS_TOKEN
    first_refresh_token=$REFRESH_TOKEN

    status=$(refresh refresh1 "$first_refresh_token")
    answer=$(refresh_answer refresh1)
    report "C13: refresh grant without the client secret: ${answer%% *}"
    report "C13: refresh answer: HTTP $status, $answer, shape $(shape "$WORK/refresh1.body")"
    if [ "${answer%% *}" != accepted ]; then
        report "C13: rotation: not run, the refresh grant was refused"
        report "C13: reused refresh token: not run, the refresh grant was refused"
        report "C13: two refreshes in parallel: not run, the refresh grant was refused"
        return 0
    fi
    keep_tokens "$WORK/refresh1.body"
    report "C13: rotation: new access token $(differs "$ACCESS_TOKEN" "$first_access_token"), new refresh token $(differs "$REFRESH_TOKEN" "$first_refresh_token"), expires_in=${EXPIRES_IN:-absent}, refresh_token_expires_in=${REFRESH_EXPIRES_IN:-absent}"
    use_token
    status=$(rest user1 user)
    report "C13: the new access token reads the user: HTTP $status"
    write_secret_file "$WORK/old.auth" "Authorization: Bearer $first_access_token"
    status=$(call olduser GET "$API/user" "" "$WORK/old.auth")
    report "C13: the first access token after the refresh: HTTP $status"

    refresh para1 "$REFRESH_TOKEN" >"$WORK/para1.status" &
    refresh para2 "$REFRESH_TOKEN" >"$WORK/para2.status" &
    wait
    report "C13: two refreshes in parallel with the same refresh token: first HTTP $(cat "$WORK/para1.status") $(refresh_answer para1); second HTTP $(cat "$WORK/para2.status") $(refresh_answer para2)"

    status=$(refresh reused "$first_refresh_token")
    report "C13: reused refresh token (the first one, already used): HTTP $status, $(refresh_answer reused)"
}

run_c14() {
    local status age interval=${C13_INTERVAL:-28800} mtime
    section C14
    if [ -n "$RESUME" ]; then
        if [ ! -f "$RESUME" ]; then
            fail "C14: no token file at $RESUME"
            return 0
        fi
        if mtime=$(stat -c %Y "$RESUME" 2>/dev/null); then :; else mtime=$(stat -f %m "$RESUME"); fi
        age=$(($(date +%s) - mtime))
        IFS= read -r ACCESS_TOKEN <"$RESUME" || true
        use_token
        status=$(call user GET "$API/user" "" "$WORK/auth")
        report "C14: token age $age s (from the file's mtime), C13's interval $interval s"
        case $status in
        200)
            if [ "$age" -gt "$interval" ]; then
                report "C14: valid at $age s, past C13's interval of $interval s"
            else
                report "C14: valid at $age s, not observed past $interval s"
            fi
            ;;
        401) report "C14: refused (401) at $age s" ;;
        *) report "C14: HTTP $status at $age s, message=$(field .message "$WORK/user.body")" ;;
        esac
        rm -f "$RESUME"
        report "C14: token file removed"
        return 0
    fi
    sign_in C14 || return 0
    if [ -n "$EXPIRES_IN" ]; then
        fail "C14: expires_in=$EXPIRES_IN came back: expiration is still on; turn it off and run again"
        return 0
    fi
    report "C14: no expires_in came back: expiration is off"
    write_secret_file "$TOKEN_FILE" "$ACCESS_TOKEN"
    report "C14: token written to $TOKEN_FILE, mode $(stat -c %a "$TOKEN_FILE" 2>/dev/null || stat -f %Lp "$TOKEN_FILE")"
    say ""
    say "Run '$PROGRAM ... --check C14 --resume $TOKEN_FILE' after C13's interval."
    say "If you never do, delete $TOKEN_FILE and revoke the app's authorization."
}

run_c10() {
    section C10
    device_code C10 || return 0
    if [ "$CHECK" = C10-denied ]; then
        say "On the device page, enter the code, then press Cancel."
        poll_until_answer C10
        report "C10: denied: after $POLL_COUNT polls, HTTP $POLL_STATUS, error=$POLL_ERROR"
    else
        say "Do not enter the code: this waits for it to expire ($CODE_LIFE s)."
        COUNTDOWN=1 poll_until_answer C10
        report "C10: expired: after $POLL_COUNT polls, HTTP $POLL_STATUS, error=$POLL_ERROR"
    fi
}

run_c15() {
    local answer
    section C15
    case $CHECK in
    C15-none) pause "Uninstall the app from the account of $REPO (Settings > Applications > Installed GitHub Apps > Configure > Uninstall). Keep your authorization of it." ;;
    C15-elsewhere) pause "Install the app on another repository or account only, not on $REPO (Configure > Only select repositories)." ;;
    C15-pending)
        pause "$REPO is an organisation repository where you are not an owner. Open $(install_link) and request the installation on it. No owner approves it yet."
        ;;
    esac
    sign_in "C15 ($CHECK)" || return 0
    installations "C15 ($CHECK)"
    repository_reads "C15 ($CHECK)"
    report "C15 ($CHECK): install link: $(install_link)"
    if [ "$CHECK" = C15-none ]; then
        say ""
        say "Open $(install_link) in your browser."
        say "Did it open the app's install page? (y/n)"
        read -r answer || answer=
        case $answer in
        y | Y | yes) answer=yes ;;
        n | N | no) answer=no ;;
        *) answer="no answer" ;;
        esac
        report "C15 ($CHECK): the install link opened the app's install page: $answer"
    fi
}

run_c16() {
    local status
    section C16
    sign_in "C16 ($CHECK)" || return 0
    if [ "$CHECK" = C16-suspended ]; then
        pause "Suspend the app's installation on $REPO (Settings > Applications > Installed GitHub Apps > Configure > Suspend)."
        installations "C16 ($CHECK)"
        repository_reads "C16 ($CHECK)"
        say "Unsuspend the installation now."
    else
        pause "In the app's settings, add the repository permission Issues (read) and save. Do not accept it on the installation."
        installations "C16 ($CHECK)"
        repository_reads "C16 ($CHECK)"
        status=$(rest issues "repos/$REPO/issues?per_page=1")
        report "C16 ($CHECK): GET issues, the permission not accepted: HTTP $status, message=$(field .message "$WORK/issues.body"); $(headers_line issues)"
        say "Remove the Issues permission from the app now, or accept it."
    fi
}

run_c17() {
    local status
    section C17
    sign_in C17 || return 0
    pause "Revoke the app's authorization (Settings > Applications > Authorized GitHub Apps > Revoke)."
    status=$(rest user user)
    report "C17: GET /user after the revocation: HTTP $status, message=$(field .message "$WORK/user.body")"
    status=$(graphql branch "$DEFAULT_BRANCH_QUERY" "$(jq -n -c --arg o "$OWNER" --arg n "$NAME" '{owner: $o, name: $n}')")
    report "C17: default branch (GraphQL) after the revocation: HTTP $status, errors=$(gql_errors "$WORK/branch.body")"
    if [ -n "$REFRESH_TOKEN" ]; then
        status=$(refresh revoked "$REFRESH_TOKEN")
        report "C17: refresh after the revocation: HTTP $status, $(refresh_answer revoked)"
    else
        report "C17: refresh after the revocation: not run, no refresh token"
    fi
}

# shellcheck disable=SC2329 # run by the EXIT trap
cleanup() {
    if [ -n "${PR_NUMBER:-}" ] && [ -f "$WORK/auth" ]; then
        close_pull_request
    fi
    rm -rf "$WORK"
}

# Arguments.
if [ $# -eq 0 ]; then
    usage >&2
    exit 2
fi
for arg in "$@"; do
    case $arg in
    -h | --help)
        usage
        exit 0
        ;;
    esac
done
POSITIONAL=()
CHECK=
TOKEN_FILE=
RESUME=
while [ $# -gt 0 ]; do
    case $1 in
    --check | --token-file | --resume)
        if [ $# -lt 2 ]; then
            say "$PROGRAM: $1 needs a value"
            exit 2
        fi
        case $1 in
        --check) CHECK=$2 ;;
        --token-file) TOKEN_FILE=$2 ;;
        --resume) RESUME=$2 ;;
        esac
        shift 2
        ;;
    -*)
        say "$PROGRAM: unknown option $1"
        exit 2
        ;;
    *)
        POSITIONAL+=("$1")
        shift
        ;;
    esac
done
if [ "${#POSITIONAL[@]}" -ne 5 ]; then
    say "$PROGRAM: expected CLIENT_ID SLUG OWNER/REPO SHA BRANCH, got ${#POSITIONAL[@]} arguments"
    usage >&2
    exit 2
fi
CLIENT_ID=${POSITIONAL[0]}
SLUG=${POSITIONAL[1]}
REPO=${POSITIONAL[2]}
SHA=${POSITIONAL[3]}
BRANCH=${POSITIONAL[4]}
case $REPO in
*/*) ;;
*)
    say "$PROGRAM: OWNER/REPO must have the form owner/name"
    exit 2
    ;;
esac
OWNER=${REPO%%/*}
NAME=${REPO#*/}
if [ -n "$CHECK" ]; then
    case " $CHECKS " in
    *" $CHECK "*) ;;
    *)
        say "$PROGRAM: unknown check $CHECK (one of: $CHECKS)"
        exit 2
        ;;
    esac
fi
if [ "$CHECK" = C14 ]; then
    if { [ -n "$TOKEN_FILE" ] && [ -n "$RESUME" ]; } || { [ -z "$TOKEN_FILE" ] && [ -z "$RESUME" ]; }; then
        say "$PROGRAM: --check C14 needs --token-file FILE or --resume FILE"
        exit 2
    fi
elif [ -n "$TOKEN_FILE" ] || [ -n "$RESUME" ]; then
    say "$PROGRAM: --token-file and --resume go with --check C14 only"
    exit 2
fi
for tool in curl jq; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        say "$PROGRAM: $tool is needed"
        exit 2
    fi
done

WORK=$(mktemp -d "${TMPDIR:-/tmp}/owlshift-app-checks.XXXXXX") || exit 2
trap cleanup EXIT
trap 'exit 130' INT TERM
FAILED=0
PR_NUMBER=
ACCESS_TOKEN=
REFRESH_TOKEN=
DEVICE_CODE=
EXPIRES_IN=
REFRESH_EXPIRES_IN=

report "github-app-checks.sh, $(date -u +%Y-%m-%dT%H:%MZ), check: ${CHECK:-C9 to C12}"
report "$(curl --version | head -n 1 | cut -d' ' -f1-2), $(jq --version), $(uname -s)"

case $CHECK in
'') run_default ;;
C10-*) run_c10 ;;
C13) run_c13 ;;
C14) run_c14 ;;
C15-*) run_c15 ;;
C16-*) run_c16 ;;
C17) run_c17 ;;
esac

exit "$FAILED"
