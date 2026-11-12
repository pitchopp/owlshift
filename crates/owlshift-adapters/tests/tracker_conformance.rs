//! The tracker conformance suite: what every tracker adapter must do, run on
//! each of them. It asserts relative times only (order, last edit not before
//! creation), never absolute ones, so a tracker that stamps comments with its
//! own clock passes the same checks as one replayed from a recording.

mod support;

use std::fs;

use owlshift_adapters::tracker::linear::{ApiKey, LinearTracker};
use owlshift_adapters::tracker::markdown::MarkdownTracker;
use owlshift_adapters::tracker::{Author, Capability, ErrorKind, Person, Tracker};
use owlshift_contracts::Priority;
use owlshift_contracts::ids::TicketId;

use support::{Recorder, Replay};

/// What the suite knows about the tracker it checks.
struct Case {
    /// A ticket that exists, and what reading it must give.
    existing: TicketId,
    title: &'static str,
    description_start: &'static str,
    priority: Priority,
    /// Whether it has an assignee. Who it is differs between a live run and
    /// its pseudonymized recording, so the suite checks presence only.
    assigned: bool,
    /// Whether the adapter reports its assignee's account as its author, by
    /// the account's identifier: what makes the description the decider's.
    author_is_assignee: bool,
    labels: &'static [&'static str],
    /// A ticket that does not exist.
    missing: TicketId,
    /// The body of the comment the suite posts.
    body: &'static str,
}

fn id(id: &str) -> TicketId {
    TicketId::new(id).unwrap()
}

/// Runs the whole suite. The calls are always made in the same order, which
/// is the order of the recorded Linear exchanges.
fn check(tracker: &dyn Tracker, case: &Case) {
    // The suite exercises reading tickets and comments: both are declared.
    let declared = tracker.capabilities();
    for needed in [Capability::ReadTicket, Capability::Comments] {
        assert!(declared.contains(&needed), "{needed:?} is not declared");
    }

    let ticket = tracker.ticket(&case.existing).unwrap();
    assert_eq!(ticket.id, case.existing);
    assert_eq!(ticket.title, case.title);
    assert!(
        ticket.description.starts_with(case.description_start),
        "{:?}",
        ticket.description
    );
    assert_eq!(ticket.priority, case.priority);
    assert_eq!(
        ticket.assignee.is_some(),
        case.assigned,
        "{:?}",
        ticket.assignee
    );
    assert_eq!(ticket.labels, case.labels);
    let author_is_assignee = matches!(
        (&ticket.author, &ticket.assignee),
        (Author::Account(author), Some(assignee)) if author.id == assignee.id
    );
    assert_eq!(
        author_is_assignee, case.author_is_assignee,
        "{:?} for {:?}",
        ticket.author, ticket.assignee
    );

    let missing = tracker.ticket(&case.missing).unwrap_err();
    assert_eq!(missing.kind, ErrorKind::NotFound, "{missing}");

    let before = tracker.comments(&case.existing).unwrap();
    for pair in before.windows(2) {
        assert!(pair[0].created_at <= pair[1].created_at, "not oldest first");
    }
    for comment in &before {
        assert!(comment.last_edit() >= comment.created_at);
    }

    let missing = tracker.comments(&case.missing).unwrap_err();
    assert_eq!(missing.kind, ErrorKind::NotFound, "{missing}");

    let missing = tracker.post_comment(&case.missing, case.body).unwrap_err();
    assert_eq!(missing.kind, ErrorKind::NotFound, "{missing}");

    let posted = tracker.post_comment(&case.existing, case.body).unwrap();
    assert_eq!(posted.body, case.body);
    assert!(
        matches!(posted.author, Author::Account(_)),
        "{:?}",
        posted.author
    );
    assert_eq!(posted.edited_at, None);

    let after = tracker.comments(&case.existing).unwrap();
    assert_eq!(after.len(), before.len() + 1);
    assert_eq!(after[..before.len()], before[..]);
    assert_eq!(
        after.last(),
        Some(&posted),
        "the posted comment reads back, last"
    );
}

