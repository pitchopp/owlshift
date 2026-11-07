//! The Config screen's answer (P8 part 7, OWL-472, runtime and operations,
//! "Observability & the local web UI"): the effective configuration with the
//! file each value comes from, as `owlshift config show` prints it, read-only.
//!
//! `GET /api/config[?rev=<hash>]` answers:
//!
//! - `machine`: the personal file's own report, the project file being not
//!   applicable to the machine (each served project has its own below);
//! - `projects`: for each served project, `record`, what `served.json` says
//!   (`kept`: the branch, commit, file name and since; `refused`: why the
//!   latest was refused, with its commit and since; or the read error), and
//!   `config`, the report of the project file kept, labelled in the project's
//!   checkout (`<checkout>/owlshift.toml`, [`served::in_checkout`]), so its
//!   lines are the ones `owlshift config show` prints run in that checkout
//!   holding the same file; or `refusal`, the reason it is not shown;
//! - `rev`, the SHA-256 of the body: a request passing it is answered
//!   `{"unchanged": true}`.
//!
//! A report is `lines` (those `config show` prints, in order) and the same
//! facts as data ([`ConfigReport`]).

use std::path::Path;

use owlshift_adapters::forge::Repo;
use owlshift_contracts::config::PersonalConfig;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::orchestrator::path_of;
use crate::config::{ConfigReport, Effective, FileState, personal_state};
use crate::project::ProjectDirs;
use crate::served::{self, Registry};

/// A report as the page reads it.
pub fn report_json(report: &ConfigReport) -> Value {
    json!({
        "lines": report.lines(),
        "project_file": report.project_file,
        "personal_file": report.personal_file,
        "entries": report
            .entries
            .iter()
            .map(|entry| json!({ "key": entry.key, "value": entry.value, "file": entry.file }))
            .collect::<Vec<_>>(),
        "always_human": report.always_human,
        "resolver": report.resolver,
        "budget": report.budget,
    })
}

fn record_json(dirs: &ProjectDirs) -> Value {
    match served::read_record(dirs) {
        Ok(Some(record)) => json!({
            "kept": record.kept.as_ref().map(|kept| json!({
                "name": served::kept_name(kept),
                "branch": kept.branch,
                "commit": kept.commit,
                "since": kept.since.to_string(),
            })),
            "refused": record.refused.as_ref().map(|refused| json!({
                "reason": refused.reason,
                "commit": refused.commit,
                "since": refused.since.to_string(),
            })),
        }),
        Ok(None) => json!({ "kept": null, "refused": null }),
        Err(why) => json!({ "error": why }),
    }
}

/// The Config answer. A request whose `rev` is the answer's gets
/// `{"unchanged": true}`.
pub fn answer(
    data: &Path,
    personal: &FileState<PersonalConfig>,
    rev: Option<&str>,
) -> Result<Value, String> {
    let personal_path = path_of(personal);
    let machine = Effective {
        project: FileState::NotApplicable(
            "the machine has none: each served project's is below".to_owned(),
        ),
        personal: personal_state(personal_path.clone()),
    };
    let served = Registry::in_data_dir(data).read()?;
    let projects: Vec<Value> = served
        .projects
        .iter()
        .map(|entry| {
            let Ok(repo) = Repo::parse(&entry.repository) else {
                return json!({
                    "repository": entry.repository,
                    "record": null,
                    "config": null,
                    "refusal": "the repository is not owner/name",
                });
            };
            let dirs = ProjectDirs::new(data, &repo);
            let (config, refusal) = match served::in_checkout(&repo, &dirs, personal_path.clone()) {
                Ok(effective) => (report_json(&effective.report()), Value::Null),
                Err(why) => (Value::Null, Value::String(why)),
            };
            json!({
                "repository": entry.repository,
                "checkout": entry.checkout,
                "record": record_json(&dirs),
                "config": config,
                "refusal": refusal,
            })
        })
        .collect();
    let body = json!({
        "machine": report_json(&machine.report()),
        "projects": projects,
    });
    let mut hash = Sha256::new();
    hash.update(super::server::json_text(&body).as_bytes());
    let hash = super::auth::hex(&hash.finalize());
    if rev == Some(hash.as_str()) {
        return Ok(json!({ "unchanged": true, "rev": hash }));
    }
    let mut body = body;
    body["rev"] = json!(hash);
    Ok(body)
}
