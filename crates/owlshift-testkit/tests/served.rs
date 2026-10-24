//! A served project's configuration (OWL-434, architecture section 5, "The
//! background service", "Its configuration"): read from the default branch
//! of the dedicated checkout after a fetch, never from the working tree it
//! was registered from; the last file accepted kept, a refusal told once.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use owlshift_adapters::forge::Repo;
use owlshift_adapters::notifier::{Error, Notifier};
use owlshift_runner::events::{EventLog, EventSink};
use owlshift_runner::executor::Git;
use owlshift_runner::project::ProjectDirs;
use owlshift_runner::served::{
    self, Refresh, Registry, Served, read_record, refresh, report_refused,
};
use owlshift_testkit::git::{GitEnv, Remote, seed};

/// A project file a served watch accepts, its gate being `gate`.
fn project(gate: &str) -> String {
    format!(
        r#"requires = ">=0.0"

[tracker]
kind = "linear"
team = "WEB"
admit = {{ label = "agent" }}
admitters = ["u1"]
states = {{ ready = "Todo", working = "Doing", needs_input = "Asked", review = "Review" }}

[stack]
gate = ["{gate}"]

[pipeline]
default = "trivial"
plan_approval = "never"

[models]

[policy]
always_human = []
"#
    )
}

/// A bare origin seeded with `first` as its project file, the person's
/// clone of it, and a data directory.
struct Bench {
    _tmp: tempfile::TempDir,
    env: GitEnv,
    remote: Remote,
    data: PathBuf,
    git: Git,
}

impl Bench {
    fn new(first: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        // Not canonicalized: on Windows that gives a verbatim `\\?\` path,
        // which git cannot read its configuration files from.
        let root = tmp.path().to_owned();
        let env = GitEnv::create(root.join("home")).unwrap();
        let fixture = root.join("fixture");
        fs::create_dir_all(&fixture).unwrap();
        fs::write(fixture.join("owlshift.toml"), first).unwrap();
        let remote = seed(&env, &root, &fixture).unwrap();
        let git_env = env.clone();
        Self {
            _tmp: tmp,
            env,
            remote,
            data: root.join("data"),
            git: Git::with_setup("git", move |command| git_env.apply(command)),
        }
    }

    fn repo() -> Repo {
        Repo::parse("acme/web").unwrap()
    }

    /// The project as the list would locate it, its origin the bare one.
    fn served(&self) -> Served {
        Served {
            repo: Self::repo(),
            checkout: self.remote.checkout.clone(),
            remote_url: self.remote.bare.to_str().unwrap().to_owned(),
            dirs: ProjectDirs::new(&self.data, &Self::repo()),
        }
    }

    /// Writes `text` as the person's project file, uncommitted.
    fn edit(&self, text: &str) {
        fs::write(self.remote.checkout.join("owlshift.toml"), text).unwrap();
    }

    /// Commits `text` as the project file and pushes it to the default
    /// branch: a merge.
    fn merge(&self, text: &[u8]) {
        let checkout = &self.remote.checkout;
        fs::write(checkout.join("owlshift.toml"), text).unwrap();
        self.env.run(checkout, &["add", "owlshift.toml"]).unwrap();
        self.env
            .run(
                checkout,
                &["commit", "--quiet", "-m", "Change the project file"],
            )
            .unwrap();
        self.env
            .run(checkout, &["push", "--quiet", "origin", "HEAD:main"])
            .unwrap();
    }

    fn refresh(&self, promote: bool) -> Refresh {
        let served = self.served();
        let lock = served.dirs.lock().unwrap().unwrap();
        let accept = |text: &str| served::accept(&Self::repo(), text, None);
        refresh(
            &self.git,
            &served,
            &lock,
            &accept,
            promote,
            Timestamp::now(),
        )
        .unwrap()
    }

    fn kept(&self) -> Option<String> {
        read_record(&self.served().dirs)
            .unwrap()
            .and_then(|record| record.kept)
            .map(|kept| kept.text)
    }

    fn refused(&self) -> Option<String> {
        read_record(&self.served().dirs)
            .unwrap()
            .and_then(|record| record.refused)
            .map(|refused| refused.reason)
    }
}

/// The notifications shown.
#[derive(Default)]
struct Shown(RefCell<Vec<String>>);

impl Notifier for Shown {
    fn notify(&self, line: &str) -> Result<(), Error> {
        self.0.borrow_mut().push(line.to_owned());
        Ok(())
    }
}

/// A working-tree edit is never read; a merged one is, kept only once
/// promoted, which a running watch never does, so its steps all read one
/// text.
#[test]
fn a_served_watch_ignores_a_working_tree_edit_and_follows_a_merged_one() {
    let bench = Bench::new(&project("make check"));
    assert_eq!(bench.refresh(true), Refresh::Changed);
    assert_eq!(bench.kept(), Some(project("make check")));

    bench.edit(&project("make edited"));
    assert_eq!(bench.refresh(false), Refresh::Same);
    assert_eq!(bench.kept(), Some(project("make check")));

    bench.merge(project("make merged").as_bytes());
    assert_eq!(bench.refresh(false), Refresh::Changed);
    assert_eq!(bench.kept(), Some(project("make check")));
    assert_eq!(bench.refresh(true), Refresh::Changed);
    assert_eq!(bench.kept(), Some(project("make merged")));
    let effective = served::effective(&Bench::repo(), &bench.served().dirs, None).unwrap();
    let owlshift_runner::config::FileState::Loaded { config, .. } = &effective.project else {
        panic!("{:?}", effective.project);
    };
    assert_eq!(config.stack.gate, ["make merged"]);
    assert_eq!(bench.refresh(false), Refresh::Same);
}

