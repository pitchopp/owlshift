//! Replies from the web UI on the bench (OWL-475, build plan "P8 · Local web
//! UI", part 10): the reply the UI composes and posts through the operator's
//! key, `ui::reply::send`, on the bench of `on_demand.rs`, the Markdown
//! tracker standing for the tracker and a tracker that posts as the decider
//! standing for the operator's key. The answer check reads the reply as the
//! decider's, and `owlshift continue` resumes the ticket.

use owlshift_adapters::graphql::Response as GraphQlResponse;
use owlshift_adapters::tracker::linear::{KeyAccount, LinearTracker};
use owlshift_core::floor::{FloorViolation, ReplyRefusal};
use owlshift_runner::tracker::github_key;
use owlshift_runner::ui::reply::{
    Answer, Choice, ComposeError, Form, Key, KeyState, ReplyError, section, send,
};
use owlshift_runner::writer::WriteError;
use owlshift_testkit::issues::Account;

use super::design_scenarios::{REVISED, answered, designed, reviewed};
use super::drain_scenarios::done_for;
use super::*;

/// The operator's key on the bench: the Markdown tracker, its comments
/// posted as `maintainer`, the ticket's decider, at the bench clock's next
/// minute, as Linear posts a comment of the key's account.
struct AsPerson<'a> {
    bench: &'a Bench,
    inner: MarkdownTracker,
}

impl<'a> AsPerson<'a> {
    fn new(bench: &'a Bench) -> Self {
        Self {
            bench,
            inner: MarkdownTracker::new(&bench.remote.checkout),
        }
    }
}

impl Tracker for AsPerson<'_> {
    fn capabilities(&self) -> &'static [Capability] {
        Tracker::capabilities(&self.inner)
    }
    fn ticket(&self, id: &TicketId) -> Result<Ticket, tracker::Error> {
        Tracker::ticket(&self.inner, id)
    }
    fn comments(&self, id: &TicketId) -> Result<Vec<Comment>, tracker::Error> {
        Tracker::comments(&self.inner, id)
    }
    fn post_comment(&self, id: &TicketId, body: &str) -> Result<Comment, tracker::Error> {
        let other = |why: String| tracker::Error::new(tracker::ErrorKind::Other, why);
        self.inner
            .post_comment(id, "maintainer", tick(&self.bench.clock), body)
            .map_err(|e| other(e.to_string()))?;
        // The newest comment is the one just posted.
        Tracker::comments(&self.inner, id)?
            .pop()
            .ok_or_else(|| other("the comment just posted is gone".to_owned()))
    }
    fn set_stage(&self, id: &TicketId, state: &str) -> Result<(), tracker::Error> {
        Tracker::set_stage(&self.inner, id, state)
    }
}

/// The key's account: `account`, an operator's.
fn key_of(account: &str) -> KeyAccount {
    KeyAccount {
        id: account.to_owned(),
        name: account.to_owned(),
        app: false,
        workspace: "owlshift".to_owned(),
    }
}

/// A transport no test reaches: the key state's own tracker, which the bench
/// never posts through.
struct Unreachable;

impl Transport for Unreachable {
    fn send(&self, _: &Value) -> Result<GraphQlResponse, String> {
        Err("unreachable".to_owned())
    }
}

/// The project's key as the UI holds it once read, its account `account`.
fn ready(account: &str) -> KeyState {
    KeyState::Ready(Key {
        account: key_of(account),
        tracker: Box::new(LinearTracker::with_transport(Unreachable)),
        names: Box::new(|_: &str| Ok(None)),
    })
}

fn pick(question: &str, letter: &str, text: &str) -> Answer {
    Answer {
        question: question.to_owned(),
        choice: Some(Choice::Letter(letter.to_owned())),
        text: text.to_owned(),
    }
}

fn recommended(question: &str) -> Answer {
    Answer {
        question: question.to_owned(),
        choice: Some(Choice::Recommendation),
        text: String::new(),
    }
}

impl Bench {
    /// The form, answering the newest ask, as the page sends it.
    fn form(&self, answers: Vec<Answer>, go: bool) -> Form {
        let record = self.record();
        Form {
            ask: record
                .questions
                .asks
                .last()
                .expect("an ask")
                .comment
                .clone(),
            answers,
            go,
        }
    }

