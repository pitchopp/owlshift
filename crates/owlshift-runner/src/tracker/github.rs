//! The GitHub Issues tracker's [`Kind`] (OWL-535): the issues of the
//! project's own repository, reached with the project's forge credential, so
//! it has no secret of its own (build plan, "The GitHub Issues adapter").
//! `owlshift init --tracker github` sets it up and `owlshift doctor` checks
//! it (OWL-537, part 6): the Issues permission, the labels, the admitters.

use std::path::Path;

use owlshift_adapters::forge::Repo;
use owlshift_adapters::tracker::github::{GitHubTracker, IssuesAccess};
use owlshift_adapters::tracker::linear::AdmissionFacts;
use owlshift_adapters::tracker::{Capability, PROPOSED_LABEL, Tracker};
use owlshift_contracts::config::{self, Admit, ProjectConfig};
use owlshift_contracts::ids::TicketId;
use owlshift_platform::keychain::Secret;

use super::{
    AdmissionWords, Connected, Doctored, ForgeCredential, GithubFacts, Kind, Opener, Probe, Probed,
    Setup, StatesError,
};
use crate::doctor::{Check, DOES_NOT_BLOCK, Section, Step};
use crate::init::SecretSpec;
use crate::secrets::{SecretSet, TrackerAccounts, TrackerSecrets};
use crate::system::System;
use crate::ui::reply::Key;

pub struct Github;

