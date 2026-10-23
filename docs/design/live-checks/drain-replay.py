#!/usr/bin/env python3
# Drain replay, quoted by the "Resources admitted at Build" bullet of docs/design/architecture.md, section 5 (OWL-428),
# and run again by OWL-429.
# Usage: python3 -I docs/design/live-checks/drain-replay.py <events.jsonl> <dedicated checkout> <owner/repo> <start>
#          [--until <time>] [--hours 30] [--draws 40] [--pessimistic] [--durx 1] [--waiters-free]
#   <events.jsonl>: the event log (`<data dir>/events.jsonl`); <dedicated checkout>: the project's checkout, whose
#   refs/owlshift/tickets/* hold intake.json, plan.json and held.json; <start>: the drain's start, ISO 8601 in UTC;
#   --until: events after it are ignored (default: none), so a replay of a drain still running can be run again.
#   --durx multiplies the median phase times of the tickets that waited (default 1).
# Read only. It replays one project's tickets from <start> under each collision rule, on a 3-minute clock:
# - the tickets that ran keep their own times (dispatch to first Build, first Build to pull request, pull request to
#   merge), plan files, changed files and blockers; one not merged yet gets its variant's median merge time;
# - the tickets that waited (an intake record with zones, not parked, not waiting for an answer) get their variant's
#   median times, as do those dispatched without a pull request by --until; their files are unknown, so a conflict at file level with them is drawn, among the pairs whose
#   intake zones conflict, at the rate measured on the tickets that ran with a plan (--pessimistic adds 25 points);
# - capacity 10, a ticket counted from its dispatch until its pull request (--waiters-free leaves a ticket that waits
#   for its Build out of the count), dispatch in ready-time then identifier order, the one dispatched first admitted
#   first at Build. Not modelled: the reviewer's pace, usage limits, the cost of rebases.
# Printed per rule: agents working (Design or Build), tickets counted in flight, pull requests and merges within
# --hours of <start>, the drain's length to the last merge, and the mean wait (time from ready to Build start that is
# not its own Design, ready meaning admitted and every blocker merged).
import argparse, datetime, itertools, json, random, statistics, subprocess

ap = argparse.ArgumentParser()
ap.add_argument("events"); ap.add_argument("checkout"); ap.add_argument("project"); ap.add_argument("start")
ap.add_argument("--until"); ap.add_argument("--hours", type=float, default=30); ap.add_argument("--draws", type=int, default=40)
ap.add_argument("--pessimistic", action="store_true"); ap.add_argument("--durx", type=float, default=1)
ap.add_argument("--waiters-free", action="store_true")
args = ap.parse_args()

def hours(iso): return datetime.datetime.fromisoformat(iso.replace("Z", "+00:00")).timestamp() / 3600
START = hours(args.start); WEND = START + args.hours; DT = 0.05; CAPACITY = 10

# ---- the events, per ticket
tk = {}
for line in open(args.events):
    if f'"{args.project}"' not in line: continue
    e = json.loads(line); t = e.get("ticket")
    if e.get("project") != args.project or not t or (args.until and hours(e["at"]) > hours(args.until)): continue
    x = tk.setdefault(t, {"dispatch": None, "runs": {}, "pr": None, "ended": None, "blk": set(), "stages": []})
    a, k, at = e["data"], e["kind"], hours(e["at"])
    if k == "dispatch" and x["dispatch"] is None: x["dispatch"] = at
    elif k == "run_started": x["runs"][e.get("run")] = [a.get("role"), at]
    elif k == "tracker_write" and a.get("action") == "stage": x["stages"].append(a.get("stage"))
    elif k == "tracker_write" and a.get("action") == "pull_request" and x["pr"] is None: x["pr"] = at
    elif k == "decision" and a.get("watch") == "ended": x["ended"] = at
    elif k == "decision" and a.get("reason") == "blocked_by": x["blk"].update(a.get("tickets", []))

def ref_json(ticket, name):
    p = subprocess.run(["git", "-C", args.checkout, "show", f"refs/owlshift/tickets/{ticket}:{name}"],
                       capture_output=True, text=True)
    try: return json.loads(p.stdout) if p.returncode == 0 else None
    except ValueError: return None

