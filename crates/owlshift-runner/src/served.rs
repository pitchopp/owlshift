//! The projects the background service serves and the project file each
//! runs on (OWL-434, architecture section 5, "The background service",
//! "Served projects" and "Its configuration").
//!
//! `<data>/service/projects.json` lists the served projects ([`Registry`]),
//! each with the checkout it was registered from, which only locates it
//! ([`Served::locate`]). A served watch reads the project file from the
//! default branch of the project's dedicated checkout, after a fetch it
//! makes itself, never from a working tree ([`refresh`]); it keeps the last
//! one Owlshift accepted in `served.json` in the project's folder, which its
//! steps' children read too ([`effective`]). A newer file that is accepted
//! takes effect once the steps under way end: the watch ends, then starts
//! again on it ([`prepare`]). A file refused keeps the previous one, the
//! operator told once until a file is accepted again ([`report_refused`]).

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use owlshift_adapters::forge::Repo;
use owlshift_adapters::notifier::Notifier;
use owlshift_contracts::config::ProjectConfig;
use owlshift_contracts::event::EventKind;
use owlshift_contracts::service::{
    KeptFile, RefusedFile, ServedConfig, ServedProject, ServedProjects,
};
use serde_json::json;

use crate::base::short;
use crate::config::{Effective, FileState, PROJECT_FILE};
use crate::events::{EventSink, data};
use crate::executor::Git;
use crate::project::{self, ProjectDirs, ProjectLock};
use crate::{notify, on_demand, rules, step, watch};

/// How long a change of the served projects waits for their lock.
const LOCK_WAIT: Duration = Duration::from_secs(10);
/// How often that wait tries the lock.
const LOCK_POLL: Duration = Duration::from_millis(50);

/// The machine's list of served projects: `projects.json` in
/// `<data>/service`, replaced whole under `projects.lock`.
#[derive(Clone, Debug)]
pub struct Registry {
    dir: PathBuf,
}

impl Registry {
    /// The list of the data directory `data`.
    pub fn in_data_dir(data: &Path) -> Self {
        Self {
            dir: data.join("service"),
        }
    }

    /// The list's file.
    pub fn file(&self) -> PathBuf {
        self.dir.join("projects.json")
    }

    fn lock_file(&self) -> PathBuf {
        self.dir.join("projects.lock")
    }