impl Kind for Github {
    fn name(&self) -> &'static str {
        "github"
    }

    fn capabilities(&self) -> &'static [Capability] {
        GitHubTracker::CAPABILITIES
    }

    /// None: the forge's credential reaches the issues.
    fn secret_specs(&self) -> &'static [SecretSpec] {
        &[]
    }

    /// None: the forge's credential reaches the issues.
    fn secret_set(&self) -> Option<&'static SecretSet> {
        None
    }

    /// Nothing: it declares no set.
    fn secrets_detail(&self, _found: &TrackerSecrets<()>) -> Vec<String> {
        Vec::new()
    }

    fn project_file_lines(&self, _team: Option<&str>) -> Result<String, String> {
        Ok("\
# Where the tickets live: \"github\" for the issues of this repository,
# reached with the project's GitHub credential; the states are labels.
kind = \"github\"
"
        .to_owned())
    }

    /// An issue's number alone: `12`, never `#12` or a team's key.
    fn check_ticket(&self, _tracker: &config::Tracker, ticket: &TicketId) -> Result<(), String> {
        let text = ticket.as_str();
        if text.bytes().all(|b| b.is_ascii_digit()) && !text.starts_with('0') {
            Ok(())
        } else {
            Err(format!(
                "{ticket} is not an issue number: the project's tickets are its GitHub issues, \
                 named by their number (`owlshift do 12`)"
            ))
        }
    }

    /// The adapter over the project's forge, opened as `owlshift do` opens
    /// its own: the GitHub App's token refreshed under the account lock, or
    /// the personal token.
    fn opener<'a>(
        &self,
        tracker: &'a config::Tracker,
        _root: &'a Path,
        _secrets: Option<&'a TrackerSecrets<Secret>>,
        forge: ForgeCredential<'a>,
    ) -> Result<Opener<'a>, String> {
        Ok(Opener::new(move || {
            let reached = crate::forge::github(forge.secret, forge.repo.clone(), forge.access)?;
            Ok(Box::new(GitHubTracker::new(reached, tracker)) as Box<dyn Tracker>)
        }))
    }

    /// `tracker access`, then `tracker labels`, from one read of the
    /// repository (OWL-537). Nothing without a repository or a readable
    /// token: the `secrets` and `forge access` lines say why.
    fn doctor(
        &self,
        system: &dyn System,
        config: &ProjectConfig,
        _accounts: Option<&TrackerAccounts>,
        repo: Option<&Repo>,
    ) -> Doctored {
        let Some(repo) = repo else {
            return Doctored::default();
        };
        let probe = Probe::GithubFacts {
            repo,
            tracker: &config.tracker,
        };
        match system.tracker_probe(&probe) {
            Probed::Github(answer) => doctored(&config.tracker, repo, answer),
            other => unreachable!("the host answered a GitHub probe with {other:?}"),
        }
    }

    /// The label fallback of architecture section 6: proposals are issues
    /// carrying [`PROPOSED_LABEL`], which `init` creates.
    fn proposals_inbox(
        &self,
        _config: &ProjectConfig,
        facts: Option<&Result<AdmissionFacts, StatesError>>,
    ) -> Option<Check> {
        const SUBJECT: &str = "proposals inbox";
        let Some(Ok(found)) = facts else {
            return None;
        };
        Some(if found.labels.iter().any(|l| l == PROPOSED_LABEL) {
            Check::ok(
                Section::Project,
                SUBJECT,
                format!("proposals are issues carrying the label {PROPOSED_LABEL:?}"),
            )
        } else {
            Check::warn(
                Section::Project,
                SUBJECT,
                format!("the repository has no label {PROPOSED_LABEL:?}, which marks a proposal"),
                format!(
                    "{DOES_NOT_BLOCK} Owlshift cannot file the follow-ups it proposes until the \
                     label exists. Run `owlshift init`, which creates it."
                ),
            )
        })
    }

    fn reply_key(&self, _repo: &Repo) -> Result<Key, String> {
        Err(
            "the project's tracker is GitHub Issues: a reply from the UI goes through a Linear \
             key, and the UI cannot reply on GitHub yet"
                .into(),
        )
    }

    fn stores_app(&self) -> bool {
        false
    }

    fn admission_lines(&self, admitter: Option<&str>) -> String {
        let quote = |text: &str| toml::Value::String(text.to_owned()).to_string();
        let admitters = match admitter {
            Some(id) => format!(
                "\
# The GitHub accounts whose label admits an issue, by numeric id, which a
# rename does not change: the account `owlshift init` signed in as.
admitters = [{}]
",
                quote(id)
            ),
            None => "\
# The GitHub accounts whose label admits an issue, by numeric id, which a
# rename does not change. `owlshift init --skip-secrets` did not sign in:
# put yours here (`gh api user --jq .id` prints it).
# admitters = [\"<your numeric GitHub account id>\"]
"
            .to_owned(),
        };
        let [ready, working, needs_input, review] = STAGES;
        format!(
            "\
# How a person admits an issue to Owlshift's queue: \"ready\" (putting the
# ready label below on it) or {{ label = \"...\" }}. `owlshift do TICKET` does
# not use it: it runs the issue it is given.
admit = \"ready\"
{admitters}# The labels that show where an issue stands; `owlshift init` creates the
# missing ones.
states = {{ ready = {}, working = {}, needs_input = {}, review = {} }}
# The label of each priority, every one optional; without them every issue
# reads `unset`.
# [tracker.priorities]
# urgent = \"priority:urgent\"
# high = \"priority:high\"
# medium = \"priority:medium\"
# low = \"priority:low\"
",
            quote(ready),
            quote(working),
            quote(needs_input),
            quote(review)
        )
    }

    /// Reads the issues with the forge credential `init` just stored, then
    /// creates the labels the project names that the repository lacks, and
    /// gives the signed-in account as the admitter.
    fn connect(&self, setup: &Setup<'_>) -> Result<Connected, String> {
        let repo = setup.repo.map_err(|reason| {
            format!(
                "a GitHub Issues project's tickets are the issues of the repository `origin` \
                 names, and {reason}"
            )
        })?;
        let mut read = |account: &str| setup.keychain.read(account).map_err(|e| e.to_string());
        let found = crate::secrets::forge(repo, &mut read)?;
        let forge = crate::forge::github(&found, repo.clone(), setup.access)
            .map_err(|refusal| refusal.message)?;
        set_up(
            &GitHubTracker::new(forge, setup.tracker),
            setup.tracker,
            repo,
        )
    }

    fn admission_words(&self) -> &'static AdmissionWords {
        &GITHUB_WORDS
    }
}

/// The stage labels a new project file names: ready, working, needs input,
/// review.
pub const STAGES: [&str; 4] = [
    "agent:ready",
    "agent:working",
    "agent:needs-input",
    "agent:review",
];