# ---- the workload
T, ran = {}, {}
for t, x in tk.items():
    intake = ref_json(t, "intake.json") or {}
    if not intake.get("zones") or not intake.get("variant"): continue
    gesture = (intake.get("gesture") or {}).get("at")
    blk = x["blk"] | {d["ticket"] for d in intake.get("dependencies") or []}
    rec = dict(var=intake["variant"], Z=intake["zones"], ready=max(START, hours(gesture) if gesture else START), blk=blk)
    if x["dispatch"] is not None and x["dispatch"] >= START and x["pr"] is not None:
        builds = [at for role, at in x["runs"].values() if role == "build"]
        first_build = min(builds) if builds else x["dispatch"]
        plan = (ref_json(t, "plan.json") or {}).get("plan", {}).get("files")
        changed = (ref_json(t, "held.json") or {}).get("files")
        rec.update(known=True, plan=bool(plan), F=plan or changed, A=changed or plan,
                   phases=[first_build - x["dispatch"], x["pr"] - first_build, None if x["ended"] is None else x["ended"] - x["pr"]])
        ran[t] = rec; T[t] = rec
    elif (x["dispatch"] is None or x["dispatch"] >= START) and not (x["stages"] and x["stages"][-1] in ("parked", "needs_input")):
        rec.update(known=False, plan=intake["variant"] != "trivial", F=None, A=None, phases=None); T[t] = rec
MED = {}
for var in ("trivial", "standard", "risky"):
    rows = [r["phases"] for r in ran.values() if r["var"] == var]
    MED[var] = [statistics.median([p[i] for p in rows if p[i] is not None] or [0]) for i in range(3)]
for r in T.values():
    if r["known"]: r["dur"] = [p if p is not None else MED[r["var"]][i] for i, p in enumerate(r["phases"])]
    else: r["dur"] = [m * args.durx for m in MED[r["var"]]]
    r["blk"] &= set(T)  # a blocker outside the workload is done

# ---- conflicts: containment, part by part, any ASCII case; levels Z (intake zones), F (plan files), D (their
# folders), A (changed files)
def parts(p): return p.lower().strip("/").split("/")
def one(a, b): a, b = parts(a), parts(b); n = min(len(a), len(b)); return a[:n] == b[:n]
def conflict(A, B): return any(one(a, b) for a in A for b in B)
def level(t, l):
    r = T[t]
    return {"Z": r["Z"], "F": r["F"], "A": r["A"], "D": sorted({f.rsplit("/", 1)[0] for f in r["F"]}) if r["F"] else None}[l]
K = [t for t in T if T[t]["known"] and T[t]["F"]]
RATE = {}
for a, b in itertools.product("ZFDA", repeat=2):
    pairs = [(i, j) for i, j in itertools.permutations(K, 2) if conflict(T[i]["Z"], T[j]["Z"])]
    RATE[a, b] = min(1.0, sum(conflict(level(i, a), level(j, b)) for i, j in pairs) / len(pairs) + (0.25 if args.pessimistic else 0)) if pairs else 1.0

def conflicts_for(rng):
    drawn = {}
    def cf(i, a, j, b):
        si = level(i, a) if T[i]["known"] or a == "Z" else None
        sj = level(j, b) if T[j]["known"] or b == "Z" else None
        if si is not None and sj is not None: return conflict(si, sj)
        if not conflict(si if si is not None else T[i]["Z"], sj if sj is not None else T[j]["Z"]): return False
        key = (min(i, j), max(i, j), a + b if i < j else b + a)
        if key not in drawn: drawn[key] = rng.random()
        return drawn[key] < RATE[a, b]
    return cf

# ---- the rules: what a ticket holds in each phase, and what it is checked with at its dispatch and its Build
def holds(t, phase, rule):
    r = T[t]; noplan = not r["plan"]; built = ["Z", "A"] if r["known"] else ["Z"]
    return {
        "today": ["Z"] if phase == "design" else built,
        "plan-files": ["Z"] if phase == "design" or noplan else ["F"],
        "at-build": [] if phase == "design" else built,
        "both": [] if phase == "design" else (["Z"] if noplan else ["F"]),
        "pr-files": ["A"] if phase == "pr" else (["Z"] if phase == "design" else built),
        "both+pr-files": [] if phase == "design" else (["Z"] if noplan else (["A"] if phase == "pr" else ["F"])),
        "fine-intake": ["Z"] if noplan else ["D"],
        "none": [],
    }[rule]
