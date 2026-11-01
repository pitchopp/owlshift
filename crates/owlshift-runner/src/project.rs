//! One project's place in Owlshift's data directory, and the dedicated
//! checkout the ticket worktrees hang off (build plan, OWL-20).
//!
//! The executor's isolation check assumes nobody changes the main checkout
//! during a run, so `owlshift do` never uses the person's own checkout:
//! it clones the project's `origin` under the data directory once, fetches
//! it before each ticket, and creates every worktree from that clone. The
//! person keeps working in their checkout (scenario S12).
//!
//! ```text
//! <data>/projects/github.com/<owner>/<repo>/
//!     checkout/            the dedicated clone: the runs' main checkout
//!     worktrees/<ticket>/  one worktree per ticket
//!     runs/<ticket>/<run>/ brief.json, stdout.log, stderr.log, gate.log
//!     lock                 held by the one `owlshift do` working the project
//!     unverified/          the runs whose isolation check has not passed
//!     quarantine           a breach, or a run left unchecked (OWL-400)
//!     quarantine-told      what was told of the quarantine (OWL-368)
//!     quarantine-comments.json  its WAITING comments not folded (OWL-423)
//!     unfinished-clone     a first clone that has not finished
//!     tickets/<ticket>/    lock, run.json (OWL-400), carry.json and
//!                          outcome-<attempt>.json (OWL-399), holder.json
//!                          and busy-told (OWL-478)
//! ```
//!
//! A first clone stopped part-way, by its deadline, a crash or a Ctrl-C,
//! leaves a folder that looks like a checkout. So the clone is marked
//! `unfinished-clone` until it succeeds, and while the marker exists, the
//! next `owlshift do` removes that folder and clones anew (OWL-80). Unlike
//! the quarantine, it needs no person: a clone runs nothing of the project's.
//!
//! A run that breaks isolation may leave something in the clone, such as a
//! hook or a configuration entry, that the next run's snapshot would take as
//! the norm and the runner's own git would run. So before each run its
//! obligation is written in `unverified/`, and it is removed only once the
//! run's isolation check passed; a breach writes the quarantine, and a crash
//! or a Ctrl-C in the middle of a run leaves the obligation, which the next
//! command or watch pass turns into the quarantine ([`crate::ticket_run`],
//! OWL-400). While the quarantine stands, every command refuses the project
//! until a person looks.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use owlshift_adapters::forge::Repo;
use owlshift_contracts::config::{ProjectConfig, check_requires};
use owlshift_contracts::ids::TicketId;
use owlshift_core::decider::ZoneOwners;
use semver::Version;
use serde_json::{Value, json};

use crate::config::{OWLSHIFT_VERSION, PROJECT_FILE};
use crate::executor::Git;
use crate::rules;
use crate::slots::{remove_stale, write_record};

/// The variables that point git at another repository, index or object
/// store than the one of its working directory. The runner's own git drops
/// them, so a `GIT_DIR` inherited from a hook or a script never turns a
/// command meant for the dedicated checkout onto another repository. The
/// ones that carry the person's authentication, such as `GIT_SSH_COMMAND`,
/// stay: the runner fetches and pushes with the person's own credentials.
pub const REPOSITORY_VARIABLES: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_PREFIX",
];

/// The runner's own git for `owlshift do`: the `git` on the `PATH`, with the
/// runner's environment less [`REPOSITORY_VARIABLES`].
pub fn runner_git() -> Git {
    Git::with_setup("git", drop_repository_variables)
}

fn drop_repository_variables(command: &mut Command) {
    for name in REPOSITORY_VARIABLES {
        command.env_remove(name);
    }
}

/// The URL of the `origin` remote of the repository at `root`.
pub fn origin_url(git: &Git, root: &Path) -> Result<String, String> {
    let url = git
        .run(root, &["remote", "get-url", "origin"])
        .map_err(|_| "the repository has no remote `origin` to clone from".to_owned())?;
    origin_text(url)
}

/// The URL in what `git remote get-url origin` printed.
pub fn origin_text(url: Vec<u8>) -> Result<String, String> {
    let url = String::from_utf8(url)
        .map_err(|_| "the URL of `origin` is not UTF-8 text".to_owned())?
        .trim()
        .to_owned();
    if url.is_empty() {
        return Err("the remote `origin` has no URL".to_owned());
    }
    Ok(url)
}

/// Where one project's files live under the data directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectDirs {
    root: PathBuf,
    /// How long a command waits for the project's lock
    /// ([`ProjectDirs::lock`]): [`PROJECT_LOCK_WAIT`].
    lock_wait: Duration,
}

impl ProjectDirs {
    /// The directory of `repo`, a GitHub repository, under `data_dir`.
    pub fn new(data_dir: &Path, repo: &Repo) -> Self {
        Self {
            root: data_dir
                .join("projects")
                .join("github.com")
                .join(component(repo.owner()))
                .join(component(repo.name())),
            lock_wait: PROJECT_LOCK_WAIT,
        }
    }

    /// The directories of every project under `data_dir`, laid out as
    /// [`Self::new`] lays them out (OWL-462): none when no project has a
    /// folder yet.
    pub fn all(data_dir: &Path) -> io::Result<Vec<Self>> {
        let host = data_dir.join("projects").join("github.com");
        let mut all = Vec::new();
        for owner in subdirectories(&host)? {
            for root in subdirectories(&owner)? {
                all.push(Self {
                    root,
                    lock_wait: PROJECT_LOCK_WAIT,
                });
            }
        }
        Ok(all)
    }

