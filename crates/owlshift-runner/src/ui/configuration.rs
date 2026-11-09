//! The Config screen's answer (P8 part 7, OWL-472, runtime and operations,
//! "Observability & the local web UI"): the effective configuration with the
//! file each value comes from, as `owlshift config show` prints it. The
//! project files are read-only; the personal file is edited through `POST
//! /api/config/personal` (OWL-473).
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
//! - `personal_edit` (OWL-473): what the editor of the personal file needs,
//!   each key of [`crate::personal_edit::KEYS`] with its kind, when an edit
//!   of it applies and its value now, and the file's own `rev`, which an
//!   edit names (`POST /api/config/personal`);
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
use crate::personal_edit::{self, KEYS, KeyInfo};
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

/// One key of the table as the editor shows it, at `path`, its value now
/// read from `table` (null when the file does not set it).
fn key_json(info: &KeyInfo, path: Vec<String>, table: Option<&toml::Table>) -> Value {
    let mut at: Option<&toml::Value> = None;
    for (depth, segment) in path.iter().enumerate() {
        at = if depth == 0 {
            table.and_then(|table| table.get(segment))
        } else {
            at.and_then(toml::Value::as_table)
                .and_then(|table| table.get(segment))
        };
    }
    json!({
        "key": personal_edit::dotted(&path),
        "path": path,
        "kind": info.kind.key(),
        "applies": info.applies.key(),
        "applies_words": info.applies.words(),
        "about": info.about,
        "value": at.and_then(|value| serde_json::to_value(value).ok()),
    })
}

/// What the editor of the personal file needs (OWL-473): the file's `path`,
/// its `rev` ([`personal_edit::rev_of`], null when absent), whether it is
/// `editable` and `why_not`, each key of the table with its kind, when it
/// applies, who reads it and its current `value`; the repositories the file
/// names, each with its keys; and `repository_keys`, the keys of a
/// repository entry, for the form that adds one.
fn personal_edit_json(personal: &FileState<PersonalConfig>) -> Value {
    let why_not = match personal {
        FileState::NotApplicable(why) | FileState::Unavailable(why) => Some(why.clone()),
        _ => None,
    };
    let path = path_of(personal);
    let bytes = path
        .as_deref()
        .and_then(|path| personal_edit::read(path).ok())
        .flatten();
    let table: Option<toml::Table> = bytes
        .as_deref()
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .and_then(|text| toml::from_str(text).ok());
    let plain = |info: &&KeyInfo| !info.path.contains(&personal_edit::REPOSITORY);
    let keys: Vec<Value> = KEYS
        .iter()
        .filter(plain)
        .map(|info| {
            let path = info
                .path
                .iter()
                .map(|segment| (*segment).to_owned())
                .collect();
            key_json(info, path, table.as_ref())
        })
        .collect();
    let per_repository: Vec<&KeyInfo> = KEYS.iter().filter(|info| !plain(info)).collect();
    let repositories: Vec<Value> = table
        .as_ref()
        .and_then(|table| table.get("repositories"))
        .and_then(toml::Value::as_table)
        .map(|listed| {
            listed
                .keys()
                .map(|repository| {
                    let keys: Vec<Value> = per_repository
                        .iter()
                        .map(|info| {
                            let path = info
                                .path
                                .iter()
                                .map(|segment| match *segment {
                                    personal_edit::REPOSITORY => repository.clone(),
                                    segment => segment.to_owned(),
                                })
                                .collect();
                            key_json(info, path, table.as_ref())
                        })
                        .collect();
                    json!({ "repository": repository, "keys": keys })
                })
                .collect()
        })
        .unwrap_or_default();
    let repository_keys: Vec<Value> = per_repository
        .iter()
        .map(|info| {
            json!({
                "name": info.path.last(),
                "kind": info.kind.key(),
                "applies": info.applies.key(),
                "applies_words": info.applies.words(),
                "about": info.about,
            })
        })
        .collect();
    json!({
        "path": path.as_ref().map(|path| path.display().to_string()),
        "rev": personal_edit::rev_of(bytes.as_deref()),
        "editable": path.is_some(),
        "why_not": why_not,
        "keys": keys,
        "repositories": repositories,
        "repository_keys": repository_keys,
    })
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
        "personal_edit": personal_edit_json(personal),
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
