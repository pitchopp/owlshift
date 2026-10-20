//! The repository's open pull requests and the files each changes, for the
//! in-flight set (OWL-410; architecture section 5, "The in-flight set", 2).
//!
//! The list is one GraphQL query, paged. The files are REST's
//! `pulls/<n>/files`, which gives a rename's old path as `previous_filename`:
//! GraphQL's `PullRequestChangedFile` gives the new path and the change's
//! type only. Whether the GitHub App's user token reads it is check (d) of
//! the build plan's P6.

use std::collections::BTreeSet;
use std::io::Read;

use serde::Deserialize;
use serde_json::json;

use super::{GitHubForge, MAX_PAGE, MAX_REST_BYTES, invalid};
use crate::forge::{CommitId, Error, ErrorKind, OpenPullRequest, PullFiles};

/// The most open pull requests read, in pages of [`MAX_PAGE`]: a repository
/// with more is refused, the caller then holding what it would dispatch.
pub const MAX_OPEN_PULL_REQUESTS: u64 = 1_000;

/// The most files GitHub lists for a pull request; a pull request that
/// changes more holds the whole repository.
pub const MAX_PULL_FILES: u64 = 3_000;

/// The most bytes one page of a pull request's files may take, its patches
/// included; a page past it holds the whole repository.
pub const MAX_FILES_PAGE_BYTES: u64 = 16 * 1024 * 1024;

impl GitHubForge {
    /// Every open pull request of the repository, drafts included, read to
    /// the last page and checked against GitHub's count. More than
    /// [`MAX_OPEN_PULL_REQUESTS`], pages that do not advance or a count that
    /// differs are refused: a list that cannot be read whole is never taken
    /// as complete.
    pub fn open_pull_requests(&self) -> Result<Vec<OpenPullRequest>, Error> {
        let mut pulls = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let variables = json!({
                "owner": self.repo.owner(), "name": self.repo.name(), "after": after,
            });
            let data: PullsData = self.call(PULLS_QUERY, variables)?;
            let found = data.repository.pull_requests;
            if found.total_count > MAX_OPEN_PULL_REQUESTS {
                return Err(invalid(format!(
                    "{} open pull requests, more than the {MAX_OPEN_PULL_REQUESTS} read",
                    found.total_count
                )));
            }
            for node in found.nodes {
                pulls.push(node.into_open()?);
            }
            if !found.page_info.has_next_page {
                if pulls.len() as u64 != found.total_count {
                    return Err(invalid(format!(
                        "read {} open pull requests, GitHub counts {}",
                        pulls.len(),
                        found.total_count
                    )));
                }
                return Ok(pulls);
            }
            match found.page_info.end_cursor {
                Some(cursor) if after.as_ref() != Some(&cursor) => after = Some(cursor),
                _ => return Err(invalid("the open pull requests' pages do not advance")),
            }
        }
    }

    /// The files `pull` changes, both paths of a rename, each once, sorted.
    /// [`PullFiles::Whole`] when it changes more than [`MAX_PULL_FILES`] or
    /// a page passes [`MAX_FILES_PAGE_BYTES`]. A count that differs from
    /// `pull.changed_files`, or a head or base that moved while more than one
    /// page was read, is [`ErrorKind::HeadMoved`]: read it again later.
    pub fn pull_request_files(&self, pull: &OpenPullRequest) -> Result<PullFiles, Error> {
        if pull.changed_files > MAX_PULL_FILES {
            return Ok(PullFiles::Whole);
        }
        let number = pull.number;
        let base = format!(
            "repos/{}/{}/pulls/{number}",
            self.repo.owner(),
            self.repo.name()
        );
        let mut paths = BTreeSet::new();
        let mut entries = 0u64;
        let mut pages = 0u64;
        loop {
            pages += 1;
            let mut answer =
                self.rest(&format!("{base}/files?per_page={MAX_PAGE}&page={pages}"))?;
            let mut body = Vec::new();
            (&mut answer.body)
                .take(MAX_FILES_PAGE_BYTES + 1)
                .read_to_end(&mut body)
                .map_err(|_| invalid(format!("reading the files of #{number} failed")))?;
            if body.len() as u64 > MAX_FILES_PAGE_BYTES {
                return Ok(PullFiles::Whole);
            }
            let files: Vec<RestFile> = serde_json::from_slice(&body)
                .map_err(|_| invalid(format!("unexpected answer for the files of #{number}")))?;
            let full = files.len() as u64 == u64::from(MAX_PAGE);
            entries += files.len() as u64;
            for file in files {
                paths.extend(file.previous_filename);
                paths.insert(file.filename);
            }
            if !full || entries >= MAX_PULL_FILES {
                break;
            }
        }
        if entries != pull.changed_files {
            return Err(moved(format!(
                "read {entries} files of #{number}, GitHub counted {}",
                pull.changed_files
            )));
        }
        if pages > 1 {
            self.same_pull(&base, pull)?;
        }
        Ok(PullFiles::Listed(paths.into_iter().collect()))
    }

    /// Refuses when `pull`'s head or base moved since it was listed: pages
    /// read across a push may mix two heads.
    fn same_pull(&self, path: &str, pull: &OpenPullRequest) -> Result<(), Error> {
        let mut answer = self.rest(path)?;
        let mut body = String::new();
        (&mut answer.body)
            .take(MAX_REST_BYTES)
            .read_to_string(&mut body)
            .map_err(|_| invalid(format!("reading #{} failed", pull.number)))?;
        let read: RestPull = serde_json::from_str(&body).map_err(|_| {
            invalid(format!(
                "unexpected answer for #{} from GitHub",
                pull.number
            ))
        })?;
        if read.head.sha != pull.head.as_str()
            || read.base.sha != pull.base.as_str()
            || read.base.branch != pull.base_branch
        {
            return Err(moved(format!(
                "#{} moved while its files were read",
                pull.number
            )));
        }
        Ok(())
    }
}

