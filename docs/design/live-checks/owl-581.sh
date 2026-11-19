#!/usr/bin/env bash
# OWL-581 live checks T1 to T3, quoted under "Live checks" of docs/design/build-plan.md, "P10 · Teams": what GitHub
# does with the refs `refs/owlshift/claims/<ticket>` a runner's claims are, pushed with a person's own git access, over
# SSH and over HTTPS with the credential helper git is configured with.
#   T1 An update and a delete with the commit last written as the expected value (`--force-with-lease=<ref>:<oid>`):
#      accepted when it matches, refused when another commit was pushed in between. One `git push --atomic` of five
#      refs, one of them stale, writes none: once with git's own view stale (git refuses before sending), then in
#      racing rounds held at a barrier until every runner has read the remote's refs, so that the server refuses. A
#      ref pointing to a commit without a parent is accepted.
#   T2 A fetch with `+refs/owlshift/claims/*:refs/remotes/owlshift/claims/*` brings the claims and, with `--prune`,
#      drops a deleted one and nothing outside that destination; a plain clone and a plain fetch bring none; the time
#      a fetch and an `ls-remote` take over 100 claims.
#   T3 A push, an update and a delete under `refs/owlshift/claims/` start no workflow run on a repository whose
#      workflow runs `on: push` with no filter: a claim's commit holds no workflow, so the deciding refs point to
#      commits without a parent whose tree holds the workflow, beside two controls that must start a run (the
#      workflow's own commit on the default branch, a branch at that commit). Then, the repository public for that
#      part alone (rulesets need GitHub Pro on a private one), no ruleset restricting the creation, update and
#      deletion of every branch and tag refuses them, the ruleset shown live by a branch and a tag it refuses; and
#      what GitHub's public pages show of the ref.
#
#   bash docs/design/live-checks/owl-581.sh OWNER/REPO            (--help; --restore OWNER/REPO after a killed run)
#
# Needs bash (3.2 is enough), git, gh signed in as the repository's admin with the `repo` and `workflow` scopes, jq,
# curl, base64 and perl (a millisecond clock: bash 3.2 and macOS's date have none). About 10 minutes. Every ref it
# writes is under `refs/owlshift/claims/PROBE581-<run>-`, or `refs/heads/owl581-probe-<run>` and
# `refs/tags/owl581-probe-<run>` for T3's controls, and is deleted at the end; T3 adds then removes one workflow file
# on the default branch, deletes the runs it started, creates then deletes two rulesets and makes the repository
# public, then private again, the EXIT trap included. It refuses pitchopp/owlshift. The report, on standard output,
# holds no token and no account identifier: paste it on the ticket.

set -u -o pipefail
umask 077

