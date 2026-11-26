#!/usr/bin/env python3
# Overlap refresh replay (OWL-654), quoted by "Tolerated overlap", *The refresh*, *Measured*, in
# docs/design/architecture.md, section 5, and by docs/design/records/OWL-654.md.
# Usage: python3 -I docs/design/live-checks/overlap-refresh-replay.py <events.jsonl> <TICKET> [<since>]
#   <events.jsonl>: the event log (`<data dir>/events.jsonl`); <TICKET>: the ticket's identifier;
#   <since>: events before it are ignored, ISO 8601 in UTC (default: none).
# Read only, standard library only. From the ticket's events, oldest first, it lists:
# - each `watch` `rebase_run` decision of cause `overlap` (a refresh that ran under the rule before OWL-654), its
#   `onto` and time, and what the pull request waited for at that moment under OWL-654's rule;
# - between them, the watch reads (verdict, head), the runs started (role), the stage moves (a round open is the
#   stage `needs_input`, a park `parked`) and the waits told.
# Then it counts the refreshes OWL-654's rule would run: a refresh becomes owed where one ran, onto its `onto`; one
# owed runs at the first moment the pull request is ready: its last watch read green, no round open, not parked, and,
# in that read's pass (until the next read or a rebase run), no run started, no stage moved to working, and no
# `merge_due_first` or `merge_owes_first` wait told. Ready at the moment a refresh ran, it runs then. The newest
# `onto` owed is the one it rebases onto.
# Approximations: the history after a refresh that ran is the old rule's, so the reads that follow it are of the
# rebased head, whose CI ran again; a read's readiness is taken from the events of its pass, not from the ref. A
# refresh the new rule runs at a read where the old rule ran none replaces the head read then: later reads of that
# same head are of a head the new rule no longer has, and count as not read, so a refresh owed after it waits for a
# read of another head.
# Prints `overlap refreshes run: N` and `under the new rule: M`, each with its base heads.
import datetime, json, sys

if len(sys.argv) not in (3, 4):
    sys.exit("usage: overlap-refresh-replay.py <events.jsonl> <TICKET> [<since>]")
path, ticket = sys.argv[1], sys.argv[2]
since = sys.argv[3] if len(sys.argv) == 4 else None


def when(iso):
    if len(iso) == 10:
        iso += "T00:00:00Z"
    return datetime.datetime.fromisoformat(iso.replace("Z", "+00:00"))


start = when(since) if since else None
events = []
with open(path, encoding="utf-8") as lines:
    for line in lines:
        if f'"{ticket}"' not in line:
            continue
        try:
            event = json.loads(line)
        except ValueError:
            continue
        if event.get("ticket") != ticket:
            continue
        at = when(event["at"])
        if start and at < start:
            continue
        events.append((at, event))
events.sort(key=lambda pair: pair[0])


def data(event):
    return event.get("data") or {}


def is_read(event):
    return event.get("kind") == "decision" and "verdict" in data(event) and "watch" in data(event)


def is_refresh(event):
    d = data(event)
    return event.get("kind") == "decision" and d.get("watch") == "rebase_run" and d.get("cause") == "overlap"


def stage_of(event):
    d = data(event)
    if event.get("kind") == "tracker_write" and d.get("action") == "stage":
        return d.get("stage")
    return None


HOLDS = ("merge_due_first", "merge_owes_first")


def pass_holds(i):
    """What the pass of the read at index i holds, from its own events: until the next read or a refresh."""
    held = []
    for _, event in events[i + 1:]:
        if is_read(event) or is_refresh(event):
            break
        d = data(event)
        if event.get("kind") == "run_started":
            held.append(f"a {d.get('role', 'run')} run started")
        elif stage_of(event) in ("working", "needs_input", "parked"):
            held.append(f"the stage moved to {stage_of(event)}")
        elif event.get("kind") == "decision" and d.get("watch") == "waits" and d.get("reason") in HOLDS:
            due = d.get("due") or ("report" if d.get("report") else "")
            held.append(f"{d['reason']} {due}".strip())
    return held


def short(commit):
    return (commit or "?")[:8]


stage, verdict, head, read_at, holds = None, None, None, None, []
owed, owed_since = None, None
# The head a refresh the new rule ran at a read replaced: its later reads are not the new rule's.
replaced = None
old, new = [], []
for i, (at, event) in enumerate(events):
    d = data(event)
    stamp = at.strftime("%Y-%m-%d %H:%M:%SZ")
    if is_read(event):
        verdict, head, read_at = d.get("verdict"), d.get("head"), at
        holds = pass_holds(i)
        line = f"  {stamp} read {verdict} at {short(head)}" + (f" ({'; '.join(holds)})" if holds else "")
        if head and head == replaced:
            verdict, holds = None, []
            line += "; under the new rule this head was replaced by its refresh: not read"
        else:
            replaced = None
        print(line)
    elif event.get("kind") == "run_started":
        print(f"  {stamp} run started: {d.get('role')}")
    elif stage_of(event):
        stage = stage_of(event)
        print(f"  {stamp} stage: {stage}")
    elif event.get("kind") == "decision" and d.get("watch") == "waits":
        print(f"  {stamp} waits: {d.get('reason')}")
    if stage in ("needs_input", "parked"):
        waits = ["a round open" if stage == "needs_input" else "parked"]
    else:
        waits = []
    if verdict != "green":
        waits.insert(0, f"its checks ({verdict or 'not read yet'})")
    waits += holds
    if is_refresh(event):
        onto = d.get("onto")
        old.append((at, onto))
        if waits:
            print(f"{stamp} refresh onto {short(onto)} ran; under the new rule it waits for: {', '.join(waits)}")
            owed, owed_since = onto, owed_since or at
        else:
            print(f"{stamp} refresh onto {short(onto)} ran; under the new rule it runs too: nothing pending")
            new.append((at, onto))
            owed, owed_since = None, None
        # Under the old rule its push follows: the next read is of the rebased head.
        verdict, holds = None, []
    elif is_read(event) and owed and not waits:
        print(f"{stamp} under the new rule the refresh owed since "
              f"{owed_since.strftime('%Y-%m-%d %H:%M:%SZ')} runs now, onto {short(owed)}")
        new.append((at, owed))
        owed, owed_since = None, None
        # Its push follows under the new rule: the next read to count is of the rebased head.
        verdict, holds, replaced = None, [], head

print(f"overlap refreshes run: {len(old)}" + (f" ({', '.join(short(onto) for _, onto in old)})" if old else ""))
print(f"under the new rule: {len(new)}" + (f" ({', '.join(short(onto) for _, onto in new)})" if new else ""))
if owed:
    print(f"still owed at the end: onto {short(owed)}, since {owed_since.strftime('%Y-%m-%d %H:%M:%SZ')}")