/// The visible stage, checked apart from [`check`] so that each adapter's
/// recording of it stays separate from the read-and-comment one: the stage
/// moves to `there`, then back to `back`, and a ticket that does not exist is
/// not found. Reading the stage back is each adapter's own test.
fn check_stage(
    tracker: &dyn Tracker,
    existing: &TicketId,
    missing: &TicketId,
    there: &str,
    back: &str,
) {
    assert!(
        tracker.capabilities().contains(&Capability::VisibleStage),
        "VisibleStage is not declared"
    );
    tracker.set_stage(existing, there).unwrap();
    tracker.set_stage(existing, back).unwrap();
    let missing = tracker.set_stage(missing, there).unwrap_err();
    assert_eq!(missing.kind, ErrorKind::NotFound, "{missing}");
}

const BODY: &str = "[owlshift] OWL-13 conformance check: a comment posted by the Linear \
                    adapter's fixture recorder · safe to delete.\n\nSecond line.";

#[test]
fn the_markdown_tracker_conforms() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("tickets/DEMO-1");
    fs::create_dir_all(dir.join("comments")).unwrap();
    fs::write(
        dir.join("ticket.md"),
        "+++\ntitle = \"Add a greeting\"\nauthor = \"maintainer\"\nstage = \"Todo\"\n\
         priority = \"medium\"\nassignee = \"maintainer\"\nlabels = [\"Feature\", \"Docs\"]\n+++\n\n\
         Say hello in the README.\n",
    )
    .unwrap();
    fs::write(
        dir.join("comments/20260927T090000Z-reporter.md"),
        "First.\n",
    )
    .unwrap();
    fs::write(
        dir.join("comments/20260927T100000Z-maintainer.md"),
        "Second.\n",
    )
    .unwrap();

    let case = Case {
        existing: id("DEMO-1"),
        title: "Add a greeting",
        description_start: "Say hello",
        priority: Priority::Medium,
        assigned: true,
        author_is_assignee: true,
        labels: &["Feature", "Docs"],
        missing: id("DEMO-404"),
        body: BODY,
    };
    let tracker = MarkdownTracker::new(root.path());
    check(&tracker, &case);
    check_stage(
        &tracker,
        &case.existing,
        &case.missing,
        "Needs Input",
        "In Progress",
    );
    assert_eq!(tracker.ticket(&case.existing).unwrap().stage, "In Progress");
    let maintainer = Person {
        id: "maintainer".to_owned(),
        name: "maintainer".to_owned(),
    };
    assert_eq!(
        Tracker::ticket(&tracker, &case.existing).unwrap().assignee,
        Some(maintainer)
    );
}

fn linear_case() -> Case {
    Case {
        existing: id("OWL-13"),
        title: "Add the Linear tracker adapter: read a ticket and post a comment",
        description_start: "**Why.**",
        priority: Priority::Medium,
        assigned: true,
        // Its assignee's account created it, but a holder of that account's
        // API key could have (OWL-74): Linear never reports the account.
        author_is_assignee: false,
        labels: &["Feature", "Adapters"],
        missing: id("OWL-99999"),
        body: BODY,
    }
}

const LINEAR_FIXTURE: &str = "conformance.json";

#[test]
fn the_linear_tracker_conforms() {
    let replay = Replay::new(LINEAR_FIXTURE);
    check(
        &LinearTracker::with_transport(replay.clone()),
        &linear_case(),
    );
    replay.assert_done();
}

