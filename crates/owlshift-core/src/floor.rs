//! The policy floor: rules no project can configure away (architecture,
//! section 8). No agent merges, deploys or changes infrastructure. The
//! runner deploys or changes infrastructure only with a human approval, and
//! merges only under the one grant a project can give, merge on green (D14),
//! at the head it pushed. Agents hold no credentials, and some question
//! categories always go to a human.

use std::fmt;

use crate::vocab::{DelegationLevel, MergeMethod};

/// The project's leave for the runner to merge one pull request at one head
/// (D14, architecture section 5, "Merge on green"). Only
/// [`crate::follow::merge_due`] makes one: once the project's
/// `[forge] merge`, read at the base branch's head, allows the ticket and
/// every condition of merge on green holds. Its head is the commit Owlshift
/// last pushed on that ticket's branch; the Writer sends it as the merge's
/// expected head, so GitHub refuses the merge of any other head, another
/// ticket's pull request included (HTTP 409, check C19). A grant is made for
/// one pass and used at once, never kept.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MergeGrant {
    head: String,
    method: MergeMethod,
}

impl MergeGrant {
    pub(crate) fn new(head: &str, method: MergeMethod) -> Self {
        Self {
            head: head.to_owned(),
            method,
        }
    }

    /// The head the merge expects: the commit Owlshift last pushed.
    pub fn head(&self) -> &str {
        &self.head
    }

    /// The project's `[forge] merge_method`.
    pub fn method(&self) -> MergeMethod {
        self.method
    }
}

/// Something the Writer does on the tracker, the forge or beyond. The Writer
/// asks [`check_action`] before each one.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    /// Post or edit a marked comment on the ticket, or resolve one the
    /// runner posted so it shows folded (OWL-353).
    Comment,
    /// Create or update a document attached to the ticket: its plan
    /// (OWL-350, architecture section 5, "Kept and shown").
    Document,
    /// Move the ticket's visible stage on the tracker.
    SetVisibleStage,
    /// Propose a follow-up in the tracker's triage inbox.
    ProposeFollowup,
    /// Push the ticket's branch.
    PushBranch,
    /// Push an Owlshift ref: a claim or the ticket's state.
    PushRef,
    /// Open or update the ticket's pull request.
    OpenPullRequest,
    /// Merge a pull request: under the project's merge on green grant, or
    /// `None` for a merge without one, which the floor refuses.
    Merge(Option<MergeGrant>),
    /// Deploy.
    Deploy,
    /// Change infrastructure: environment variables, feature flags, DNS, CI
    /// settings.
    ChangeInfrastructure,
    /// Write a `blocks` relation between two tickets: a dependency intake
    /// found (architecture section 5, "Dependencies to relations").
    Relate,
}

/// Whether a human approval of the action is recorded on the ticket.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HumanApproval {
    Absent,
    Recorded,
}

/// Checks a Writer action against the floor: a merge passes only with a
/// merge grant whose expected head is a full commit id, whatever approval is
/// recorded (without the grant a human merges); a deploy or an
/// infrastructure change needs a human approval recorded on the ticket;
/// everything else passes.
pub fn check_action(action: Action, approval: HumanApproval) -> Result<(), FloorViolation> {
    // No wildcard: a new action does not compile until the floor decides it.
    match action {
        Action::Merge(Some(grant)) if is_commit_id(grant.head()) => Ok(()),
        Action::Merge(_) => Err(FloorViolation::Merge),
        Action::Deploy | Action::ChangeInfrastructure => match approval {
            HumanApproval::Recorded => Ok(()),
            HumanApproval::Absent => Err(FloorViolation::Unapproved(action)),
        },
        Action::Comment
        | Action::Document
        | Action::SetVisibleStage
        | Action::ProposeFollowup
        | Action::PushBranch
        | Action::PushRef
        | Action::OpenPullRequest => Ok(()),
        Action::Relate => Ok(()),
    }
}

