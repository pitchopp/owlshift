#!/bin/sh
# OWL-452 live probe, quoted by the L7 bullets of docs/design/build-plan.md, "P7 · Background service".
#   On macOS:                     sh docs/design/live-checks/owl-452.sh [claude]
#   In a Linux container with bwrap (the rig of OWL-101, Ubuntu 24.04):
#     docker run --rm --security-opt seccomp=unconfined --security-opt systempaths=unconfined \
#       -v "$PWD/docs/design/live-checks:/probe:ro" owlshift-linux-rig sh /probe/owl-452.sh
# Whether a job left by a finished call, as a Claude Code Bash call leaves `cmd &` (owl-451.sh), can be told from
# the process table as a run's own: its parent (pid 1 once orphaned, or a process of the run's tree), whether its
# group's and session's leaders are gone, whether it started after the run's root, and whether its working
# directory, read by another process of the same user (`proc_pidinfo` PROC_PIDVNODEPATHINFO on macOS,
# `/proc/<pid>/cwd` on Linux), is inside the run's worktree. Then whether killing the root's group ends it, and what
# reading every process's working directory costs. Run unconfined, under sandbox-exec on macOS and under bwrap with
# its own pid namespace on Linux, as Owlshift confines a run. With `claude` on macOS, also one short `claude -p` run
# on this machine's own login (at most 4 turns, 0.50 USD) whose Bash call leaves a job and returns.
# It prints pids, yes/no answers and timings: never a path, a command's arguments or its output, or an environment
# variable. Scratch files under ${TMPDIR:-/tmp}, removed at the end with every process the probe started.
set -u
date -u +"date=%Y-%m-%dT%H:%M:%SZ"
case $(uname -s) in
  Darwin) echo "os=macOS $(sw_vers -productVersion) arch=$(uname -m)" ;;
  *) echo "os=$(. /etc/os-release && echo "$PRETTY_NAME") kernel=$(uname -r) arch=$(uname -m)" ;;
esac
d=$(mktemp -d "${TMPDIR:-/tmp}/owl452.XXXXXX")
d=$(cd "$d" && pwd -P)
wt="$d/wt"
mkdir "$wt"
seen=
cleanup() {
  for p in $seen; do kill -s KILL "$p" 2>/dev/null; done
  rm -rf "$d"
}
trap cleanup EXIT

# The reader: what the process table says of a process, with no `ps`.
cat > "$d/p452.c" <<'EOF'
#include <dirent.h>
#include <errno.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/time.h>
#include <unistd.h>
#ifdef __APPLE__
#include <libproc.h>
#include <sys/proc_info.h>
#endif

struct info { int ppid, pgid, sid; unsigned long long start; };

/* 0 when the system describes `pid` (not a zombie). */
static int describe(int pid, struct info *out) {
#ifdef __APPLE__
  struct proc_bsdinfo bi;
  if (proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, &bi, sizeof bi) != (int)sizeof bi) return -1;
  out->ppid = (int)bi.pbi_ppid;
  out->pgid = (int)bi.pbi_pgid;
  out->sid = getsid(pid);
  out->start = bi.pbi_start_tvsec * 1000000ULL + bi.pbi_start_tvusec;
  return 0;
#else
  char path[64], buf[4096];
  snprintf(path, sizeof path, "/proc/%d/stat", pid);
  FILE *f = fopen(path, "r");
  if (!f) return -1;
  size_t n = fread(buf, 1, sizeof buf - 1, f);
  fclose(f);
  buf[n] = 0;
  char *rest = strrchr(buf, ')');
  if (!rest) return -1;
  char state;
  unsigned long long start = 0;
  int field = 0;
  char *tok = strtok(rest + 1, " ");
  while (tok) {
    switch (field) {
      case 0: state = tok[0]; break;
      case 1: out->ppid = atoi(tok); break;
      case 2: out->pgid = atoi(tok); break;
      case 3: out->sid = atoi(tok); break;
      case 19: start = strtoull(tok, 0, 10); break;
    }
    field++;
    tok = strtok(0, " ");
  }
  if (state == 'Z' || state == 'X') return -1;
  out->start = start;
  return 0;
#endif
}

/* 0 with the working directory in `buf`, else the errno. */
static int cwd_of(int pid, char *buf, size_t len) {
#ifdef __APPLE__
  struct proc_vnodepathinfo vpi;
  if (proc_pidinfo(pid, PROC_PIDVNODEPATHINFO, 0, &vpi, sizeof vpi) != (int)sizeof vpi) return errno ? errno : -1;
  snprintf(buf, len, "%s", vpi.pvi_cdir.vip_path);
  return 0;
#else
  char path[64];
  snprintf(path, sizeof path, "/proc/%d/cwd", pid);
  ssize_t n = readlink(path, buf, len - 1);
  if (n < 0) return errno;
  buf[n] = 0;
  return 0;
#endif
}