    /// What the list holds, without its lock: empty when there is none.
    pub fn read(&self) -> Result<ServedProjects, String> {
        let path = self.file();
        match fs::read_to_string(&path) {
            Ok(text) => {
                ServedProjects::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(ServedProjects::default()),
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    }

    /// The entry of `repo`, when it is served.
    pub fn find(&self, repo: &Repo) -> Result<Option<ServedProject>, String> {
        Ok(self.read()?.find(&repo.to_string()).cloned())
    }

    /// Serves `repo`, registered from `checkout`, an absolute path that is
    /// UTF-8 text: `true` when the list changed, `false` when it already
    /// served `repo` from there.
    pub fn register(&self, repo: &Repo, checkout: &Path) -> Result<bool, String> {
        let checkout = checkout
            .to_str()
            .filter(|_| checkout.is_absolute())
            .ok_or_else(|| {
                format!(
                    "{} is not an absolute path written in UTF-8, which the list of served \
                     projects needs",
                    checkout.display()
                )
            })?
            .to_owned();
        let repository = repo.to_string();
        self.update(|served| {
            let entry = ServedProject {
                repository: repository.clone(),
                checkout,
            };
            match served
                .projects
                .iter_mut()
                .find(|project| project.repository.eq_ignore_ascii_case(&repository))
            {
                Some(project) if *project == entry => false,
                Some(project) => {
                    *project = entry;
                    true
                }
                None => {
                    served.projects.push(entry);
                    true
                }
            }
        })
    }

    /// Stops serving `repo`: `true` when it was served.
    pub fn forget(&self, repo: &Repo) -> Result<bool, String> {
        let repository = repo.to_string();
        self.update(|served| {
            let before = served.projects.len();
            served
                .projects
                .retain(|project| !project.repository.eq_ignore_ascii_case(&repository));
            served.projects.len() != before
        })
    }

    /// Applies `change` under the list's lock, the list replaced whole when
    /// `change` says it changed it.
    fn update(&self, change: impl FnOnce(&mut ServedProjects) -> bool) -> Result<bool, String> {
        fs::create_dir_all(&self.dir).map_err(|e| format!("{}: {e}", self.dir.display()))?;
        let lock_path = self.lock_file();
        let _lock = project::lock_file_within(&lock_path, LOCK_WAIT, LOCK_POLL)
            .map_err(|e| format!("{}: {e}", lock_path.display()))?
            .ok_or_else(|| {
                format!(
                    "{} stayed held for {} seconds",
                    lock_path.display(),
                    LOCK_WAIT.as_secs()
                )
            })?;
        let mut served = self.read()?;
        let changed = change(&mut served);
        if changed {
            let path = self.file();
            step::write_atomically(&path, &served.render())
                .map_err(|e| format!("{}: {e}", path.display()))?;
        }
        Ok(changed)
    }
}

/// A served project, located from the list: its repository, the checkout
/// it was registered from, the URL of that checkout's `origin`, and its
/// folders in the data directory.
#[derive(Clone, Debug)]
pub struct Served {
    pub repo: Repo,
    pub checkout: PathBuf,
    pub remote_url: String,
    pub dirs: ProjectDirs,
}

impl Served {
    /// `repo` as the list of the data directory `data` serves it: refused
    /// when it does not, or when its registered checkout is gone or its
    /// `origin` names another repository, which a new `owlshift start`
    /// mends.
    pub fn locate(git: &Git, data: &Path, repo: &Repo) -> Result<Self, String> {
        let entry = Registry::in_data_dir(data).find(repo)?.ok_or_else(|| {
            format!(
                "{repo} is not served on this machine: `owlshift start` in its checkout serves it"
            )
        })?;
        let checkout = PathBuf::from(&entry.checkout);
        let again = "run `owlshift start` in the project's checkout again";
        if !checkout.is_dir() {
            return Err(format!(
                "{repo} was registered from {}, which is gone: {again}",
                checkout.display()
            ));
        }
        let remote_url = project::origin_url(git, &checkout)
            .and_then(|url| on_demand::check_origin(&url).map(|found| (url, found)))
            .map_err(|e| {
                format!(
                    "{repo} was registered from {}, whose `origin` does not do ({e}): {again}",
                    checkout.display()
                )
            })
            .and_then(|(url, found)| {
                if found.to_string().eq_ignore_ascii_case(&repo.to_string()) {
                    Ok(url)
                } else {
                    Err(format!(
                        "{repo} was registered from {}, whose `origin` is now {found}: {again}",
                        checkout.display()
                    ))
                }
            })?;
        Ok(Self {
            repo: repo.clone(),
            checkout,
            remote_url,
            dirs: ProjectDirs::new(data, repo),
        })
    }
}

/// The record of the project file a served project runs on, in its folder.
pub fn record_file(dirs: &ProjectDirs) -> PathBuf {
    dirs.root().join("served.json")
}

/// The record in `dirs`, `None` before a served watch wrote one.
pub fn read_record(dirs: &ProjectDirs) -> Result<Option<ServedConfig>, String> {
    let path = record_file(dirs);
    match fs::read_to_string(&path) {
        Ok(text) => ServedConfig::parse(&text)
            .map(Some)
            .map_err(|e| format!("{}: {e}", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// What the data directory `data` says of `repo` as a served project, for
/// `owlshift doctor` and `owlshift status`: `None` when it is not served;
/// its record otherwise, an empty one when its watch has read nothing yet.
pub fn standing(data: &Path, repo: &Repo) -> Result<Option<ServedConfig>, String> {
    if Registry::in_data_dir(data).find(repo)?.is_none() {
        return Ok(None);
    }
    let record = read_record(&ProjectDirs::new(data, repo))?;
    Ok(Some(
        record.unwrap_or_else(|| ServedConfig::new(repo.to_string())),
    ))
}

/// The project file `kept` names: `owlshift.toml at origin/main (<12
/// hexadecimal digits>)`.
pub fn kept_name(kept: &KeptFile) -> String {
    format!(
        "{PROJECT_FILE} at origin/{} ({})",
        kept.branch,
        short(&kept.commit)
    )
}

/// Whether Owlshift accepts the project file `text` of `repo`, the
/// personal file at `personal` applying: it must parse, ask for no
/// variable or cache folder the operator does not allow, and suit the
/// drain, which a served watch runs. Why not, otherwise.
pub fn accept(repo: &Repo, text: &str, personal: Option<PathBuf>) -> Result<(), String> {
    check(&Effective::served(
        repo,
        PathBuf::from(PROJECT_FILE),
        text,
        personal,
    ))
    .map(drop)
}

/// The configuration a served project's watch and its steps run on: the
/// project file kept in `dirs`, read from there and never fetched, with the
/// personal file at `personal`. Refused when none is kept, or when the one
/// kept is not accepted any more.
pub fn effective(
    repo: &Repo,
    dirs: &ProjectDirs,
    personal: Option<PathBuf>,
) -> Result<Effective, String> {
    let record = read_record(dirs)?;
    let Some(kept) = record.as_ref().and_then(|record| record.kept.as_ref()) else {
        return Err(not_served(repo, record.as_ref()));
    };
    let effective = Effective::served(repo, record_file(dirs), &kept.text, personal);
    check(&effective).map_err(|reason| {
        format!(
            "{repo} is not served: {}, the file kept, is not accepted any more: {reason}",
            kept_name(kept)
        )
    })?;
    Ok(effective)
}

/// The project of `effective`, when Owlshift accepts it for a served watch.
fn check(effective: &Effective) -> Result<&ProjectConfig, String> {
    let config = match &effective.project {
        FileState::Loaded { config, .. } => config,
        FileState::Invalid { error, .. } => return Err(error.clone()),
        FileState::Absent(_) | FileState::NotApplicable(_) | FileState::Unavailable(_) => {
            return Err(format!("no {PROJECT_FILE}"));
        }
    };
    if !effective.is_valid() {
        return Err("the personal configuration is invalid: see `owlshift config show`".to_owned());
    }
    if let Some(refusal) = watch::drain_refusal(config) {
        return Err(refusal);
    }
    Ok(config)
}

/// Why `repo` is not served, from its record: the refusal it holds.
pub fn not_served(repo: &Repo, record: Option<&ServedConfig>) -> String {
    let reason = record
        .and_then(|record| record.refused.as_ref())
        .map_or_else(
            || "no project file was read from its default branch yet".to_owned(),
            |refused| refused.reason.clone(),
        );
    format!(
        "{repo} is not served: {reason}. Merge a valid {PROJECT_FILE} to its default branch: \
         a served project reads it there, not in a working tree"
    )
}

/// What a read of the default branch's project file found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refresh {
    /// The text kept: nothing changes.
    Same,
    /// Another text, accepted: kept once promoted.
    Changed,
    /// Refused, the previous text kept; `first` when no refusal stood
    /// before, which the operator is told of.
    Refused { first: bool, reason: String },
}

/// Fetches the dedicated checkout of `served` under the project's `_lock`
/// and reads the project file at `refs/remotes/origin/HEAD`, as the fetch
/// left it. A text other than the one kept is checked with `accept`: kept
/// when it is accepted and `promote` is set, which a watch sets only before
/// it starts, so that the steps of a running watch all read one text;
/// recorded as refused otherwise, the text kept staying. A fetch, or the
/// record, that fails is an error, the record left as it was.
pub fn refresh(
    git: &Git,
    served: &Served,
    _lock: &ProjectLock,
    accept: &dyn Fn(&str) -> Result<(), String>,
    promote: bool,
    now: Timestamp,
) -> Result<Refresh, String> {
    let base = project::sync_checkout(git, &served.dirs, &served.remote_url)?;
    let before = read_record(&served.dirs)?;
    let mut record = before
        .clone()
        .unwrap_or_else(|| ServedConfig::new(served.repo.to_string()));
    let at = format!("{} ({})", base.remote_ref, short(&base.commit));
    let found = rules::text_at_base(
        git,
        &served.dirs.checkout(),
        &base,
        PROJECT_FILE,
        project::MAX_PROJECT_FILE_BYTES,
        "the project file",
    )
    .and_then(|text| text.ok_or_else(|| format!("{at} holds no {PROJECT_FILE}")))
    .and_then(|text| {
        let same = record.kept.as_ref().is_some_and(|kept| kept.text == text);
        if same {
            return Ok(None);
        }
        accept(&text)
            .map(|()| Some(text))
            .map_err(|reason| format!("{PROJECT_FILE} at {at}: {reason}"))
    });
    let outcome = match found {
        Ok(None) => {
            if let Some(kept) = &mut record.kept {
                kept.branch = base.branch;
                kept.commit = base.commit;
            }
            record.refused = None;
            Refresh::Same
        }
        Ok(Some(text)) => {
            if promote {
                record.kept = Some(KeptFile {
                    branch: base.branch,
                    commit: base.commit,
                    text,
                    since: now,
                });
                record.refused = None;
            }
            Refresh::Changed
        }
        Err(reason) => {
            let first = record.refused.is_none();
            let since = record.refused.as_ref().map_or(now, |refused| refused.since);
            record.refused = Some(RefusedFile {
                commit: Some(base.commit),
                reason: reason.clone(),
                since,
            });
            Refresh::Refused { first, reason }
        }
    };
    if before.as_ref() != Some(&record) {
        let path = record_file(&served.dirs);
        step::write_atomically(&path, &record.render())
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(outcome)
}

/// Tells the operator that `served`'s default branch holds a project file
/// Owlshift refused: one `warning` event and one notification, the text
/// kept named.
pub fn report_refused(
    notifier: Option<&dyn Notifier>,
    sink: &mut EventSink<'_>,
    served: &Served,
    reason: &str,
) {
    let kept = read_record(&served.dirs)
        .ok()
        .flatten()
        .and_then(|record| record.kept);
    let line = match &kept {
        Some(kept) => format!(
            "{} keeps running on {}: {reason}",
            served.repo,
            kept_name(kept)
        ),
        None => format!("{} is not served: {reason}", served.repo),
    };
    sink.emit_project(
        None,
        EventKind::Warning,
        data([
            ("what", json!("served_config_refused")),
            ("reason", json!(reason)),
            ("kept", json!(kept.map(|kept| kept.commit))),
        ]),
    );
    if let Some(notifier) = notifier
        && let Err(error) = notifier.notify(&format!("Owlshift: {line}"))
    {
        sink.emit_project(None, EventKind::Warning, notify::not_shown(&error));
    }
}

/// Before a served watch starts, and again before it starts on a newer
/// file: under the project's lock, the default branch read and its file
/// kept when accepted ([`refresh`]), a refusal told when it is new; then the
/// text kept. A fetch that fails leaves the text kept before, with a
/// warning; with none kept, the project is not served and the record says
/// why.
pub fn prepare(
    git: &Git,
    served: &Served,
    accept: &dyn Fn(&str) -> Result<(), String>,
    notifier: Option<&dyn Notifier>,
    sink: &mut EventSink<'_>,
) -> Result<KeptFile, String> {
    let lock = served
        .dirs
        .lock()
        .map_err(|e| format!("the project's lock: {e}"))?
        .ok_or_else(|| {
            format!(
                "another command held the lock of {} for a minute",
                served.repo
            )
        })?;
    match refresh(git, served, &lock, accept, true, Timestamp::now()) {
        Ok(Refresh::Refused {
            first: true,
            reason,
        }) => {
            report_refused(notifier, sink, served, &reason);
        }
        Ok(_) => {}
        Err(error) => fetch_failed(sink, &error),
    }
    drop(lock);
    let record = read_record(&served.dirs)?;
    record
        .as_ref()
        .and_then(|record| record.kept.clone())
        .ok_or_else(|| not_served(&served.repo, record.as_ref()))
}

fn fetch_failed(sink: &mut EventSink<'_>, error: &str) {
    sink.emit_project(
        None,
        EventKind::Warning,
        data([
            ("what", json!("served_config_not_read")),
            ("reason", json!(error)),
        ]),
    );
}

/// What a served watch does between two passes: at most once an interval,
/// the default branch read again, when no other command holds the
/// project's lock. A fetch that fails is told once until it succeeds.
#[derive(Debug)]
pub struct Between {
    interval: Duration,
    last: Option<Instant>,
    failing: Option<String>,
}

impl Between {
    /// Reads no more often than once an `interval`, the first time at once.
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: None,
            failing: None,
        }
    }

    /// Whether a read is due at `now`.
    pub fn due(&self, now: Instant) -> bool {
        self.last
            .is_none_or(|last| now.saturating_duration_since(last) >= self.interval)
    }

    /// Reads the default branch again when due at `now`: `true` when it holds
    /// another file Owlshift accepts, which the watch is to start again on,
    /// once its steps under way end. Said on `out`.
    #[allow(clippy::too_many_arguments)]
    pub fn check(
        &mut self,
        git: &Git,
        served: &Served,
        accept: &dyn Fn(&str) -> Result<(), String>,
        notifier: Option<&dyn Notifier>,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        now: Instant,
    ) -> bool {
        if !self.due(now) {
            return false;
        }
        self.last = Some(now);
        // One try: a busy project is read again at the next interval.
        let Ok(Some(lock)) = served.dirs.try_lock() else {
            return false;
        };
        let outcome = refresh(git, served, &lock, accept, false, Timestamp::now());
        drop(lock);
        match outcome {
            Ok(outcome) => {
                self.failing = None;
                match outcome {
                    Refresh::Same => false,
                    Refresh::Changed => {
                        let _ = writeln!(
                            out,
                            "{PROJECT_FILE} changed on the default branch of {}: the watch starts \
                             again on it once the steps under way end.",
                            served.repo
                        );
                        true
                    }
                    Refresh::Refused { first, reason } => {
                        if first {
                            report_refused(notifier, sink, served, &reason);
                        }
                        false
                    }
                }
            }
            Err(error) => {
                if self.failing.as_deref() != Some(error.as_str()) {
                    fetch_failed(sink, &error);
                    self.failing = Some(error);
                }
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A project file a served watch accepts.
    const PROJECT: &str = r#"requires = ">=0.0"

[tracker]
kind = "linear"
team = "WEB"
admit = { label = "agent" }
admitters = ["u1"]
states = { ready = "Todo", working = "Doing", needs_input = "Asked", review = "Review" }

[stack]
gate = ["true"]

[pipeline]
default = "trivial"
plan_approval = "never"

[models]

[policy]
always_human = []
"#;

    fn repo() -> Repo {
        Repo::parse("acme/web").unwrap()
    }

    #[test]
    fn the_list_is_changed_only_when_it_changes_and_refuses_a_relative_path() {
        let data = tempfile::tempdir().unwrap();
        let registry = Registry::in_data_dir(data.path());
        let checkout = data.path().join("web");
        assert_eq!(registry.find(&repo()), Ok(None));
        assert_eq!(registry.register(&repo(), &checkout), Ok(true));
        assert_eq!(registry.register(&repo(), &checkout), Ok(false));
        // Another checkout of the same repository replaces the first.
        let other = data.path().join("web2");
        let upper = Repo::parse("Acme/Web").unwrap();
        assert_eq!(registry.register(&upper, &other), Ok(true));
        let served = registry.read().unwrap();
        assert_eq!(served.projects.len(), 1);
        assert_eq!(
            served.projects[0].checkout,
            other.to_str().unwrap().to_owned()
        );
        let error = registry
            .register(&repo(), Path::new("relative"))
            .unwrap_err();
        assert!(error.contains("not an absolute path"), "{error}");

        assert_eq!(registry.forget(&repo()), Ok(true));
        assert_eq!(registry.forget(&repo()), Ok(false));
        assert_eq!(registry.read().unwrap().projects, Vec::new());
    }

    #[test]
    fn a_newer_list_is_refused_and_left_alone() {
        let data = tempfile::tempdir().unwrap();
        let registry = Registry::in_data_dir(data.path());
        fs::create_dir_all(data.path().join("service")).unwrap();
        let newer = "{\"format\": 2, \"projects\": []}";
        fs::write(registry.file(), newer).unwrap();
        let error = registry
            .register(&repo(), &data.path().join("web"))
            .unwrap_err();
        assert!(error.contains("newer Owlshift"), "{error}");
        assert_eq!(fs::read_to_string(registry.file()).unwrap(), newer);
    }

    #[test]
    fn a_read_is_due_once_an_interval() {
        let between = Between::new(Duration::from_secs(60));
        let start = Instant::now();
        assert!(between.due(start));
        let between = Between {
            last: Some(start),
            ..between
        };
        assert!(!between.due(start + Duration::from_secs(59)));
        assert!(between.due(start + Duration::from_secs(60)));
    }

    #[test]
    fn a_step_reads_the_kept_file_without_a_checkout() {
        let data = tempfile::tempdir().unwrap();
        let dirs = ProjectDirs::new(data.path(), &repo());
        let error = effective(&repo(), &dirs, None).unwrap_err();
        assert!(
            error.contains("acme/web is not served: no project file was read"),
            "{error}"
        );
        let mut record = ServedConfig::new("acme/web");
        record.kept = Some(KeptFile {
            branch: "main".to_owned(),
            commit: "c".repeat(40),
            text: PROJECT.to_owned(),
            since: Timestamp::now(),
        });
        step::write_atomically(&record_file(&dirs), &record.render()).unwrap();
        let effective = effective(&repo(), &dirs, None).unwrap();
        let FileState::Loaded { config, path, .. } = &effective.project else {
            panic!("{:?}", effective.project);
        };
        assert_eq!(config.tracker.team.as_deref(), Some("WEB"));
        assert_eq!(*path, record_file(&dirs));
        assert!(!dirs.checkout().exists());
    }
}