/// Records the Linear conformance fixture by running the suite against the
/// live API. The suite posts a comment on OWL-13: it is sent only when
/// `OWLSHIFT_RECORD_WRITES=OWL-13`; otherwise it is made up and flagged.
/// What was recorded is written even when the suite fails, so a failed run
/// never needs a second write to be understood.
#[test]
#[ignore = "records against the live Linear API"]
fn record_conformance_fixture() {
    let key = support::live_key();
    support::assert_owlshift_workspace(&key);
    let case = linear_case();
    let writes =
        std::env::var("OWLSHIFT_RECORD_WRITES").ok().as_deref() == Some(case.existing.as_str());
    let recorder = if writes {
        Recorder::new(&key)
    } else {
        Recorder::new(&key).synthesizing_posts_on(&key, case.existing.as_str())
    };
    let tracker = LinearTracker::with_transport(recorder.clone());
    // After a panic only the recorder's log is read, never the tracker.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(&tracker, &case)));
    let how = if writes {
        "recorded live"
    } else {
        "recorded live, except the comment posted on OWL-13, made up (see `synthesized`)"
    };
    recorder.write(
        LINEAR_FIXTURE,
        &format!("{} on the Owlshift workspace, {how}", today()),
    );
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// The ticket the stage fixture moves, from In Progress to Needs Input and
/// back: the issue of the change that recorded it.
const STAGE_TICKET: &str = "OWL-137";
const STAGE_FIXTURE: &str = "stage-owl-137.json";

#[test]
fn the_linear_tracker_moves_the_visible_stage() {
    let replay = Replay::new(STAGE_FIXTURE);
    check_stage(
        &LinearTracker::with_transport(replay.clone()),
        &id(STAGE_TICKET),
        &linear_case().missing,
        "Needs Input",
        "In Progress",
    );
    replay.assert_done();
}

/// Records the stage fixture against the live API. It writes to Linear:
/// only with `OWLSHIFT_RECORD_WRITES=OWL-137`, on the Owlshift workspace,
/// and only while OWL-137 is In Progress, where the check leaves it. Once
/// the issue was found, it is put back in the state it was found in
/// whatever happens, and the state Linear answers is printed.
#[test]
#[ignore = "records against the live Linear API, and moves OWL-137"]
fn record_stage_fixture() {
    let key = support::live_key();
    support::assert_owlshift_workspace(&key);
    assert_eq!(
        std::env::var("OWLSHIFT_RECORD_WRITES").ok().as_deref(),
        Some(STAGE_TICKET),
        "refusing to move {STAGE_TICKET} without OWLSHIFT_RECORD_WRITES={STAGE_TICKET}"
    );
    let found = support::live_query(
        &key,
        &format!("{{ issue(id: \"{STAGE_TICKET}\") {{ id state {{ id name }} }} }}"),
    );
    let issue = &found["data"]["issue"];
    assert_eq!(
        issue["state"]["name"], "In Progress",
        "{STAGE_TICKET} must be In Progress to record the stage fixture"
    );
    let _restore = Restore {
        key: key.clone(),
        issue: issue["id"].as_str().unwrap().to_owned(),
        state: issue["state"]["id"].as_str().unwrap().to_owned(),
    };

    let recorder = Recorder::new(&key);
    let tracker = LinearTracker::with_transport(recorder.clone());
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        check_stage(
            &tracker,
            &id(STAGE_TICKET),
            &linear_case().missing,
            "Needs Input",
            "In Progress",
        )
    }));
    recorder.write(
        STAGE_FIXTURE,
        &format!("{} on the Owlshift workspace, recorded live", today()),
    );
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// The ticket whose stage is read back: OWL-112, which added the read and is
/// Done, so the answer the fixture holds is the one Linear still gives.
const READ_STAGE_TICKET: &str = "OWL-112";
const READ_STAGE_FIXTURE: &str = "read-stage-owl-112.json";

fn check_read_stage(tracker: &dyn Tracker) {
    assert_eq!(tracker.stage(&id(READ_STAGE_TICKET)).unwrap(), "Done");
    let missing = tracker.stage(&linear_case().missing).unwrap_err();
    assert_eq!(missing.kind, ErrorKind::NotFound, "{missing}");
}

#[test]
fn the_linear_tracker_reads_a_tickets_stage() {
    let replay = Replay::new(READ_STAGE_FIXTURE);
    check_read_stage(&LinearTracker::with_transport(replay.clone()));
    replay.assert_done();
}

/// Records the stage read (OWL-112) against the live API. It only reads, and
/// the answer is checked against the recording's, so a ticket that has moved
/// since fails here rather than recording another stage.
#[test]
#[ignore = "records against the live Linear API"]
fn record_read_stage_fixture() {
    let key = support::live_key();
    support::assert_owlshift_workspace(&key);
    let recorder = Recorder::new(&key);
    let tracker = LinearTracker::with_transport(recorder.clone());
    let outcome =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check_read_stage(&tracker)));
    recorder.write(
        READ_STAGE_FIXTURE,
        &format!("{} on the Owlshift workspace, recorded live", today()),
    );
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// Puts the issue back in the state it was found in when dropped, and says
/// whether Linear confirms it.
struct Restore {
    key: ApiKey,
    issue: String,
    state: String,
}