/// Whether `id` is a full commit id as git prints it: 40 or 64 lower-case
/// hexadecimal digits (SHA-1 or SHA-256), never a branch name or an
/// abbreviation.
pub fn is_commit_id(id: &str) -> bool {
    matches!(id.len(), 40 | 64) && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Checks a follow-up proposal against the floor (architecture section 5,
/// "Where", OWL-253): one that carries the admission label would enter the
/// queue without a person (principle 6), so it is refused before it is
/// filed. `carried` is the one label a proposal may carry, the proposals
/// inbox's label on a team without a triage state; `admission_label` is the
/// project's `[tracker].admit` label, `None` for another gesture. Names
/// compare ASCII case-insensitively, the stricter reading. A proposal filed
/// in the ready state is refused by the tracker adapter, where the target
/// state is known.
pub fn check_proposal(admission_label: Option<&str>, carried: &str) -> Result<(), FloorViolation> {
    match admission_label {
        Some(label) if label.eq_ignore_ascii_case(carried) => {
            Err(FloorViolation::ProposalAdmitted(label.to_owned()))
        }
        _ => Ok(()),
    }
}

/// Environment variables that carry a model, tracker, forge or cloud
/// credential. No agent receives one, even when a project declares it for its
/// gate. A guardrail, not a proof: agents also start from an empty
/// environment plus a few inherited and the declared variables
/// ([`crate::agent_env`]).
pub const CREDENTIAL_VARIABLES: &[&str] = &[
    // Model providers: Owlshift never handles a model API key. The one model
    // login it holds, the agent runs' `claude setup-token` token, is handed
    // by the runner to the harness command alone, on a descriptor the second
    // variable names, never through the agent environment (OWL-94, OWL-96).
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
    "OPENAI_API_KEY",
    // Codex's API key: `codex exec` sends it as its login (OWL-125).
    "CODEX_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    // Tracker. The Linear app's pair, as its maintainer's dotenv names it:
    // the runner keeps it in the keychain and its token in memory (OWL-157).
    "LINEAR_API_KEY",
    "LINEAR_APP_CLIENT_ID",
    "LINEAR_APP_CLIENT_SECRET",
    "JIRA_API_TOKEN",
    "ATLASSIAN_API_TOKEN",
    // Forge.
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "GITLAB_TOKEN",
    "GITLAB_PRIVATE_TOKEN",
    "CI_JOB_TOKEN",
    "GITEA_TOKEN",
    "BITBUCKET_TOKEN",
    "BITBUCKET_APP_PASSWORD",
    "SSH_AUTH_SOCK",
    // Cloud.
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    // Claude Code's Bedrock API key (OWL-120).
    "AWS_BEARER_TOKEN_BEDROCK",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "AZURE_CLIENT_SECRET",
    "AZURE_CLIENT_CERTIFICATE_PATH",
];

/// Checks the names of the variables an agent would receive, such as those a
/// project declares for its gate. Names compare ASCII case-insensitively, as
/// on Windows. Every credential variable found is reported; none is dropped
/// silently. A whole agent environment, whose gh token variables hold a
/// placeholder, is checked with [`crate::agent_env::check_agent_variables`].
pub fn check_agent_environment<'a>(
    names: impl IntoIterator<Item = &'a str>,
) -> Result<(), FloorViolation> {
    let found: Vec<String> = names
        .into_iter()
        .filter(|name| {
            CREDENTIAL_VARIABLES
                .iter()
                .any(|credential| credential.eq_ignore_ascii_case(name))
        })
        .map(str::to_owned)
        .collect();
    if found.is_empty() {
        Ok(())
    } else {
        Err(FloorViolation::CredentialVariables(found))
    }
}

/// A question category that goes to a human by default (architecture
/// section 4, "Always human"). Two are the floor proper, `security` and
/// `irreversible`, which reach a human at every delegation level; the
/// project's level may hand the other four to the resolver
/// ([`FloorCategory::human_at`]). Projects may add categories (see
/// [`crate::gate::GatePolicy`]), which stay human at every level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FloorCategory {
    Security,
    DataLoss,
    Money,
    /// Legal wording.
    Legal,
    /// Irreversible external actions.
    Irreversible,
    /// Scope changes.
    Scope,
}

impl FloorCategory {
    pub const ALL: [Self; 6] = [
        Self::Security,
        Self::DataLoss,
        Self::Money,
        Self::Legal,
        Self::Irreversible,
        Self::Scope,
    ];

    /// The token a role writes as a question's category.
    pub const fn token(self) -> &'static str {
        match self {
            Self::Security => "security",
            Self::DataLoss => "data_loss",
            Self::Money => "money",
            Self::Legal => "legal",
            Self::Irreversible => "irreversible",
            Self::Scope => "scope",
        }
    }

    /// Whether a question of this category goes to a human at `level`: the
    /// floor (`security`, `irreversible`) at every level; `scope` below
    /// `ticket`; `data_loss`, `money` and `legal` below `all` (OWL-328).
    /// The one place the levels' always-human sets are written.
    pub fn human_at(self, level: DelegationLevel) -> bool {
        match self {
            Self::Security | Self::Irreversible => true,
            Self::Scope => level < DelegationLevel::Ticket,
            Self::DataLoss | Self::Money | Self::Legal => level < DelegationLevel::All,
        }
    }
}

