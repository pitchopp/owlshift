//! The background service's files about the projects it serves (OWL-434,
//! architecture section 5, "The background service", "Served projects" and
//! "Its configuration"): the machine's list of served projects,
//! `<data>/service/projects.json` ([`ServedProjects`]), written by `owlshift
//! start` under a lock of its own and replaced whole; and the project file a
//! served project runs on, `served.json` in the project's folder
//! ([`ServedConfig`]), read from its default branch and kept by its watch.
//!
//! And the daemon's own (OWL-435, "A supervisor"): the record of the daemon
//! that runs, `<data>/service/daemon.json` ([`DaemonRecord`]); and the
//! requests the service's files carry, a stop or a pause, each one line of
//! JSON saying who asked, when and why ([`Request`]).

use jiff::Timestamp;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::format::{
    self, ContractError, DAEMON_FORMAT, Format, SERVED_CONFIG_FORMAT, SERVED_PROJECTS_FORMAT,
};

/// Who asks in a [`Request`] the daemon writes: a project's pause after a
/// watch that could not start, the stop of a watch it no longer serves, or
/// the stop it relays.
pub const BY_DAEMON: &str = "owlshift daemon";

/// Who asks in a [`Request`] `owlshift stop` writes.
pub const BY_STOP: &str = "owlshift stop";

/// The daemon that runs on a data directory, written once it holds the
/// directory's daemon lock and removed when it ends: a record left by a
/// daemon killed outright names a process that no longer holds the lock.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift daemon")]
pub struct DaemonRecord {
    pub format: Format<DAEMON_FORMAT>,
    /// Its process id.
    pub pid: u32,
    /// The version of its binary.
    pub version: String,
    /// The binary it runs and starts every watch from, resolved when it
    /// started.
    pub binary: String,
    /// When it started.
    pub since: Timestamp,
}

impl DaemonRecord {
    const CONTRACT: &str = "daemon record";

    pub fn parse(input: &str) -> Result<Self, ContractError> {
        format::parse_json(Self::CONTRACT, DAEMON_FORMAT, input)
    }

    pub fn render(&self) -> String {
        format::render_json(self)
    }
}

/// A request one of the service's files holds: `<data>/service/stop`,
/// `<project data>/stop` or `<project data>/pause`. The file asks by
/// standing; what it holds says who asked, when and why, read leniently, so
/// a file another writer left still asks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "Owlshift service request")]
pub struct Request {
    /// Who asked: a command, such as [`BY_STOP`] or [`BY_DAEMON`].
    pub by: String,
    /// When.
    pub at: Timestamp,
    /// Why, in words for a person.
    pub why: String,
}

impl Request {
    /// A request made now by `by`, for `why`.
    pub fn new(by: &str, at: Timestamp, why: impl Into<String>) -> Self {
        Self {
            by: by.to_owned(),
            at,
            why: why.into(),
        }
    }

    /// The request a file holds; `None` when it does not read as one.
    pub fn parse(input: &str) -> Option<Self> {
        serde_json::from_str(input.trim()).ok()
    }

    /// One line of JSON, with its newline.
    pub fn render(&self) -> String {
        let mut line = serde_json::to_string(self).expect("a request always serializes to JSON");
        line.push('\n');
        line
    }
}

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

    #[test]
    fn a_daemon_record_reads_back_and_a_newer_one_is_refused() {
        let record = DaemonRecord {
            format: Format,
            pid: 4242,
            version: "0.4.0".to_owned(),
            binary: "/opt/homebrew/bin/owlshift".to_owned(),
            since: at("2026-10-25T08:00:00Z"),
        };
        let text = record.render();
        assert_eq!(DaemonRecord::parse(&text).unwrap(), record);
        let newer = text.replace("\"format\": 1", "\"format\": 2");
        assert!(matches!(
            DaemonRecord::parse(&newer),
            Err(ContractError::NewerFormat { found: 2, .. })
        ));
    }

    #[test]
    fn a_request_is_one_line_read_leniently() {
        let request = Request::new(BY_STOP, at("2026-10-25T08:00:00Z"), "asked");
        let line = request.render();
        assert_eq!(line.lines().count(), 1);
        assert!(line.ends_with('\n'));
        assert_eq!(Request::parse(&line), Some(request.clone()));
        // A field another writer added does not keep it from reading.
        let more = line.replace("}\n", ",\"host\":\"mac\"}\n");
        assert_eq!(Request::parse(&more), Some(request));
        assert_eq!(Request::parse("stop"), None);
    }
}