static int alive(int pid) { struct info i; return describe(pid, &i) == 0; }

static int inside(const char *cwd, const char *wt) {
  size_t n = strlen(wt);
  return strncmp(cwd, wt, n) == 0 && (cwd[n] == 0 || cwd[n] == '/');
}

static int listing(int *pids, int room) {
#ifdef __APPLE__
  int n = proc_listallpids(pids, room * (int)sizeof(int));
  return n < 0 ? 0 : n;
#else
  int n = 0;
  DIR *dir = opendir("/proc");
  struct dirent *e;
  while (dir && (e = readdir(dir)) && n < room) {
    int pid = atoi(e->d_name);
    if (pid > 0) pids[n++] = pid;
  }
  if (dir) closedir(dir);
  return n;
#endif
}

int main(int argc, char **argv) {
  char cwd[PATH_MAX];
  if (argc == 3 && !strcmp(argv[1], "start")) {
    struct info i;
    if (describe(atoi(argv[2]), &i)) return 1;
    printf("%llu\n", i.start);
    return 0;
  }
  if (argc >= 6 && !strcmp(argv[1], "info")) {
    int root = atoi(argv[2]);
    unsigned long long since = strtoull(argv[3], 0, 10);
    const char *wt = argv[4];
    for (int a = 5; a < argc; a += 2) {
      int pid = atoi(argv[a + 1]);
      struct info i;
      if (describe(pid, &i)) { printf("  %s pid=%d gone\n", argv[a], pid); continue; }
      const char *parent = i.ppid == 1 ? "1" : "other";
      for (int p = i.ppid, hops = 0; p > 1 && hops < 64; hops++) {
        if (p == root) { parent = "a process of the root's tree"; break; }
        struct info up;
        if (describe(p, &up)) break;
        p = up.ppid;
      }
      int err = cwd_of(pid, cwd, sizeof cwd);
      printf("  %s pid=%d parent=%s group_leader=%s session_leader=%s started_after_root=%s cwd=%s",
             argv[a], pid, parent, alive(i.pgid) ? "alive" : "gone",
             i.sid < 0 ? "unknown" : alive(i.sid) ? "alive" : "gone",
             i.start >= since ? "yes" : "no", err ? "unreadable" : inside(cwd, wt) ? "inside" : "outside");
      if (err) printf(" (%s)", strerror(err));
      printf("\n");
    }
    return 0;
  }
  if (argc == 3 && !strcmp(argv[1], "scan")) {
    static int pids[65536];
    struct timeval t0, t1;
    gettimeofday(&t0, 0);
    int n = listing(pids, 65536), described = 0, read = 0, in = 0;
    for (int k = 0; k < n; k++) {
      struct info i;
      if (describe(pids[k], &i)) continue;
      described++;
      if (cwd_of(pids[k], cwd, sizeof cwd) == 0) { read++; in += inside(cwd, argv[2]); }
    }
    gettimeofday(&t1, 0);
    printf("  processes=%d described=%d cwd_read=%d cwd_inside=%d elapsed_ms=%.1f\n", n, described, read, in,
           (t1.tv_sec - t0.tv_sec) * 1e3 + (t1.tv_usec - t0.tv_usec) / 1e3);
    return 0;
  }
  return 2;
}
EOF
cc -O2 -o "$d/p452" "$d/p452.c" 2> "$d/cc.err" || { echo "cc failed: $(head -c 300 "$d/cc.err")"; exit 1; }

# leave.sh FILE SECONDS: a shell in a session and group of its own, as Claude Code starts each Bash call, that
# leaves `sleep SECONDS` as a background job, writes its pid to FILE and ends.
cat > "$d/leave.sh" <<'EOF'
exec perl -MPOSIX -e 'POSIX::setsid() > 0 or die "setsid"; exec @ARGV or die' \
  /bin/sh -c 'sleep "$1" >/dev/null 2>&1 & echo $! > "$0.tmp" && mv "$0.tmp" "$0"' "$1" "$2"
EOF
# root.sh: a run's root, in the worktree, leaving a job as a finished Bash call does, then sleeping.
printf 'sh %s/leave.sh %s/job.pid 301\nsleep 60\n' "$d" "$wt" > "$d/root.sh"

