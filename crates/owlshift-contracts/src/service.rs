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
//!
//! And the install manifest (OWL-437, runtime and operations, "Install,
//! lifecycle & uninstall"), `<data>/service/manifest.json`
//! ([`InstallManifest`]): what Owlshift wrote outside its data directory,
//! which `owlshift uninstall` removes.

use jiff::Timestamp;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::format::{
    self, ContractError, DAEMON_FORMAT, DIGEST_FORMAT, Format, MANIFEST_FORMAT,
    SERVED_CONFIG_FORMAT, SERVED_PROJECTS_FORMAT,
};

/// Who asks in a [`Request`] the daemon writes: a project's pause after a
/// watch that could not start, the stop of a watch it no longer serves, or
/// the stop it relays.
pub const BY_DAEMON: &str = "owlshift daemon";

/// Who asks in a [`Request`] `owlshift stop` writes.
pub const BY_STOP: &str = "owlshift stop";

/// Who asks in a [`Request`] `owlshift pause` writes (OWL-438).
pub const BY_PAUSE: &str = "owlshift pause";

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

/// The daily digest's record, `<data>/service/digest.json` (OWL-440): when
/// the latest digest was made, so a machine asleep at the hour sends it once
/// at wake and never twice.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift digest")]
pub struct DigestRecord {
    pub format: Format<DIGEST_FORMAT>,
    /// When the latest digest was made; before the first one, when the
    /// daemon first counted from. The next digest is due at the first
    /// digest hour after it, and tells what happened since.
    pub at: Timestamp,
    /// The latest digest's text; absent until one is made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
}

impl DigestRecord {
    const CONTRACT: &str = "digest record";

    pub fn parse(input: &str) -> Result<Self, ContractError> {
        format::parse_json(Self::CONTRACT, DIGEST_FORMAT, input)
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
    /// The pid of the daemon it is for, when it names one: an upgrade's
    /// drain names the daemon it hands over (OWL-436), so a daemon that
    /// finds at its start a drain naming another one removes it, and one
    /// naming itself is honoured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

impl Request {
    /// A request made now by `by`, for `why`.
    pub fn new(by: &str, at: Timestamp, why: impl Into<String>) -> Self {
        Self {
            by: by.to_owned(),
            at,
            why: why.into(),
            pid: None,
        }
    }

    /// This request, for the daemon of pid `pid`.
    pub fn for_pid(self, pid: u32) -> Self {
        Self {
            pid: Some(pid),
            ..self
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

/// How the line of [`InstallManifest::render`] that holds the service
/// begins: the service's launcher, finding its binary gone, removes the line
/// that begins so with `grep -v`, the rest staying a manifest that parses.
pub const MANIFEST_SERVICE_LINE: &str = "  \"service\": ";

/// What Owlshift wrote outside its data directory: the service installed at
/// login and the keychain entries `owlshift init` stored. `owlshift
/// uninstall` removes exactly what it lists, then the manifest.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift install manifest")]
pub struct InstallManifest {
    pub format: Format<MANIFEST_FORMAT>,
    /// The service installed at login, while it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<InstalledService>,
    /// The keychain entries `owlshift init` stored.
    #[serde(default)]
    pub keychain: Vec<KeychainEntry>,
}

/// The service manager a service is installed with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ServiceManager {
    /// A user LaunchAgent, on macOS.
    Launchd,
    /// A systemd user unit, on Linux and WSL2.
    Systemd,
}

/// The service installed at login: its files, each an absolute path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InstalledService {
    pub manager: ServiceManager,
    /// The unit: the LaunchAgent's property list, or the systemd unit file.
    pub unit: String,
    /// The `/bin/sh` launcher the unit runs, beside it.
    pub launcher: String,
    /// The link that enables a systemd unit at login, which `systemctl
    /// enable` makes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// The binary the launcher starts.
    pub binary: String,
}

impl InstalledService {
    /// Its files, in the order they are removed: the unit and its link
    /// first, so nothing starts the launcher once it is gone.
    pub fn files(&self) -> Vec<&str> {
        let mut files = vec![self.unit.as_str()];
        files.extend(self.link.as_deref());
        files.push(&self.launcher);
        files
    }
}

/// A keychain entry: the service it is filed under and its account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeychainEntry {
    pub service: String,
    pub account: String,
}

impl InstallManifest {
    const CONTRACT: &str = "install manifest";

    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let manifest: Self = format::parse_json(Self::CONTRACT, MANIFEST_FORMAT, input)?;
        if let Some(service) = &manifest.service
            && (service.unit.is_empty()
                || service.launcher.is_empty()
                || service.binary.is_empty()
                || service.link.as_deref() == Some(""))
        {
            return Err(ContractError::invalid(
                Self::CONTRACT,
                "the service names an empty path",
            ));
        }
        if manifest
            .keychain
            .iter()
            .any(|entry| entry.service.is_empty() || entry.account.is_empty())
        {
            return Err(ContractError::invalid(
                Self::CONTRACT,
                "a keychain entry without its service or its account",
            ));
        }
        Ok(manifest)
    }

    /// The manifest as its file holds it: the service on one line beginning
    /// with [`MANIFEST_SERVICE_LINE`] and ending with a comma, since the
    /// keychain entries always follow, so the file stays JSON without it.
    pub fn render(&self) -> String {
        let mut out = format!("{{\n  \"format\": {MANIFEST_FORMAT},\n");
        if let Some(service) = &self.service {
            out.push_str(MANIFEST_SERVICE_LINE);
            out.push_str(&compact(service));
            out.push_str(",\n");
        }
        if self.keychain.is_empty() {
            out.push_str("  \"keychain\": []\n");
        } else {
            out.push_str("  \"keychain\": [\n");
            let last = self.keychain.len() - 1;
            for (index, entry) in self.keychain.iter().enumerate() {
                out.push_str("    ");
                out.push_str(&compact(entry));
                out.push_str(if index == last { "\n" } else { ",\n" });
            }
            out.push_str("  ]\n");
        }
        out.push_str("}\n");
        out
    }

    /// Lists `entry`, unless it is listed already: `true` when it was not.
    pub fn record(&mut self, entry: KeychainEntry) -> bool {
        if self.keychain.contains(&entry) {
            return false;
        }
        self.keychain.push(entry);
        true
    }

    /// Whether it lists nothing.
    pub fn is_empty(&self) -> bool {
        self.service.is_none() && self.keychain.is_empty()
    }
}

/// One line of JSON for a value of the manifest.
fn compact<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("a manifest entry always serializes to JSON")
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

