//! A fix run's brief (OWL-112, architecture section 5, "The fix run"): the
//! failed checks of a delivered pull request, in the forge's order, each
//! with its state on the base head and its excerpt or why it has none. The
//! forge reads the excerpts ([`Excerpts`]); this module only assembles them
//! within the brief's limits.

use owlshift_adapters::forge::github::GitHubForge;
use owlshift_adapters::forge::{Check, CheckKind, CheckState, Error, ErrorKind};
use owlshift_contracts::brief::{
    BaseState, CheckFailure, FailedCheck, FailedCheckKind, MAX_EXCERPT_BYTES, MAX_EXCERPTS_BYTES,
};
use owlshift_core::follow::{self, CheckOutcome};

use crate::follow::check_read;

/// The app whose check runs are GitHub Actions jobs, whose logs are read.
pub const ACTIONS_APP: &str = "github-actions";

/// Where a failed check's excerpt is read.
pub trait Excerpts {
    /// The excerpt of a GitHub Actions job's log.
    fn job_log(&self, job: u64) -> Result<String, Error>;
    /// Another app's check run's output.
    fn output(&self, check_run: u64) -> Result<String, Error>;
}

impl Excerpts for GitHubForge {
    fn job_log(&self, job: u64) -> Result<String, Error> {
        self.job_log_excerpt(job)
    }

    fn output(&self, check_run: u64) -> Result<String, Error> {
        self.check_run_output(check_run)
    }
}

/// The pull request a fix run is given.
#[derive(Clone, Copy, Debug)]
pub struct Red<'a> {
    pub pull_request: u64,
    pub url: &'a str,
    /// The head the checks ran on.
    pub head: &'a str,
    /// The base head at its push, when the ref keeps it.
    pub base: Option<&'a str>,
    /// The checks of the head, in the forge's order.
    pub checks: &'a [Check],
    /// The base head's checks, when they were read.
    pub base_checks: Option<&'a [Check]>,
    /// The checks still unfinished an hour after the first failure.
    pub unfinished: &'a [String],
}

/// A fix run's `check_failure`, or why no run should start: when no failed
/// check got an excerpt and the forge refused a read for the token's
/// permission or its spent budget, an hour of agent time would go to a
/// guess (architecture section 5, "Failing closed on the token").
pub fn check_failure(red: Red<'_>, excerpts: &dyn Excerpts) -> Result<CheckFailure, Error> {
    let mut checks = Vec::new();
    let mut total = 0;
    let mut spent = false;
    let mut refused: Option<Error> = None;
    for check in red.checks.iter().filter(|c| c.state == CheckState::Failed) {
        let (excerpt, no_excerpt) = if spent {
            (None, Some(LEFT_OUT.to_owned()))
        } else {
            match excerpt(check, excerpts) {
                Ok(text) if text.trim().is_empty() => (None, Some("it printed nothing".to_owned())),
                Ok(text) if total + text.len() > MAX_EXCERPTS_BYTES => {
                    spent = true;
                    (None, Some(LEFT_OUT.to_owned()))
                }
                Ok(text) => {
                    total += text.len();
                    (Some(text), None)
                }
                Err(error) => {
                    if matches!(
                        error.kind,
                        ErrorKind::Unauthorized | ErrorKind::SignInAgain | ErrorKind::RateLimited
                    ) && refused.is_none()
                    {
                        refused = Some(error.clone());
                    }
                    (None, Some(format!("it could not be read: {error}")))
                }
            }
        };
        checks.push(FailedCheck {
            name: check.name.clone(),
            kind: match &check.kind {
                CheckKind::CheckRun { app, workflow } => FailedCheckKind::CheckRun {
                    app: app.clone(),
                    workflow: workflow.clone(),
                },
                CheckKind::Status => FailedCheckKind::Status,
            },
            conclusion: check.raw.clone(),
            required: check.required,
            url: check.url.clone(),
            on_base: on_base(red.base_checks, &check.name),
            excerpt,
            no_excerpt,
        });
    }
    if let Some(error) = refused
        && checks.iter().all(|c| c.excerpt.is_none())
    {
        return Err(error);
    }
    Ok(CheckFailure {
        pull_request: red.pull_request,
        url: red.url.to_owned(),
        head: red.head.to_owned(),
        base: red.base.map(str::to_owned),
        checks,
        unfinished: red.unfinished.to_vec(),
    })
}

