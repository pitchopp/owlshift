//! Live check (d) of the build plan's P6 (OWL-410): what the GitHub App's
//! user token (D13: Pull requests write, Contents read) reads of a pull
//! request's files, a rename's old and new paths included. Read-only; the
//! maintainer runs it by hand, since only their sign-in holds the token:
//!
//! ```sh
//! OWLSHIFT_LIVE_REPOS=pitchopp/owlshift,pitchopp/locary \
//! OWLSHIFT_LIVE_PULLS=pitchopp/owlshift#153 \
//!   cargo test -p owlshift-runner --test pull_files_live -- --ignored --nocapture
//! ```
//!
//! For each repository of `OWLSHIFT_LIVE_REPOS` it takes the project's
//! GitHub App token from the system keychain, as `owlshift watch` does
//! (`secrets::forge`, then the Writer's refresh under the project's account
//! lock, which writes a refreshed pair back), lists the open pull requests
//! through GraphQL and reads each one's files twice: GraphQL's
//! `files { path changeType }` and REST's `pulls/<n>/files`
//! (`filename`, `status`, `previous_filename`). `OWLSHIFT_LIVE_PULLS` adds
//! pull requests by number, open or not, such as a merged one with a
//! rename (#153 on Owlshift renames three files).
//!
//! It prints outcomes only: HTTP statuses, counts, and the paths of renamed
//! files; never a token. A repository with no App token stored is reported
//! and skipped.

use std::io::Read;
use std::sync::Arc;

use owlshift_adapters::forge::Repo;
use owlshift_adapters::forge::github::{HttpTransport, TokenSource};
use owlshift_adapters::forge::github_app::{DeviceFlow, HttpLogin};
use owlshift_adapters::graphql::Transport;
use owlshift_platform::keychain::Keychain;
use owlshift_runner::forge::GitHubApp;
use owlshift_runner::github_token::AppToken;
use owlshift_runner::project::ProjectDirs;
use owlshift_runner::secrets::{self, ForgeKind};
use serde_json::{Value, json};

const OPEN: &str = "query Open($owner: String!, $name: String!) { repository(owner: $owner, \
     name: $name) { pullRequests(states: OPEN, first: 50) { totalCount nodes { number isDraft \
     changedFiles baseRefName files(first: 100) { totalCount pageInfo { hasNextPage } nodes { \
     path changeType } } } } } }";

const ONE: &str = "query One($owner: String!, $name: String!, $number: Int!) { \
     repository(owner: $owner, name: $name) { pullRequest(number: $number) { number state \
     isDraft changedFiles files(first: 100) { totalCount pageInfo { hasNextPage } nodes { path \
     changeType } } } } }";