    /// Sends `form` from the UI through the key of `account`.
    fn send_from_ui(&self, account: &str, form: &Form) -> Result<Comment, ReplyError> {
        send(
            &AsPerson::new(self),
            &key_of(account),
            &self.record(),
            &ticket(),
            form,
        )
    }

    /// The decider's comments a brief's thread holds, oldest first.
    fn decider_comments(brief: &Brief) -> Vec<String> {
        brief
            .thread
            .iter()
            .filter_map(|entry| match entry {
                ThreadEntry::Comment { author, body, .. }
                    if author.relation == Relation::Decider =>
                {
                    Some(body.clone())
                }
                _ => None,
            })
            .collect()
    }
}

/// The round of `LETTERED`, asked by the Build of DEMO-1.
fn asked() -> Bench {
    let bench = Bench::new(true);
    let (asked, printed) = bench.run(vec![bench.reply(None, Some(LETTERED))], None);
    assert!(
        matches!(&asked, Err(Stop::NeedsInput { posted: Ok(_), .. })),
        "{asked:?}\n{printed}"
    );
    bench
}

/// The acceptance: a reply from the UI, its last line `go`, is the decider's
/// in the answer check's brief, counts at once (no quiet window passed), and
/// the ticket resumes and delivers.
#[test]
fn a_reply_from_the_ui_is_the_deciders_and_resumes_the_ticket() {
    let bench = asked();
    let form = bench.form(vec![pick("Q1", "B", ""), recommended("Q2")], true);
    let posted = bench.send_from_ui("maintainer", &form).unwrap();
    assert_eq!(posted.body, "Q1: B\nQ2: B\n\ngo");
    // Read at once: only `go` makes it count.
    bench.waited.set(SignedDuration::ZERO);
    let both = check(&[
        ("Q1", "answered", "B, French."),
        ("Q2", "answered", "B, no sign-off."),
    ]);
    let (delivered, printed) = bench.continue_ticket(vec![
        bench.reply(None, Some(&both)),
        bench.reply(Some("Hello"), Some(DONE)),
    ]);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let check_brief = bench
        .briefs()
        .into_iter()
        .rfind(|brief| brief.role == Role::AnswerCheck)
        .expect("the answer check ran");
    assert_eq!(
        Bench::decider_comments(&check_brief),
        ["Q1: B\nQ2: B\n\ngo"]
    );
    let comments = bench.comments();
    assert!(
        comments
            .iter()
            .any(|body| body.starts_with("[owlshift] RESUME · round 1")),
        "{comments:#?}"
    );
    assert!(comments.last().unwrap().starts_with("[owlshift] DELIVERY"));
}

/// "Count it now" unticked: the same reply, without `go`, settles through
/// the quiet window as a typed reply does.
#[test]
fn a_ui_reply_without_go_settles() {
    let bench = asked();
    let form = bench.form(vec![pick("Q1", "B", ""), recommended("Q2")], false);
    let posted = bench.send_from_ui("maintainer", &form).unwrap();
    assert_eq!(posted.body, "Q1: B\nQ2: B");
    bench.waited.set(SignedDuration::ZERO);
    let (settling, printed) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(&settling, Err(Stop::Settling { .. })),
        "{settling:?}\n{printed}"
    );
    assert!(bench.briefs().iter().all(|b| b.role != Role::AnswerCheck));
    assert_eq!(bench.record().state.waiting, Some(Waiting::NeedsInput));
}

/// A key of another account than the ask's decider: the ticket's reply
/// section offers no form, and the Writer refuses the reply, nothing posted.
#[test]
fn the_form_is_absent_and_the_writer_refuses_for_another_decider() {
    let bench = asked();
    let record = bench.record();
    let offer = section(&record, &ready("someone-else"), &ticket(), &|_| None);
    assert_eq!(offer["offered"], false, "{offer:#}");
    assert_eq!(offer["decider"]["account"], "maintainer");
    assert!(
        offer["why_not"]
            .as_str()
            .unwrap()
            .contains("not the decider's"),
        "{offer:#}"
    );
    assert_eq!(
        section(&record, &ready("maintainer"), &ticket(), &|_| None)["offered"],
        true
    );
    let before = bench.comments().len();
    let form = bench.form(vec![pick("Q1", "B", "")], true);
    let refused = bench.send_from_ui("someone-else", &form);
    assert!(
        matches!(
            refused,
            Err(ReplyError::Write(WriteError::Floor(FloorViolation::Reply(
                ReplyRefusal::NotTheDecider
            ))))
        ),
        "{refused:?}"
    );
    assert_eq!(bench.comments().len(), before);
}

