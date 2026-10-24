//! The background service's files about the projects it serves (OWL-434,
//! architecture section 5, "The background service", "Served projects" and
//! "Its configuration"): the machine's list of served projects,
//! `<data>/service/projects.json` ([`ServedProjects`]), written by `owlshift
//! start` under a lock of its own and replaced whole; and the project file a
//! served project runs on, `served.json` in the project's folder
//! ([`ServedConfig`]), read from its default branch and kept by its watch.

use jiff::Timestamp;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::format::{self, ContractError, Format, SERVED_CONFIG_FORMAT, SERVED_PROJECTS_FORMAT};

/// The projects this machine serves.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift served projects")]
pub struct ServedProjects {
    pub format: Format<SERVED_PROJECTS_FORMAT>,
    #[serde(default)]
    pub projects: Vec<ServedProject>,
}

/// One served project.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServedProject {
    /// Its GitHub repository, `owner/name`.
    pub repository: String,
    /// The checkout it was registered from, an absolute path: it only
    /// locates the project, whose `origin` it names.
    pub checkout: String,
}

impl ServedProjects {
    const CONTRACT: &str = "served projects";

    /// Parses the file: one entry per repository at most, compared as the
    /// data directory places it, ignoring ASCII case.
    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let served: Self = format::parse_json(Self::CONTRACT, SERVED_PROJECTS_FORMAT, input)?;
        for (index, project) in served.projects.iter().enumerate() {
            if project.repository.is_empty() || project.checkout.is_empty() {
                return Err(ContractError::invalid(
                    Self::CONTRACT,
                    "a project without its repository or its checkout",
                ));
            }
            if served.projects[..index]
                .iter()
                .any(|other| other.repository.eq_ignore_ascii_case(&project.repository))
            {
                return Err(ContractError::invalid(
                    Self::CONTRACT,
                    format!("{} is listed twice", project.repository),
                ));
            }
        }
        Ok(served)
    }

    pub fn render(&self) -> String {
        format::render_json(self)
    }

    /// The entry of `repository`, ignoring ASCII case.
    pub fn find(&self, repository: &str) -> Option<&ServedProject> {
        self.projects
            .iter()
            .find(|project| project.repository.eq_ignore_ascii_case(repository))
    }
}

/// The project file a served project runs on, and why a newer one is not.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift served project file")]
pub struct ServedConfig {
    pub format: Format<SERVED_CONFIG_FORMAT>,
    /// The project's GitHub repository, `owner/name`.
    pub repository: String,
    /// The last project file read at the default branch that Owlshift
    /// accepted: none until one is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept: Option<KeptFile>,
    /// Why the default branch's project file is not the one kept, while it
    /// is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<RefusedFile>,
}

/// A project file kept.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeptFile {
    /// The default branch, such as `main`.
    pub branch: String,
    /// The latest commit of the default branch read that holds this text, a
    /// full id.
    pub commit: String,
    /// The file's text, as that commit holds it.
    pub text: String,
    /// When this text was first kept.
    pub since: Timestamp,
}

/// A project file refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RefusedFile {
    /// The commit of the default branch read last, a full id; none when the
    /// default branch could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Why it is refused.
    pub reason: String,
    /// Since when a refusal has stood without a break: the operator is told
    /// once, when it starts.
    pub since: Timestamp,
}

impl ServedConfig {
    const CONTRACT: &str = "served project file";

    /// A record of `repository` holding nothing yet.
    pub fn new(repository: impl Into<String>) -> Self {
        Self {
            format: Format,
            repository: repository.into(),
            kept: None,
            refused: None,
        }
    }

    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let served: Self = format::parse_json(Self::CONTRACT, SERVED_CONFIG_FORMAT, input)?;
        if served.repository.is_empty() {
            return Err(ContractError::invalid(
                Self::CONTRACT,
                "the record names no repository",
            ));
        }
        Ok(served)
    }

    pub fn render(&self) -> String {
        format::render_json(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    #[test]
    fn served_projects_read_back_and_refuse_a_repository_twice() {
        let served = ServedProjects {
            format: Format,
            projects: vec![ServedProject {
                repository: "acme/web".to_owned(),
                checkout: "/home/ada/web".to_owned(),
            }],
        };
        let text = served.render();
        assert_eq!(ServedProjects::parse(&text).unwrap(), served);
        assert!(served.find("ACME/Web").is_some());

        let twice = text.replace(
            "  ]",
            "  ,{\"repository\": \"Acme/Web\", \"checkout\": \"/x\"}]",
        );
        let error = ServedProjects::parse(&twice).unwrap_err().to_string();
        assert!(error.contains("Acme/Web is listed twice"), "{error}");

        let newer = text.replace("\"format\": 1", "\"format\": 2");
        assert!(matches!(
            ServedProjects::parse(&newer),
            Err(ContractError::NewerFormat { found: 2, .. })
        ));
    }

    #[test]
    fn a_served_record_reads_back_and_a_newer_one_is_refused() {
        let mut served = ServedConfig::new("acme/web");
        served.kept = Some(KeptFile {
            branch: "main".to_owned(),
            commit: "a".repeat(40),
            text: "[tracker]\nkind = \"linear\"\n".to_owned(),
            since: at("2026-10-24T08:00:00Z"),
        });
        served.refused = Some(RefusedFile {
            commit: Some("b".repeat(40)),
            reason: "invalid".to_owned(),
            since: at("2026-10-24T09:00:00Z"),
        });
        let text = served.render();
        assert_eq!(ServedConfig::parse(&text).unwrap(), served);
        let newer = text.replace("\"format\": 1", "\"format\": 2");
        assert!(matches!(
            ServedConfig::parse(&newer),
            Err(ContractError::NewerFormat { found: 2, .. })
        ));
    }
}