fn list(name: &str) -> Vec<String> {
    std::env::var(name)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The transport of `repo`'s App token, or why there is none.
fn transport(keychain: &Arc<Keychain>, repo: &Repo) -> Result<HttpTransport, String> {
    let mut look = |account: &str| keychain.read(account).map_err(|e| e.to_string());
    let found = secrets::forge(repo, &mut look)?;
    let (Some(entry), ForgeKind::App) = (&found.token, found.account.kind) else {
        return Err(format!(
            "no GitHub App token: the account found is {:?} ({:?})",
            found.account.token,
            found.kind()
        ));
    };
    let data = owlshift_platform::paths::data_dir().ok_or("no data directory")?;
    let lock = ProjectDirs::new(&data, repo).account_lock_file();
    if let Some(dir) = lock.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let app = GitHubApp::from_personal(None);
    let flow = DeviceFlow::new(HttpLogin::new(), &app.client_id);
    let mut source = AppToken::new(
        keychain.clone(),
        found.account.token.clone(),
        lock,
        flow,
        entry,
    )
    .map_err(|e| e.message)?;
    let token = source.token().map_err(|e| e.message)?;
    Ok(HttpTransport::new(token))
}

fn graphql(transport: &HttpTransport, query: &str, variables: Value) -> Option<Value> {
    match transport.send(&json!({ "query": query, "variables": variables })) {
        Ok(response) => {
            let body: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
            println!(
                "  graphql: HTTP {}, errors: {}",
                response.status,
                body.get("errors")
                    .map_or("none".to_owned(), |e| e.to_string())
            );
            body.get("data").cloned()
        }
        Err(error) => {
            println!("  graphql: no answer: {error}");
            None
        }
    }
}

fn graphql_files(pull: &Value) {
    let files = &pull["files"];
    let nodes = files["nodes"].as_array().cloned().unwrap_or_default();
    println!(
        "    graphql files: totalCount {}, read {}, next page {}",
        files["totalCount"],
        nodes.len(),
        files["pageInfo"]["hasNextPage"]
    );
    for node in nodes
        .iter()
        .filter(|n| n["changeType"] != json!("MODIFIED"))
    {
        println!(
            "      {} {}",
            node["changeType"].as_str().unwrap_or("?"),
            node["path"].as_str().unwrap_or("?")
        );
    }
}

fn rest_files(transport: &HttpTransport, repo: &Repo, number: u64) {
    let mut read = 0;
    for page in 1..=30 {
        let path = format!(
            "repos/{}/{}/pulls/{number}/files?per_page=100&page={page}",
            repo.owner(),
            repo.name()
        );
        let mut answer = match transport.get(&path) {
            Ok(answer) => answer,
            Err(error) => {
                println!("    rest page {page}: no answer: {error}");
                return;
            }
        };
        let mut body = String::new();
        let bytes = answer.body.read_to_string(&mut body).unwrap_or(0);
        if answer.status != 200 {
            let message: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            println!(
                "    rest page {page}: HTTP {}, message {}",
                answer.status, message["message"]
            );
            return;
        }
        let files: Vec<Value> = serde_json::from_str(&body).unwrap_or_default();
        println!(
            "    rest page {page}: HTTP 200, {} files, {bytes} bytes",
            files.len()
        );
        read += files.len();
        for file in files.iter().filter(|f| f["status"] != json!("modified")) {
            println!(
                "      {} {} (previous_filename {})",
                file["status"].as_str().unwrap_or("?"),
                file["filename"].as_str().unwrap_or("?"),
                file["previous_filename"]
            );
        }
        if files.len() < 100 {
            break;
        }
    }
    println!("    rest files: read {read}");
}

#[test]
#[ignore = "live: reads GitHub with the App's token; set OWLSHIFT_LIVE_REPOS"]
fn the_apps_token_reads_a_pull_requests_files_with_renames() {
    let repos = list("OWLSHIFT_LIVE_REPOS");
    assert!(!repos.is_empty(), "set OWLSHIFT_LIVE_REPOS=owner/name,...");
    let pulls = list("OWLSHIFT_LIVE_PULLS");
    let keychain = Arc::new(Keychain::system().expect("the system keychain"));
    println!("check (d), {}", jiff::Timestamp::now());
    for repo in &repos {
        let repo = Repo::parse(repo).unwrap();
        println!("{repo}:");
        let transport = match transport(&keychain, &repo) {
            Ok(transport) => transport,
            Err(reason) => {
                println!("  skipped: {reason}");
                continue;
            }
        };
        let vars = json!({ "owner": repo.owner(), "name": repo.name() });
        if let Some(data) = graphql(&transport, OPEN, vars.clone()) {
            let prs = &data["repository"]["pullRequests"];
            let nodes = prs["nodes"].as_array().cloned().unwrap_or_default();
            println!("  open pull requests: totalCount {}", prs["totalCount"]);
            for pull in &nodes {
                let number = pull["number"].as_u64().unwrap_or(0);
                println!(
                    "  #{number}: draft {}, changedFiles {}, base {}",
                    pull["isDraft"], pull["changedFiles"], pull["baseRefName"]
                );
                graphql_files(pull);
                rest_files(&transport, &repo, number);
            }
        }
        for pull in &pulls {
            let Some((name, number)) = pull.split_once('#') else {
                continue;
            };
            if name != repo.to_string() {
                continue;
            }
            let number: u64 = number.parse().expect("a pull request number");
            let mut vars = vars.clone();
            vars["number"] = json!(number);
            println!("  #{number}, asked:");
            if let Some(data) = graphql(&transport, ONE, vars) {
                let pull = &data["repository"]["pullRequest"];
                println!(
                    "  #{number}: state {}, changedFiles {}",
                    pull["state"], pull["changedFiles"]
                );
                graphql_files(pull);
            }
            rest_files(&transport, &repo, number);
        }
    }
}
