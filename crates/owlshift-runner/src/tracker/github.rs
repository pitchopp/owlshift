//! The GitHub Issues tracker's [`Kind`] (OWL-535): the issues of the
//! project's own repository, reached with the project's forge credential, so
//! it has no secret of its own (build plan, "The GitHub Issues adapter").
//! `owlshift init --tracker github` sets it up and `owlshift doctor` checks
//! it (OWL-537, part 6): the Issues permission, the labels, the admitters,
//! through a probe of its own ([`Probe`], OWL-544).

use std::path::Path;
use std::sync::Arc;

use owlshift_adapters::forge::Repo;
use owlshift_adapters::forge::github::GitHubForge;
use owlshift_adapters::tracker::github::{GitHubTracker, IssuesAccess};
use owlshift_adapters::tracker::linear::AdmissionFacts;
use owlshift_adapters::tracker::{Capability, PROPOSED_LABEL, Tracker};
use owlshift_contracts::config::{self, Admit, PersonalConfig, ProjectConfig};
use owlshift_contracts::ids::TicketId;
use owlshift_platform::keychain::{Keychain, Secret};

use super::{
    AdmissionWords, Connected, Doctored, ForgeCredential, GithubFacts, Kind, Opener, Probe, Probed,
    Setup, StatesError,
};
use crate::config::FileState;
use crate::doctor::{Check, DOES_NOT_BLOCK, Section, Step};
use crate::forge::AppAccess;
use crate::init::SecretSpec;
use crate::project::ProjectDirs;
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
        let probe = FactsProbe {
            repo: repo.clone(),
            tracker: config.tracker.clone(),
        };
        doctored(&config.tracker, repo, super::ask::<Facts>(system, &probe))
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

    /// The project's forge credential, opened as `owlshift do` opens it:
    /// the GitHub App's user token, refreshed under the project's account
    /// lock so a long-lived UI keeps posting after the token's 8 hours, or
    /// the personal token. Both act as the signed-in person, so a comment
    /// through the adapter is theirs (G1) and `GET user` names its account
    /// (OWL-552).
    fn reply_key(
        &self,
        data: &Path,
        repo: &Repo,
        config: &ProjectConfig,
        personal: &FileState<PersonalConfig>,
    ) -> Result<Key, String> {
        let keychain =
            Arc::new(Keychain::system().map_err(|e| format!("the keychain cannot be read: {e}"))?);
        let mut look = |account: &str| keychain.read(account).map_err(|e| e.to_string());
        let found = crate::secrets::forge(repo, &mut look)?;
        let access = AppAccess {
            keychain: keychain.clone(),
            lock: ProjectDirs::new(data, repo).account_lock_file(),
            app: crate::forge::GitHubApp::from_personal(match personal {
                FileState::Loaded { config, .. } => Some(config),
                _ => None,
            }),
        };
        let open = || {
            crate::forge::github(&found, repo.clone(), &access)
                .map_err(|refusal| refusal.to_string())
        };
        key_of(open()?, open()?, &config.tracker)
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

type Facts = Result<GithubFacts, StatesError>;

/// What a GitHub Issues project's repository says of its setup
/// ([`super::github_facts`], OWL-537).
#[derive(Debug)]
struct FactsProbe {
    repo: Repo,
    tracker: config::Tracker,
}

impl Probe for FactsProbe {
    fn read(&self, keychain: &Keychain) -> Probed {
        Probed::new::<Facts>(super::github_facts(keychain, &self.repo, &self.tracker))
    }

    fn keychain_error(&self, reason: String) -> Probed {
        Probed::new::<Facts>(Err(StatesError::Keychain(reason)))
    }

    /// As [`crate::system::fake::FakeSystem::github_facts_are`] scripted
    /// it; by default the issues read, every label the project names
    /// exists, every admitter is a person's account.
    #[cfg(test)]
    fn fake(&self, host: &crate::system::fake::FakeSystem) -> Probed {
        let scripted = host
            .scripted::<Script>()
            .and_then(|script| script.facts.clone());
        Probed::new::<Facts>(scripted.unwrap_or_else(|| {
            Ok(GithubFacts {
                access: IssuesAccess::Granted,
                labels: Some(
                    named_labels(&self.tracker)
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                ),
                accounts: Some(
                    self.tracker
                        .admitters
                        .iter()
                        .flatten()
                        .map(|id| owlshift_adapters::tracker::AccountFact {
                            id: id.clone(),
                            app: false,
                            active: true,
                        })
                        .collect(),
                ),
            })
        }))
    }
}

/// What a GitHub Issues project's repository answers the test host.
#[cfg(test)]
#[derive(Default)]
struct Script {
    facts: Option<Facts>,
}

#[cfg(test)]
impl crate::system::fake::FakeSystem {
    /// A GitHub Issues project's repository answers so (OWL-537).
    pub(crate) fn github_facts_are(self, answer: Facts) -> Self {
        self.script(|script: &mut Script| script.facts = Some(answer))
    }
}

/// The reply key of a GitHub Issues project from two opened forges of the
/// same credential: `writer` posts through the adapter, `reader` reads the
/// key's own account and, later, the name of another (as Linear's opens two
/// adapters, so the key's reads never wait on a post).
pub fn key_of(
    writer: GitHubForge,
    reader: GitHubForge,
    tracker: &config::Tracker,
) -> Result<Key, String> {
    let reader = GitHubTracker::new(reader, tracker);
    let account = reader
        .key_account()
        .map_err(|e| format!("the GitHub account of the project's token could not be read: {e}"))?;
    Ok(Key {
        account,
        tracker: Box::new(GitHubTracker::new(writer, tracker)),
        names: Box::new(move |id: &str| reader.account_name(id).map_err(|e| e.to_string())),
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
            // A label is named by its `name`; any other write by its body.
            let said = if body["name"].is_null() {
                body.to_string()
            } else {
                body["name"].to_string()
            };
            let (status, answer) = self.answer(format!("POST {path} {said}"));
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

    const ISSUE: &str = "repos/demo/project/issues/7";

    /// The reply key of a GitHub Issues project is the credential's own
    /// account (OWL-552): the forge's `GET user` gives its numeric id, its
    /// login and its type; a comment posted through the key's tracker is
    /// read back under that account; another account's name is read by its
    /// id, an unknown one is none.
    #[test]
    fn the_reply_key_is_the_forges_account_and_posts_as_it() {
        let repo = Repo::parse("demo/project").unwrap();
        let tracker = github_tracker();
        let bea = json!({ "login": "bea", "id": 2, "type": "User" });
        let comment = json!({
            "id": 50,
            "issue_url": format!("https://api.github.com/{ISSUE}"),
            "user": bea,
            "created_at": "2026-11-13T10:00:00Z",
            "updated_at": "2026-11-13T10:00:00Z",
            "body": "Q1: B",
        });
        let rest = Rest::new(&[
            ("GET user", 200, bea.clone()),
            (
                "GET user/3",
                200,
                json!({ "login": "cy", "id": 3, "type": "User" }),
            ),
            ("GET user/4", 404, json!({ "message": "Not Found" })),
            (
                "GET repos/demo/project/issues/7",
                200,
                json!({
                    "url": format!("https://api.github.com/{ISSUE}"),
                    "number": 7,
                    "title": "Say hello",
                    "body": null,
                    "user": bea,
                    "assignees": [bea],
                    "labels": [],
                    "state": "open",
                }),
            ),
            (
                "POST repos/demo/project/issues/7/comments {\"body\":\"Q1: B\"}",
                201,
                comment.clone(),
            ),
            (
                "GET repos/demo/project/issues/7/comments?per_page=100&page=1",
                200,
                json!([comment]),
            ),
        ]);
        let forge = || GitHubForge::with_transport(rest.clone(), repo.clone());
        let key = key_of(forge(), forge(), &tracker).unwrap();
        assert_eq!(
            key.account,
            owlshift_adapters::tracker::KeyAccount {
                id: "2".into(),
                name: "bea".into(),
                app: false,
                workspace: String::new(),
            }
        );
        let posted = key
            .tracker
            .post_comment(&TicketId::new("7").unwrap(), "Q1: B")
            .unwrap();
        assert_eq!(posted.body, "Q1: B");
        assert_eq!(
            posted.author,
            owlshift_adapters::tracker::Author::Account(owlshift_adapters::tracker::Person {
                id: "2".into(),
                name: "bea".into(),
            })
        );
        assert_eq!((key.names)("3"), Ok(Some("cy".to_owned())));
        assert_eq!((key.names)("4"), Ok(None));
        assert_eq!((key.names)("cy"), Ok(None));
    }

    /// A credential whose account cannot be read gives no key, and the text
    /// says so without blaming the Issues permission.
    #[test]
    fn a_credential_that_cannot_read_its_account_gives_no_key() {
        let repo = Repo::parse("demo/project").unwrap();
        let rest = Rest::new(&[("GET user", 401, json!({ "message": "Bad credentials" }))]);
        let forge = || GitHubForge::with_transport(rest.clone(), repo.clone());
        let error = key_of(forge(), forge(), &github_tracker()).unwrap_err();
        assert!(
            error.starts_with("the GitHub account of the project's token could not be read: "),
            "{error}"
        );
        assert!(!error.contains("Issues"), "{error}");
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