/// A `risky` ticket stopped at its plan's approval (OWL-32), as
/// `a_risky_plan_is_approved_and_its_build_starts_from_the_moved_base` sets
/// it up.
fn awaiting_approval() -> Bench {
    let bench = Bench::of("risky");
    let replies = vec![
        bench.read(&designed(super::design_scenarios::FORKED)),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
    ];
    let (outcome, printed) = bench.run(replies, None);
    assert!(
        matches!(&outcome, Err(Stop::NeedsInput { .. })),
        "{outcome:?}\n{printed}"
    );
    let record = bench.record();
    let offer = section(&record, &ready("maintainer"), &ticket(), &|_| None);
    assert_eq!(offer["approval"], true, "{offer:#}");
    assert_eq!(offer["offered"], true, "{offer:#}");
    bench
}

/// Approving from the UI: the reply is `ok` and `go`, the approving Design
/// run reads it, the approval is kept with the reply's comment, and the
/// Build runs.
#[test]
fn a_plan_approved_from_the_ui_reaches_build() {
    let bench = awaiting_approval();
    let form = bench.form(vec![pick("Q1", "A", "")], true);
    let posted = bench.send_from_ui("maintainer", &form).unwrap();
    assert_eq!(posted.body, "ok\n\ngo");
    bench.waited.set(SignedDuration::ZERO);
    let replies = vec![
        bench.read(&check(&[("Q1", "answered", "Approved as it is.")])),
        bench.read(&answered("approved", None)),
        bench.built(),
    ];
    let (outcome, printed) = bench.continue_ticket(replies);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let approving = bench
        .briefs()
        .into_iter()
        .find(|brief| brief.role == Role::Design && brief.approval)
        .expect("the approving Design run");
    assert!(approving.plan.is_some());
    assert_eq!(Bench::decider_comments(&approving), ["ok\n\ngo"]);
    let approval = bench.record().plan.unwrap().approval.expect("kept");
    assert_eq!(approval.comment, posted.id);
    assert!(bench.roles().contains(&Role::Build));
}

/// Changing the plan from the UI: the first option past approval, with the
/// change in words, which the approving Design run reads as the decider's;
/// the revised plan gets its pass and its approval is asked again.
#[test]
fn a_plan_changed_from_the_ui_goes_back_to_design() {
    let bench = awaiting_approval();
    let record = bench.record();
    let question = &record.questions.asks.last().unwrap().questions[0];
    assert!(question.options.len() > 1, "{question:?}");
    let letter = owlshift_contracts::result::option_letter(1);
    // A change option that needs words, as the reply section lists them, is
    // refused without them, nothing posted.
    let offer = section(&record, &ready("maintainer"), &ticket(), &|_| None);
    let needs_words = offer["words"]["Q1"]
        .as_array()
        .unwrap()
        .iter()
        .any(|listed| listed == letter.as_str());
    if needs_words {
        let before = bench.comments().len();
        let bare = bench.form(vec![pick("Q1", &letter, "")], true);
        assert!(matches!(
            bench.send_from_ui("maintainer", &bare),
            Err(ReplyError::Compose(ComposeError::ChangeWithoutText { .. }))
        ));
        assert_eq!(bench.comments().len(), before);
    }
    let form = bench.form(vec![pick("Q1", &letter, "Use plain text instead.")], true);
    let posted = bench.send_from_ui("maintainer", &form).unwrap();
    let body = format!("Q1: {letter} — Use plain text instead.\n\ngo");
    assert_eq!(posted.body, body);
    bench.waited.set(SignedDuration::ZERO);
    let replies = vec![
        bench.read(&check(&[("Q1", "answered", "Plain text instead.")])),
        bench.read(&answered("changed", Some(REVISED))),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
    ];
    let (outcome, printed) = bench.continue_ticket(replies);
    let stop = outcome.expect_err("asked again");
    assert!(
        matches!(&stop, Stop::NeedsInput { posted: Ok(round), .. } if round.get() == 2),
        "{stop:?}\n{printed}"
    );
    let approving = bench
        .briefs()
        .into_iter()
        .find(|brief| brief.role == Role::Design && brief.approval)
        .expect("the approving Design run");
    assert_eq!(Bench::decider_comments(&approving), [body]);
    let kept = bench.record().plan.unwrap();
    assert!(kept.plan.approach.contains("the word checked"));
    assert!(kept.approval.is_none());
}