    /// OWL-437: the manifest reads back; without the line the launcher
    /// removes, it still parses, its keychain entries kept.
    #[test]
    fn a_manifest_reads_back_and_parses_without_its_service_line() {
        let mut manifest = InstallManifest {
            service: Some(InstalledService {
                manager: ServiceManager::Systemd,
                unit: "/home/ada/.config/systemd/user/owlshift.service".to_owned(),
                launcher: "/home/ada/.config/systemd/user/owlshift-launcher.sh".to_owned(),
                link: Some(
                    "/home/ada/.config/systemd/user/default.target.wants/owlshift.service"
                        .to_owned(),
                ),
                binary: "/home/ada/bin/own \"lift\"\n".to_owned(),
            }),
            ..InstallManifest::default()
        };
        let entry = |account: &str| KeychainEntry {
            service: "owlshift".to_owned(),
            account: account.to_owned(),
        };
        for keychain in [
            vec![],
            vec![entry("claude-agent"), entry("github.com/a/b/linear")],
        ] {
            manifest.keychain = keychain;
            let text = manifest.render();
            assert_eq!(InstallManifest::parse(&text).unwrap(), manifest, "{text}");
            let service_lines: Vec<&str> = text
                .lines()
                .filter(|line| line.starts_with(MANIFEST_SERVICE_LINE))
                .collect();
            assert_eq!(service_lines.len(), 1, "{text}");
            assert!(service_lines[0].ends_with(','), "{text}");
            let without: String = text
                .lines()
                .filter(|line| !line.starts_with(MANIFEST_SERVICE_LINE))
                .map(|line| format!("{line}\n"))
                .collect();
            let left = InstallManifest::parse(&without).unwrap();
            assert_eq!(left.service, None);
            assert_eq!(left.keychain, manifest.keychain);
        }
        assert!(!manifest.record(entry("claude-agent")), "listed once");
        assert!(manifest.record(entry("github.com/a/b/github-app")));
        assert!(!manifest.is_empty());
        assert!(InstallManifest::default().is_empty());

        let text = manifest.render();
        let newer = text.replace("\"format\": 1", "\"format\": 2");
        assert!(matches!(
            InstallManifest::parse(&newer),
            Err(ContractError::NewerFormat { found: 2, .. })
        ));
        let empty = text.replace("\"account\":\"claude-agent\"", "\"account\":\"\"");
        assert!(InstallManifest::parse(&empty).is_err());
    }

    #[test]
    fn a_digest_record_reads_back_and_a_newer_one_is_refused() {
        let record = DigestRecord {
            format: Format,
            at: at("2026-10-25T08:00:00Z"),
            line: Some("Owlshift: 1 delivered".to_owned()),
        };
        let text = record.render();
        assert_eq!(DigestRecord::parse(&text).unwrap(), record);
        let newer = text.replace("\"format\": 1", "\"format\": 2");
        assert!(matches!(
            DigestRecord::parse(&newer),
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