/// Whether a question's category goes to a human at `level` by the
/// categories of [`FloorCategory`] ([`FloorCategory::human_at`]), in any
/// spelling, as a whole-word token anywhere in it. A blank category names
/// none: since OWL-342 a question that does not say what it is about reaches
/// the resolver, whose own label, a token, then routes it
/// ([`crate::gate::GatePolicy::route_decided`]). A project's own additions
/// are the gate's ([`crate::gate::GatePolicy::always_human`]).
pub fn is_human_category(category: &str, level: DelegationLevel) -> bool {
    let category = normalize(category);
    FloorCategory::ALL
        .into_iter()
        .any(|floor| floor.human_at(level) && matches_token(&category, floor.token()))
}

/// Whether a category names one of [`FloorCategory`]'s six as a whole-word
/// token, in any spelling, whatever the level hands to the resolver.
pub fn names_floor_category(category: &str, floor: FloorCategory) -> bool {
    matches_token(&normalize(category), floor.token())
}

/// Whether a category is written as a token: words of ASCII lowercase
/// letters and digits, joined by single `_`, not blank. A token is its own
/// normalized form, so the floor's matcher reads it as written; a label
/// such as `dataLoss` (normalized `dataloss`) or `sécurité` could name a
/// floor topic the matcher does not see.
pub fn is_token(category: &str) -> bool {
    category.split('_').all(|word| {
        !word.is_empty()
            && word
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    })
}

/// Normalizes a category: ASCII lowercase words, joined by single `_`, with
/// every other character a separator. `"Data  Loss"`, `"data-loss"` and
/// `"DATA_LOSS!"` all become `data_loss`.
pub fn normalize(category: &str) -> String {
    category
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join("_")
}

/// Whether a normalized category is a normalized token, or contains it as a
/// whole-word run anywhere in it: `scope_change` matches `scope`,
/// `legal_wording` matches `legal`, `risk_of_data_loss` matches `data_loss`.
pub(crate) fn matches_token(category: &str, token: &str) -> bool {
    !token.is_empty() && format!("_{category}_").contains(&format!("_{token}_"))
}

/// An action or a set-up the floor refuses.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum FloorViolation {
    /// A merge without the project's merge on green grant, or whose expected
    /// head is not a full commit id: a human merges.
    Merge,
    /// The action needs a human approval recorded on the ticket.
    Unapproved(Action),
    /// These credential variables would reach an agent.
    CredentialVariables(Vec<String>),
    /// A proposal would carry this admission label (OWL-253).
    ProposalAdmitted(String),
}

impl fmt::Display for FloorViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Merge => f.write_str(
                "Owlshift merges a pull request only under the project's merge on green, at \
                 the head it pushed: otherwise a human does",
            ),
            Self::Unapproved(action) => write!(
                f,
                "{action:?} needs a human approval recorded on the ticket"
            ),
            Self::CredentialVariables(names) => write!(
                f,
                "agents never receive credentials; refused variables: {}",
                names.join(", ")
            ),
            Self::ProposalAdmitted(label) => write!(
                f,
                "a proposal would carry the admission label {label:?} and enter the queue \
                 without a person"
            ),
        }
    }
}

impl std::error::Error for FloorViolation {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without the project's merge on green grant a human merges, approval
    /// or not.
    #[test]
    fn a_merge_is_refused_even_when_approved() {
        for approval in [HumanApproval::Absent, HumanApproval::Recorded] {
            assert_eq!(
                check_action(Action::Merge(None), approval),
                Err(FloorViolation::Merge)
            );
        }
    }

    /// A grant passes at a full expected head, and only there.
    #[test]
    fn a_merge_passes_with_a_grant_at_a_full_expected_head() {
        let sha1 = "0123456789abcdef0123456789abcdef01234567";
        let sha256 = "a".repeat(64);
        for head in [sha1, sha256.as_str()] {
            let grant = MergeGrant::new(head, MergeMethod::Squash);
            assert_eq!(
                check_action(Action::Merge(Some(grant)), HumanApproval::Absent),
                Ok(())
            );
        }
        for head in [
            "",
            "main",
            &sha1[..12],
            &sha1.to_uppercase(),
            "g".repeat(40).as_str(),
        ] {
            let grant = MergeGrant::new(head, MergeMethod::Squash);
            assert_eq!(
                check_action(Action::Merge(Some(grant)), HumanApproval::Recorded),
                Err(FloorViolation::Merge),
                "{head:?}"
            );
        }
    }

    #[test]
    fn a_deploy_or_an_infrastructure_change_needs_a_recorded_approval() {
        for action in [Action::Deploy, Action::ChangeInfrastructure] {
            assert_eq!(
                check_action(action.clone(), HumanApproval::Absent),
                Err(FloorViolation::Unapproved(action.clone()))
            );
            assert_eq!(check_action(action, HumanApproval::Recorded), Ok(()));
        }
    }