// GitHub Issues (OWL-552): the same reply on a bench whose tracker is the
// real GitHub Issues adapter over the Issues fake, the key built as
// `Kind::reply_key` builds it, from the credential's forge.

/// The decider, the project's viewer on the fake, a person.
fn maintainer() -> Account {
    Account::new("maintainer", 1)
}

/// The ticket of the GitHub bench: issue #1.
fn issue() -> TicketId {
    TicketId::new("1").unwrap()
}

/// A GitHub Issues bench whose issue #1, assigned to the maintainer, was
/// taken by `owlshift do 1`, whose Build asked `LETTERED`.
fn github_asked() -> (Bench, FakeIssues) {
    let start = "2026-10-02T08:00:00Z".parse().unwrap();
    let issues = FakeIssues::new(REPO, maintainer(), start);
    issues.open(
        1,
        "Add a greeting",
        "Add GREETING.md saying Hello.",
        &maintainer(),
        Some(&maintainer()),
        &[("agent:ready", &maintainer())],
    );
    let bench = Bench::on_github_issues(&maintainer(), &issues);
    let replies = vec![bench.reply(None, Some(LETTERED))];
    let (asked, printed) = bench.with_on_demand(replies, None, Vec::new(), |on_demand, sink| {
        on_demand.run(&issue(), sink)
    });
    assert!(
        matches!(&asked, Err(Stop::NeedsInput { posted: Ok(_), .. })),
        "{asked:?}\n{printed}"
    );
    (bench, issues)
}

impl Bench {
    /// What the ticket ref of issue #1 holds.
    fn issue_record(&self) -> TicketRecord {
        self.record_of("1").expect("a ticket ref")
    }

    /// The reply key of the project as `Kind::reply_key` builds it once its
    /// credential is open: two forges over `issues`, whose token is its
    /// viewer's.
    fn github_key(&self, issues: &FakeIssues) -> Key {
        let config_text = fs::read_to_string(self.remote.checkout.join("owlshift.toml")).unwrap();
        let config = ProjectConfig::parse(&config_text).unwrap();
        let forge = || GitHubForge::with_transport(issues.clone(), Repo::parse(REPO).unwrap());
        github_key(forge(), forge(), &config.tracker).unwrap()
    }

    /// The form answering the newest ask of issue #1.
    fn issue_form(&self, answers: Vec<Answer>, go: bool) -> Form {
        let record = self.issue_record();
        Form {
            ask: record
                .questions
                .asks
                .last()
                .expect("an ask")
                .comment
                .clone(),
            answers,
            go,
        }
    }
}