PROGRAM=${0##*/}
FORBIDDEN=pitchopp/owlshift
CLAIMS=refs/owlshift/claims
WF_PATH=.github/workflows/owl581-probe.yml
# T1's racing rounds: runners per round, rounds.
RACERS=4
ROUNDS=2

usage() {
    cat <<EOF
Usage: bash $PROGRAM OWNER/REPO
       bash $PROGRAM --restore OWNER/REPO

  OWNER/REPO  the sandbox, private, that gh's account administers:
              pitchopp/owlshift-live-sandbox. Never $FORBIDDEN.

OWL581_CHECKS="T1 T2 T3" (the default) runs those checks, in that order.
--restore     after a run killed beyond its trap: deletes the owl581-probe
              rulesets (readable only while the repository is public), makes
              it private, deletes every PROBE581 claim and owl581-probe ref,
              removes the probe workflow file.
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
# bool FILTER FILE: a boolean as true or false (field's `//` would drop a false), empty when absent.
bool() { jq -r "$1 | if . == null then \"\" else tostring end" <"$2" 2>/dev/null; }
yes_no() { if [ "$1" = "$2" ]; then echo yes; else echo no; fi; }
now_ms() { perl -MTime::HiRes=time -e 'printf "%d\n", time() * 1000'; }

# expect LABEL GOT WANTED: a failure, named, when GOT is not WANTED.
expect() {
    if [ "$2" != "$3" ]; then fail "$1: FAILED: got '$2', expected '$3'"; fi
}

# rest NAME PATH [METHOD] [BODY_FILE]: `gh api`; the answer in $WORK/NAME.body, the status printed.
rest() {
    local name=$1 path=$2 method=${3:-GET} body=${4:-}
    local args=(api -i -X "$method" -H 'X-GitHub-Api-Version: 2022-11-28' "$path")
    if [ -n "$body" ]; then args+=(--input "$body"); fi
    gh "${args[@]}" </dev/null >"$WORK/$name.raw" 2>"$WORK/$name.err" || true
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

lg() { git -C "$LOCAL" "$@"; }

# claim_commit LABEL: a commit without a parent whose tree holds a claim.json of format 2, its fields made up; the
# commit printed. Each is distinct: its time and label differ.
claim_commit() {
    local blob tree runner now
    runner=$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')
    now=$(date -u +%Y-%m-%dT%H:%M:%SZ)
    blob=$(jq -n -c --arg r "$runner" --arg n "$now" --arg l "$1" \
        '{format: 2, runner: $r, operator: "probe", claimed_at: $n, expires_at: $n, held: [], probe: $l}' |
        lg hash-object -w --stdin) || return 1
    tree=$(printf '100644 blob %s\tclaim.json\n' "$blob" | lg mktree) || return 1
    lg commit-tree "$tree" -m "OWL-581 probe claim $1 $(now_ms)"
}

# push URL ARGS...: `git push --porcelain URL ARGS...`, its output in $WORK/push.out; its exit status returned.
push() {
    local url=$1
    shift
    lg push --porcelain "$url" "$@" >"$WORK/push.out" 2>&1
}

# flag REF [FILE]: the porcelain flag git printed for REF (`*` new, `+` forced, `-` deleted, `!` rejected), and its
# summary; "none" when git printed no line for it. Every commit here is parentless, so no update is a fast-forward and
# no flag is blank.
flag() {
    awk -F '\t' -v r="$1" 'NF >= 3 { n = split($2, a, ":"); if (a[n] == r) { print $1 " " $3; found = 1; exit } }
        END { if (!found) print "none" }' "${2:-$WORK/push.out}"
}
flag_char() { flag "$@" | cut -c1; }

# remote_oid URL REF: the commit REF holds on the remote, empty when absent.
remote_oid() { lg ls-remote "$1" "$2" 2>/dev/null | awk -v r="$2" '$2 == r { print $1 }'; }

# run_refs URL: this run's claims and controls on the remote, as "oid ref" lines.
run_refs() {
    lg ls-remote "$1" "$P-*" "refs/heads/owl581-probe-$RUN*" "refs/tags/owl581-probe-$RUN*" 2>/dev/null
}

# delete_refs URL FILE: deletes the refs listed in FILE ("oid ref" lines): one atomic push with each oid as the
# expected value, else one plain delete a ref; prints how many are left.
delete_refs() {
    local url=$1 list=$2 args=() oid ref
    if [ ! -s "$list" ]; then
        echo 0
        return 0
    fi
    while read -r oid ref; do args+=("--force-with-lease=$ref:$oid" ":$ref"); done <"$list"
    if ! push "$url" --atomic "${args[@]}"; then
        while read -r oid ref; do push "$url" ":$ref" || true; done <"$list"
    fi
    run_refs "$url" | wc -l | tr -d ' '
}

# ---------------------------------------------------------------------------------------------------------------- T1

t1() {
    local tr=$1 url=$2 r c1 c2 c3 c4 st i oids=() news=() args=() stale
    section "T1 · $tr"
    r="$P-t1-$tr"
    if ! { c1=$(claim_commit "t1-$tr-1") && c2=$(claim_commit "t1-$tr-2") && c3=$(claim_commit "t1-$tr-3") &&
        c4=$(claim_commit "t1-$tr-4"); }; then
        fail "T1 $tr: FAILED: no local commit"
        return 1
    fi
    push "$url" "--force-with-lease=$r:" "$c1:$r"
    st=$?
    report "T1 $tr take, expected absent, a commit without a parent: exit $st, flag $(flag "$r"); remote holds it: $(yes_no "$(remote_oid "$url" "$r")" "$c1")"
    expect "T1 $tr take" "$st $(flag_char "$r")" "0 *"
    push "$url" "--force-with-lease=$r:$c1" "$c2:$r"
    st=$?
    report "T1 $tr renew, expected the commit last written: exit $st, flag $(flag "$r"); remote holds the new one: $(yes_no "$(remote_oid "$url" "$r")" "$c2")"
    expect "T1 $tr renew" "$st $(flag_char "$r")" "0 +"
    push "$url" "--force-with-lease=$r:$c2" "$c3:$r"
    st=$?
    report "T1 $tr another runner writes in between: exit $st, flag $(flag "$r")"
    push "$url" "--force-with-lease=$r:$c2" "$c4:$r"
    st=$?
    report "T1 $tr renew, its expected value stale: exit $st, flag $(flag "$r"); remote unchanged: $(yes_no "$(remote_oid "$url" "$r")" "$c3")"
    expect "T1 $tr stale renew" "$(flag_char "$r") $(remote_oid "$url" "$r")" "! $c3"
    push "$url" "--force-with-lease=$r:$c2" ":$r"
    st=$?
    report "T1 $tr delete, its expected value stale: exit $st, flag $(flag "$r"); remote unchanged: $(yes_no "$(remote_oid "$url" "$r")" "$c3")"
    expect "T1 $tr stale delete" "$(flag_char "$r") $(remote_oid "$url" "$r")" "! $c3"
    push "$url" "--force-with-lease=$r:$c3" ":$r"
    st=$?
    report "T1 $tr delete, expected the commit last written: exit $st, flag $(flag "$r"); remote absent: $(yes_no "$(remote_oid "$url" "$r")" "")"
    expect "T1 $tr delete" "$st $(flag_char "$r") $(remote_oid "$url" "$r")" "0 - "

    # Five refs taken in one atomic push, then renewed in one atomic push whose third expected value is stale.
    for i in 1 2 3 4 5; do
        oids[i]=$(claim_commit "t1-$tr-a$i-old")
        args+=("--force-with-lease=$P-t1-$tr-a$i:" "${oids[i]}:$P-t1-$tr-a$i")
    done
    push "$url" --atomic "${args[@]}"
    st=$?
    report "T1 $tr take five in one atomic push: exit $st, flags $(for i in 1 2 3 4 5; do flag_char "$P-t1-$tr-a$i"; done | tr -d '\n')"
    expect "T1 $tr atomic take" "$st" 0
    stale=$(claim_commit "t1-$tr-a3-other")
    push "$url" "--force-with-lease=$P-t1-$tr-a3:${oids[3]}" "$stale:$P-t1-$tr-a3"
    report "T1 $tr another runner writes the third in between: flag $(flag "$P-t1-$tr-a3")"
    args=()
    for i in 1 2 3 4 5; do
        news[i]=$(claim_commit "t1-$tr-a$i-new")
        args+=("--force-with-lease=$P-t1-$tr-a$i:${oids[i]}" "${news[i]}:$P-t1-$tr-a$i")
    done
    push "$url" --atomic "${args[@]}"
    st=$?
    report "T1 $tr renew five in one atomic push, the third stale: exit $st; $(for i in 1 2 3 4 5; do printf 'a%s %s; ' "$i" "$(flag "$P-t1-$tr-a$i")"; done)"
    local kept=0
    for i in 1 2 4 5; do [ "$(remote_oid "$url" "$P-t1-$tr-a$i")" = "${oids[i]}" ] && kept=$((kept + 1)); done
    report "T1 $tr after it: the four others unchanged: $kept of 4; the third the other runner's: $(yes_no "$(remote_oid "$url" "$P-t1-$tr-a3")" "$stale")"
    expect "T1 $tr atomic renew, one stale, writes none" "$st $kept" "1 4"
    if grep -q 'stale info' "$WORK/push.out"; then
        report "T1 $tr that refusal came from git itself, its view of the remote stale (\"stale info\"): nothing was sent; the server's own refusal is the race below"
    fi
    t1_race "$tr" "$url"
    lg ls-remote "$url" "$P-t1-$tr-*" >"$WORK/t1-left"
    report "T1 $tr delete this check's refs: left $(delete_refs "$url" "$WORK/t1-left") of this run's"
}

# t1_race TRANSPORT URL: ROUNDS rounds of RACERS concurrent atomic renewals of five refs from the same expected
# values, each runner held by a pre-push hook until every runner has read the remote's refs, so that git's own check
# passes for all and the server decides.
t1_race() {
    local tr=$1 url=$2 i k round st win server client other mixed b
    local base=() args=()
    for i in 1 2 3 4 5; do
        base[i]=$(claim_commit "t1-$tr-b$i")
        args+=("--force-with-lease=$P-t1-$tr-b$i:" "${base[i]}:$P-t1-$tr-b$i")
    done
    push "$url" --atomic "${args[@]}" || {
        fail "T1 $tr race: FAILED: the five refs were not taken"
        return 1
    }
    mkdir -p "$WORK/hooks"
    cat >"$WORK/hooks/pre-push" <<'EOF'
#!/bin/sh
: >"$OWL581_BARRIER/$$"
i=0
while [ "$(ls "$OWL581_BARRIER" | wc -l)" -lt "$OWL581_RACERS" ] && [ "$i" -lt 300 ]; do
    sleep 0.1
    i=$((i + 1))
done
cat >/dev/null
exit 0
EOF
    chmod 755 "$WORK/hooks/pre-push"
    round=1
    while [ "$round" -le "$ROUNDS" ]; do
        rm -rf "$WORK/barrier"
        mkdir "$WORK/barrier"
        k=1
        while [ "$k" -le "$RACERS" ]; do
            args=()
            for i in 1 2 3 4 5; do
                b=$(claim_commit "t1-$tr-r$round-k$k-b$i")
                printf '%s %s\n' "$b" "$P-t1-$tr-b$i" >>"$WORK/race-$tr-$round-$k.mine"
                args+=("--force-with-lease=$P-t1-$tr-b$i:${base[i]}" "$b:$P-t1-$tr-b$i")
            done
            (OWL581_BARRIER=$WORK/barrier OWL581_RACERS=$RACERS lg -c "core.hooksPath=$WORK/hooks" \
                push --porcelain --atomic "$url" "${args[@]}" >"$WORK/race-$tr-$round-$k.out" 2>&1
            echo $? >"$WORK/race-$tr-$round-$k.st") &
            k=$((k + 1))
        done
        wait
        win=0 server=0 client=0 other=0
        k=1
        while [ "$k" -le "$RACERS" ]; do
            st=$(cat "$WORK/race-$tr-$round-$k.st")
            if [ "$st" = 0 ]; then
                win=$((win + 1))
                cp "$WORK/race-$tr-$round-$k.mine" "$WORK/race-winner"
            elif grep -q 'stale info' "$WORK/race-$tr-$round-$k.out"; then
                client=$((client + 1))
            elif grep -q '^!' "$WORK/race-$tr-$round-$k.out"; then
                server=$((server + 1))
                awk -F '\t' '$1 == "!" { print $3 }' "$WORK/race-$tr-$round-$k.out" | sed 's/[0-9a-f]\{40\}/<oid>/g' >>"$WORK/race-$tr-reasons"
            else
                other=$((other + 1))
            fi
            k=$((k + 1))
        done
        lg ls-remote "$url" "$P-t1-$tr-b*" | sort -k2 >"$WORK/race-now"
        mixed=no
        if [ "$win" = 1 ]; then
            sort -k2 "$WORK/race-winner" | tr ' ' '\t' | diff -q - "$WORK/race-now" >/dev/null || mixed=yes
        fi
        report "T1 $tr race round $round, $RACERS runners: accepted $win; refused by the server $server; refused by git before sending $client; failed otherwise $other; the five refs all the winner's: $([ "$win" = 1 ] && [ "$mixed" = no ] && echo yes || echo no)"
        if [ "$other" != 0 ]; then
            fail "T1 $tr race round $round: INCONCLUSIVE: $other pushes failed without a refusal (transport?): $(grep -v '^To ' "$WORK"/race-"$tr"-"$round"-*.out | grep -vi '^done' | head -n 3 | tr '\n' ' ')"
        elif [ "$win" != 1 ] || [ "$mixed" != no ]; then
            fail "T1 $tr race round $round: FAILED: not one winner whose five refs alone are on the remote"
        elif [ "$server" = 0 ]; then
            fail "T1 $tr race round $round: INCONCLUSIVE: no refusal came from the server"
        fi
        for i in 1 2 3 4 5; do base[i]=$(remote_oid "$url" "$P-t1-$tr-b$i"); done
        round=$((round + 1))
    done
    if [ -s "$WORK/race-$tr-reasons" ]; then
        report "T1 $tr the server's refusals, distinct texts: $(sort -u "$WORK/race-$tr-reasons" | sed "s|$P|<probe>|g" | tr '\n' ';')"
    fi
}

# ---------------------------------------------------------------------------------------------------------------- T2

T2_SPEC='+refs/owlshift/claims/*:refs/remotes/owlshift/claims/*'

t2() {
    local i n args=() st t0 t1 clone=$WORK/clone sp
    section "T2 · reading the claims"
    : >"$WORK/t2-pushed"
    i=0
    while [ "$i" -lt 100 ]; do
        n=$(printf '%03d' "$i")
        c=$(claim_commit "t2-$n") || return 1
        printf '%s\t%s\n' "$c" "$P-t2-$n" >>"$WORK/t2-pushed"
        args+=("--force-with-lease=$P-t2-$n:" "$c:$P-t2-$n")
        i=$((i + 1))
    done
    push "$SSH_URL" --atomic "${args[@]}"
    st=$?
    report "T2 take 100 claims in one atomic push over ssh: exit $st, new: $(grep -c '^\*' "$WORK/push.out")"
    expect "T2 take 100" "$st" 0
    git clone -q --no-checkout "$SSH_URL" "$clone" 2>"$WORK/clone.err"
    report "T2 plain clone: exit $?; its fetch refspecs: $(git -C "$clone" config --get-all remote.origin.fetch | tr '\n' ' ')"
    # Every ref of the clone, whatever its namespace: a claim would name its ticket's PROBE581 prefix.
    n=$(git -C "$clone" for-each-ref --format='%(refname)' | grep -c -e '^refs/owlshift/' -e '/owlshift/claims/' -e PROBE581)
    report "T2 refs of the clone under refs/owlshift/, or naming a claim: $n (of $(git -C "$clone" for-each-ref | wc -l | tr -d ' ') refs)"
    expect "T2 plain clone brings no claim" "$n" 0
    git -C "$clone" fetch -q 2>/dev/null
    n=$(git -C "$clone" for-each-ref --format='%(refname)' | grep -c -e '^refs/owlshift/' -e '/owlshift/claims/' -e PROBE581)
    report "T2 after a plain fetch: $n"
    expect "T2 plain fetch brings no claim" "$n" 0
    # Refs a prune must leave: outside the refspec's destination, or beside it.
    git -C "$clone" update-ref refs/remotes/origin/zzz-local HEAD
    git -C "$clone" update-ref refs/heads/zzz-local HEAD
    git -C "$clone" update-ref refs/remotes/owlshift/other/x HEAD
    t0=$(now_ms)
    git -C "$clone" fetch -q origin "$T2_SPEC"
    st=$?
    t1=$(now_ms)
    git -C "$clone" for-each-ref --format='%(objectname)%09%(refname)' "refs/remotes/owlshift/claims/" |
        grep "PROBE581-$RUN-t2-" | sed 's|refs/remotes/owlshift/claims/|refs/owlshift/claims/|' >"$WORK/t2-got"
    report "T2 fetch with $T2_SPEC over ssh: exit $st, $((t1 - t0)) ms; this run's claims fetched: $(wc -l <"$WORK/t2-got" | tr -d ' '), each at the commit pushed: $(diff -q "$WORK/t2-pushed" "$WORK/t2-got" >/dev/null && echo yes || echo no)"
    expect "T2 the refspec brings the 100" "$(diff -q "$WORK/t2-pushed" "$WORK/t2-got" >/dev/null && echo same)" same
    t0=$(now_ms)
    git -C "$clone" fetch -q origin "$T2_SPEC"
    t1=$(now_ms)
    report "T2 the same fetch again, nothing new: $((t1 - t0)) ms"
    sp=$(awk -F '\t' '$2 ~ /-t2-050$/ { print $1 }' "$WORK/t2-pushed")
    push "$SSH_URL" "--force-with-lease=$P-t2-050:$sp" ":$P-t2-050"
    report "T2 delete one claim on the remote: flag $(flag "$P-t2-050")"
    t0=$(now_ms)
    git -C "$clone" fetch -q --prune origin "$T2_SPEC"
    st=$?
    t1=$(now_ms)
    n=$(git -C "$clone" for-each-ref --format='%(refname)' "refs/remotes/owlshift/claims/" | grep -c "PROBE581-$RUN-t2-")
    report "T2 fetch --prune: exit $st, $((t1 - t0)) ms; this run's claims left: $n; the deleted one gone: $(git -C "$clone" show-ref -q --verify "refs/remotes/owlshift/claims/PROBE581-$RUN-t2-050" && echo no || echo yes); kept: refs/remotes/origin/zzz-local $(git -C "$clone" show-ref -q --verify refs/remotes/origin/zzz-local && echo yes || echo no), refs/heads/zzz-local $(git -C "$clone" show-ref -q --verify refs/heads/zzz-local && echo yes || echo no), refs/remotes/owlshift/other/x $(git -C "$clone" show-ref -q --verify refs/remotes/owlshift/other/x && echo yes || echo no)"
    expect "T2 prune drops the deleted one alone" "$n" 99
    for sp in refs/remotes/origin/zzz-local refs/heads/zzz-local refs/remotes/owlshift/other/x; do
        git -C "$clone" show-ref -q --verify "$sp" || fail "T2 prune: FAILED: $sp was removed"
    done
    t2_time ssh "$SSH_URL"
    t2_time https "$HTTPS_URL"
    lg ls-remote "$SSH_URL" "$P-t2-*" >"$WORK/t2-left"
    report "T2 delete this check's refs: left $(delete_refs "$SSH_URL" "$WORK/t2-left") of this run's"
}

# t2_time TRANSPORT URL: the time an ls-remote, a first fetch into an empty repository and a fetch with nothing new
# take over the 99 claims left, beside an ls-remote of HEAD alone.
t2_time() {
    local tr=$1 url=$2 t0 t1 n dir=$WORK/time-$1 ls first again base
    # The connection's own cost: an ls-remote of the default branch alone.
    t0=$(now_ms)
    lg ls-remote "$url" HEAD >/dev/null
    t1=$(now_ms)
    base=$((t1 - t0))
    t0=$(now_ms)
    n=$(lg ls-remote "$url" "$CLAIMS/*" | grep -c "PROBE581-$RUN-t2-")
    t1=$(now_ms)
    ls=$((t1 - t0))
    git init -q "$dir"
    t0=$(now_ms)
    git -C "$dir" fetch -q "$url" "$T2_SPEC"
    t1=$(now_ms)
    first=$((t1 - t0))
    t0=$(now_ms)
    git -C "$dir" fetch -q "$url" "$T2_SPEC"
    t1=$(now_ms)
    again=$((t1 - t0))
    report "T2 time over $tr, $n of this run's claims (and any other on the remote): ls-remote of HEAD alone $base ms; ls-remote of the claims $ls ms; fetch into an empty repository $first ms; again, nothing new $again ms"
}

# ---------------------------------------------------------------------------------------------------------------- T3

t3() {
    local status w tree tr url c1 c2 c3 wa wb
    section "T3 · what a claim sets off"
    status=$(rest repo "repos/$REPO")
    BASE=$(field .default_branch "$WORK/repo.body")
    report "T3 the repository: HTTP $status, private: $(bool .private "$WORK/repo.body"), default branch read: $([ -n "$BASE" ] && echo yes || echo no), forks: $(field .forks_count "$WORK/repo.body")"
    if [ "$(bool .private "$WORK/repo.body")" != true ] || [ -z "$BASE" ]; then
        fail "T3: not run: the repository must be private at the start (a killed run? bash $PROGRAM --restore $REPO)"
        return 1
    fi
    if [ "$(rest wfold "repos/$REPO/contents/$WF_PATH?ref=$BASE")" = 200 ]; then
        fail "T3: not run: $WF_PATH is already on $BASE (a killed run? bash $PROGRAM --restore $REPO)"
        return 1
    fi
    SINCE=$(date -u -v-5M +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d '-5 min' +%Y-%m-%dT%H:%M:%SZ)
    cat >"$WORK/wf.yml" <<'EOF'
on: push
jobs:
  probe:
    runs-on: ubuntu-latest
    steps:
      - run: echo "OWL-581 probe at ${{ github.ref }}"
EOF
    jq -n -c --arg m "OWL-581 probe: add a workflow on push" --arg c "$(base64 <"$WORK/wf.yml" | tr -d '\n')" \
        --arg b "$BASE" '{message: $m, content: $c, branch: $b}' >"$WORK/wfput.json"
    status=$(rest wfput "repos/$REPO/contents/$WF_PATH" PUT "$WORK/wfput.json")
    w=$(field .commit.sha "$WORK/wfput.body")
    if [ "$status" != 201 ] || [ -z "$w" ]; then
        fail "T3: FAILED: the workflow was not added: HTTP $status $(field .message "$WORK/wfput.body")"
        return 1
    fi
    WF_ADDED=yes
    W=$w
    report "T3 add $WF_PATH on $BASE (on: push, no filter) through the contents API: HTTP $status; its commit, W, is control 1"
    lg fetch -q "$SSH_URL" "refs/heads/$BASE" || {
        fail "T3: FAILED: W not fetched"
        return 1
    }
    tree=$(lg rev-parse "$W^{tree}")
    for tr in ssh https; do
        if [ "$tr" = ssh ]; then url=$SSH_URL; else url=$HTTPS_URL; fi
        c1=$(claim_commit "t3-$tr-1") && c2=$(claim_commit "t3-$tr-2")
        printf '%s\n%s\n' "$c1" "$c2" >>"$WORK/t3-claims"
        push "$url" "--force-with-lease=$P-t3-$tr:" "$c1:$P-t3-$tr"
        c3="take $(flag_char "$P-t3-$tr")"
        push "$url" "--force-with-lease=$P-t3-$tr:$c1" "$c2:$P-t3-$tr"
        c3="$c3, renew $(flag_char "$P-t3-$tr")"
        push "$url" "--force-with-lease=$P-t3-$tr:$c2" ":$P-t3-$tr"
        c3="$c3, delete $(flag_char "$P-t3-$tr")"
        report "T3 $tr a claim as a runner writes it, its tree claim.json alone (no run can start from it): $c3"
        expect "T3 $tr claim pushes" "$c3" "take *, renew +, delete -"
        wa=$(lg commit-tree "$tree" -m "OWL-581 probe: W's tree, no parent, $tr a")
        wb=$(lg commit-tree "$tree" -m "OWL-581 probe: W's tree, no parent, $tr b")
        printf '%s\n%s\n' "$wa" "$wb" >>"$WORK/t3-deciding"
        push "$url" "--force-with-lease=$P-t3-$tr-wf:" "$wa:$P-t3-$tr-wf"
        c3="take $(flag_char "$P-t3-$tr-wf")"
        push "$url" "--force-with-lease=$P-t3-$tr-wf:$wa" "$wb:$P-t3-$tr-wf"
        c3="$c3, renew $(flag_char "$P-t3-$tr-wf")"
        push "$url" "--force-with-lease=$P-t3-$tr-wf:$wb" ":$P-t3-$tr-wf"
        c3="$c3, delete $(flag_char "$P-t3-$tr-wf")"
        report "T3 $tr the deciding refs, under $CLAIMS/, commits without a parent whose tree holds the workflow: $c3"
        expect "T3 $tr deciding pushes" "$c3" "take *, renew +, delete -"
    done
    push "$SSH_URL" "$W:refs/heads/owl581-probe-$RUN"
    report "T3 control 2, branch owl581-probe-$RUN at W: flag $(flag "refs/heads/owl581-probe-$RUN")"
    PROBE_BRANCH=owl581-probe-$RUN
    t3_runs
    push "$SSH_URL" ":refs/heads/owl581-probe-$RUN"
    t3_public
    t3_close_window
    t3_remove_workflow
}

# t3_runs: waits for both controls' runs, then 30 s more, and classifies every run of the window by its commit.
t3_runs() {
    local waited=0 c1 c2 bad odd
    while :; do
        rest runs "repos/$REPO/actions/runs?per_page=100&created=%3E%3D$SINCE" >/dev/null
        c1=$(jq --arg w "$W" --arg b "$BASE" '[.workflow_runs[] | select(.head_sha == $w and .head_branch == $b)] | length' <"$WORK/runs.body" 2>/dev/null)
        c2=$(jq --arg w "$W" --arg b "$PROBE_BRANCH" '[.workflow_runs[] | select(.head_sha == $w and .head_branch == $b)] | length' <"$WORK/runs.body" 2>/dev/null)
        if { [ "${c1:-0}" -ge 1 ] && [ "${c2:-0}" -ge 1 ]; } || [ "$waited" -ge 180 ]; then break; fi
        sleep 15
        waited=$((waited + 15))
    done
    sleep 30
    rest runs "repos/$REPO/actions/runs?per_page=100&created=%3E%3D$SINCE" >/dev/null
    cat "$WORK/t3-claims" "$WORK/t3-deciding" >"$WORK/t3-ours"
    jq -r --arg w "$W" --rawfile ours "$WORK/t3-ours" \
        '.workflow_runs[] | select(.head_sha == $w or (.head_sha as $s | $ours | split("\n") | any(. == $s))) | .id' \
        <"$WORK/runs.body" >"$WORK/run-ids" 2>/dev/null
    c1=$(jq --arg w "$W" --arg b "$BASE" '[.workflow_runs[] | select(.head_sha == $w and .head_branch == $b)] | length' <"$WORK/runs.body")
    c2=$(jq --arg w "$W" --arg b "$PROBE_BRANCH" '[.workflow_runs[] | select(.head_sha == $w and .head_branch == $b)] | length' <"$WORK/runs.body")
    bad=$(jq --rawfile ours "$WORK/t3-ours" '[.workflow_runs[] | select(.head_sha as $s | $ours | split("\n") | any(. == $s))] | length' <"$WORK/runs.body")
    odd=$(jq --arg w "$W" --arg b "$BASE" --arg p "$PROBE_BRANCH" '[.workflow_runs[] | select(.head_sha == $w and .head_branch != $b and .head_branch != $p)] | length' <"$WORK/runs.body")
    report "T3 runs since the workflow was added, after ${waited}s and 30 s more: control 1 (W on $BASE) $c1; control 2 (W on the branch) $c2; on a claim's or a deciding ref's commit $bad; on W under another name $odd"
    jq -r --arg w "$W" --rawfile ours "$WORK/t3-ours" '.workflow_runs[] | select(.head_sha == $w or (.head_sha as $s | $ours | split("\n") | any(. == $s))) | "T3   run: event \(.event), head_branch \(.head_branch // "null"), commit \(if .head_sha == $w then "W" else "a probe ref'"'"'s" end), status \(.status)"' <"$WORK/runs.body"
    if [ "$c1" -lt 1 ] || [ "$c2" -lt 1 ]; then
        fail "T3 runs: INCONCLUSIVE: a control started no run within 180 s"
        return 1
    fi
    if [ "$bad" != 0 ] || [ "$odd" != 0 ]; then
        fail "T3 runs: FAILED: a ref under $CLAIMS/ started a run"
        return 1
    fi
    return 0
}

# set_private true|false: PATCHes the repository's visibility, tried again every 5 s up to 6 times while GitHub
# refuses it (a change right after another answered 422 on 2026-11-19); sets VISIBILITY to what it answered.
set_private() {
    local status try=0
    jq -n -c --argjson p "$1" '{private: $p}' >"$WORK/vis.json"
    while :; do
        status=$(rest vis "repos/$REPO" PATCH "$WORK/vis.json")
        try=$((try + 1))
        if { [ "$status" = 200 ] && [ "$(bool .private "$WORK/vis.body")" = "$1" ]; } || [ "$try" -ge 6 ]; then break; fi
        say "visibility: HTTP $status $(field .message "$WORK/vis.body"); trying again"
        sleep 5
    done
    VISIBILITY="HTTP $status after $try tries$([ "$status" = 200 ] || printf ', %s' "$(field .message "$WORK/vis.body")")"
}

# t3_public: the ruleset part and the pages, the repository public.
t3_public() {
    local status i c1 c2 ref name try ok
    WINDOW=open
    set_private false
    report "T3 make the repository public: $VISIBILITY"
    if [ "$(bool .private "$WORK/vis.body")" != false ]; then
        fail "T3 public part: FAILED: the repository did not become public"
        return 1
    fi
    for target in branch tag; do
        jq -n -c --arg n "owl581-probe-$RUN-$target" --arg t "$target" '{name: $n, target: $t, enforcement: "active",
            conditions: {ref_name: {include: ["~ALL"], exclude: []}},
            rules: [{type: "creation"}, {type: "update"}, {type: "deletion"}, {type: "non_fast_forward"}]}' >"$WORK/rs.json"
        status=$(rest rs "repos/$REPO/rulesets" POST "$WORK/rs.json")
        i=$(field .id "$WORK/rs.body")
        report "T3 ruleset on every $target (~ALL): restrict creation, update, deletion, block force pushes, no bypass: HTTP $status"
        if [ -z "$i" ]; then
            fail "T3 ruleset: FAILED: not created: $(field .message "$WORK/rs.body")"
            return 1
        fi
        RULESETS+=("$i")
    done
    for ref in "refs/heads/owl581-probe-$RUN-ruled" "refs/tags/owl581-probe-$RUN"; do
        ok=no
        try=0
        while [ "$try" -lt 12 ]; do
            push "$SSH_URL" "$W:$ref"
            if [ "$(flag_char "$ref")" = '!' ]; then
                ok=yes
                break
            fi
            push "$SSH_URL" ":$ref" || true
            try=$((try + 1))
            sleep 5
        done
        report "T3 control: $ref refused by the ruleset: $ok after $try accepted tries; $(flag "$ref" | sed 's/[0-9a-f]\{40\}/<oid>/g')"
        if [ "$ok" != yes ]; then
            fail "T3 ruleset: INCONCLUSIVE: the ruleset never refused $ref"
            return 1
        fi
    done
    name=$P-t3-public
    c1=$(claim_commit t3-public-1)
    c2=$(claim_commit t3-public-2)
    push "$SSH_URL" "--force-with-lease=$name:" "$c1:$name"
    ok="take $(flag_char "$name")"
    push "$SSH_URL" "--force-with-lease=$name:$c1" "$c2:$name"
    ok="$ok, renew $(flag_char "$name")"
    report "T3 under the rulesets, a claim: $ok"
    expect "T3 rulesets refuse no claim push" "$ok" "take *, renew +"
    t3_pages "$name" "$c2"
    push "$SSH_URL" "--force-with-lease=$name:$c2" ":$name"
    report "T3 under the rulesets, its delete: $(flag_char "$name")"
    expect "T3 rulesets refuse no claim delete" "$(flag_char "$name")" -
}

# t3_pages REF COMMIT: what GitHub shows of the ref, unauthenticated, the repository public.
t3_pages() {
    local ref=$1 sha=$2 short=${1#refs/owlshift/claims/} path code hits try
    for path in "branches/all" "tags" "activity" "commit/$sha" "tree/$ref"; do
        try=0
        while :; do
            code=$(curl -s -L -o "$WORK/page.html" -w '%{http_code}' "https://github.com/$REPO/$path" 2>/dev/null)
            hits=$(grep -c -e "$short" -e "$sha" "$WORK/page.html" 2>/dev/null)
            if [ "${hits:-0}" != 0 ] || [ "$try" -ge 1 ]; then break; fi
            try=$((try + 1))
            sleep 10
        done
        report "T3 page /$(printf '%s' "$path" | sed "s|$sha|<claim commit>|; s|$short|<claim>|"), signed out: HTTP $code; lines naming the claim or its commit: ${hits:-0}"
    done
    code=$(curl -s -o "$WORK/events.json" -w '%{http_code}' "https://api.github.com/repos/$REPO/events?per_page=100")
    report "T3 public events API, signed out: HTTP $code; events naming the claim: $(jq --arg r "$short" '[.[] | select((.payload.ref // "") | contains($r))] | length' <"$WORK/events.json" 2>/dev/null)"
    rest mref "repos/$REPO/git/matching-refs/owlshift/claims/$short" >/dev/null
    report "T3 REST git/matching-refs, signed in: lists the claim: $(jq --arg r "$ref" '[.[] | select(.ref == $r)] | length' <"$WORK/mref.body" 2>/dev/null)"
}

# t3_close_window: deletes the rulesets while the repository is public (their API needs it), then makes it private.
t3_close_window() {
    local id status try
    if [ "${WINDOW:-}" != open ]; then return 0; fi
    for id in ${RULESETS[@]+"${RULESETS[@]}"}; do
        try=0
        while :; do
            status=$(rest rsdel "repos/$REPO/rulesets/$id" DELETE)
            if [ "$status" = 204 ] || [ "$status" = 404 ] || [ "$try" -ge 3 ]; then break; fi
            try=$((try + 1))
            sleep 3
        done
        report "T3 delete a ruleset: HTTP $status"
        if [ "$status" != 204 ] && [ "$status" != 404 ]; then fail "T3 cleanup: FAILED: ruleset $id left on the repository"; fi
    done
    RULESETS=()
    set_private true
    rest repo "repos/$REPO" >/dev/null
    report "T3 make the repository private again: $VISIBILITY; read back, private: $(bool .private "$WORK/repo.body"), forks: $(field .forks_count "$WORK/repo.body")"
    if [ "$(bool .private "$WORK/repo.body")" = true ]; then
        WINDOW=closed
    else
        fail "T3 cleanup: FAILED: THE REPOSITORY IS STILL PUBLIC: gh api -X PATCH repos/$REPO -F private=true"
    fi
}

# t3_remove_workflow: waits for the runs of the window to complete (cancelling them after 3 minutes), deletes them,
# then removes the workflow file.
t3_remove_workflow() {
    local id waited=0 open status sha left=0
    if [ -s "$WORK/run-ids" ]; then
        while :; do
            open=0
            while read -r id; do
                rest one "repos/$REPO/actions/runs/$id" >/dev/null
                if [ "$(field .status "$WORK/one.body")" != completed ]; then
                    open=$((open + 1))
                    if [ "$waited" -ge 180 ]; then rest cancel "repos/$REPO/actions/runs/$id/cancel" POST >/dev/null; fi
                fi
            done <"$WORK/run-ids"
            if [ "$open" = 0 ] || [ "$waited" -ge 240 ]; then break; fi
            sleep 10
            waited=$((waited + 10))
        done
        while read -r id; do
            status=$(rest delrun "repos/$REPO/actions/runs/$id" DELETE)
            if [ "$status" != 204 ]; then left=$((left + 1)); fi
        done <"$WORK/run-ids"
        report "cleanup: delete the $(wc -l <"$WORK/run-ids" | tr -d ' ') runs of the window: left $left"
        if [ "$left" != 0 ]; then fail "cleanup: FAILED: $left runs left"; fi
        : >"$WORK/run-ids"
    fi
    if [ "${WF_ADDED:-}" = yes ]; then
        rest wfget "repos/$REPO/contents/$WF_PATH?ref=$BASE" >/dev/null
        sha=$(field .sha "$WORK/wfget.body")
        jq -n -c --arg s "$sha" --arg b "$BASE" --arg m "OWL-581 probe: remove the workflow" '{message: $m, sha: $s, branch: $b}' >"$WORK/wfdel.json"
        status=$(rest wfdel "repos/$REPO/contents/$WF_PATH" DELETE "$WORK/wfdel.json")
        report "cleanup: remove $WF_PATH from $BASE: HTTP $status"
        if [ "$status" = 200 ]; then WF_ADDED=removed; else fail "cleanup: FAILED: $WF_PATH left on $BASE"; fi
    fi
}

# ----------------------------------------------------------------------------------------------------------- cleanup

# shellcheck disable=SC2329 # run by the EXIT trap
cleanup() {
    local left all
    if [ "${CLEANED:-}" = yes ]; then return 0; fi
    CLEANED=yes
    trap '' INT TERM
    if [ "${STARTED:-no}" = yes ]; then
        section cleanup
        t3_close_window
        run_refs "$SSH_URL" >"$WORK/left"
        report "cleanup: this run's refs on the remote: $(wc -l <"$WORK/left" | tr -d ' '), left after deleting them: $(delete_refs "$SSH_URL" "$WORK/left")"
        t3_remove_workflow
        left=$(run_refs "$SSH_URL" | wc -l | tr -d ' ')
        all=$(lg ls-remote "$SSH_URL" "$CLAIMS/PROBE581-*" "refs/heads/owl581-probe-*" "refs/tags/owl581-probe-*" | wc -l | tr -d ' ')
        report "cleanup: final ls-remote: this run's refs $left; any PROBE581 claim or owl581-probe ref $all; refs under refs/owlshift/: $(lg ls-remote "$SSH_URL" 'refs/owlshift/*' | wc -l | tr -d ' ')"
        if [ "$left" != 0 ]; then fail "cleanup: FAILED: refs left: bash $PROGRAM --restore $REPO"; fi
        if [ "${T3_RAN:-no}" = yes ]; then
            rest repo "repos/$REPO" >/dev/null
            report "cleanup: the repository's private, read back: $(bool .private "$WORK/repo.body")"
        fi
        report ""
        report "done, failures: $FAILED."
    fi
    rm -rf "${WORK:-/nonexistent}"
}

restore() {
    local status id
    section restore
    status=$(rest repo "repos/$REPO")
    BASE=$(field .default_branch "$WORK/repo.body")
    report "restore: the repository: HTTP $status, private: $(bool .private "$WORK/repo.body")"
    if [ "$(bool .private "$WORK/repo.body")" = false ]; then
        rest rsl "repos/$REPO/rulesets" >/dev/null
        for id in $(jq -r '.[] | select(.name | startswith("owl581-probe-")) | .id' <"$WORK/rsl.body" 2>/dev/null); do
            RULESETS+=("$id")
        done
        WINDOW=open
        t3_close_window
    else
        report "restore: private: rulesets cannot be read on this plan; make it public with gh to list them if one was left"
    fi
    lg ls-remote "$SSH_URL" "$CLAIMS/PROBE581-*" "refs/heads/owl581-probe-*" "refs/tags/owl581-probe-*" >"$WORK/left"
    delete_refs "$SSH_URL" "$WORK/left" >/dev/null
    report "restore: probe refs: $(wc -l <"$WORK/left" | tr -d ' '), left after deleting them: $(lg ls-remote "$SSH_URL" "$CLAIMS/PROBE581-*" "refs/heads/owl581-probe-*" "refs/tags/owl581-probe-*" | wc -l | tr -d ' ')"
    if [ "$(rest wfold "repos/$REPO/contents/$WF_PATH?ref=$BASE")" = 200 ]; then
        WF_ADDED=yes
        t3_remove_workflow
    fi
}

# -------------------------------------------------------------------------------------------------------------- main

MODE=run
case ${1:-} in
-h | --help)
    usage
    exit 0
    ;;
--restore)
    MODE=restore
    shift
    ;;
esac
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
    say "$PROGRAM: never run against $FORBIDDEN"
    exit 2
fi
for tool in git gh jq curl base64 perl; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        say "$PROGRAM: $tool is needed"
        exit 2
    fi
done
CHECKS=${OWL581_CHECKS:-T1 T2 T3}
for c in $CHECKS; do
    case $c in
    T1 | T2 | T3) ;;
    *)
        say "$PROGRAM: unknown check $c in OWL581_CHECKS"
        exit 2
        ;;
    esac