alive() { kill -0 "$1" 2>/dev/null && [ -n "$("$d/p452" start "$1")" ]; }
await() { i=0; while [ ! -s "$1" ] && [ $i -lt 100 ]; do sleep 0.1; i=$((i+1)); done; [ -s "$1" ]; }
# Starts "$@" in the worktree as the leader of a process group of its own, as a run's tree starts; sets root and
# since, its start as the process table gives it.
start_root() {
  rm -f "$d/root.pid" "$wt/job.pid"
  (cd "$wt" && exec perl -e 'setpgrp(0, 0); my $f = shift; open(my $h, ">", $f) or die; print $h $$; close $h;
     exec @ARGV or die' "$d/root.pid" "$@") > /dev/null 2>&1 &
  sub=$!
  await "$d/root.pid" || { echo "  the root never started"; exit 1; }
  root=$(cat "$d/root.pid"); seen="$seen $root"
  since=$("$d/p452" start "$root")
}

probe() { # title, then the command that runs root.sh
  echo "$1"; shift
  (cd "$wt" && sh "$d/leave.sh" "$d/earlier.pid" 302) && await "$d/earlier.pid"
  earlier=$(cat "$d/earlier.pid"); seen="$seen $earlier"
  sleep 0.1
  start_root "$@"
  await "$wt/job.pid" || { echo "  the root's job never started"; kill -s KILL -- "-$root"; return; }
  job=$(cat "$wt/job.pid")
  # In a pid namespace of its own the job writes its pid there: the pid this probe sees is found by its arguments.
  [ -d /proc/self ] && for p in /proc/[0-9]*; do
    [ "$(tr '\0' ' ' < "$p/cmdline" 2>/dev/null)" = "sleep 301 " ] && job=${p#/proc/}
  done
  seen="$seen $job"
  (cd "$d" && sh "$d/leave.sh" "$d/outside.pid" 303) && await "$d/outside.pid"
  outside=$(cat "$d/outside.pid"); seen="$seen $outside"
  sleep 0.5
  "$d/p452" info "$root" "$since" "$wt" "job" "$job" "earlier_job_in_worktree" "$earlier" \
    "later_job_outside_worktree" "$outside"
  kill -s KILL -- "-$root"
  wait "$sub" 2>/dev/null
  sleep 1
  if alive "$job"; then echo "  1 s after SIGKILL to the root's group: the job still runs"
  else echo "  1 s after SIGKILL to the root's group: the job is gone"; fi
  for p in $job $earlier $outside; do kill -s KILL "$p" 2>/dev/null; done
  rm -f "$d/earlier.pid" "$d/outside.pid"
}

probe "probe 1: unconfined" /bin/sh "$d/root.sh"
case $(uname -s) in
  Darwin) probe "probe 2: under sandbox-exec" /usr/bin/sandbox-exec -p '(version 1)(allow default)' /bin/sh "$d/root.sh" ;;
  *) probe "probe 2: under bwrap with a pid namespace of its own" bwrap --die-with-parent --unshare-pid \
       --ro-bind / / --dev /dev --proc /proc --bind "$d" "$d" --chdir "$wt" /bin/sh "$d/root.sh" ;;
esac

echo "probe 3: reading the working directory of every process"
for k in 1 2 3; do "$d/p452" scan "$wt"; done

if [ "${1:-}" = claude ] && [ "$(uname -s)" = Darwin ]; then
  echo "probe 4: a claude -p run whose Bash call leaves a job and returns, then the run ends by itself"
  echo "  claude=$(claude --version 2>/dev/null | head -1 || echo missing)"
  printf 'sleep 304 >/dev/null 2>&1 &\necho $! > %s/job.pid\n' "$wt" > "$d/orphan.sh"
  start_root claude -p "Use the Bash tool once, not in the background, to run: sh $d/orphan.sh -- then reply with the word done." \
    --max-turns 4 --max-budget-usd 0.50 --allowedTools Bash --output-format json
  i=0; while alive "$root" && [ $i -lt 1200 ]; do sleep 0.1; i=$((i+1)); done
  alive "$root" && { echo "  the run did not end within 120 s"; kill -s KILL -- "-$root"; }
  wait "$sub" 2>/dev/null
  sleep 1
  if [ -s "$wt/job.pid" ]; then
    job=$(cat "$wt/job.pid"); seen="$seen $job"
    "$d/p452" info "$root" "$since" "$wt" "the call's job, 1 s after the run ended" "$job"
  else
    echo "  the Bash call never ran"
  fi
fi