/// The acceptance (OWL-552): on a GitHub Issues project the decider's key is
/// the credential's account, `GET user`; the form is offered to them, a reply
/// sent from the UI lands as a comment on the issue, authored by that
/// account with no Owlshift marker, the answer check's brief reads it as the
/// decider's, counts it at once for its `go`, and the ticket resumes and
/// delivers. The page links no issue.
#[test]
fn a_reply_from_the_ui_lands_on_the_github_issue_and_resumes_it() {
    let (bench, issues) = github_asked();
    let key = bench.github_key(&issues);
    assert_eq!(key.account.id, "1");
    assert_eq!(key.account.name, "maintainer");
    assert!(!key.account.app && key.account.workspace.is_empty());
    let offer = section(
        &bench.issue_record(),
        &KeyState::Ready(bench.github_key(&issues)),
        &issue(),
        &|_| None,
    );
    assert_eq!(offer["offered"], true, "{offer:#}");
    assert_eq!(offer["decider"]["account"], "1");
    assert_eq!(
        offer["link"],
        format!("https://github.com/{REPO}/issues/1"),
        "{offer:#}"
    );

    let before = issues.comments(1).len();
    let form = bench.issue_form(vec![pick("Q1", "B", ""), recommended("Q2")], true);
    let posted = send(
        key.tracker.as_ref(),
        &key.account,
        &bench.issue_record(),
        &issue(),
        &form,
    )
    .unwrap();
    assert_eq!(posted.body, "Q1: B\nQ2: B\n\ngo");

    // On the issue: one more comment, the key's account's, no marker.
    let comments = issues.comments(1);
    assert_eq!(comments.len(), before + 1);
    let landed = comments.last().unwrap();
    assert_eq!(landed["body"], "Q1: B\nQ2: B\n\ngo");
    assert_eq!(landed["user"]["id"], 1);
    assert_eq!(landed["user"]["login"], "maintainer");
    assert!(!landed["body"].as_str().unwrap().contains("[owlshift]"));

    // Read at once: only `go` makes it count.
    bench.waited.set(SignedDuration::ZERO);
    let both = check(&[
        ("Q1", "answered", "B, French."),
        ("Q2", "answered", "B, no sign-off."),
    ]);
    let replies = vec![
        bench.reply(None, Some(&both)),
        bench.reply(Some("Hello"), Some(&done_for("1"))),
    ];
    let (delivered, printed) =
        bench.with_on_demand(replies, None, Vec::new(), |on_demand, sink| {
            on_demand.continue_ticket(&issue(), sink)
        });
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let check_brief = bench
        .briefs()
        .into_iter()
        .rfind(|brief| brief.role == Role::AnswerCheck)
        .expect("the answer check ran");
    assert_eq!(
        Bench::decider_comments(&check_brief),
        ["Q1: B\nQ2: B\n\ngo"]
    );
    let last = issues.comments(1).pop().unwrap();
    assert!(
        last["body"]
            .as_str()
            .unwrap()
            .starts_with("[owlshift] DELIVERY"),
        "{last}"
    );
}

/// A key of another account than the ask's decider: no form, the Writer
/// refuses the reply, and the issue's comments are as they were. A key of an
/// app (type `Bot`) is no operator's either.
#[test]
fn the_form_is_absent_and_the_writer_refuses_for_another_github_account() {
    let (bench, issues) = github_asked();
    let record = bench.issue_record();
    let start = "2026-10-02T09:00:00Z".parse().unwrap();
    let before = issues.comments(1).len();
    for (viewer, why, refusal) in [
        (
            Account::new("stranger", 7),
            "not the decider's",
            ReplyRefusal::NotTheDecider,
        ),
        (
            Account::bot("owl-shift[bot]", 1),
            "app",
            ReplyRefusal::AppKey,
        ),
    ] {
        // The viewer's own copy of the issue: a reply the floor let through
        // would land on it, so its comments are what the refusal keeps.
        let others = FakeIssues::new(REPO, viewer.clone(), start);
        others.open(
            1,
            "Add a greeting",
            "Add GREETING.md saying Hello.",
            &maintainer(),
            Some(&maintainer()),
            &[("agent:ready", &maintainer())],
        );
        let posted = others.comments(1).len();
        let key = bench.github_key(&others);
        assert_eq!(key.account.app, viewer.kind == "Bot");
        let state = KeyState::Ready(bench.github_key(&others));
        let offer = section(&record, &state, &issue(), &|_| None);
        assert_eq!(offer["offered"], false, "{offer:#}");
        assert!(
            offer["why_not"].as_str().unwrap().contains(why),
            "{offer:#}"
        );
        assert_eq!(
            offer["link"],
            format!("https://github.com/{REPO}/issues/1"),
            "{offer:#}"
        );
        let form = bench.issue_form(vec![pick("Q1", "B", "")], true);
        let refused = send(key.tracker.as_ref(), &key.account, &record, &issue(), &form);
        assert!(
            matches!(
                &refused,
                Err(ReplyError::Write(WriteError::Floor(FloorViolation::Reply(r)))) if *r == refusal
            ),
            "{refused:?}"
        );
        // Nothing was posted through the stranger's or the app's key.
        assert_eq!(others.comments(1).len(), posted);
    }
    assert_eq!(issues.comments(1).len(), before);
}