/// A merge Owlshift refuses keeps the file before it, and is told once until
/// a file is accepted again: unparsable, asking for a variable the operator
/// does not allow, or over 64 KiB.
#[test]
fn a_refused_merge_keeps_the_previous_file_and_is_told_once() {
    let bench = Bench::new(&project("make check"));
    assert_eq!(bench.refresh(true), Refresh::Changed);

    bench.merge(b"[tracker\n");
    let Refresh::Refused {
        first: true,
        reason,
    } = bench.refresh(false)
    else {
        panic!("{:?}", bench.refresh(false));
    };
    assert!(
        reason.contains("owlshift.toml at origin/main ("),
        "{reason}"
    );
    let shown = Shown::default();
    let mut out = Vec::new();
    let mut sink = EventSink::new("acme/web", EventLog::in_dir(&bench.data), &mut out);
    report_refused(Some(&shown), &mut sink, &bench.served(), &reason);
    assert_eq!(shown.0.borrow().len(), 1);
    assert!(
        shown.0.borrow()[0]
            .starts_with("Owlshift: acme/web keeps running on owlshift.toml at origin/main ("),
        "{:?}",
        shown.0.borrow()
    );
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains("served_config_refused"), "{out}");
    assert!(matches!(
        bench.refresh(false),
        Refresh::Refused { first: false, .. }
    ));
    assert_eq!(bench.kept(), Some(project("make check")));

    let unallowed = project("make check").replace(
        "gate = [\"make check\"]",
        "gate = [\"make check\"]\ngate_env = [\"DATABASE_URL\"]",
    );
    bench.merge(unallowed.as_bytes());
    let Refresh::Refused {
        first: false,
        reason,
    } = bench.refresh(true)
    else {
        panic!("{:?}", bench.refresh(true));
    };
    assert!(reason.contains("DATABASE_URL"), "{reason}");

    let mut large = project("make check").into_bytes();
    large.extend(std::iter::repeat_n(b'#', 65 * 1024));
    bench.merge(&large);
    let Refresh::Refused {
        first: false,
        reason,
    } = bench.refresh(true)
    else {
        panic!("{:?}", bench.refresh(true));
    };
    assert!(reason.contains("owlshift.toml"), "{reason}");
    assert_eq!(bench.kept(), Some(project("make check")));

    bench.merge(project("make fixed").as_bytes());
    assert_eq!(bench.refresh(true), Refresh::Changed);
    assert_eq!(bench.kept(), Some(project("make fixed")));
    assert_eq!(bench.refused(), None);
}

/// With no file ever accepted, the project is not served, and its record
/// says why, in words `owlshift status` can show.
#[test]
fn a_project_never_accepted_is_not_served() {
    let bench = Bench::new("[tracker\n");
    let shown = Shown::default();
    let mut out = Vec::new();
    let mut sink = EventSink::new("acme/web", EventLog::in_dir(&bench.data), &mut out);
    let served = bench.served();
    let accept = |text: &str| served::accept(&Bench::repo(), text, None);
    let error = served::prepare(&bench.git, &served, &accept, Some(&shown), &mut sink).unwrap_err();
    assert!(
        error.starts_with("acme/web is not served: owlshift.toml at origin/main ("),
        "{error}"
    );
    assert!(
        error.ends_with(
            "Merge a valid owlshift.toml to its default branch: a served project reads it \
             there, not in a working tree"
        ),
        "{error}"
    );
    assert_eq!(shown.0.borrow().len(), 1);
    assert!(bench.refused().is_some());
    assert_eq!(bench.kept(), None);
}

/// The list only locates a project: refused when it does not serve it, when
/// its checkout is gone or when that checkout's `origin` names another
/// repository. Nothing is fetched.
#[test]
fn a_served_project_is_located_from_the_list() {
    let bench = Bench::new(&project("make check"));
    let repo = Bench::repo();
    let locate = || Served::locate(&bench.git, &bench.data, &repo);
    let error = locate().unwrap_err();
    assert!(
        error.contains("acme/web is not served on this machine: `owlshift start`"),
        "{error}"
    );

    let checkout = &bench.remote.checkout;
    Registry::in_data_dir(&bench.data)
        .register(&repo, checkout)
        .unwrap();
    let set_origin = |url: &str| {
        bench
            .env
            .run(checkout, &["remote", "set-url", "origin", url])
            .unwrap();
    };
    // The bare path is no GitHub repository.
    let error = locate().unwrap_err();
    assert!(error.contains("whose `origin` does not do"), "{error}");
    set_origin("git@github.com:acme/other.git");
    let error = locate().unwrap_err();
    assert!(
        error.contains("whose `origin` is now acme/other"),
        "{error}"
    );
    set_origin("git@github.com:Acme/Web.git");
    let served = locate().unwrap();
    assert_eq!(served.remote_url, "git@github.com:Acme/Web.git");
    assert_eq!(served.checkout, *checkout);
    assert!(!served.dirs.root().exists());

    let moved: &Path = &checkout.with_file_name("moved");
    fs::rename(checkout, moved).unwrap();
    let error = locate().unwrap_err();
    assert!(
        error.contains("which is gone: run `owlshift start`"),
        "{error}"
    );
}