impl Drop for Restore {
    fn drop(&mut self) {
        let (issue, state) = (&self.issue, &self.state);
        let moved = support::live_query(
            &self.key,
            &format!(
                "mutation {{ issueUpdate(id: \"{issue}\", input: {{ stateId: \"{state}\" }}) \
                 {{ success issue {{ state {{ id name }} }} }} }}"
            ),
        );
        let now = &moved["data"]["issueUpdate"]["issue"]["state"];
        if now["id"] == state.as_str() {
            eprintln!("{STAGE_TICKET} is back in {}", now["name"]);
        } else {
            eprintln!("RESTORE FAILED: {STAGE_TICKET} is not back in its state: {moved}");
        }
    }
}

fn today() -> String {
    jiff::Zoned::now().date().to_string()
}

/// The GitHub Issues tracker (OWL-535), on recorded exchanges of the private
/// repository `pitchopp/owlshift-live-sandbox`, whose issue #11 was opened
/// and commented on by live check G1 and G2 (build plan, "P9 ·
/// Open-source readiness"). Its reads were recorded live; its writes, the
/// comment and the labels, are made up in the shape GitHub answers them and
/// flagged `synthesized`, the reads after them amended to show them. To
/// record again, with a token that reads the repository in
/// `OWLSHIFT_GITHUB_TOKEN` or a logged-in `gh`:
///
/// ```sh
/// cargo test -p owlshift-adapters --test tracker_conformance -- --ignored record_github
/// ```
///
/// With `OWLSHIFT_RECORD_WRITES=11` the writes are sent, through the token,
/// on issue #11, and nothing is made up.
mod github {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use serde_json::{Value, json};

    use owlshift_adapters::forge::Repo;
    use owlshift_adapters::forge::github::{GitHubForge, HttpTransport};
    use owlshift_adapters::tracker::github::GitHubTracker;
    use owlshift_adapters::tracker::{ErrorKind, Tracker};
    use owlshift_contracts::Priority;
    use owlshift_contracts::config;

    use super::support::{self, Recorder, Replay};
    use super::{BODY, Case, check, check_stage, id, today};

    const SANDBOX: &str = "pitchopp/owlshift-live-sandbox";
    /// Issue #11, G1's: three comments, two of them in one second (G2).
    const ISSUE: &str = "11";
    /// Pull request #18, G5's.
    const PULL_REQUEST: &str = "18";
    const CONFORMANCE: &str = "conformance.json";
    const STAGE: &str = "stage.json";
    const THERE: &str = "agent:needs-input";
    const BACK: &str = "agent:in-progress";

    fn dir() -> PathBuf {
        support::fixtures("github-tracker")
    }

    fn states() -> config::Tracker {
        config::Tracker {
            kind: config::TrackerKind::Github,
            team: None,
            admit: config::Admit::Label("agent:ready".to_owned()),
            admitters: None,
            exclude: None,
            states: config::States {
                ready: "agent:ready".to_owned(),
                working: BACK.to_owned(),
                needs_input: THERE.to_owned(),
                review: "agent:review".to_owned(),
                parked: None,
            },
            priorities: None,
        }
    }

    fn tracker(
        transport: impl owlshift_adapters::graphql::Transport + Send + Sync + 'static,
    ) -> GitHubTracker {
        let forge = GitHubForge::with_transport(transport, Repo::parse(SANDBOX).unwrap());
        GitHubTracker::new(forge, &states())
    }

    fn case() -> Case {
        Case {
            existing: id(ISSUE),
            title: "Owlshift live check: G1",
            description_start: "Probe of OWL-532",
            priority: Priority::Unset,
            assigned: false,
            // Never an account on GitHub, as on Linear (OWL-74).
            author_is_assignee: false,
            labels: &[],
            missing: id("9999"),
            body: BODY,
        }
    }

    /// What the stage fixture checks beyond [`check_stage`]: issue #11 is
    /// closed, so it reads `closed` whatever its labels, and a pull
    /// request's number names no ticket (G5).
    fn check_reads(tracker: &dyn Tracker) {
        assert_eq!(tracker.stage(&id(ISSUE)).unwrap(), "closed");
        let pull = tracker.ticket(&id(PULL_REQUEST)).unwrap_err();
        assert_eq!(pull.kind, ErrorKind::NotFound, "{pull}");
    }

    #[test]
    fn the_github_tracker_conforms() {
        let replay = Replay::of(&dir(), CONFORMANCE);
        check(&tracker(replay.clone()), &case());
        replay.assert_done();
    }