fn moved(message: String) -> Error {
    Error::new(ErrorKind::HeadMoved, message)
}

const PULLS_QUERY: &str = "query RepositoryPulls($owner: String!, $name: String!, \
    $after: String) { repository(owner: $owner, name: $name) { \
    pullRequests(states: [OPEN], first: 100, after: $after) { totalCount \
    pageInfo { hasNextPage endCursor } nodes { number isDraft changedFiles headRefName \
    headRefOid baseRefName baseRefOid } } } }";

#[derive(Deserialize)]
struct PullsData {
    repository: PullsRepository,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullsRepository {
    pull_requests: PullsPage,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullsPage {
    total_count: u64,
    page_info: Cursor,
    nodes: Vec<PullNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Cursor {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullNode {
    number: u64,
    is_draft: bool,
    changed_files: u64,
    head_ref_name: String,
    head_ref_oid: String,
    base_ref_name: String,
    base_ref_oid: String,
}

impl PullNode {
    fn into_open(self) -> Result<OpenPullRequest, Error> {
        Ok(OpenPullRequest {
            number: self.number,
            head_branch: self.head_ref_name,
            head: CommitId::new(self.head_ref_oid)?,
            base_branch: self.base_ref_name,
            base: CommitId::new(self.base_ref_oid)?,
            draft: self.is_draft,
            changed_files: self.changed_files,
        })
    }
}

#[derive(Deserialize)]
struct RestFile {
    filename: String,
    #[serde(default)]
    previous_filename: Option<String>,
}

#[derive(Deserialize)]
struct RestPull {
    head: RestRef,
    base: RestRef,
}

#[derive(Deserialize)]
struct RestRef {
    sha: String,
    #[serde(rename = "ref")]
    branch: String,
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::Value;

    use super::*;
    use crate::forge::Repo;
    use crate::graphql::{Response, RestAnswer, Transport};

    /// GraphQL bodies in order; REST reads by path, each once.
    struct Script {
        graphql: Mutex<Vec<String>>,
        rest: Mutex<Vec<(String, u16, String)>>,
    }

    impl Transport for Script {
        fn send(&self, _: &Value) -> Result<Response, String> {
            Ok(Response {
                status: 200,
                body: self.graphql.lock().unwrap().remove(0),
                ..Default::default()
            })
        }

        fn get(&self, path: &str) -> Result<RestAnswer, String> {
            let mut answers = self.rest.lock().unwrap();
            let at = answers
                .iter()
                .position(|(p, _, _)| p == path)
                .unwrap_or_else(|| panic!("unexpected read of {path}"));
            let (_, status, body) = answers.remove(at);
            Ok(RestAnswer::new(status, body))
        }
    }

    fn forge(graphql: Vec<String>, rest: Vec<(String, u16, String)>) -> GitHubForge {
        GitHubForge::with_transport(
            Script {
                graphql: Mutex::new(graphql),
                rest: Mutex::new(rest),
            },
            Repo::parse("demo/project").unwrap(),
        )
    }

    fn sha(c: char) -> String {
        c.to_string().repeat(40)
    }

    fn node(number: u64, files: u64) -> Value {
        json!({ "number": number, "isDraft": number.is_multiple_of(2), "changedFiles": files,
                "headRefName": format!("topic-{number}"), "headRefOid": sha('a'),
                "baseRefName": "main", "baseRefOid": sha('b') })
    }

    fn pulls_page(total: u64, nodes: Vec<Value>, next: Option<&str>) -> String {
        json!({ "data": { "repository": { "pullRequests": {
            "totalCount": total,
            "pageInfo": { "hasNextPage": next.is_some(), "endCursor": next },
            "nodes": nodes,
        } } } })
        .to_string()
    }

    fn pull(files: u64) -> OpenPullRequest {
        OpenPullRequest {
            number: 7,
            head_branch: "topic-7".to_owned(),
            head: CommitId::new(sha('a')).unwrap(),
            base_branch: "main".to_owned(),
            base: CommitId::new(sha('b')).unwrap(),
            draft: false,
            changed_files: files,
        }
    }

    fn files_path(page: u32) -> String {
        format!("repos/demo/project/pulls/7/files?per_page=100&page={page}")
    }

    /// Every page read, drafts included; a count that differs, pages that
    /// do not advance and more than the cap are refused.
    #[test]
    fn the_open_pull_requests_are_read_whole_or_refused() {
        let read = forge(
            vec![
                pulls_page(3, vec![node(1, 1), node(2, 5)], Some("c1")),
                pulls_page(3, vec![node(3, 2)], None),
            ],
            vec![],
        )
        .open_pull_requests()
        .unwrap();
        assert_eq!(read.len(), 3);
        assert_eq!(
            (read[1].number, read[1].draft, read[1].changed_files),
            (2, true, 5)
        );
        assert_eq!(read[0].head_branch, "topic-1");
        assert_eq!(read[0].base.as_str(), sha('b'));

        for pages in [
            vec![pulls_page(2, vec![node(1, 1)], None)],
            vec![
                pulls_page(2, vec![node(1, 1)], Some("c1")),
                pulls_page(2, vec![node(2, 1)], Some("c1")),
            ],
            vec![pulls_page(1_001, vec![], Some("c1"))],
        ] {
            assert!(forge(pages, vec![]).open_pull_requests().is_err());
        }
    }

    /// Both paths of a rename, each path once; past 3,000 files the whole
    /// repository with no read; a count that differs is a moved head.
    #[test]
    fn a_pull_requests_files_keep_both_paths_of_a_rename() {
        let page = json!([
            { "filename": "app/new.rs", "status": "renamed", "previous_filename": "app/old.rs" },
            { "filename": "docs/a.md", "status": "modified" },
            { "filename": "app/old.rs", "status": "added" },
        ])
        .to_string();
        let read = forge(vec![], vec![(files_path(1), 200, page.clone())])
            .pull_request_files(&pull(3))
            .unwrap();
        assert_eq!(
            read,
            PullFiles::Listed(vec![
                "app/new.rs".to_owned(),
                "app/old.rs".to_owned(),
                "docs/a.md".to_owned()
            ])
        );
        let whole = forge(vec![], vec![]).pull_request_files(&pull(3_001));
        assert_eq!(whole.unwrap(), PullFiles::Whole);
        let short = forge(vec![], vec![(files_path(1), 200, page)])
            .pull_request_files(&pull(4))
            .unwrap_err();
        assert_eq!(short.kind, ErrorKind::HeadMoved);
        let refused = forge(
            vec![],
            vec![(
                files_path(1),
                403,
                r#"{"message":"Resource not accessible"}"#.to_owned(),
            )],
        )
        .pull_request_files(&pull(1))
        .unwrap_err();
        assert_eq!(refused.kind, ErrorKind::Unauthorized);
    }

    /// Past one page, the pull request is read again after its pages: a
    /// head that moved meanwhile is refused, one that did not is kept.
    #[test]
    fn a_pull_request_read_over_several_pages_must_not_move() {
        let full: Vec<Value> = (0..100)
            .map(|n| json!({ "filename": format!("f/{n}.rs") }))
            .collect();
        let last = json!([{ "filename": "g.rs" }]).to_string();
        let at = |head: char| {
            json!({ "head": { "sha": sha(head), "ref": "topic-7" },
                    "base": { "sha": sha('b'), "ref": "main" } })
            .to_string()
        };
        let answers = |head| {
            vec![
                (files_path(1), 200, Value::Array(full.clone()).to_string()),
                (files_path(2), 200, last.clone()),
                ("repos/demo/project/pulls/7".to_owned(), 200, at(head)),
            ]
        };
        let PullFiles::Listed(paths) = forge(vec![], answers('a'))
            .pull_request_files(&pull(101))
            .unwrap()
        else {
            panic!("listed");
        };
        assert_eq!(paths.len(), 101);
        let moved = forge(vec![], answers('c'))
            .pull_request_files(&pull(101))
            .unwrap_err();
        assert_eq!(moved.kind, ErrorKind::HeadMoved);
    }

    /// A page past the byte cap holds the whole repository.
    #[test]
    fn a_page_too_large_holds_the_whole_repository() {
        let huge = format!(
            r#"[{{"filename":"a","patch":"{}"}}]"#,
            "x".repeat(MAX_FILES_PAGE_BYTES as usize)
        );
        let read = forge(vec![], vec![(files_path(1), 200, huge)])
            .pull_request_files(&pull(1))
            .unwrap();
        assert_eq!(read, PullFiles::Whole);
    }
}