    #[test]
    fn the_other_writer_actions_pass() {
        let actions = [
            Action::Comment,
            Action::Document,
            Action::SetVisibleStage,
            Action::ProposeFollowup,
            Action::PushBranch,
            Action::PushRef,
            Action::OpenPullRequest,
        ];
        for action in actions {
            assert_eq!(check_action(action, HumanApproval::Absent), Ok(()));
        }
    }

    #[test]
    fn credential_variables_never_reach_an_agent() {
        for &name in CREDENTIAL_VARIABLES {
            assert_eq!(
                check_agent_environment([name]),
                Err(FloorViolation::CredentialVariables(vec![name.to_owned()]))
            );
        }
        assert_eq!(
            check_agent_environment([
                "DATABASE_URL",
                "github_token",
                "PATH",
                "Openai_Api_Key",
                "gitlab_token",
                "SSH_AUTH_SOCK",
            ]),
            Err(FloorViolation::CredentialVariables(vec![
                "github_token".to_owned(),
                "Openai_Api_Key".to_owned(),
                "gitlab_token".to_owned(),
                "SSH_AUTH_SOCK".to_owned(),
            ]))
        );
        assert_eq!(
            check_agent_environment(["DATABASE_URL", "PATH", "NODE_ENV"]),
            Ok(())
        );
    }

    #[test]
    fn floor_categories_are_recognized_in_any_spelling() {
        let default = DelegationLevel::Discoverable;
        for category in FloorCategory::ALL {
            assert!(is_human_category(category.token(), default), "{category:?}");
        }
        let spellings = [
            "Security",
            "Data Loss",
            "data  loss",
            "data-loss",
            "DATA_LOSS!",
            "data\tloss",
            "money",
            "legal wording",
            "irreversible external actions",
            "scope_change",
            "  Scope Changes ",
            "risk_of_data_loss",
            "possible security issue",
        ];
        for spelling in spellings {
            assert!(is_human_category(spelling, default), "{spelling:?}");
        }
    }

    #[test]
    fn a_blank_category_or_another_word_names_no_human_category() {
        for level in DelegationLevel::ALL {
            // Since OWL-342 a blank category reaches the resolver.
            for blank in ["", "   ", "--"] {
                assert!(!is_human_category(blank, level), "{blank:?} at {level:?}");
            }
            for other in ["testing", "naming", "securities", "scoped", "datalossy"] {
                assert!(!is_human_category(other, level), "{other:?} at {level:?}");
            }
        }
    }

    /// The floor proper reaches a human at every level; `scope` stops at
    /// `ticket`, `data_loss`, `money` and `legal` at `all`.
    #[test]
    fn each_level_keeps_its_categories_for_a_human() {
        use DelegationLevel as L;
        use FloorCategory::{DataLoss, Irreversible, Legal, Money, Scope, Security};
        let kept: [(L, &[FloorCategory]); 5] = [
            (L::None, &FloorCategory::ALL),
            (L::Facts, &FloorCategory::ALL),
            (L::Discoverable, &FloorCategory::ALL),
            (L::Ticket, &[Security, DataLoss, Money, Legal, Irreversible]),
            (L::All, &[Security, Irreversible]),
        ];
        for (level, human) in kept {
            for category in FloorCategory::ALL {
                assert_eq!(
                    category.human_at(level),
                    human.contains(&category),
                    "{category:?} at {level:?}"
                );
                assert_eq!(
                    is_human_category(&format!("Possible {} issue", category.token()), level),
                    human.contains(&category),
                    "{category:?} at {level:?}"
                );
            }
        }
        assert!(names_floor_category("Scope change", Scope));
        assert!(!names_floor_category("scoped", Scope));
    }

    #[test]
    fn a_proposal_never_carries_the_admission_label() {
        assert_eq!(check_proposal(None, "agent:proposed"), Ok(()));
        assert_eq!(check_proposal(Some("agent"), "agent:proposed"), Ok(()));
        assert_eq!(
            check_proposal(Some("Agent:Proposed"), "agent:proposed"),
            Err(FloorViolation::ProposalAdmitted(
                "Agent:Proposed".to_owned()
            ))
        );
    }

    #[test]
    fn violations_explain_themselves() {
        assert_eq!(
            FloorViolation::CredentialVariables(vec!["GH_TOKEN".to_owned()]).to_string(),
            "agents never receive credentials; refused variables: GH_TOKEN"
        );
        assert_eq!(
            FloorViolation::Unapproved(Action::Deploy).to_string(),
            "Deploy needs a human approval recorded on the ticket"
        );
        assert_eq!(
            FloorViolation::Merge.to_string(),
            "Owlshift merges a pull request only under the project's merge on green, at the \
             head it pushed: otherwise a human does"
        );
    }
}