    #[test]
    fn the_github_tracker_moves_the_visible_stage() {
        let replay = Replay::of(&dir(), STAGE);
        let tracker = tracker(replay.clone());
        let case = case();
        check_stage(&tracker, &case.existing, &case.missing, THERE, BACK);
        check_reads(&tracker);
        replay.assert_done();
    }

    /// No GitHub account of a recording is a real one: each user object is
    /// a pseudonym, `person-n` with the id n.
    #[test]
    fn the_github_tracker_fixtures_name_no_real_account() {
        fn users(value: &Value, found: &mut Vec<Value>) {
            match value {
                Value::Object(map) if map.contains_key("login") => found.push(value.clone()),
                Value::Object(map) => map.values().for_each(|v| users(v, found)),
                Value::Array(items) => items.iter().for_each(|v| users(v, found)),
                _ => {}
            }
        }
        for name in [CONFORMANCE, STAGE, DRAIN] {
            let fixture = support::load(&dir(), name);
            let mut found = Vec::new();
            for exchange in &fixture.exchanges {
                users(&exchange.response, &mut found);
            }
            assert!(!found.is_empty(), "{name}: no user at all");
            for user in found {
                let n = user["id"].as_u64().expect("a numeric id");
                assert_eq!(user["login"], format!("person-{n}"), "{name}: {user}");
                assert_eq!(user.as_object().unwrap().len(), 3, "{name}: {user}");
            }
        }
    }

    /// What the recording made up on issue #11: the comments posted and the
    /// labels it carries since.
    #[derive(Default)]
    struct Made {
        comments: Vec<Value>,
        labels: Vec<String>,
    }

    /// The recorder over the live API. Without `OWLSHIFT_RECORD_WRITES=11`
    /// every write is made up and the reads of issue #11 amended to show
    /// it; any other write is refused.
    fn recorder() -> (Recorder, &'static str) {
        let live = Recorder::over(HttpTransport::new(support::github_token()), dir())
            .pseudonymizing_github_users();
        if std::env::var("OWLSHIFT_RECORD_WRITES").ok().as_deref() == Some(ISSUE) {
            return (live, "recorded live");
        }
        let viewer = live.live_get("user");
        let issue = format!("repos/{SANDBOX}/issues/{ISSUE}");
        let made = Arc::new(Mutex::new(Made::default()));
        let (written, path) = (Arc::clone(&made), issue.clone());
        let recorder = live
            .making_up(move |request| {
                let method = request["method"].as_str()?;
                let at = request["path"].as_str()?;
                let mut made = written.lock().unwrap();
                match (method, at.strip_prefix(&path)) {
                    ("POST", Some("/comments")) => {
                        let now = jiff::Timestamp::now()
                            .round(jiff::Unit::Second)
                            .unwrap()
                            .to_string();
                        let comment = json!({
                            "id": 9_000_000_001_u64 + made.comments.len() as u64,
                            "issue_url": format!("https://api.github.com/{path}"),
                            "user": viewer.clone(),
                            "created_at": now,
                            "updated_at": now,
                            "author_association": "OWNER",
                            "body": request["body"]["body"],
                            "performed_via_github_app": { "slug": "owl-shift" },
                        });
                        made.comments.push(comment.clone());
                        Some((
                            comment,
                            "not posted: shaped like the recorded comments".to_owned(),
                        ))
                    }
                    ("POST", Some("/labels")) => {
                        for label in request["body"]["labels"].as_array()? {
                            made.labels.push(label.as_str()?.to_owned());
                        }
                        Some((
                            labels(&made.labels),
                            "not sent: GitHub answers the issue's labels".to_owned(),
                        ))
                    }
                    ("DELETE", Some(rest)) => {
                        let gone = rest.strip_prefix("/labels/")?.replace("%3A", ":");
                        made.labels.retain(|label| *label != gone);
                        Some((
                            labels(&made.labels),
                            "not sent: GitHub answers the issue's labels".to_owned(),
                        ))
                    }
                    _ => panic!(
                        "refusing to write {method} {at} without OWLSHIFT_RECORD_WRITES={ISSUE}"
                    ),
                }
            })
            .amending(move |at, response| {
                let made = made.lock().unwrap();
                if at == issue && !made.labels.is_empty() {
                    response["labels"] = labels(&made.labels);
                    return Some("recorded, with the labels made up since".to_owned());
                }
                if at.starts_with(&format!("{issue}/comments?")) && !made.comments.is_empty() {
                    let page = response.as_array_mut()?;
                    page.extend(made.comments.iter().cloned());
                    return Some("recorded, plus the made-up comment".to_owned());
                }
                None
            });
        (
            recorder,
            "recorded live, except the writes on issue #11, made up (see `synthesized`)",
        )
    }

