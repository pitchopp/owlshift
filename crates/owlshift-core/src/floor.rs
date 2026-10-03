//! The policy floor: rules no project can configure away (architecture,
//! section 8). Owlshift never merges, deploys or changes infrastructure on
//! its own, agents hold no credentials, and some question categories always
//! go to a human.

use std::fmt;

/// Something the Writer does on the tracker, the forge or beyond. The Writer
/// asks [`check_action`] before each one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    /// Post or edit a marked comment on the ticket.
    Comment,
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
    /// Merge a pull request.
    Merge,
    /// Deploy.
    Deploy,
    /// Change infrastructure: environment variables, feature flags, DNS, CI
    /// settings.
    ChangeInfrastructure,
}

/// Whether a human approval of the action is recorded on the ticket.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HumanApproval {
    Absent,
    Recorded,
}

/// Checks a Writer action against the floor: a merge is always refused (a
/// human merges); a deploy or an infrastructure change needs a human approval
/// recorded on the ticket; everything else passes.
pub fn check_action(action: Action, approval: HumanApproval) -> Result<(), FloorViolation> {
    // No wildcard: a new action does not compile until the floor decides it.
    match action {
        Action::Merge => Err(FloorViolation::Merge),
        Action::Deploy | Action::ChangeInfrastructure => match approval {
            HumanApproval::Recorded => Ok(()),
            HumanApproval::Absent => Err(FloorViolation::Unapproved(action)),
        },
        Action::Comment
        | Action::SetVisibleStage
        | Action::ProposeFollowup
        | Action::PushBranch
        | Action::PushRef
        | Action::OpenPullRequest => Ok(()),
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
    // Tracker.
    "LINEAR_API_KEY",
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

/// A question category that always goes to a human. Projects may add
/// categories (see [`crate::gate::GatePolicy`]), never remove one of these.
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
}

/// Whether a question's category is one of the floor's. A blank category
/// counts: a question that does not say what it is about goes to a human.
pub fn is_floor_category(category: &str) -> bool {
    let category = normalize(category);
    category.is_empty()
        || FloorCategory::ALL
            .into_iter()
            .any(|floor| matches_token(&category, floor.token()))
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
    /// Owlshift never merges: a human does.
    Merge,
    /// The action needs a human approval recorded on the ticket.
    Unapproved(Action),
    /// These credential variables would reach an agent.
    CredentialVariables(Vec<String>),
}

impl fmt::Display for FloorViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Merge => f.write_str("Owlshift never merges a pull request: a human does"),
            Self::Unapproved(action) => write!(
                f,
                "{action:?} needs a human approval recorded on the ticket"
            ),
            Self::CredentialVariables(names) => write!(
                f,
                "agents never receive credentials; refused variables: {}",
                names.join(", ")
            ),
        }
    }
}

impl std::error::Error for FloorViolation {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_merge_is_refused_even_when_approved() {
        for approval in [HumanApproval::Absent, HumanApproval::Recorded] {
            assert_eq!(
                check_action(Action::Merge, approval),
                Err(FloorViolation::Merge)
            );
        }
    }

    #[test]
    fn a_deploy_or_an_infrastructure_change_needs_a_recorded_approval() {
        for action in [Action::Deploy, Action::ChangeInfrastructure] {
            assert_eq!(
                check_action(action, HumanApproval::Absent),
                Err(FloorViolation::Unapproved(action))
            );
            assert_eq!(check_action(action, HumanApproval::Recorded), Ok(()));
        }
    }

    #[test]
    fn the_other_writer_actions_pass() {
        let actions = [
            Action::Comment,
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
        for category in FloorCategory::ALL {
            assert!(is_floor_category(category.token()), "{category:?}");
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
            assert!(is_floor_category(spelling), "{spelling:?}");
        }
    }

    #[test]
    fn a_blank_category_counts_as_the_floor_and_others_do_not() {
        for blank in ["", "   ", "--"] {
            assert!(is_floor_category(blank), "{blank:?}");
        }
        for other in ["testing", "naming", "securities", "scoped", "datalossy"] {
            assert!(!is_floor_category(other), "{other:?}");
        }
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
    }
}