/// Why a check past the brief's limit has no excerpt.
const LEFT_OUT: &str = "left out: the brief's 64 KiB of excerpts went to the checks before it";

/// A failed check's excerpt: an Actions job's log, another app's output,
/// a status's description; at most [`MAX_EXCERPT_BYTES`].
fn excerpt(check: &Check, excerpts: &dyn Excerpts) -> Result<String, Error> {
    let text = match (&check.kind, check.id) {
        (CheckKind::CheckRun { app, .. }, Some(id)) if app.as_deref() == Some(ACTIONS_APP) => {
            excerpts.job_log(id)?
        }
        (CheckKind::CheckRun { .. }, Some(id)) => excerpts.output(id)?,
        (CheckKind::CheckRun { .. }, None) => {
            return Err(Error::new(
                ErrorKind::Other,
                "the forge gave no id for this check run",
            ));
        }
        (CheckKind::Status, _) => check.description.clone().unwrap_or_default(),
    };
    Ok(head(&text, MAX_EXCERPT_BYTES))
}

/// The beginning of `text` within `max` bytes, cut on a character boundary.
fn head(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// A check's state on the base head, by the table's own reading
/// ([`follow::outcome_of`]).
fn on_base(base: Option<&[Check]>, name: &str) -> BaseState {
    let Some(base) = base else {
        return BaseState::Unread;
    };
    let reads: Vec<_> = base.iter().map(check_read).collect();
    match follow::outcome_of(&reads, name) {
        None => BaseState::Absent,
        Some(CheckOutcome::Passed) => BaseState::Passed,
        Some(CheckOutcome::Pending) => BaseState::Running,
        Some(CheckOutcome::CodeFailure | CheckOutcome::OtherFailure) => BaseState::Failed,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    /// Answers each read with the next answer, recording which was read.
    struct Canned(RefCell<Vec<Result<String, Error>>>, RefCell<Vec<String>>);

    impl Canned {
        fn new(answers: Vec<Result<String, Error>>) -> Self {
            Self(RefCell::new(answers), RefCell::new(Vec::new()))
        }

        fn next(&self, read: String) -> Result<String, Error> {
            self.1.borrow_mut().push(read);
            self.0.borrow_mut().remove(0)
        }
    }

    impl Excerpts for Canned {
        fn job_log(&self, job: u64) -> Result<String, Error> {
            self.next(format!("log {job}"))
        }

        fn output(&self, check_run: u64) -> Result<String, Error> {
            self.next(format!("output {check_run}"))
        }
    }

    fn run(name: &str, app: &str, id: Option<u64>, state: CheckState) -> Check {
        Check {
            kind: CheckKind::CheckRun {
                app: Some(app.to_owned()),
                workflow: None,
            },
            name: name.to_owned(),
            state,
            raw: match state {
                CheckState::Failed => "FAILURE",
                CheckState::Passed => "SUCCESS",
                CheckState::Pending => "IN_PROGRESS",
            }
            .to_owned(),
            required: true,
            url: None,
            id,
            description: None,
        }
    }

    fn red<'a>(checks: &'a [Check], base: Option<&'a [Check]>) -> Red<'a> {
        Red {
            pull_request: 7,
            url: "https://github.com/demo/project/pull/7",
            head: "1111111111111111111111111111111111111111",
            base: None,
            checks,
            base_checks: base,
            unfinished: &[],
        }
    }

    fn refused() -> Error {
        Error::new(
            ErrorKind::Unauthorized,
            "GitHub refused the token: HTTP 403",
        )
    }

    /// Each failed check in order, with its state on the base and its
    /// excerpt from where its kind says; an unreadable one says why.
    #[test]
    fn each_failed_check_gets_its_excerpt_or_why_none() {
        let mut status = run("coverage", "", None, CheckState::Failed);
        status.kind = CheckKind::Status;
        status.description = Some("Coverage fell to 71%".to_owned());
        let checks = [
            run("build", ACTIONS_APP, Some(2), CheckState::Failed),
            run("lint", ACTIONS_APP, Some(3), CheckState::Passed),
            run("sonar", "sonarcloud", Some(4), CheckState::Failed),
            status,
            run("docs", ACTIONS_APP, Some(5), CheckState::Failed),
        ];
        let base = [run("build", ACTIONS_APP, None, CheckState::Passed)];
        let canned = Canned::new(vec![
            Ok("##[error]boom\n".to_owned()),
            Ok("Quality gate failed".to_owned()),
            Err(Error::new(ErrorKind::NotFound, "the log is gone")),
        ]);
        let failure = check_failure(red(&checks, Some(&base)), &canned).unwrap();
        assert_eq!(*canned.1.borrow(), ["log 2", "output 4", "log 5"]);
        let seen: Vec<_> = failure
            .checks
            .iter()
            .map(|c| (c.name.as_str(), c.on_base, c.excerpt.as_deref()))
            .collect();
        assert_eq!(
            seen,
            [
                ("build", BaseState::Passed, Some("##[error]boom\n")),
                ("sonar", BaseState::Absent, Some("Quality gate failed")),
                ("coverage", BaseState::Absent, Some("Coverage fell to 71%")),
                ("docs", BaseState::Absent, None),
            ]
        );
        assert!(
            failure.checks[3]
                .no_excerpt
                .as_deref()
                .unwrap()
                .contains("the log is gone")
        );
        let unread = check_failure(red(&checks[..1], None), &Canned::new(vec![Ok("x".into())]));
        assert_eq!(unread.unwrap().checks[0].on_base, BaseState::Unread);
    }

    /// The excerpts stop at 64 KiB together: the check that would pass it
    /// and every later one are listed without, and not read.
    #[test]
    fn excerpts_past_their_limit_are_left_out() {
        let checks: Vec<Check> = (0..6)
            .map(|n| {
                run(
                    &format!("leg {n}"),
                    ACTIONS_APP,
                    Some(n),
                    CheckState::Failed,
                )
            })
            .collect();
        let full = "x".repeat(MAX_EXCERPT_BYTES * 2);
        let canned = Canned::new(vec![Ok(full); 5]);
        let failure = check_failure(red(&checks, None), &canned).unwrap();
        assert_eq!(canned.1.borrow().len(), 5);
        let excerpts: Vec<bool> = failure.checks.iter().map(|c| c.excerpt.is_some()).collect();
        assert_eq!(excerpts, [true, true, true, true, false, false]);
        assert_eq!(
            failure.checks[0].excerpt.as_ref().unwrap().len(),
            MAX_EXCERPT_BYTES
        );
        assert_eq!(failure.checks[5].no_excerpt.as_deref(), Some(LEFT_OUT));
    }

    /// No excerpt at all because the token was refused: no run. One excerpt
    /// read is enough to run.
    #[test]
    fn a_token_refused_every_log_starts_no_run() {
        let checks = [
            run("a", ACTIONS_APP, Some(1), CheckState::Failed),
            run("b", ACTIONS_APP, Some(2), CheckState::Failed),
        ];
        let error = check_failure(
            red(&checks, None),
            &Canned::new(vec![Err(refused()), Err(refused())]),
        )
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Unauthorized);
        let partial = check_failure(
            red(&checks, None),
            &Canned::new(vec![Err(refused()), Ok("##[error]b\n".to_owned())]),
        )
        .unwrap();
        assert!(partial.checks[1].excerpt.is_some());
    }
}