/// GitHub's words in doctor's `drain admission` line.
const GITHUB_WORDS: AdmissionWords = AdmissionWords {
    label_missing: "is not a label of the repository",
    label_fix: "Run `owlshift init`, which creates the labels the project names, or create it on \
                GitHub (Issues > Labels); names match without regard to case.",
    no_account: "is no GitHub account",
    app_account: "is not a person's account (a bot or an organization), which never admits",
    deactivated: "is a deactivated account",
    accounts_fix: "Set `admitters` to the numeric ids of the people whose label admits an issue: \
                   `gh api users/<login> --jq .id` prints one.",
    admitters_unset_fix: "List under `admitters` the numeric ids of the GitHub accounts whose \
                          label admits an issue; with none, no issue is ever admitted. `owlshift \
                          init` writes the account it signs in as.",
    delegation_fix: "GitHub Issues has no agent account to delegate to: set `admit` under \
                     `[tracker]` to `\"ready\"` or `{ label = … }`.",
    checked: "labels and admitters exist on GitHub",
};

/// The labels `init` creates when missing (OWL-537): the stage labels, the
/// admission and exclusion labels, and [`PROPOSED_LABEL`].
fn wanted_labels(tracker: &config::Tracker) -> Vec<&str> {
    let states = &tracker.states;
    let mut wanted: Vec<&str> = vec![
        &states.ready,
        &states.working,
        &states.needs_input,
        &states.review,
    ];
    wanted.extend(states.parked.as_deref());
    if let Admit::Label(label) = &tracker.admit {
        wanted.push(label);
    }
    wanted.extend(tracker.exclude.as_deref());
    wanted.push(PROPOSED_LABEL);
    wanted
}

/// Every label the project's `[tracker]` names: those `init` creates, then
/// the priorities'.
pub(crate) fn named_labels(tracker: &config::Tracker) -> Vec<&str> {
    let mut named = wanted_labels(tracker);
    if let Some(priorities) = &tracker.priorities {
        named.extend(priorities.labels().into_iter().map(|(_, label)| label));
    }
    named
}

/// Why the credential does not read the issues of `repo`, and its fix;
/// `None` when it does.
fn access_problem(access: &IssuesAccess, repo: &Repo) -> Option<(String, String)> {
    Some(match access {
        IssuesAccess::Granted => return None,
        IssuesAccess::Refused(said) => (
            format!("the GitHub credential may not read the issues of github.com/{repo} ({said})"),
            "Accept the GitHub App's Issues (read and write) permission on GitHub, in the \
             installation's settings (Settings > Applications > Installed GitHub Apps > \
             Configure); with `--github-token`, give the personal token Issues (read and \
             write)"
                .to_owned(),
        ),
        IssuesAccess::TokenRefused(said) => (
            format!("GitHub refused the credential ({said})"),
            "Run `owlshift init` in a terminal to sign in again".to_owned(),
        ),
        IssuesAccess::NotFound(said) => (
            format!("the GitHub credential does not see github.com/{repo} ({said})"),
            format!(
                "Install the GitHub App on github.com/{repo}, or give the personal token access \
                 to it"
            ),
        ),
        IssuesAccess::Disabled(said) => (
            format!("github.com/{repo} has its issues turned off ({said})"),
            "Turn Issues on in the repository's settings (Settings > General > Features)"
                .to_owned(),
        ),
    })
}

/// What `init` does on the repository once the credential is open: checks
/// it reads the issues, reads the signed-in account, creates the missing
/// labels.
pub(crate) fn set_up(
    issues: &GitHubTracker,
    tracker: &config::Tracker,
    repo: &Repo,
) -> Result<Connected, String> {
    let access = issues
        .issues_access()
        .map_err(|e| format!("GitHub did not answer for the issues of github.com/{repo}: {e}"))?;
    if let Some((detail, fix)) = access_problem(&access, repo) {
        return Err(format!("{detail}. {fix}."));
    }
    let id = issues
        .signed_in()
        .map_err(|e| format!("GitHub did not say which account signed in: {e}"))?;
    let created = issues
        .create_missing_labels(&wanted_labels(tracker))
        .map_err(|e| e.to_string())?;
    Ok(Connected {
        admitter: Some(id.to_string()),
        created,
    })
}