    /// Labels as GitHub answers them, made up.
    fn labels(names: &[String]) -> Value {
        json!(
            names
                .iter()
                .map(|name| json!({ "name": name, "color": "ededed", "default": false }))
                .collect::<Vec<_>>()
        )
    }

    #[test]
    #[ignore = "records against the live GitHub API"]
    fn record_github_conformance_fixture() {
        let (recorder, how) = recorder();
        let tracker = tracker(recorder.clone());
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(&tracker, &case())));
        recorder.write(CONFORMANCE, &format!("{} on {SANDBOX}, {how}", today()));
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    /// The label of OWL-547's gesture checks (G3): #30 got it through the
    /// app's token, #31 through the maintainer's own `gh`, #32 added after
    /// the opening, through the token.
    const GESTURE_LABEL: &str = "owlshift-547-ready";
    const DRAIN: &str = "drain.json";
    const PROPOSAL_TITLE: &str = "Owlshift live check: OWL-536 proposal";
    const PROPOSAL_MARKER: &str = "fingerprint 0536abcdef01";

    /// What the drain reads (OWL-536), on the sandbox's probes of G1, G3 and
    /// G4, then the writes it makes, made up when recorded.
    fn check_drain(tracker: &dyn Tracker) {
        use owlshift_adapters::tracker::{Author, Column, Proposal, StateType};
        use owlshift_contracts::config::Admit;

        let admit = Admit::Label(GESTURE_LABEL.to_owned());
        let listed = tracker
            .admitted(&Column {
                team: None,
                ready: GESTURE_LABEL,
                admit: &admit,
            })
            .unwrap();
        let ids: Vec<&str> = listed.iter().map(|l| l.ticket.id.as_str()).collect();
        assert_eq!(ids, ["31"]);
        assert!(listed[0].blockers.is_empty());

        // G3: a label put through a person's own login is their gesture; one
        // put through the app's token is no account's; one removed since
        // stands nowhere.
        let gesture = tracker.gesture(&id("31"), &admit).unwrap();
        assert!(matches!(gesture.account, Author::Account(_)), "{gesture:?}");
        assert_eq!(gesture.at.unwrap().to_string(), "2026-11-12T19:44:38Z");
        for through_app in ["30", "32"] {
            let gesture = tracker.gesture(&id(through_app), &admit).unwrap();
            let Author::Other { name } = gesture.account else {
                panic!("{through_app}: {gesture:?}");
            };
            assert!(name.ends_with("through the GitHub App owl-shift"), "{name}");
        }
        let removed = Admit::Label("owlshift-check:ready".to_owned());
        assert!(tracker.gesture(&id("12"), &removed).is_err());

        // G4 and G5: the closing reasons, and a pull request's number.
        for (number, state) in [
            ("14", Some(StateType::Completed)),
            ("15", Some(StateType::Canceled)),
            ("16", Some(StateType::Duplicate)),
            ("29", None),
        ] {
            assert_eq!(tracker.state_type(&id(number)).unwrap(), state, "#{number}");
        }
        let pull = tracker.state_type(&id(PULL_REQUEST)).unwrap_err();
        assert_eq!(pull.kind, ErrorKind::NotFound);
        let relations = tracker.relations(&id("13")).unwrap();
        let blockers: Vec<(&str, Option<StateType>)> = relations
            .blockers
            .iter()
            .map(|b| (b.id.as_str(), b.state))
            .collect();
        assert_eq!(
            blockers,
            [
                ("15", Some(StateType::Canceled)),
                ("16", Some(StateType::Duplicate))
            ]
        );
        let links = tracker.links(&id("13")).unwrap();
        assert_eq!(links.blocked_by.len(), 2);

        // G1: a comment's reactions, its thumbs-up a person's.
        let comments = tracker.comments(&id(ISSUE)).unwrap();
        let reacted: Vec<_> = comments
            .iter()
            .flat_map(|comment| &comment.reactions)
            .collect();
        assert!(
            reacted
                .iter()
                .any(|r| r.is_thumbs_up() && matches!(r.author, Author::Account(_))),
            "{reacted:?}"
        );
        assert!(tracker.working(None, BACK).unwrap().is_empty());

        // The proposals: none yet, then one filed, blocked by its parent; a
        // relation written once.
        assert!(
            tracker
                .proposed(None, PROPOSAL_TITLE, PROPOSAL_MARKER)
                .unwrap()
                .is_empty()
        );
        let parent = id("29");
        let filed = tracker
            .propose(&Proposal {
                parent: &parent,
                title: PROPOSAL_TITLE,
                description: &format!("Why.\n\n[owlshift] PROPOSAL · 29 · {PROPOSAL_MARKER}"),
                blocked_by_parent: true,
                ready: "agent:ready",
            })
            .unwrap();
        assert_eq!(filed.state, "agent:proposed");
        assert!(filed.problems.is_empty(), "{:?}", filed.problems);
        tracker.block(&id("14"), &parent).unwrap();
    }

    #[test]
    fn the_github_tracker_drains() {
        let replay = Replay::of(&dir(), DRAIN);
        check_drain(&tracker(replay.clone()));
        replay.assert_done();
    }

    /// The recorder of the drain's reads, which only reads the sandbox:
    /// the list of the gesture's label shows #31 open with its label, as
    /// when its gesture was made (it is closed since, its labels removed),
    /// and the writes, an issue and two relations, are made up.
    fn drain_recorder() -> Recorder {
        let live = Recorder::over(HttpTransport::new(support::github_token()), dir())
            .pseudonymizing_github_users();
        let repo = format!("repos/{SANDBOX}");
        let mut opened = live.live_get(&format!("{repo}/issues/31"));
        opened["state"] = json!("open");
        opened["state_reason"] = Value::Null;
        opened["closed_at"] = Value::Null;
        opened["labels"] = labels(&[GESTURE_LABEL.to_owned()]);
        let shape = live.live_get(&format!("{repo}/issues/29"));
        let list = format!("{repo}/issues?state=open&labels={GESTURE_LABEL}&");
        let issues = format!("{repo}/issues");
        live.making_up(move |request| {
            let method = request["method"].as_str()?;
            let at = request["path"].as_str()?;
            match method {
                "POST" if at == issues => {
                    let mut issue = shape.clone();
                    for (key, value) in [
                        ("id", json!(9_000_536_u64)),
                        ("number", json!(9001)),
                        ("title", request["body"]["title"].clone()),
                        ("body", request["body"]["body"].clone()),
                        ("labels", labels(&["agent:proposed".to_owned()])),
                        ("url", json!(format!("https://api.github.com/{issues}/9001"))),
                    ] {
                        issue[key] = value;
                    }
                    Some((issue, "not opened: shaped like issue #29".to_owned()))
                }
                "POST" if at.ends_with("/dependencies/blocked_by") => Some((
                    json!({}),
                    "not written: G4 wrote such a relation with 201".to_owned(),
                )),
                _ => panic!("refusing to write {method} {at} while recording the drain"),
            }
        })
        .amending(move |at, response| {
            at.starts_with(&list).then(|| {
                *response = json!([opened.clone()]);
                "recorded, then #31 shown open with its label, as when its gesture was made"
                    .to_owned()
            })
        })
    }

    #[test]
    #[ignore = "records against the live GitHub API"]
    fn record_github_drain_fixture() {
        let recorder = drain_recorder();
        let tracker = tracker(recorder.clone());
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check_drain(&tracker)));
        recorder.write(
            DRAIN,
            &format!(
                "{} on {SANDBOX}, recorded live, read-only: one list amended and the writes made \
                 up (see `synthesized`)",
                today()
            ),
        );
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    #[test]
    #[ignore = "records against the live GitHub API"]
    fn record_github_stage_fixture() {
        let (recorder, how) = recorder();
        let tracker = tracker(recorder.clone());
        let case = case();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            check_stage(&tracker, &case.existing, &case.missing, THERE, BACK);
            check_reads(&tracker);
        }));
        recorder.write(STAGE, &format!("{} on {SANDBOX}, {how}", today()));
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }
}