    /// These directories, a command waiting `wait` for the project's lock
    /// before it answers busy: what a test that stands for another command
    /// holding the lock gives its command, rather than the minute every
    /// command waits.
    pub fn with_lock_wait(self, wait: Duration) -> Self {
        Self {
            lock_wait: wait,
            ..self
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The dedicated clone.
    pub fn checkout(&self) -> PathBuf {
        self.root.join("checkout")
    }

    /// Every ticket's worktree lives here.
    pub fn worktrees(&self) -> PathBuf {
        self.root.join("worktrees")
    }

    /// The ticket's worktree.
    pub fn worktree(&self, ticket: &TicketId) -> PathBuf {
        self.worktrees().join(component(ticket.as_str()))
    }

    /// The ticket's run directories live here.
    pub fn runs(&self, ticket: &TicketId) -> PathBuf {
        self.root.join("runs").join(component(ticket.as_str()))
    }

    /// The ticket's own folder, outside its ref (OWL-399): what a step
    /// carries to the next, `carry.json`, and each step's outcome record,
    /// `outcome-<attempt>.json` ([`crate::step`]).
    pub fn ticket_dir(&self, ticket: &TicketId) -> PathBuf {
        self.root.join("tickets").join(component(ticket.as_str()))
    }

    fn lock_file(&self) -> PathBuf {
        self.root.join("lock")
    }

    /// The ticket lock's file, `lock` in the ticket's folder (OWL-409,
    /// architecture section 5, "Locks"). Never removed, so that two
    /// processes always lock the same file.
    pub fn ticket_lock_file(&self, ticket: &TicketId) -> PathBuf {
        self.ticket_dir(ticket).join("lock")
    }

    /// The holder record of a command's ticket lock (OWL-478),
    /// `holder.json` in the ticket's folder ([`ProjectDirs::ticket_lock`]).
    pub fn ticket_holder_file(&self, ticket: &TicketId) -> PathBuf {
        self.ticket_dir(ticket).join(TICKET_HOLDER_FILE)
    }

    /// The mark of a busy refusal told (OWL-478), `busy-told` in the
    /// ticket's folder: made by a step that found the ticket's lock held and
    /// said so (`step::told`), and removed by the next command's take of the
    /// lock and by its end, so the refusal is told once per holder.
    pub fn busy_told_file(&self, ticket: &TicketId) -> PathBuf {
        self.ticket_dir(ticket).join(BUSY_TOLD_FILE)
    }

    /// Takes the lock of `ticket` for a command, held until the returned
    /// value is dropped or the process ends; `None` when another open file
    /// description, in this process or another, still holds it once
    /// [`LOCK_GRACE`] has passed: two commands never work one ticket. A
    /// command takes it before the project's lock and lets it go after
    /// ([`ProjectDirs::lock_for_write`] is watch's reverse order, which never
    /// waits). The bound outlasts the moment a lock just released can still
    /// look held (see [`ProjectDirs::lock`]).
    ///
    /// Who holds it is written beside it (OWL-478), as a run slot's holder
    /// is (`slots.rs`): once the lock is held, a dead holder's record and a
    /// stale [`ProjectDirs::busy_told_file`] are removed and this process's
    /// [`TicketHolder`] is written, best effort; both files are removed
    /// before the lock is let go. Watch's single tries record nothing.
    pub fn ticket_lock(&self, ticket: &TicketId) -> io::Result<Option<TicketLock>> {
        let Some(mut lock) = self.ticket_lock_within(ticket, LOCK_GRACE)? else {
            return Ok(None);
        };
        let record = self.ticket_holder_file(ticket);
        let told = self.busy_told_file(ticket);
        remove_stale(&record);
        remove_stale(&told);
        let holder = TicketHolder {
            ticket: ticket.clone(),
            pid: std::process::id(),
            since: Timestamp::now(),
        };
        write_record(&record, &holder.render());
        lock.left = vec![record, told];
        Ok(Some(lock))
    }

    /// The ticket locks a command holds now in this project (OWL-478), each
    /// as its holder record says, in ticket order. Only a ticket folder
    /// whose record exists is looked at: its lock is tried once, never
    /// waited on, so a held lock is never taken; a free one is a dead
    /// holder's, whose record is removed while the try holds the lock, and
    /// is never read. A record that does not read leaves its holder out.
    pub fn ticket_holders(&self) -> io::Result<Vec<TicketHolder>> {
        let mut held = Vec::new();
        for dir in subdirectories(&self.tickets())? {
            let record = dir.join(TICKET_HOLDER_FILE);
            if !exists(&record)? {
                continue;
            }
            match try_lock_file(&dir.join("lock"))? {
                Some(_free) => remove_stale(&record),
                None => held.extend(
                    fs::read_to_string(&record)
                        .ok()
                        .and_then(|text| TicketHolder::parse(&text)),
                ),
            }
        }
        held.sort_by(|a, b| a.ticket.cmp(&b.ticket));
        Ok(held)
    }

    /// [`ProjectDirs::ticket_lock`] with a single try: what `owlshift
    /// unquarantine` takes to bring a ticket's run file up to date, never
    /// waiting on a command that holds it (OWL-400).
    pub(crate) fn try_ticket_lock(&self, ticket: &TicketId) -> io::Result<Option<TicketLock>> {
        self.ticket_lock_within(ticket, Duration::ZERO)
    }

    /// [`ProjectDirs::ticket_lock`] within `wait`: a single try when zero.
    fn ticket_lock_within(
        &self,
        ticket: &TicketId,
        wait: Duration,
    ) -> io::Result<Option<TicketLock>> {
        fs::create_dir_all(self.ticket_dir(ticket))?;
        Ok(
            lock_file_within(&self.ticket_lock_file(ticket), wait, LOCK_POLL)?.map(|file| {
                TicketLock {
                    left: Vec::new(),
                    _file: file,
                }
            }),
        )
    }

    /// The project's lock, then the lock of `ticket`, each with a single
    /// try: what watch holds for a write of that ticket's ref (OWL-409).
    /// `None` when either is held elsewhere; the project's lock is then let
    /// go at once, so watch's reverse order never waits on a command, and
    /// watch's own takes never wait (OWL-414). Dropped in order: the
    /// project's lock first.
    pub fn lock_for_write(
        &self,
        ticket: &TicketId,
    ) -> io::Result<Option<(ProjectLock, TicketLock)>> {
        let Some(project) = self.try_lock()? else {
            return Ok(None);
        };
        Ok(self
            .ticket_lock_within(ticket, Duration::ZERO)?
            .map(|ticket| (project, ticket)))
    }

    /// The folder of the runs' obligations (OWL-400): one file per run whose
    /// isolation check has not passed ([`crate::ticket_run`]). An Owlshift
    /// before P6 kept a single marker file at this path instead, which
    /// [`ProjectDirs::quarantine`] reads as the quarantine.
    pub fn obligations_dir(&self) -> PathBuf {
        self.root.join("unverified")
    }

    /// The project's quarantine (OWL-400): written by a breach, or by the
    /// first command or watch pass that finds an obligation left behind,
    /// and removed only by `owlshift unquarantine`.
    pub fn quarantine_file(&self) -> PathBuf {
        self.root.join("quarantine")
    }

    /// Every ticket's own folder lives here.
    pub fn tickets(&self) -> PathBuf {
        self.root.join("tickets")
    }

    /// The ticket's run file (OWL-400, [`crate::ticket_run`]).
    pub fn run_file(&self, ticket: &TicketId) -> PathBuf {
        self.ticket_dir(ticket).join("run.json")
    }

    /// Whether the lock of the ticket whose folder is `dir` (one of
    /// [`ProjectDirs::tickets`]) is held elsewhere: one try, let go at once,
    /// a look that never waits (OWL-400). A lock just released can look
    /// held a moment longer (see [`ProjectDirs::lock`]), which reads the
    /// ticket as running for that moment.
    pub fn ticket_lock_held_in(&self, dir: &Path) -> io::Result<bool> {
        Ok(try_lock_file(&dir.join("lock"))?.is_none())
    }

    /// [`ProjectDirs::ticket_lock_held_in`] for `ticket`; a ticket with no
    /// folder has no holder.
    pub fn ticket_lock_held(&self, ticket: &TicketId) -> io::Result<bool> {
        let dir = self.ticket_dir(ticket);
        if !exists(&dir)? {
            return Ok(false);
        }
        self.ticket_lock_held_in(&dir)
    }

    /// Takes the project's lock for one of a command's short sections, held
    /// until the returned value is dropped or the process ends; `None` when
    /// another open file description still holds it once
    /// [`PROJECT_LOCK_WAIT`] has passed (or the wait
    /// [`ProjectDirs::with_lock_wait`] set).
    ///
    /// Since OWL-401 a command holds it for short sections only, never
    /// across a harness run or a gate (architecture section 5, "Locks"):
    /// the look at the quarantine, the fetch and the reads of the ticket ref
    /// and its run file, an admission, a branch's or a worktree's creation
    /// or removal, the delivery's collision check. So a command waits for
    /// it, a bounded loop over a try, through the helper the account lock
    /// shares (`lock_file_within`): a holder that crashed (the kernel drops
    /// its lock) or a child that keeps a lock just released a moment is
    /// outlasted, and a holder that never lets go is answered busy. The lock
    /// belongs to an open file description (`flock` on Unix), so a second
    /// take in the process that holds it waits too: no section is taken
    /// inside another.
    ///
    /// A child spawned while it is held shares it until it execs, and on
    /// macOS a moment longer: a child the standard library starts through
    /// `posix_spawn` can keep the parent's close-on-exec descriptors for
    /// some milliseconds after the spawn returned, even once it ended
    /// (OWL-414; architecture section 5, "A lock just released"), which the
    /// wait outlasts. Watch's own takes never wait
    /// ([`ProjectDirs::try_lock`]).
    ///
    /// A child forked by another thread of this process holds it until it
    /// execs too; only the tests spawn beside a thread that locks, so a test
    /// that takes a lock again runs alone (`alone::run_alone`).
    pub fn lock(&self) -> io::Result<Option<ProjectLock>> {
        self.lock_within(self.lock_wait)
    }

    /// [`ProjectDirs::lock`] with a single try: watch's snapshot and notes,
    /// and a look at whether a run is in flight, never wait. A lock just
    /// released can then look held, which costs watch one pass, as a real
    /// holder does.
    pub fn try_lock(&self) -> io::Result<Option<ProjectLock>> {
        self.lock_within(Duration::ZERO)
    }

    fn lock_within(&self, wait: Duration) -> io::Result<Option<ProjectLock>> {
        fs::create_dir_all(&self.root)?;
        Ok(lock_file_within(&self.lock_file(), wait, LOCK_POLL)?
            .map(|file| ProjectLock { _file: file }))
    }

    /// The account lock's file beside the project's `lock`: it serialises
    /// the refresh of the GitHub App's token and its keychain write
    /// (architecture section 8, "The account lock";
    /// [`crate::github_token`]).
    pub fn account_lock_file(&self) -> PathBuf {
        self.root.join("github-app.lock")
    }

    /// Where the quarantine stands: its text, and the file it is in. The
    /// quarantine file first; else the `unverified` marker an Owlshift
    /// before P6 left, anything at [`ProjectDirs::obligations_dir`] that is
    /// not a folder, which only `owlshift unquarantine` removes (OWL-400).
    /// A file that cannot be read is an error, never taken for none.
    pub fn quarantine_at(&self) -> io::Result<Option<(PathBuf, String)>> {
        let file = self.quarantine_file();
        match fs::read_to_string(&file) {
            Ok(text) => return Ok(Some((file, text))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let legacy = self.obligations_dir();
        match fs::symlink_metadata(&legacy) {
            Ok(meta) if meta.is_dir() => Ok(None),
            Ok(_) => fs::read_to_string(&legacy).map(|text| Some((legacy, text))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// What the quarantine says, when one stands ([`ProjectDirs::quarantine_at`]).
    pub fn quarantine(&self) -> io::Result<Option<String>> {
        Ok(self.quarantine_at()?.map(|(_, text)| text))
    }

    /// Writes `text` as the project's quarantine, or adds it to the one that
    /// stands (OWL-400): a quarantine is never replaced, so a second breach
    /// keeps the first one's words. Returns the text that stands then.
    pub(crate) fn write_quarantine(&self, text: &str) -> io::Result<String> {
        fs::create_dir_all(&self.root)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.quarantine_file())?;
        io::Write::write_all(&mut file, text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::read_to_string(self.quarantine_file())
    }

    /// Removes the quarantine, the `unverified` marker an Owlshift before P6
    /// left, then what was told of either (OWL-368): a record left behind
    /// names another text, so it is read as empty anyway.
    pub(crate) fn clear_quarantine(&self) -> io::Result<()> {
        remove_file_if_any(&self.quarantine_file())?;
        let legacy = self.obligations_dir();
        if fs::symlink_metadata(&legacy).is_ok_and(|meta| !meta.is_dir()) {
            fs::remove_file(&legacy)?;
        }
        let _ = remove_file_if_any(&self.told_file());
        let _ = remove_file_if_any(&self.root.join("unverified-told"));
        Ok(())
    }

    /// What was told of the current quarantine: whether the operator was
    /// notified and which tickets were told (OWL-368,
    /// [`crate::quarantine::Record`]).
    pub fn told_file(&self) -> PathBuf {
        self.root.join("quarantine-told")
    }

    /// The quarantine's WAITING comments watch posted and has not folded
    /// yet, by ticket (OWL-423): watch's alone, and kept by
    /// `owlshift unquarantine`'s clear, so that the next pass folds them.
    pub fn quarantine_comments_file(&self) -> PathBuf {
        self.root.join("quarantine-comments.json")
    }

    /// Whether any obligation stands (OWL-400): any entry in
    /// [`ProjectDirs::obligations_dir`]. A folder that cannot be listed
    /// counts as one standing, so the shared checkout's rules for runs in
    /// flight apply: they only ever write less.
    pub fn obligations_stand(&self) -> bool {
        match fs::read_dir(self.obligations_dir()) {
            Ok(mut entries) => entries.next().is_some(),
            Err(error) => error.kind() != io::ErrorKind::NotFound,
        }
    }

    /// The marker of a first clone that has not finished (OWL-80).
    pub fn unfinished_clone_file(&self) -> PathBuf {
        self.root.join("unfinished-clone")
    }
}

/// How long a command waits for a ticket's lock before it answers busy
/// (OWL-414): about six times the longest moment a lock just released was
/// seen to look held, under load on macOS (see [`ProjectDirs::lock`]).
pub const LOCK_GRACE: Duration = Duration::from_millis(250);

/// How long a command waits for the project's lock before it answers busy
/// (OWL-401): its holders keep it for short sections only, the longest a
/// fetch, so a minute, as the account lock's wait, outlasts them while a
/// holder that never lets go, such as a first clone, still answers busy.
pub const PROJECT_LOCK_WAIT: Duration = Duration::from_secs(60);

/// How often a command tries again within [`LOCK_GRACE`]: the moment it
/// waits out lasts milliseconds, so a lock freed is taken soon after.
const LOCK_POLL: Duration = Duration::from_millis(10);

#[cfg(test)]
thread_local! {
    /// The tries [`try_lock_file`] made on this thread: what tells a single
    /// try from a wait in the tests, with no clock.
    static TRIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Opens the lock file at `path`, creating it, and takes its lock without
/// waiting: the file while it is held, `None` when another open file
/// description holds it (`flock` on Unix, `LockFileEx` on Windows). The
/// locks of the project, the tickets, the account and the run slots share
/// it; see [`ProjectDirs::lock`] for the moment a lock just released can
/// still look held.
pub(crate) fn try_lock_file(path: &Path) -> io::Result<Option<File>> {
    #[cfg(test)]
    TRIES.with(|tries| tries.set(tries.get() + 1));
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

/// [`try_lock_file`], tried again every `poll` until `wait` has passed: a
/// bounded wait, as a loop over the try, so a holder that crashed (the
/// kernel drops its lock) or a child that still holds a lock just released
/// (see [`ProjectDirs::lock`]) is outlasted, and a holder that never lets
/// go is reported. A single try when `wait` is zero; `None` once the wait
/// is spent.
pub(crate) fn lock_file_within(
    path: &Path,
    wait: Duration,
    poll: Duration,
) -> io::Result<Option<File>> {
    retry_within(wait, poll, || try_lock_file(path))
}

/// How long a change of one of the service's small files waits for its lock:
/// the list of served projects (OWL-434) or the install manifest (OWL-437).
const SHARED_FILE_LOCK_WAIT: Duration = Duration::from_secs(10);
/// How often that wait tries the lock.
const SHARED_FILE_LOCK_POLL: Duration = Duration::from_millis(50);

/// Runs `change` holding the lock of the file at `lock`, its folder made
/// first, waited for up to ten seconds: one read-modify-write of a small file
/// several commands change, each replacing it whole. The error says which
/// lock stayed held.
pub(crate) fn under_lock<T>(
    lock: &Path,
    change: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    if let Some(dir) = lock.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let _held = lock_file_within(lock, SHARED_FILE_LOCK_WAIT, SHARED_FILE_LOCK_POLL)
        .map_err(|e| format!("{}: {e}", lock.display()))?
        .ok_or_else(|| {
            format!(
                "{} stayed held for {} seconds",
                lock.display(),
                SHARED_FILE_LOCK_WAIT.as_secs()
            )
        })?;
    change()
}

/// [`lock_file_within`], the lock taken shared (OWL-435): several holders
/// may share it, none while one holds it alone.
pub(crate) fn lock_file_shared_within(
    path: &Path,
    wait: Duration,
    poll: Duration,
) -> io::Result<Option<File>> {
    retry_within(wait, poll, || {
        let file = open_lock_file(path)?;
        match file.try_lock_shared() {
            Ok(()) => Ok(Some(file)),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error),
        }
    })
}

/// Whether a holder has the lock of the file at `path` to itself now
/// (OWL-435): one shared try, let go at once, which a shared holder does not
/// fail. No file is no holder.
pub(crate) fn held_alone(path: &Path) -> io::Result<bool> {
    let file = match OpenOptions::new().write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    match file.try_lock_shared() {
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

/// The folders in `dir`, by name; none when `dir` is not there.
fn subdirectories(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            found.push(entry.path());
        }
    }
    found.sort();
    Ok(found)
}

/// Opens the lock file at `path`, creating it and its folder.
fn open_lock_file(path: &Path) -> io::Result<File> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
}

/// Calls `try_once` again every `poll` until it gives a file or `wait` has
/// passed.
fn retry_within(
    wait: Duration,
    poll: Duration,
    mut try_once: impl FnMut() -> io::Result<Option<File>>,
) -> io::Result<Option<File>> {
    let started = Instant::now();
    loop {
        if let Some(file) = try_once()? {
            return Ok(Some(file));
        }
        let waited = started.elapsed();
        if waited >= wait {
            return Ok(None);
        }
        thread::sleep(poll.min(wait - waited));
    }
}

/// Removes the file at `path`, if there is one.
fn remove_file_if_any(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Whether anything is at `path`, a symbolic link included. Only a missing
/// entry means no: any other error is returned, never taken for absence.
fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// The project's lock: see [`ProjectDirs::lock`].
#[derive(Debug)]
pub struct ProjectLock {
    _file: File,
}

/// A ticket's lock: see [`ProjectDirs::ticket_lock`].
#[derive(Debug)]
pub struct TicketLock {
    /// What a command's take wrote beside the lock, its holder record and
    /// the busy mark it cleared: removed before the lock is let go.
    left: Vec<PathBuf>,
    _file: File,
}

impl Drop for TicketLock {
    /// Removes the holder record and the busy mark while the lock is still
    /// held, so it never removes the next holder's; the lock goes after,
    /// with the file.
    fn drop(&mut self) {
        for path in &self.left {
            let _ = fs::remove_file(path);
        }
    }
}

/// The holder record's name in a ticket's folder (OWL-478).
const TICKET_HOLDER_FILE: &str = "holder.json";

/// The busy mark's name in a ticket's folder (OWL-478).
const BUSY_TOLD_FILE: &str = "busy-told";

/// Who holds a ticket's lock for a command (OWL-478), as its holder record,
/// `holder.json` beside the lock, says: what `owlshift status` shows of a
/// step no run slot names, such as one a person typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TicketHolder {
    pub ticket: TicketId,
    /// The holding process.
    pub pid: u32,
    /// When it took the lock.
    pub since: Timestamp,
}

impl TicketHolder {
    fn render(&self) -> String {
        json!({
            "ticket": self.ticket.as_str(),
            "pid": self.pid,
            "since": self.since.to_string(),
        })
        .to_string()
    }

    /// The record read back; `None` when it is not one this binary reads.
    fn parse(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        let field = |key: &str| value.get(key).and_then(Value::as_str);
        Some(Self {
            ticket: TicketId::new(field("ticket")?).ok()?,
            pid: u32::try_from(value.get("pid")?.as_u64()?).ok()?,
            since: field("since")?.parse().ok()?,
        })
    }
}

/// A path component every platform takes as is, and the same whatever the
/// case of the name: lower case, with `_` added after a Windows device name
/// (`con`, `nul`, `com1`…) or a trailing dot.
pub(crate) fn component(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    let stem = lower.split('.').next().unwrap_or_default();
    let numbered = |prefix: &str| {
        stem.strip_prefix(prefix)
            .is_some_and(|n| n.len() == 1 && n.as_bytes()[0].is_ascii_digit())
    };
    let device =
        matches!(stem, "con" | "prn" | "aux" | "nul") || numbered("com") || numbered("lpt");
    if device || lower.ends_with('.') {
        format!("{lower}_")
    } else {
        lower
    }
}

/// The branch new ticket branches start from, as the dedicated checkout
/// knows it after a fetch. [`Base::ancestor`] also names an older commit
/// of that branch, the one a ticket's branch was made from, which a run's
/// project file and rules are then read at (OWL-331): such a value is
/// read from, never started from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Base {
    /// The remote-tracking name, such as `origin/main`, for messages and
    /// events only: a run can move a remote-tracking ref (OWL-51), so a later
    /// step is given `commit`, never this name.
    pub remote_ref: String,
    /// The branch on the forge, such as `main`: the pull request's base.
    pub branch: String,
    /// The commit `refs/remotes/origin/<branch>` points to right after the
    /// fetch, resolved once through that full name: what new branches start
    /// from and what the project's rules are read at.
    pub commit: String,
}

impl Base {
    /// `commit`, an ancestor of this base's commit, named in messages as
    /// this base at that commit (`origin/main at 0123456789ab`).
    pub fn ancestor(&self, commit: &str) -> Base {
        let short = commit.get(..12).unwrap_or(commit);
        Base {
            remote_ref: format!("{} at {short}", self.remote_ref),
            branch: self.branch.clone(),
            commit: commit.to_owned(),
        }
    }
}

/// How long the first clone of a project may take (OWL-60): a large
/// repository's whole history crosses the network once, and can outlive the
/// runner git's 120 s deadline on a slow link. The fetch of a later `do` is
/// incremental and keeps the short deadline, so a stalled network still stops
/// it fast.
pub const CLONE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// What the `unfinished-clone` marker says to a person who finds it.
const UNFINISHED_CLONE: &str = "A first clone of the project started and has not finished: \
     the next `owlshift do` removes `checkout` and clones the project anew.\n";

/// How long removing a clone that did not finish keeps trying (OWL-80). On
/// Windows, the files of a git just stopped can stay locked for a moment
/// after its Job Object is terminated; on any platform, a child still dying
/// can add a file while the folder is emptied.
const REMOVAL_BUDGET: Duration = Duration::from_secs(10);

/// Removes the folder of a clone that did not finish, if there is one,
/// trying again with a growing pause until `budget` has passed; the error is
/// the last attempt's.
fn remove_partial_clone(checkout: &Path, budget: Duration) -> io::Result<()> {
    let deadline = Instant::now() + budget;
    let mut pause = Duration::from_millis(50);
    loop {
        let error = match fs::remove_dir_all(checkout) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => error,
        };
        if Instant::now() + pause > deadline {
            return Err(error);
        }
        thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_secs(1));
    }
}

/// Brings the dedicated checkout up to date: clones `remote_url` into it the
/// first time, and otherwise points its `origin` at `remote_url`, fetches,
/// pruning what the forge deleted, and reads the forge's default branch
/// again, since a project can change it. Worktrees whose folder was removed
/// are forgotten. Returns `origin`'s default branch, which a clone records
/// as `refs/remotes/origin/HEAD` (checked with git 2.54 and on GitHub on
/// 2026-09-29) and `git remote set-head origin --auto` refreshes, with the
/// commit it points to after the fetch.
///
/// The first clone runs under [`CLONE_TIMEOUT`]; every other command runs
/// under the runner git's own deadline, 120 s. It is marked `unfinished-clone`
/// until it succeeds (OWL-80): a failed clone's folder is removed, trying
/// again for up to `REMOVAL_BUDGET`, and while the marker exists, the next
/// call removes whatever is left in `checkout` before cloning anew, or
/// refuses when it cannot. The marker relies on the project lock, which
/// `owlshift do` holds around this call: no other clone of the project runs.
pub fn sync_checkout(git: &Git, dirs: &ProjectDirs, remote_url: &str) -> Result<Base, String> {
    sync_checkout_within(git, dirs, remote_url, CLONE_TIMEOUT)
}

/// [`sync_checkout`] with `clone_timeout` as the first clone's deadline: for
/// a test that cannot wait [`CLONE_TIMEOUT`].
pub fn sync_checkout_within(
    git: &Git,
    dirs: &ProjectDirs,
    remote_url: &str,
    clone_timeout: Duration,
) -> Result<Base, String> {
    // Before anything in the checkout is touched (OWL-205).
    git.require_minimum_version()?;
    let checkout = dirs.checkout();
    let marker = dirs.unfinished_clone_file();
    let failed = |e: crate::executor::GitError| e.to_string();
    let io_failed = |path: &Path, e: io::Error| format!("{}: {e}", path.display());
    if exists(&marker).map_err(|e| io_failed(&marker, e))? {
        remove_partial_clone(&checkout, REMOVAL_BUDGET).map_err(|e| {
            format!(
                "{} holds a clone of the project that did not finish, and it could not be \
                 removed ({e}); remove it and run again",
                checkout.display()
            )
        })?;
    }
    if !exists(&checkout).map_err(|e| io_failed(&checkout, e))? {
        fs::create_dir_all(dirs.root()).map_err(|e| io_failed(dirs.root(), e))?;
        fs::write(&marker, UNFINISHED_CLONE).map_err(|e| io_failed(&marker, e))?;
        let args: [&OsStr; 5] = [
            "clone".as_ref(),
            "--quiet".as_ref(),
            "--".as_ref(),
            remote_url.as_ref(),
            "checkout".as_ref(),
        ];
        git.run_within(dirs.root(), &args, clone_timeout)
            .map_err(|e| {
                let mut message = format!(
                    "could not clone the project into {}: {}",
                    checkout.display(),
                    e.detail
                );
                // The marker stays whatever happens here, so the next run
                // removes what is left rather than taking it for the
                // project's checkout.
                if let Err(error) = remove_partial_clone(&checkout, REMOVAL_BUDGET) {
                    message.push_str(&format!(
                        "; the partial clone could not be removed ({error}), the next run removes it"
                    ));
                }
                message
            })?;
        remove_file_if_any(&marker).map_err(|e| io_failed(&marker, e))?;
    } else {
        // Runs in flight read what this writes (OWL-400, live check (f)):
        // `set-url` takes `config.lock`, which a second writer at the same
        // moment fails on, so it runs only when the address differs; a tag
        // is a ref their checks guard, so none is fetched while one stands.
        let in_flight = dirs.obligations_stand();
        let usable = |e: crate::executor::GitError| {
            format!(
                "{} is not a usable clone ({}); remove it and run again to clone the project anew",
                checkout.display(),
                e.detail
            )
        };
        let current = git
            .run(&checkout, &["config", "--get", "remote.origin.url"])
            .ok()
            .map(|url| String::from_utf8_lossy(&url).trim_end().to_owned());
        if current.as_deref() != Some(remote_url) {
            let args: [&OsStr; 4] = [
                "remote".as_ref(),
                "set-url".as_ref(),
                "origin".as_ref(),
                remote_url.as_ref(),
            ];
            git.run(&checkout, &args).map_err(usable)?;
        }
        let fetch: &[&str] = if in_flight {
            &["fetch", "--quiet", "--no-tags", "--prune", "origin"]
        } else {
            &["fetch", "--quiet", "--prune", "origin"]
        };
        git.run(&checkout, fetch).map_err(failed)?;
        set_head(git, &checkout)?;
    }
    // `worktree prune` removes entries a run in flight's check reads: it
    // waits for a call with no obligation standing (OWL-400).
    if !dirs.obligations_stand() {
        git.run(&checkout, &["worktree", "prune"]).map_err(failed)?;
    }

    // The full name: `--short` would answer `remotes/origin/main` when a
    // local branch `origin/main` exists (git 2.54, 2026-09-29). One level
    // only: `set-head` has just written it.
    let head = [
        "symbolic-ref",
        "--quiet",
        "--no-recurse",
        "refs/remotes/origin/HEAD",
    ];
    let target = match git.run(&checkout, &head) {
        Ok(name) => name,
        // A clone of a repository without a default branch records none.
        Err(_) => {
            set_head(git, &checkout)?;
            git.run(&checkout, &head).map_err(failed)?
        }
    };
    // Resolved now, right after the fetch reset every remote-tracking ref a
    // run could have moved (OWL-51).
    resolve_origin_ref(git, &checkout, String::from_utf8_lossy(&target).trim())
}

/// The base `target`, the full name `refs/remotes/origin/HEAD` points to,
/// names in the repository at `checkout`: its branch of origin and the
/// commit it is at, resolved by that full name, which no local branch
/// shadows (OWL-51). A run can make that ref symbolic, pointing at another
/// branch of origin: a fetch keeps such a ref, and git follows it (git
/// 2.54, 2026-09-29), so it is refused. Shared by [`sync_checkout`] and
/// [`project_file_at_origin_head`].
fn resolve_origin_ref(git: &Git, checkout: &Path, target: &str) -> Result<Base, String> {
    let failed = |e: crate::executor::GitError| e.to_string();
    let branch = target
        .strip_prefix("refs/remotes/origin/")
        .filter(|branch| !branch.is_empty())
        .ok_or_else(|| format!("origin's default branch is {target:?}, not a branch of origin"))?
        .to_owned();
    let symbolic = git
        .output(checkout, &["symbolic-ref", "--quiet", target], None)
        .map_err(failed)?
        .success();
    if symbolic {
        return Err(format!(
            "{target} in {} is a symbolic ref, not a branch fetched from origin; \
             remove it with `git update-ref --no-deref -d {target}` there and run again",
            checkout.display()
        ));
    }
    let full = format!("{target}^{{commit}}");
    let commit = git
        .run(
            checkout,
            &["rev-parse", "--verify", "--end-of-options", full.as_str()],
        )
        .map_err(|e| format!("origin's default branch {target} does not resolve: {e}"))?;
    let commit = String::from_utf8_lossy(&commit).trim().to_owned();
    Ok(Base {
        remote_ref: format!("origin/{branch}"),
        branch,
        commit,
    })
}

/// The project file at the default branch, as the person's own checkout at
/// `root` last fetched it (OWL-332): what `owlshift doctor` checks the gate
/// of, since `owlshift do` reads the file at that branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefaultBranchFile {
    /// The default branch is not known here, for this reason: no
    /// `refs/remotes/origin/HEAD`, or one that does not resolve to a branch
    /// of origin.
    Unknown(String),
    /// The default branch's commit, and its project file, `None` when it
    /// has none.
    Read {
        base: Base,
        file: Option<Box<ProjectConfig>>,
    },
}

/// The project file at `refs/remotes/origin/HEAD` of the checkout at
/// `root`, as its last fetch left that ref (OWL-332), read as
/// [`project_file_at`] reads it: nothing is fetched, written or asked of
/// the network, so a stale checkout gives what its last fetch saw. A
/// default branch that cannot be resolved is [`DefaultBranchFile::Unknown`];
/// only a file `project_file_at` refuses is an error.
pub fn project_file_at_origin_head(git: &Git, root: &Path) -> Result<DefaultBranchFile, String> {
    let head = [
        "symbolic-ref",
        "--quiet",
        "--no-recurse",
        "refs/remotes/origin/HEAD",
    ];
    let unknown = |reason: String| Ok(DefaultBranchFile::Unknown(reason));
    let target = match git.output(root, &head, None) {
        Ok(out) if out.success() => String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        Ok(_) => return unknown("`origin/HEAD` is not set".to_owned()),
        Err(e) => return unknown(format!("`origin/HEAD` could not be read: {e}")),
    };
    let base = match resolve_origin_ref(git, root, &target) {
        Ok(base) => base,
        Err(reason) => return unknown(reason),
    };
    let file = project_file_at(git, root, &base)?.map(Box::new);
    Ok(DefaultBranchFile::Read { base, file })
}

/// The most bytes of the project file read at the base commit.
pub(crate) const MAX_PROJECT_FILE_BYTES: u64 = 64 * 1024;

/// The zone owners of the project file as the base commit holds it, read in
/// the dedicated `checkout` as the project's rules are
/// ([`rules::project_rules`]): never the person's checkout, which may differ
/// from what the forge's default branch holds, nor a worktree, which agents
/// write, so a run cannot choose who instructs it (architecture section 4).
/// No project file at the base means no owners. A file over 64 KiB, not
/// text, requiring a newer Owlshift or invalid is an error naming it.
pub fn zone_owners_at(git: &Git, checkout: &Path, base: &Base) -> Result<ZoneOwners, String> {
    match project_file_at(git, checkout, base)? {
        Some(config) => config
            .zone_owners()
            .map_err(|e| format!("{PROJECT_FILE} on {}: {e}", base.remote_ref)),
        None => Ok(ZoneOwners::default()),
    }
}

/// The `[tracker].admitters` of the project file as the base commit holds
/// it, read as [`zone_owners_at`] reads the owners, for the same reason: a
/// run cannot choose who admits (architecture section 5, "By whom"). `None`
/// when the file, or its key, is absent.
pub fn admitters_at(
    git: &Git,
    checkout: &Path,
    base: &Base,
) -> Result<Option<Vec<String>>, String> {
    Ok(project_file_at(git, checkout, base)?.and_then(|config| config.tracker.admitters))
}

/// The `[forge].reviewers` of the project file as the base commit holds it
/// (OWL-307, architecture section 5, "Review comments on the pull
/// request"), read as [`admitters_at`] reads the admitters, for the same
/// reason: a run cannot choose whose review instructs it. `None` when the
/// file, its `[forge]` table or its reviewers are absent (a table may set
/// only merge on green, D14): no review is read.
pub fn forge_reviewers_at(
    git: &Git,
    checkout: &Path,
    base: &Base,
) -> Result<Option<BTreeMap<String, String>>, String> {
    Ok(project_file_at(git, checkout, base)?
        .and_then(|config| config.forge)
        .map(|forge| forge.reviewers)
        .filter(|reviewers| !reviewers.is_empty()))
}

/// The `[forge] merge` and `merge_method` of the project file as the base
/// commit holds it (D14, architecture section 5, "Merge on green"), read as
/// [`forge_reviewers_at`] reads the reviewers, for the same reason: a
/// ticket's own branch cannot turn merge on green on. `never` and `squash`
/// when the file, its `[forge]` table or the keys are absent.
pub fn forge_merge_at(
    git: &Git,
    checkout: &Path,
    base: &Base,
) -> Result<
    (
        owlshift_core::vocab::MergeSwitch,
        owlshift_core::vocab::MergeMethod,
    ),
    String,
> {
    Ok(project_file_at(git, checkout, base)?
        .and_then(|config| config.forge)
        .map(|forge| (forge.merge(), forge.merge_method()))
        .unwrap_or_default())
}

/// The `policy.overlap` of the project file as the base commit holds it
/// (OWL-449, architecture section 5, "Tolerated overlap"), read as
/// [`forge_merge_at`] reads the merge switch, for the same reason: a
/// ticket's own branch cannot turn it on. `exclusive` when the file or the
/// key is absent.
pub fn overlap_at(
    git: &Git,
    checkout: &Path,
    base: &Base,
) -> Result<owlshift_core::vocab::Overlap, String> {
    Ok(project_file_at(git, checkout, base)?
        .map(|config| config.policy.overlap())
        .unwrap_or_default())
}

#[cfg(test)]
mod forge_merge_tests {
    use owlshift_core::vocab::{MergeMethod, MergeSwitch};

    use super::*;

    /// A project file whose `[forge]` table is `forge`.
    fn project_file(forge: &str) -> String {
        format!(
            "requires = \">=0.0\"\n[tracker]\nkind = \"markdown\"\nadmit = \"delegation\"\n\
             states = {{ ready = \"a\", working = \"b\", needs_input = \"c\", review = \"d\" }}\n\
             [stack]\ngate = []\n[pipeline]\ndefault = \"trivial\"\n\
             plan_approval = \"never\"\n[models]\n[policy]\nalways_human = []\n[forge]\n{forge}"
        )
    }

    /// Commits `text` as the project file in `repo` and gives the commit.
    fn commit(git: &Git, repo: &Path, text: &str) -> String {
        fs::write(repo.join(PROJECT_FILE), text).unwrap();
        let identity = [
            "-c",
            "user.name=Bench",
            "-c",
            "user.email=bench@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ];
        git.run(repo, &["add", PROJECT_FILE]).unwrap();
        let mut args = identity.to_vec();
        args.extend(["commit", "-q", "-m", "project file"]);
        git.run(repo, &args).unwrap();
        let head = git.run(repo, &["rev-parse", "HEAD"]).unwrap();
        String::from_utf8(head).unwrap().trim().to_owned()
    }

    /// A branch that turns merge on green on changes nothing for its own
    /// ticket: the switch and the method are read at the base commit.
    #[test]
    fn a_branch_cannot_turn_merge_on_green_on() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let git = runner_git();
        git.run(repo, &["init", "-q"]).unwrap();
        let base_commit = commit(
            &git,
            repo,
            &project_file("reviewers = { ada = \"u-ann\" }\n"),
        );
        git.run(repo, &["checkout", "-q", "-b", "owl-1"]).unwrap();
        let branch_commit = commit(
            &git,
            repo,
            &project_file("merge = \"all\"\nmerge_method = \"rebase\"\n"),
        );
        let at = |commit: &str| Base {
            remote_ref: "origin/main".to_owned(),
            branch: "main".to_owned(),
            commit: commit.to_owned(),
        };
        assert_eq!(
            forge_merge_at(&git, repo, &at(&base_commit)),
            Ok((MergeSwitch::Never, MergeMethod::Squash))
        );
        assert_eq!(
            forge_merge_at(&git, repo, &at(&branch_commit)),
            Ok((MergeSwitch::All, MergeMethod::Rebase))
        );
        // A table that sets only the merge lists no reviewer.
        assert_eq!(
            forge_reviewers_at(&git, repo, &at(&branch_commit)),
            Ok(None)
        );
    }

    /// OWL-449: a branch that turns `policy.overlap` on changes nothing for
    /// its own ticket: the switch is read at the base commit, exclusive
    /// when absent.
    #[test]
    fn a_branch_cannot_turn_the_overlap_on() {
        use owlshift_core::vocab::Overlap;
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let git = runner_git();
        git.run(repo, &["init", "-q"]).unwrap();
        let file = project_file("merge = \"never\"\n");
        let base_commit = commit(&git, repo, &file);
        git.run(repo, &["checkout", "-q", "-b", "owl-1"]).unwrap();
        let rebase = file.replacen(
            "always_human = []",
            "always_human = []\noverlap = \"rebase\"",
            1,
        );
        let branch_commit = commit(&git, repo, &rebase);
        let at = |commit: &str| Base {
            remote_ref: "origin/main".to_owned(),
            branch: "main".to_owned(),
            commit: commit.to_owned(),
        };
        assert_eq!(
            overlap_at(&git, repo, &at(&base_commit)),
            Ok(Overlap::Exclusive)
        );
        assert_eq!(
            overlap_at(&git, repo, &at(&branch_commit)),
            Ok(Overlap::Rebase)
        );
    }
}

/// The project file at `base.commit`, parsed, `None` when that commit has
/// none: read in the dedicated `checkout` as the rules are, a file over
/// 64 KiB, not text, requiring a newer Owlshift or invalid being an error
/// naming it. `base` is the fetched base, or an ancestor of it
/// ([`Base::ancestor`]) such as the commit a ticket's branch was made from,
/// whose gate and rules a run of that branch takes (OWL-331): any commit
/// the default branch held, never a ticket's branch, which agents write.
pub fn project_file_at(
    git: &Git,
    checkout: &Path,
    base: &Base,
) -> Result<Option<ProjectConfig>, String> {
    let file = PROJECT_FILE;
    let Some(text) = rules::text_at_base(
        git,
        checkout,
        base,
        file,
        MAX_PROJECT_FILE_BYTES,
        "the project file",
    )?
    else {
        return Ok(None);
    };
    let current = Version::parse(OWLSHIFT_VERSION).expect("the crate version is semver");
    check_requires(&text, &current)
        .and_then(|()| ProjectConfig::parse(&text))
        .map(Some)
        .map_err(|e| format!("{file} on {}: {e}", base.remote_ref))
}

/// `git remote set-head origin --auto`: asks the forge for its default
/// branch and records it. A network call; its failure stops `owlshift do`
/// rather than leaving a stale base in place.
fn set_head(git: &Git, checkout: &Path) -> Result<(), String> {
    git.run(checkout, &["remote", "set-head", "origin", "--auto"])
        .map(drop)
        .map_err(|e| format!("could not read origin's default branch: {}", e.detail))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_folders_are_portable_and_case_blind() {
        let data = Path::new("data");
        let dirs = ProjectDirs::new(data, &Repo::parse("Pitchopp/OwlShift").unwrap());
        assert_eq!(
            dirs.root(),
            Path::new("data/projects/github.com/pitchopp/owlshift")
        );
        let ticket = TicketId::new("OWL-12").unwrap();
        assert_eq!(dirs.worktree(&ticket), dirs.root().join("worktrees/owl-12"));
        assert_eq!(dirs.runs(&ticket), dirs.root().join("runs/owl-12"));

        for (name, expected) in [
            ("CON", "con_"),
            ("nul.txt", "nul.txt_"),
            ("com1", "com1_"),
            ("LPT9", "lpt9_"),
            ("com10", "com10"),
            ("console", "console"),
            ("name.", "name._"),
            ("owlshift", "owlshift"),
        ] {
            assert_eq!(component(name), expected, "{name}");
        }
    }

    /// OWL-478: a command's ticket lock names its holder until it ends, and
    /// clears the busy mark; a record under a free lock, a killed holder's,
    /// is never read and is removed; watch's single try records nothing, and
    /// no other file of the ticket's folder is touched.
    #[test]
    fn a_commands_ticket_lock_names_its_holder_until_it_ends() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = ProjectDirs::new(tmp.path(), &Repo::parse("acme/web").unwrap());
        let ticket = TicketId::new("OWL-7").unwrap();
        assert_eq!(dirs.ticket_holders().unwrap(), []);

        fs::create_dir_all(dirs.ticket_dir(&ticket)).unwrap();
        let other = dirs.ticket_dir(&ticket).join("cancel");
        fs::write(&other, "").unwrap();
        fs::write(dirs.busy_told_file(&ticket), "").unwrap();
        let held = dirs.ticket_lock(&ticket).unwrap().expect("a free ticket");
        assert!(
            !dirs.busy_told_file(&ticket).exists(),
            "cleared at the take"
        );
        let holders = dirs.ticket_holders().unwrap();
        assert_eq!(holders.len(), 1, "{holders:?}");
        assert_eq!(holders[0].ticket, ticket);
        assert_eq!(holders[0].pid, std::process::id());
        fs::write(dirs.busy_told_file(&ticket), "").unwrap();
        drop(held);
        assert!(!dirs.ticket_holder_file(&ticket).exists());
        assert!(!dirs.busy_told_file(&ticket).exists(), "cleared at the end");
        assert!(other.exists(), "another file of the folder stays");
        assert_eq!(dirs.ticket_holders().unwrap(), []);

        // A holder killed: its record outlives its lock.
        let dead = TicketHolder {
            ticket: ticket.clone(),
            pid: 1,
            since: Timestamp::UNIX_EPOCH,
        };
        fs::write(dirs.ticket_holder_file(&ticket), dead.render()).unwrap();
        assert_eq!(dirs.ticket_holders().unwrap(), []);
        assert!(!dirs.ticket_holder_file(&ticket).exists(), "removed");

        let written = dirs.lock_for_write(&ticket).unwrap().expect("free");
        assert!(!dirs.ticket_holder_file(&ticket).exists(), "watch's try");
        drop(written);
        assert!(other.exists());
    }

    /// The lock is checked alone, in a fresh copy of this test binary
    /// (`alone::run_alone`): in this process, other tests spawn children
    /// (`git`, `sh`), and a child spawned while a lock is held keeps it until
    /// it execs (see [`ProjectDirs::lock`]), so a lock released then looked
    /// held (seen on macOS on 2026-09-29). Alone, nothing else spawns.
    #[test]
    fn one_do_at_a_time_per_project() {
        crate::alone::run_alone("project::tests::helper_one_do_at_a_time_per_project");
    }

    /// The tries [`try_lock_file`] makes on this thread while `take` runs.
    fn tries<T>(take: impl FnOnce() -> T) -> (T, usize) {
        let before = TRIES.with(|tries| tries.get());
        let taken = take();
        (taken, TRIES.with(|tries| tries.get()) - before)
    }

    #[test]
    #[ignore = "helper, run by the test above"]
    fn helper_one_do_at_a_time_per_project() {
        if !crate::alone::running_alone() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::parse("demo/project").unwrap();
        assert_eq!(
            ProjectDirs::new(dir.path(), &repo).lock_wait,
            PROJECT_LOCK_WAIT
        );
        let dirs = ProjectDirs::new(dir.path(), &repo).with_lock_wait(LOCK_GRACE);
        let held = dirs.lock().unwrap().expect("the first lock is free");
        // A command waits for the bound before it answers busy (OWL-414,
        // OWL-401: a minute, shortened here); watch tries once, counted
        // rather than timed.
        let started = Instant::now();
        assert!(dirs.lock().unwrap().is_none(), "a second lock waits");
        assert!(started.elapsed() >= LOCK_GRACE);
        let (second, tried) = tries(|| dirs.try_lock().unwrap());
        assert!(second.is_none() && tried == 1, "{tried}");
        drop(held);
        assert!(dirs.lock().unwrap().is_some(), "free again once released");

        // The account lock (OWL-384): the same helper, waited for within a
        // bound, never past it, and taken as soon as it is free.
        let account = dirs.account_lock_file();
        assert_eq!(account, dirs.root().join("github-app.lock"));
        let poll = Duration::from_millis(10);
        let held = lock_file_within(&account, Duration::ZERO, poll)
            .unwrap()
            .expect("the account lock is free");
        let started = Instant::now();
        let wait = Duration::from_millis(100);
        assert!(lock_file_within(&account, wait, poll).unwrap().is_none());
        let waited = started.elapsed();
        assert!(
            waited >= wait && waited < Duration::from_secs(5),
            "{waited:?}"
        );
        let release = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            drop(held);
        });
        assert!(
            lock_file_within(&account, Duration::from_secs(5), poll)
                .unwrap()
                .is_some(),
            "taken once released"
        );
        release.join().unwrap();
        let project = dirs.lock().unwrap().expect("the two locks are apart");

        // A ticket's lock (OWL-409): one holder, the ticket's own file,
        // the same whatever the case of its id, apart from the project's.
        let ticket = TicketId::new("OWL-1").unwrap();
        let other = TicketId::new("OWL-2").unwrap();
        assert_eq!(
            dirs.ticket_lock_file(&ticket),
            dirs.root().join("tickets/owl-1/lock")
        );
        let held = dirs.ticket_lock(&ticket).unwrap().expect("a free ticket");
        let lower = TicketId::new("owl-1").unwrap();
        let started = Instant::now();
        assert!(dirs.ticket_lock(&lower).unwrap().is_none(), "one holder");
        assert!(started.elapsed() >= LOCK_GRACE);
        assert!(dirs.ticket_lock(&other).unwrap().is_some(), "per ticket");
        // Watch's reverse order: nothing while the project or the ticket is
        // held elsewhere, one try each, and the project's lock let go when
        // the ticket's was busy.
        let (busy, tried) = tries(|| dirs.lock_for_write(&other).unwrap());
        assert!(busy.is_none() && tried == 1, "project held: {tried}");
        drop(project);
        let (busy, tried) = tries(|| dirs.lock_for_write(&ticket).unwrap());
        assert!(busy.is_none() && tried == 2, "ticket held: {tried}");
        assert!(dirs.lock().unwrap().is_some(), "the project's lock let go");
        drop(held);
        let both = dirs.lock_for_write(&ticket).unwrap().expect("both free");
        assert!(dirs.ticket_lock(&ticket).unwrap().is_none());
        assert!(dirs.lock().unwrap().is_none());
        drop(both);
        // A ticket whose folder is not made yet: watch makes it.
        let new = TicketId::new("OWL-3").unwrap();
        assert!(!dirs.ticket_dir(&new).exists());
        assert!(dirs.lock_for_write(&new).unwrap().is_some(), "made");

        // Close-on-exec (architecture section 5, "Locks", check (e)): a
        // child that execs while the lock is held does not keep it. On macOS
        // a child `posix_spawn` started can keep it a moment after the spawn
        // returned (OWL-414), so the release is waited for, while the child
        // still runs: one that inherited the file would hold it to its end.
        #[cfg(unix)]
        {
            let held = dirs.ticket_lock(&ticket).unwrap().expect("free again");
            let mut child = Command::new("sleep").arg("30").spawn().unwrap();
            drop(held);
            let file = dirs.ticket_lock_file(&ticket);
            let free = lock_file_within(&file, Duration::from_secs(5), poll)
                .unwrap()
                .is_some();
            let running = child.try_wait().unwrap().is_none();
            let _ = child.kill();
            let _ = child.wait();
            assert!(running, "the child ended before the check");
            assert!(free, "a child that exec'd kept the ticket's lock");
        }
    }