/// Doctor's lines and admission facts from what the repository answered.
fn doctored(
    tracker: &config::Tracker,
    repo: &Repo,
    answer: Result<GithubFacts, StatesError>,
) -> Doctored {
    const ACCESS: &str = "tracker access";
    let found = match answer {
        // The `secrets` and `forge access` lines say it.
        Err(StatesError::Keychain(_) | StatesError::NoKey) => return Doctored::default(),
        Err(StatesError::Tracker(error)) => {
            return Doctored {
                lines: vec![Check::warn(
                    Section::Project,
                    ACCESS,
                    format!("not checked: {error}"),
                    format!(
                        "{DOES_NOT_BLOCK} doctor could not ask GitHub whether the credential \
                         reads the issues of github.com/{repo}."
                    ),
                )],
                facts: Some(Err(StatesError::Tracker(error))),
            };
        }
        Ok(found) => found,
    };
    if let Some((detail, fix)) = access_problem(&found.access, repo) {
        return Doctored {
            lines: vec![Check::fail(
                Section::Project,
                ACCESS,
                detail,
                "`owlshift do` reads the ticket, its comments and its labels from the issues of \
                 the repository.",
                vec![Step::act(fix)],
            )],
            facts: None,
        };
    }
    let mut lines = vec![Check::ok(
        Section::Project,
        ACCESS,
        format!("the GitHub credential reads the issues of github.com/{repo}"),
    )];
    let (Some(labels), Some(accounts)) = (found.labels, found.accounts) else {
        return Doctored { lines, facts: None };
    };
    let has = |name: &str| labels.iter().any(|l| l.eq_ignore_ascii_case(name));
    let mut missing: Vec<&str> = Vec::new();
    for name in named_labels(tracker) {
        if !has(name) && !missing.iter().any(|m| m.eq_ignore_ascii_case(name)) {
            missing.push(name);
        }
    }
    const LABELS: &str = "tracker labels";
    lines.push(if missing.is_empty() {
        Check::ok(
            Section::Project,
            LABELS,
            format!("each label the project names is a label of github.com/{repo}"),
        )
    } else {
        Check::warn(
            Section::Project,
            LABELS,
            format!(
                "github.com/{repo} has no label {}",
                missing
                    .iter()
                    .map(|m| format!("{m:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            format!(
                "{DOES_NOT_BLOCK} a stage whose label is missing does not show, and a priority's \
                 never reads. Run `owlshift init`, which creates the stage labels and \
                 {PROPOSED_LABEL:?}; create a priority's label on GitHub (Issues > Labels)."
            ),
        )
    });
    // The labels the admission line asks of, as the project file names them.
    let mut asked: Vec<&str> = vec![PROPOSED_LABEL];
    if let Admit::Label(label) = &tracker.admit {
        asked.push(label);
    }
    asked.extend(tracker.exclude.as_deref());
    let facts = AdmissionFacts {
        inbox: Ok(true),
        labels: asked
            .into_iter()
            .filter(|name| has(name))
            .map(str::to_owned)
            .collect(),
        accounts,
    };
    Doctored {
        lines,
        facts: Some(Ok(facts)),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use owlshift_adapters::forge::github::GitHubForge;
    use owlshift_adapters::graphql::{Response, RestAnswer, Transport};
    use serde_json::{Value, json};

    use super::*;

    /// GitHub's REST answers by method and path, the calls kept. The bodies
    /// follow the documented shapes and G1's statuses (2026-11-12).
    #[derive(Clone, Default)]
    struct Rest {
        answers: Arc<Vec<(String, u16, Value)>>,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl Rest {
        fn new(answers: &[(&str, u16, Value)]) -> Self {
            Self {
                answers: Arc::new(
                    answers
                        .iter()
                        .map(|(c, s, b)| ((*c).to_owned(), *s, b.clone()))
                        .collect(),
                ),
                calls: Arc::default(),
            }
        }

        fn answer(&self, call: String) -> (u16, String) {
            self.calls.lock().unwrap().push(call.clone());
            let (_, status, body) = self
                .answers
                .iter()
                .find(|(c, _, _)| *c == call)
                .unwrap_or_else(|| panic!("no answer for {call}"));
            (*status, body.to_string())
        }
    }

    impl Transport for Rest {
        fn send(&self, request: &Value) -> Result<Response, String> {
            panic!("no GraphQL here: {request}")
        }

        fn get(&self, path: &str) -> Result<RestAnswer, String> {
            let (status, body) = self.answer(format!("GET {path}"));
            Ok(RestAnswer::new(status, body))
        }

        fn post(&self, path: &str, body: &Value) -> Result<Response, String> {
            let (status, answer) = self.answer(format!("POST {path} {}", body["name"]));
            Ok(Response::new(status, answer))
        }
    }

    fn github_tracker() -> config::Tracker {
        let text = format!(
            "requires = \">=0.1\"\n[tracker]\nkind = \"github\"\n{}\n[stack]\ngate = [\"make\"]\n\
             [pipeline]\ndefault = \"trivial\"\nplan_approval = \"never\"\n[models]\n[policy]\n\
             always_human = []\n",
            Github.admission_lines(Some("1"))
        );
        ProjectConfig::parse(&text).unwrap().tracker
    }

    /// OWL-537: `init`'s setup reads the issues, gives the signed-in account
    /// and creates the labels the repository lacks; a credential that may
    /// not read the issues stops it before any label, saying the fix.
    #[test]
    fn init_sets_up_the_labels_and_the_admitter_or_says_why_not() {
        let repo = Repo::parse("demo/project").unwrap();
        let tracker = github_tracker();
        let labels = |names: &[&str]| {
            json!(
                names
                    .iter()
                    .map(|n| json!({ "name": n }))
                    .collect::<Vec<_>>()
            )
        };
        let issues = ("GET repos/demo/project/issues?per_page=1", 200, json!([]));
        let user = (
            "GET user",
            200,
            json!({ "login": "ann", "id": 583231, "type": "User" }),
        );
        let listed = (
            "GET repos/demo/project/labels?per_page=100&page=1",
            200,
            labels(&["bug", "agent:ready", "agent:working", "agent:review"]),
        );
        let rest = Rest::new(&[
            issues.clone(),
            user.clone(),
            listed,
            (
                "POST repos/demo/project/labels \"agent:needs-input\"",
                201,
                json!({}),
            ),
            (
                "POST repos/demo/project/labels \"agent:proposed\"",
                201,
                json!({}),
            ),
        ]);
        let open = |rest: &Rest| {
            GitHubTracker::new(
                GitHubForge::with_transport(rest.clone(), repo.clone()),
                &tracker,
            )
        };
        assert_eq!(
            set_up(&open(&rest), &tracker, &repo),
            Ok(Connected {
                admitter: Some("583231".into()),
                created: vec!["agent:needs-input".into(), "agent:proposed".into()],
            })
        );

        let refused = Rest::new(&[(
            "GET repos/demo/project/issues?per_page=1",
            403,
            json!({ "message": "Resource not accessible by integration" }),
        )]);
        let error = set_up(&open(&refused), &tracker, &repo).unwrap_err();
        assert!(
            error.contains("HTTP 403") && error.contains("Issues (read and write)"),
            "{error}"
        );
        assert_eq!(refused.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_ticket_is_an_issue_number() {
        let tracker = ProjectConfig::parse(
            r#"
            requires = ">=0.1"
            [tracker]
            kind = "github"
            admit = { label = "agent:ready" }
            states = { ready = "agent:ready", working = "agent:working", needs_input = "agent:asked", review = "agent:review" }
            [stack]
            gate = ["make test"]
            [pipeline]
            default = "trivial"
            plan_approval = "never"
            [models]
            [policy]
            always_human = []
            "#,
        )
        .unwrap()
        .tracker;
        let check = |id: &str| Github.check_ticket(&tracker, &TicketId::new(id).unwrap());
        assert_eq!(check("12"), Ok(()));
        for refused in ["OWL-12", "012"] {
            let error = check(refused).unwrap_err();
            assert!(error.contains("not an issue number"), "{refused}: {error}");
        }
    }
}