def checked(t, gate, rule):
    noplan = not T[t]["plan"]
    if rule == "none": return []
    if gate == "dispatch":
        if rule in ("at-build", "both", "both+pr-files"): return []
        return ["Z"] if rule != "fine-intake" or noplan else ["D"]
    if rule == "at-build": return ["Z"]
    if rule in ("both", "both+pr-files"): return ["Z"] if noplan else ["F"]
    return []

def replay(rule, seed):
    cf = conflicts_for(random.Random(seed))
    st = {t: "ready" for t in T}; left, disp, pr, end, build = {}, {}, {}, {}, {}
    order = sorted(T, key=lambda t: (T[t]["ready"], int(t.rsplit("-", 1)[1])))
    def free(me, levels):
        for o in T:
            if o == me or st[o] in ("ready", "done"): continue
            held = holds(o, "design" if st[o] in ("design", "bwait") else st[o], rule)
            if any(cf(me, a, o, b) for a in levels for b in held): return False
        return True
    def start_build(x, now): st[x] = "build"; left[x] = T[x]["dur"][1]; build[x] = now
    now, counted, working = START, [], []
    while now < START + 5000:
        for x in T:
            if st[x] in ("design", "build", "pr"):
                left[x] -= DT
                if left[x] <= 1e-9:
                    if st[x] == "design": st[x] = "bwait"
                    elif st[x] == "build": st[x] = "pr"; left[x] = T[x]["dur"][2]; pr[x] = now
                    else: st[x] = "done"; end[x] = now
        if all(s == "done" for s in st.values()): break
        counting = ("design", "build") if args.waiters_free else ("design", "bwait", "build")
        cap = sum(1 for s in st.values() if s in counting)
        for x in sorted((x for x in T if st[x] == "bwait"), key=lambda x: disp[x]):
            if free(x, checked(x, "build", rule)): start_build(x, now)
        for x in order:
            if st[x] != "ready" or cap >= CAPACITY or T[x]["ready"] > now: continue
            if any(st[b] != "done" for b in T[x]["blk"]) or not free(x, checked(x, "dispatch", rule)): continue
            disp[x] = now; cap += 1
            if T[x]["dur"][0] > 0: st[x] = "design"; left[x] = T[x]["dur"][0]
            else:
                st[x] = "bwait"
                if free(x, checked(x, "build", rule)): start_build(x, now)
        if now <= WEND:
            counted.append(sum(1 for s in st.values() if s in ("design", "bwait", "build")))
            working.append(sum(1 for s in st.values() if s in ("design", "build")))
        now += DT
    ready = {x: max([T[x]["ready"]] + [end[b] for b in T[x]["blk"]]) for x in T}
    return dict(working=statistics.mean(working), counted=statistics.mean(counted),
                prs=sum(v <= WEND for v in pr.values()), merged=sum(v <= WEND for v in end.values()),
                length=max(end.values()) - START,
                wait=statistics.mean(build[x] - T[x]["dur"][0] - ready[x] for x in T))

print(f"{args.project} from {args.start}: {len(T)} tickets, {len(ran)} that ran, {len(T) - len(ran)} that waited")
print("conflict rates given conflicting intake zones:", {a + b: round(v, 2) for (a, b), v in RATE.items() if a + b in ("FZ", "FF", "DD", "AA")})
print(f"real: in flight {sum(max(0, min(WEND, tk[t]['pr']) - max(START, tk[t]['dispatch'])) for t in ran) / args.hours:.2f}, "
      f"pull requests within {args.hours:g} h {sum(tk[t]['pr'] <= WEND for t in ran)}")
for rule in ("today", "plan-files", "at-build", "both", "pr-files", "both+pr-files", "fine-intake", "none"):
    rs = [replay(rule, seed) for seed in range(args.draws)]
    m = lambda key: statistics.mean(r[key] for r in rs)
    print(f"{rule:14s} working {m('working'):4.1f}  in flight {m('counted'):4.1f}  pull requests {m('prs'):4.1f}  "
          f"merged {m('merged'):4.1f}  length {m('length'):6.1f} h  mean wait {m('wait'):5.1f} h")