    /// The quarantine is written, added to, read and cleared; the marker of
    /// an Owlshift before P6, a file where the obligations' folder goes,
    /// reads as the quarantine until the clear removes it (OWL-400).
    #[test]
    fn the_quarantine_is_written_added_to_read_and_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = ProjectDirs::new(dir.path(), &Repo::parse("demo/project").unwrap());
        assert_eq!(dirs.quarantine().unwrap(), None);
        assert!(!dirs.obligations_stand());
        let first = "Run r1 of OWL-1 broke isolation:\n- x\n";
        assert_eq!(dirs.write_quarantine(first).unwrap(), first);
        let both = dirs
            .write_quarantine("Run r2 of OWL-2 broke isolation:\n- y\n")
            .unwrap();
        assert!(both.starts_with(first) && both.ends_with("- y\n"), "{both}");
        assert_eq!(dirs.quarantine().unwrap().as_deref(), Some(both.as_str()));
        fs::write(dirs.told_file(), "told").unwrap();
        dirs.clear_quarantine().unwrap();
        dirs.clear_quarantine().unwrap();
        assert_eq!(dirs.quarantine().unwrap(), None);
        assert!(!dirs.told_file().exists());

        fs::write(dirs.obligations_dir(), "Run r0 of OWL-1 started at …\n").unwrap();
        assert_eq!(
            dirs.quarantine_at().unwrap(),
            Some((
                dirs.obligations_dir(),
                "Run r0 of OWL-1 started at …\n".to_owned()
            ))
        );
        dirs.clear_quarantine().unwrap();
        assert_eq!(dirs.quarantine().unwrap(), None);
        fs::create_dir_all(dirs.obligations_dir()).unwrap();
        assert!(!dirs.obligations_stand());
        fs::write(dirs.obligations_dir().join("owl-1-r1"), "{}").unwrap();
        assert!(dirs.obligations_stand());
        assert_eq!(dirs.quarantine().unwrap(), None, "a folder is no marker");
    }

    /// A partial clone that stays in the way is reported once the budget is
    /// spent, not waited on forever: on Unix, a file where the folder should
    /// be; on Windows, a file inside it held open without delete sharing.
    #[test]
    fn a_partial_clone_that_stays_in_the_way_is_reported_within_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("checkout");
        #[cfg(unix)]
        fs::write(&checkout, "").unwrap();
        #[cfg(windows)]
        let _held = held_open(&checkout);
        let started = Instant::now();
        assert!(remove_partial_clone(&checkout, Duration::from_millis(200)).is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// The case the retry is for (OWL-80): a stopped git's file stays locked
    /// for a moment, which a single attempt does not get past.
    #[cfg(windows)]
    #[test]
    fn a_partial_clone_locked_for_a_moment_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("checkout");
        let held = held_open(&checkout);
        assert!(fs::remove_dir_all(&checkout).is_err(), "not locked");
        let release = thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        remove_partial_clone(&checkout, Duration::from_secs(10)).unwrap();
        release.join().unwrap();
        assert!(fs::symlink_metadata(&checkout).is_err());
    }

    /// A `checkout` folder with a file in its `.git` held open without
    /// delete sharing, as a git still being stopped holds it.
    #[cfg(windows)]
    fn held_open(checkout: &Path) -> File {
        use std::os::windows::fs::OpenOptionsExt;
        let git_dir = checkout.join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        let file = git_dir.join("index");
        fs::write(&file, "").unwrap();
        OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(file)
            .unwrap()
    }

    #[test]
    fn the_runner_git_drops_what_points_it_elsewhere() {
        let mut command = Command::new("git");
        command
            .env("GIT_DIR", "/elsewhere/.git")
            .env("GIT_INDEX_FILE", "/elsewhere/index")
            .env("GIT_SSH_COMMAND", "ssh -i key");
        drop_repository_variables(&mut command);
        let envs: Vec<_> = command.get_envs().collect();
        for name in REPOSITORY_VARIABLES {
            assert!(
                envs.contains(&(OsStr::new(name), None)),
                "{name} is not removed"
            );
        }
        assert!(envs.contains(&(
            OsStr::new("GIT_SSH_COMMAND"),
            Some(OsStr::new("ssh -i key"))
        )));
    }
}