done

WORK=$(mktemp -d "${TMPDIR:-/tmp}/owl581.XXXXXX") || exit 2
LOCAL=$WORK/local
git init -q "$LOCAL" || exit 2
export GIT_AUTHOR_NAME="OWL-581 probe" GIT_AUTHOR_EMAIL=probe@invalid
export GIT_COMMITTER_NAME="OWL-581 probe" GIT_COMMITTER_EMAIL=probe@invalid
export GIT_TERMINAL_PROMPT=0
# No shared SSH connection: each command opens its own, as a runner's would, and the times say so.
export GIT_SSH_COMMAND="ssh -o ControlMaster=no -o ControlPath=none"
SSH_URL="git@github.com:$REPO.git"
HTTPS_URL="https://github.com/$REPO.git"
RUN=$(date -u +%Y%m%d%H%M%S)-$(od -An -N2 -tx1 /dev/urandom | tr -d ' \n')
P=$CLAIMS/PROBE581-$RUN
FAILED=0
BASE=
W=
SINCE=
PROBE_BRANCH=
WF_ADDED=
WINDOW=
VISIBILITY=
RULESETS=()
trap cleanup EXIT
trap 'exit 130' INT TERM

report "OWL-581 checks $CHECKS, $(date -u +%Y-%m-%dT%H:%M:%SZ), $(git --version), $(gh --version | head -n 1 | cut -d' ' -f1-3), $(uname -s), script sha256 $(script_sha), run $RUN"
report "If this run is killed beyond its trap: bash docs/design/live-checks/$PROGRAM --restore $REPO"
if ! gh auth status >/dev/null 2>&1 </dev/null; then
    say "$PROGRAM: gh is not signed in (gh auth status)"
    exit 2
fi
if [ "$MODE" = restore ]; then
    restore
    exit "$FAILED"
fi
status=$(rest repo "repos/$REPO")
report "start: gh reads $REPO: HTTP $status, admin: $(bool .permissions.admin "$WORK/repo.body"), private: $(bool .private "$WORK/repo.body")"
if [ "$status" != 200 ] || [ "$(bool .permissions.admin "$WORK/repo.body")" != true ]; then
    fail "start: gh's account does not administer $REPO; nothing run"
    exit 1
fi
for url in "$SSH_URL" "$HTTPS_URL"; do
    if ! lg ls-remote "$url" HEAD >/dev/null 2>"$WORK/ls.err"; then
        fail "start: git cannot read $url: $(tail -n 1 "$WORK/ls.err"); nothing run"
        exit 1
    fi
done
STARTED=yes
for c in $CHECKS; do
    case $c in
    T1)
        t1 ssh "$SSH_URL"
        t1 https "$HTTPS_URL"
        ;;
    T2) t2 ;;
    T3)
        T3_RAN=yes
        t3
        ;;
    esac
done
exit "$FAILED"
