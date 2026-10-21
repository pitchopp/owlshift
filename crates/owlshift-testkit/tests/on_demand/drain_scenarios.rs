//! The drain's scenarios (OWL-254, build plan part K): S3, S4, S6, S12 and
//! S13 of `docs/design/scenarios.md`, each played end to end by `owlshift
//! watch --drain` (`Watch::run_drain`) on the bench of `on_demand.rs`: the
//! Markdown tracker, the fake harness and the fake GitHub. Each test plays
//! the bullets of its scenario that P4 builds; part K says which wait for a
//! later step. Beside S4, the guards of its branch start (OWL-274): a kept
//! branch with commits of its own, and a fast-forward that would overwrite
//! a file.

use owlshift_adapters::tracker::markdown;
use owlshift_runner::notify;

use super::*;

/// A Build run's `done` delivering `id` on its own branch.
pub(super) fn done_for(id: &str) -> String {
    DONE.replace(BRANCH, &format!("owlshift/{}", id.to_lowercase()))
}

impl Bench {
    /// Writes the ticket `id` at `stage`, created by `author`, decided by
    /// `maintainer`, labelled `labels`, with `front` added to its front
    /// matter, and `description`.
    pub(super) fn put_ticket(
        &self,
        id: &str,
        author: &str,
        stage: &str,
        labels: &[&str],
        front: &str,
        description: &str,
    ) {
        let dir = self.remote.checkout.join("tickets").join(id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("ticket.md"),
            format!(
                "+++\ntitle = \"Ticket {id}\"\nauthor = \"{author}\"\nstage = \"{stage}\"\n\
                 assignee = \"maintainer\"\nlabels = {labels:?}\n{front}+++\n\n{description}\n"
            ),
        )
        .unwrap();
    }

    /// The ticket `id` as the tracker shows it.
    pub(super) fn shown(&self, id: &str) -> markdown::Ticket {
        MarkdownTracker::new(&self.remote.checkout)
            .ticket(&TicketId::new(id).unwrap())
            .unwrap()
    }

    /// The bodies of the comments on `id`, oldest first.
    pub(super) fn comments_on(&self, id: &str) -> Vec<String> {
        MarkdownTracker::new(&self.remote.checkout)
            .comments(&TicketId::new(id).unwrap())
            .unwrap()
            .into_iter()
            .map(|comment| comment.body)
            .collect()
    }

    /// What the ticket ref of `id` holds, if it has one.
    pub(super) fn record_of(&self, id: &str) -> Option<TicketRecord> {
        let env = self.env.clone();
        let git = Git::with_setup("git", move |command| env.apply(command));
        ticket_ref::read(&git, &self.dirs().checkout(), &TicketId::new(id).unwrap())
            .unwrap()
            .map(|stored| stored.record)
    }

    /// The tip of `id`'s branch on the remote.
    fn tip_of(&self, id: &str) -> String {
        let branch = format!("refs/heads/owlshift/{}", id.to_lowercase());
        let out = self.env.run(&self.remote.bare, &["rev-parse", &branch]);
        String::from_utf8(out.unwrap()).unwrap().trim().to_owned()
    }

    /// Merges `id`'s pull request as the forge does: `main` moves to its
    /// branch, a fast-forward, and the pull request reads merged. Nothing
    /// moves the ticket's stage. Returns `main`'s new head.
    pub(super) fn merge(&self, id: &str) -> String {
        let tip = self.tip_of(id);
        self.env
            .run(&self.remote.bare, &["update-ref", "refs/heads/main", &tip])
            .unwrap();
        self.github.followed.lock().unwrap().state = "MERGED";
        let open = self.github.open.lock().unwrap().take();
        assert!(open.is_some(), "{id} has no open pull request to merge");
        tip
    }

    /// The next pull request read is the one opened since the merge: open.
    fn reopened(&self) {
        self.github.followed.lock().unwrap().state = "OPEN";
    }

    /// [`Self::drain_passes`], with what happens while given runs work.
    fn drain_during(
        &self,
        replies: Vec<Reply>,
        during: Vec<During>,
        passes: usize,
        mut between: impl FnMut(usize),
    ) -> (Vec<Result<Delivered, Stop>>, String, String) {
        let mut done = 0;
        let ((outcomes, lines), printed) =
            self.with_on_demand(replies, None, during, |on_demand, sink| {
                let (mut outcomes, mut lines) = (Vec::new(), Vec::new());
                let watch = Watch {
                    on_demand,
                    interval: POLL_INTERVAL,
                };
                watch.run_drain(
                    sink,
                    &mut lines,
                    &mut |_, outcome, _| outcomes.push(outcome.clone()),
                    &mut |_| {
                        done += 1;
                        if done < passes {
                            between(done);
                        }
                        done < passes
                    },
                );
                (outcomes, String::from_utf8(lines).unwrap())
            });
        (outcomes, lines, printed)
    }

    /// Each run's role and ticket, oldest first.
    fn runs(&self) -> Vec<(Role, String)> {
        self.briefs()
            .iter()
            .map(|brief| (brief.role, brief.ticket.id.as_str().to_owned()))
            .collect()
    }
}

/// The ticket of each `dispatch` event of `events`, checking that the drain
/// made it.
pub(super) fn dispatched(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter(|event| event.kind == EventKind::Dispatch)
        .map(|event| {
            assert_eq!(event.data["command"], json!("drain"), "{event:?}");
            event.ticket.as_ref().unwrap().as_str().to_owned()
        })
        .collect()
}

/// The events of each pass of a drain: `events` from `start`, cut at `ends`,
/// the event count after each pass but the last.
pub(super) fn by_pass(events: &[Event], start: usize, ends: &[usize]) -> Vec<Vec<Event>> {
    let mut cuts = vec![start];
    cuts.extend_from_slice(ends);
    cuts.push(events.len());
    cuts.windows(2)
        .map(|w| events[w[0]..w[1]].to_vec())
        .collect()
}

/// Each `waits` decision of `events`: its ticket, reason and the tickets it
/// names.
pub(super) fn waits(events: &[Event]) -> Vec<(String, Value, Value)> {
    events
        .iter()
        .filter(|e| e.kind == EventKind::Decision && e.data.get("drain") == Some(&json!("waits")))
        .map(|e| {
            (
                e.ticket.as_ref().unwrap().as_str().to_owned(),
                e.data["reason"].clone(),
                e.data.get("tickets").cloned().unwrap_or(Value::Null),
            )
        })
        .collect()
}

/// The position in `events` of watch's reading `id`'s pull request merged.
pub(super) fn merge_read(events: &[Event], id: &str) -> Option<usize> {
    events.iter().position(|e| {
        e.kind == EventKind::Decision
            && e.ticket.as_ref().is_some_and(|t| t.as_str() == id)
            && e.data.get("watch") == Some(&json!("ended"))
            && e.data["reason"] == json!("merged")
    })
}

/// S4's chain in one `watch --drain` of six passes: DEMO-2 waits for DEMO-1
/// by a relation, DEMO-3 for DEMO-2 by a "Blocked by" line; DEMO-1's pull
/// request is merged on the forge after pass 3, DEMO-2's after pass 5.
/// Returns the bench, the event count after each pass but the last, `main`
/// after each merge, and watch's lines.
fn drain_a_chain() -> (Bench, Vec<usize>, Vec<String>, String) {
    let bench = Bench::draining("maintainer");
    bench.put_ticket(
        "DEMO-2",
        "maintainer",
        "Todo",
        &["agent"],
        "blocked_by = [\"DEMO-1\"]\n",
        "Greet again.",
    );
    bench.put_ticket(
        "DEMO-3",
        "maintainer",
        "Todo",
        &["agent"],
        "",
        "Greet at last.\n\nBlocked by DEMO-2.",
    );
    let intake = || bench.reply(None, Some(&intake_result("done", None, "")));
    let replies = vec![
        intake(),
        intake(),
        bench.reply(Some("Hello"), Some(&done_for("DEMO-1"))),
        intake(),
        bench.reply(Some("Hello again"), Some(&done_for("DEMO-2"))),
        bench.reply(Some("Hello at last"), Some(&done_for("DEMO-3"))),
    ];
    let ends = RefCell::new(Vec::new());
    let mains = RefCell::new(Vec::new());
    let (outcomes, lines, printed) = bench.drain(replies, 6, |pass| {
        ends.borrow_mut().push(bench.events().len());
        match pass {
            3 => mains.borrow_mut().push(bench.merge("DEMO-1")),
            4 => bench.reopened(),
            5 => mains.borrow_mut().push(bench.merge("DEMO-2")),
            _ => {}
        }
    });
    let delivered = outcomes.iter().filter(|outcome| outcome.is_ok()).count();
    assert_eq!(delivered, 3, "{outcomes:?}\n{lines}\n{printed}");
    (bench, ends.into_inner(), mains.into_inner(), lines)
}

/// Scenario S4, "Unblocked by a merge", on [`drain_a_chain`]: the drain
/// builds DEMO-1; the pass that reads its merge dispatches DEMO-2, and the
/// pass that reads DEMO-2's merge dispatches DEMO-3. No stage is moved to
/// Done: watch's read of each merge releases the next. Where each branch
/// starts is [`scenario_s4_the_blocked_branch_starts_from_the_merge`]'s.
#[test]
fn scenario_s4_unblocked_by_a_merge() {
    let (bench, ends, _, lines) = drain_a_chain();
    let events = bench.events();
    let passes = by_pass(&events, 0, &ends);
    let per_pass: Vec<Vec<String>> = passes.iter().map(|pass| dispatched(pass)).collect();
    assert_eq!(
        per_pass,
        [
            vec![],
            vec!["DEMO-1".to_owned()],
            vec![],
            vec!["DEMO-2".to_owned()],
            vec![],
            vec!["DEMO-3".to_owned()],
        ],
        "{lines}"
    );
    // Each blocked ticket is dispatched in the pass that reads its blocker's
    // merge, after that read.
    for (pass, blocker, blocked) in [(3, "DEMO-1", "DEMO-2"), (5, "DEMO-2", "DEMO-3")] {
        let read = merge_read(&passes[pass], blocker).expect("the merge read");
        let dispatch = passes[pass]
            .iter()
            .position(|e| e.kind == EventKind::Dispatch)
            .unwrap();
        assert!(
            read < dispatch,
            "{blocked} dispatched before {blocker}'s merge"
        );
    }
    // Told why each waited, by relation and by line.
    let told = waits(&events);
    for (blocked, blocker) in [("DEMO-2", "DEMO-1"), ("DEMO-3", "DEMO-2")] {
        assert!(
            told.contains(&(blocked.to_owned(), json!("blocked_by"), json!([blocker]))),
            "{told:?}"
        );
    }
    // The release came from the merges watch read, not from a stage.
    for id in ["DEMO-1", "DEMO-2", "DEMO-3"] {
        assert_eq!(bench.shown(id).stage, "In Review", "{id}");
    }
    assert_eq!(
        bench.runs(),
        [
            (Role::Intake, "DEMO-1".to_owned()),
            (Role::Intake, "DEMO-2".to_owned()),
            (Role::Build, "DEMO-1".to_owned()),
            (Role::Verify, "DEMO-1".to_owned()),
            (Role::Intake, "DEMO-3".to_owned()),
            (Role::Build, "DEMO-2".to_owned()),
            (Role::Verify, "DEMO-2".to_owned()),
            (Role::Build, "DEMO-3".to_owned()),
            (Role::Verify, "DEMO-3".to_owned()),
        ]
    );
}

/// Scenario S4's "Its branch starts from the updated default branch", on
/// [`drain_a_chain`]: DEMO-2's branch holds DEMO-1's merge, DEMO-3's holds
/// DEMO-2's. The first intake made each ticket's worktree and branch at the
/// base of its time; the dispatch after the merge fast-forwards the branch
/// to the base it fetched (OWL-274).
#[test]
fn scenario_s4_the_blocked_branch_starts_from_the_merge() {
    let (bench, _, mains, _) = drain_a_chain();
    assert!(is_ancestor(&bench, &mains[0], &bench.tip_of("DEMO-2")));
    assert!(is_ancestor(&bench, &mains[1], &bench.tip_of("DEMO-3")));
}

impl Bench {
    /// The head of DEMO-1's branch in the dedicated checkout.
    fn kept_head(&self) -> String {
        let out = self
            .env
            .run(&self.dirs().checkout(), &["rev-parse", "--verify", BRANCH])
            .unwrap();
        String::from_utf8(out).unwrap().trim().to_owned()
    }
}

/// A kept branch with commits of its own is dispatched as it is, never
/// moved to a newer base (OWL-274): a person's commit on it, after a `do`
/// that stopped, is in the delivery, and the merge made since is not.
#[test]
fn a_kept_branch_with_commits_of_its_own_is_dispatched_as_it_is() {
    let bench = Bench::new(true);
    let blocked = r#"{"format":14,"status":"blocked","summary":"Waiting."}"#;
    // The run leaves a commit of its own, then blocks: since OWL-400 a
    // commit made outside a run is a breach at the ticket's next start.
    let (outcome, printed) = bench.run(vec![bench.reply(Some("Hi"), Some(blocked))], None);
    assert!(
        matches!(outcome, Err(Stop::Parked { .. })),
        "{outcome:?}\n{printed}"
    );
    let own = bench.kept_head();
    let merged = bench.merge_into_main(&[("MERGED.md", "merged\n")]);

    let (outcome, printed) = bench.run(vec![bench.reply(Some("Hello"), Some(DONE))], None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let tip = bench.remote_branch().expect("the branch was pushed");
    assert!(is_ancestor(&bench, &own, &tip));
    assert!(!is_ancestor(&bench, &merged, &tip));
}

/// A drained dispatch whose fast-forward would overwrite a file the worktree
/// holds is refused with nothing moved, and the ticket is held as after any
/// refusal (OWL-274): the next pass leaves it, and once a person removed the
/// file and the hold passed, its Build starts from the merge.
#[test]
fn a_start_that_would_overwrite_a_local_change_holds_the_drained_ticket() {
    let bench = Bench::draining("maintainer");
    let replies = vec![
        bench.reply(None, Some(&intake_result("done", None, ""))),
        bench.reply(Some("Hello"), Some(DONE)),
    ];
    let obstacle = bench.worktree().join("NEW.md");
    let merged = RefCell::new(String::new());
    let before = RefCell::new((String::new(), String::new()));
    let (outcomes, lines, printed) = bench.drain(replies, 4, |pass| match pass {
        // Intake made the worktree; a file sits where the merge adds one.
        1 => {
            fs::write(&obstacle, "mine\n").unwrap();
            *merged.borrow_mut() = bench.merge_into_main(&[("NEW.md", "merged\n")]);
            *before.borrow_mut() = (bench.kept_head(), bench.ref_commit());
        }
        // The dispatch was refused, nothing moved.
        2 => {
            assert_eq!(*before.borrow(), (bench.kept_head(), bench.ref_commit()));
            assert_eq!(fs::read_to_string(&obstacle).unwrap(), "mine\n");
        }
        3 => {
            fs::remove_file(&obstacle).unwrap();
            let later = bench.clock.get().checked_add(SignedDuration::from_mins(11));
            bench.clock.set(later.unwrap());
        }
        _ => {}
    });
    let refused: Vec<&String> = outcomes
        .iter()
        .filter_map(|outcome| match outcome {
            Err(Stop::Refused(reason)) => Some(reason),
            _ => None,
        })
        .collect();
    assert_eq!(refused.len(), 1, "{outcomes:?}\n{lines}\n{printed}");
    assert!(refused[0].contains("NEW.md"), "{}", refused[0]);
    assert_eq!(
        bench.runs(),
        [
            (Role::Intake, TICKET.to_owned()),
            (Role::Build, TICKET.to_owned()),
            (Role::Verify, TICKET.to_owned())
        ]
    );
    assert_eq!(dispatched(&bench.events()), [TICKET]);
    let tip = bench.remote_branch().expect("the branch was pushed");
    assert!(is_ancestor(&bench, &merged.borrow(), &tip));
}

/// An intake result of `status`, asking `questions` when not empty, whose
/// record names each `(ticket, quote)` of `dependencies`.
fn intake_quoting(status: &str, dependencies: &[(&str, &str)], questions: &str) -> String {
    let mut result: Value = serde_json::from_str(&intake_result(status, None, questions)).unwrap();
    result["intake"]["dependencies"] = dependencies
        .iter()
        .map(|(ticket, quote)| json!({ "ticket": ticket, "quote": quote }))
        .collect();
    result.to_string()
}

/// The INTAKE comments on `id`, oldest first.
fn intake_comments(bench: &Bench, id: &str) -> Vec<String> {
    bench
        .comments_on(id)
        .into_iter()
        .filter(|comment| comment.starts_with("[owlshift] INTAKE"))
        .collect()
}

/// Scenario S3, "Questions at intake", under `watch --drain`: the first
/// intake asks the decider, the ticket waiting at needs input to return to
/// Intake, and relates the dependency DEMO-1's text names in prose; watch
/// continues the answer, intake runs again and relates the prerequisite the
/// answer alone names, then the ticket is back in the column, blocked. Intake
/// does not run again for the same text; once a person edits it, intake runs
/// again before any dispatch, even with every blocker done. Each INTAKE
/// comment shows the routing and the dependencies; no model runs between the
/// last intake and Build.
#[test]
fn scenario_s3_questions_at_intake() {
    let bench = Bench::draining("maintainer");
    let prose = "It reuses the banner DEMO-3 adds.";
    bench.describe(prose);
    bench.other_ticket("DEMO-2", "In Progress");
    bench.other_ticket("DEMO-3", "In Progress");
    let answer = "It is DEMO-2: it must land first.";
    let question = r#"[{"id":"Q1","category":"scope",
        "context":"The ticket waits for the billing rework, named without its identifier.",
        "text":"Which ticket is the billing rework?","recommendation":{"why":"The simplest reading of the ticket."}}]"#;
    let both = [("DEMO-3", prose), ("DEMO-2", answer)];
    let replies = vec![
        bench.reply(
            None,
            Some(&intake_quoting("questions", &both[..1], question)),
        ),
        bench.reply(None, Some(&check(&[("Q1", "answered", "DEMO-2.")]))),
        bench.reply(None, Some(&intake_quoting("done", &both, ""))),
        bench.reply(None, Some(&intake_quoting("done", &both, ""))),
        bench.reply(Some("Hello"), Some(DONE)),
    ];
    let routing = "**Routing:** claude on the fast tier, effort low";
    let ends = RefCell::new(Vec::new());
    let (outcomes, lines, printed) = bench.drain(replies, 6, |pass| {
        ends.borrow_mut().push(bench.events().len());
        match pass {
            1 => {
                // The round waits at needs input, to return to Intake, the
                // dependency in prose already related.
                let state = TicketState::try_from(&bench.record().state).unwrap();
                assert_eq!(
                    state.status(),
                    Status::NeedsInput {
                        return_to: Stage::Intake
                    }
                );
                assert_eq!(bench.stage(), "Needs Input");
                assert_eq!(bench.blocked_by(), ["DEMO-3"]);
                let intake = intake_comments(&bench, TICKET).pop().unwrap();
                assert!(intake.contains(routing), "{intake}");
                assert!(
                    intake.contains(&format!("DEMO-3, named by `{prose}`: relation written")),
                    "{intake}"
                );
            }
            2 => bench.answer(answer),
            4 => {
                bench.other_ticket("DEMO-2", "Done");
                bench.other_ticket("DEMO-3", "Done");
                bench.describe("Say it in English.");
            }
            _ => {}
        }
    });
    // The questions reach the decider, told as a person's blocker; the
    // answer check's step (OWL-367) and the ticket back in the column are
    // not.
    assert_eq!(outcomes.len(), 5, "{outcomes:?}\n{lines}\n{printed}");
    assert!(
        matches!(&outcomes[0], Err(Stop::NeedsInput { .. })),
        "{outcomes:?}"
    );
    assert!(notify::blocker_line(&ticket(), outcomes[0].as_ref().unwrap_err()).is_some());
    let stepped = outcomes[1].as_ref().unwrap_err();
    assert!(
        matches!(
            stepped,
            Stop::Stepped {
                stage: Stage::Intake,
                ..
            }
        ),
        "{stepped}"
    );
    assert_eq!(notify::blocker_line(&ticket(), stepped), None);
    for outcome in &outcomes[2..4] {
        let stop = outcome.as_ref().unwrap_err();
        assert!(matches!(stop, Stop::Ready { .. }), "{stop}");
        assert_eq!(notify::blocker_line(&ticket(), stop), None);
    }
    assert!(outcomes[4].is_ok(), "{outcomes:?}");

    let events = bench.events();
    let passes = by_pass(&events, 0, &ends.borrow());
    let runs_in = |pass: &[Event]| -> Vec<Value> {
        pass.iter()
            .filter(|e| e.kind == EventKind::RunStarted)
            .map(|e| e.data["role"].clone())
            .collect()
    };
    let per_pass: Vec<Vec<Value>> = passes.iter().map(|pass| runs_in(pass)).collect();
    assert_eq!(
        per_pass,
        [
            vec![json!("intake")],
            vec![],
            vec![json!("answer_check"), json!("intake")],
            vec![],
            vec![json!("intake")],
            vec![json!("build"), json!("verify")],
        ],
        "{lines}"
    );
    // Dispatched once, after the intake of the edited text.
    let dispatches: Vec<usize> = passes
        .iter()
        .enumerate()
        .filter(|(_, pass)| !dispatched(pass).is_empty())
        .map(|(n, _)| n + 1)
        .collect();
    assert_eq!(dispatches, [6]);
    let build = bench.last_of(Role::Build);
    assert!(build.ticket.description.contains("Say it in English."));
    // Back in the column after the answered round, blocked by both.
    let mut told = waits(&passes[2]);
    assert_eq!(told.len(), 1, "{told:?}");
    let (id, reason, mut blockers) = told.pop().unwrap();
    blockers
        .as_array_mut()
        .unwrap()
        .sort_by_key(ToString::to_string);
    assert_eq!(
        (id.as_str(), reason, blockers),
        ("DEMO-1", json!("blocked_by"), json!(["DEMO-2", "DEMO-3"]))
    );
    // Each dependency related once, the one named only in the answer too.
    assert_eq!(
        bench.relations_written(),
        ["DEMO-3 -> DEMO-1", "DEMO-2 -> DEMO-1"]
    );
    let record = bench.record().intake.unwrap();
    let kept: Vec<(&str, DependencyOutcome)> = record
        .dependencies
        .iter()
        .map(|d| (d.ticket.as_str(), d.outcome))
        .collect();
    assert_eq!(
        kept,
        [
            ("DEMO-3", DependencyOutcome::Existed),
            ("DEMO-2", DependencyOutcome::Existed),
        ]
    );
    let intakes = intake_comments(&bench, TICKET);
    assert_eq!(intakes.len(), 3, "{intakes:?}");
    assert!(intakes.iter().all(|c| c.contains(routing)), "{intakes:?}");
    assert!(
        intakes[1].contains(&format!("DEMO-2, named by `{answer}`: relation written")),
        "{}",
        intakes[1]
    );
    assert!(
        intakes[2].contains(&format!(
            "DEMO-2, named by `{answer}`: relation already there"
        )),
        "{}",
        intakes[2]
    );
}

/// An intake result `done` that declares `zones` and `resources` (OWL-402).
pub(super) fn intake_declaring(zones: &[&str], resources: &[&str]) -> String {
    let mut result: Value = serde_json::from_str(&intake_result("done", None, "")).unwrap();
    result["intake"]["zones"] = json!(zones);
    result["intake"]["resources"] = json!(resources);
    result.to_string()
}

/// OWL-402, part 4 of P6: intake declares zones and named resources. The
/// project names two resources; DEMO-1 carries a `zone:web` label. A first
/// intake run names a resource the project does not list: its result is
/// refused, and the next run is told why. The second declares the zone
/// `db/main` and the resource `assets`: the record keeps that declaration,
/// and the INTAKE comment and the Build's brief show what the ticket holds,
/// the label's zone added and `migrations` held through `db/main`.
#[test]
fn intake_declares_zones_and_named_resources() {
    let bench = Bench::build_admitting(
        "maintainer",
        "assignee = \"maintainer\"\nlabels = [\"agent\", \"zone:web\"]\n",
        "[stack.resources]\nmigrations = \"db/*/migrations/**\"\nassets = \"static/**\"\n",
        ", parked = \"Parked\"",
        "admit = { label = \"agent\" }\nadmitters = [\"maintainer\"]",
    );
    let replies = vec![
        bench.reply(None, Some(&intake_declaring(&["db/main"], &["nope"]))),
        bench.reply(None, Some(&intake_declaring(&["db/main"], &["assets"]))),
        bench.reply(Some("Hello"), Some(DONE)),
    ];
    let (outcomes, lines, printed) = bench.drain(replies, 2, |_| {});
    assert!(
        outcomes.iter().any(Result::is_ok),
        "{outcomes:?}\n{lines}\n{printed}"
    );

    let intakes: Vec<Brief> = bench
        .briefs()
        .into_iter()
        .filter(|brief| brief.role == Role::Intake)
        .collect();
    assert_eq!(intakes.len(), 2, "{lines}\n{printed}");
    let patterns: Vec<(&str, &str)> = intakes[0]
        .named_resources
        .iter()
        .map(|(name, pattern)| (name.as_str(), pattern.as_str()))
        .collect();
    assert_eq!(
        patterns,
        [
            ("assets", "static/**"),
            ("migrations", "db/*/migrations/**")
        ]
    );
    assert_eq!(intakes[0].result_refusal, None);
    let told = intakes[1].result_refusal.as_deref().unwrap_or_default();
    assert!(
        told.contains("the resource \"nope\" is not one the brief's named_resources lists"),
        "{told}"
    );

    let record = bench.record().intake.unwrap();
    assert_eq!(
        (record.zones, record.resources),
        (vec!["db/main".to_owned()], vec!["assets".to_owned()])
    );
    let comments = intake_comments(&bench, TICKET);
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(
        comments[0].contains("**Zones:** `db/main`, `web`\n**Resources:** `assets`, `migrations`"),
        "{}",
        comments[0]
    );
    let build = bench.last_of(Role::Build);
    assert_eq!(build.zones, ["db/main", "web"]);
    assert_eq!(build.resources, ["assets", "migrations"]);
    assert!(build.named_resources.is_empty());
}

/// OWL-412: the zones intake declared choose the decider of a ticket without
/// an assignee, as its labels' do. Intake itself runs only on a ticket that
/// its assignee or labels give a decider, so a ticket whose zones come from
/// intake alone is one whose assignee left after its intake: DEMO-1, labelled
/// with no zone, is taken in under its assignee, intake declares `docs`, and
/// the assignee is removed before the dispatch. The drain still dispatches
/// it, and its Build's round goes to the owner of `docs`.
#[test]
fn the_owner_of_a_zone_intake_declared_decides_a_ticket_without_an_assignee() {
    let bench = Bench::build_admitting(
        "maintainer",
        "assignee = \"maintainer\"\nlabels = [\"agent\"]\n",
        DOCS_OWNED,
        ", parked = \"Parked\"",
        "admit = { label = \"agent\" }\nadmitters = [\"maintainer\"]",
    );
    let replies = vec![
        bench.reply(None, Some(&intake_declaring(&["docs"], &[]))),
        bench.reply(None, Some(ROUND_1)),
    ];
    let (outcomes, lines, printed) = bench.drain_passes(replies, 3, |pass, _| {
        if pass == 1 {
            let text = fs::read_to_string(bench.ticket_file()).unwrap();
            fs::write(
                bench.ticket_file(),
                text.replace("assignee = \"maintainer\"\n", ""),
            )
            .unwrap();
        }
    });
    assert!(
        outcomes
            .iter()
            .any(|outcome| matches!(outcome, Err(Stop::NeedsInput { posted: Ok(_), .. }))),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    assert_eq!(bench.record().intake.unwrap().zones, ["docs"]);
    let build = bench.last_of(Role::Build);
    assert_eq!(build.decider, "owner");
    assert_eq!(
        bench.record().questions.asks[0].decider,
        AskDecider {
            account: "owner".to_owned(),
            by: DeciderRule::ZoneOwner,
        }
    );
}

/// Each `decision` event of `events` that `key` names `waits` with the reason
/// `collision`: its ticket, the holder and the resource.
pub(super) fn collisions(events: &[Event], key: &str) -> Vec<(String, Value, Value)> {
    events
        .iter()
        .filter(|e| e.kind == EventKind::Decision && e.data.get(key) == Some(&json!("waits")))
        .filter(|e| e.data["reason"] == json!("collision"))
        .map(|e| {
            (
                e.ticket.as_ref().unwrap().as_str().to_owned(),
                e.data["tickets"].clone(),
                e.data["resource"].clone(),
            )
        })
        .collect()
}

/// Scenario S5, "Collision with work in flight", its part between
/// Owlshift's tickets (OWL-403), one step at a time. DEMO-1 declares `app`
/// and asks its decider at Build: its ref holds `app` while it waits, so
/// DEMO-2, which declares `app/sub`, waits for it at the dispatch, told once,
/// and DEMO-3, which declares `docs`, is dispatched. DEMO-3's Build changes
/// `app/x.md` too, which the widening after the run finds: its Verify pass
/// passes, and its delivery waits at Verify for DEMO-1, dispatched first.
/// DEMO-1, answered, takes the turn DEMO-3 gives up and delivers; DEMO-3 waits
/// on while DEMO-1's pull request is open, and once it is merged DEMO-3
/// delivers the verified commit with no second Verify pass.
#[test]
fn scenario_s5_collisions_between_owlshifts_tickets() {
    let bench = Bench::draining("maintainer");
    for (id, text) in [
        ("DEMO-2", "Greet in the sub-app."),
        ("DEMO-3", "Document it."),
    ] {
        bench.put_ticket(id, "maintainer", "Todo", &["agent"], "", text);
    }
    // The project's gate greps GREETING.md, so DEMO-3 writes it too.
    let mut widening = bench.reply(Some("Hello"), Some(&done_for("DEMO-3")));
    for (path, text) in [("app/x.md", "Touched.\n"), ("docs/y.md", "Documented.\n")] {
        widening
            .files
            .insert(RelativePath::new(path).unwrap(), text.to_owned());
    }
    let replies = vec![
        bench.reply(None, Some(&intake_declaring(&["app"], &[]))),
        bench.reply(None, Some(&intake_declaring(&["app/sub"], &[]))),
        bench.reply(None, Some(ROUND_1)),
        bench.reply(None, Some(&intake_declaring(&["docs"], &[]))),
        widening,
        bench.reply(
            None,
            Some(&check(&[
                ("Q1", "answered", "English."),
                ("Q2", "answered", "Hello."),
            ])),
        ),
        bench.reply(Some("Hello"), Some(DONE)),
        // DEMO-2, free once DEMO-1's pull request is merged, asks too.
        bench.reply(None, Some(ROUND_1)),
    ];
    let (answered, merged, reopened) = (Cell::new(false), Cell::new(false), Cell::new(false));
    let (outcomes, lines, printed) = bench.drain_passes(replies, 16, |_, _| {
        let events = bench.events();
        let held = events
            .iter()
            .any(|e| e.data.get("delivery") == Some(&json!("held")));
        if held && !answered.get() {
            bench.answer("Q1: English. Q2: Hello.");
            answered.set(true);
        }
        let delivered = bench
            .record_of(TICKET)
            .and_then(|record| record.delivery)
            .is_some_and(|delivery| delivery.followed());
        if delivered && !merged.get() {
            bench.merge(TICKET);
            merged.set(true);
        }
        if merged.get() && !reopened.get() && merge_read(&events, TICKET).is_some() {
            bench.reopened();
            reopened.set(true);
        }
    });
    let report = format!("{outcomes:?}\n{lines}\n{printed}");
    let events = bench.events();

    // A held zone holds a dispatch: DEMO-2 waits for DEMO-1's `app`, told
    // once, while DEMO-3 is dispatched; DEMO-2 goes once DEMO-1's pull
    // request is merged and the slot DEMO-3 held is free.
    let app = json!({ "zone": "app" });
    let told = collisions(&events, "drain");
    assert_eq!(
        told,
        [("DEMO-2".to_owned(), json!(["DEMO-1"]), app.clone())],
        "{report}"
    );
    assert!(
        lines.contains("DEMO-2 waits: DEMO-1 holds zone `app`"),
        "{lines}"
    );
    let from_ready = |e: &Event| e.kind == EventKind::Dispatch && e.data["command"] == "drain";
    let drained: Vec<&str> = events
        .iter()
        .filter(|e| from_ready(e))
        .map(|e| e.ticket.as_ref().unwrap().as_str())
        .collect();
    assert_eq!(drained, ["DEMO-1", "DEMO-3", "DEMO-2"], "{report}");
    let dispatch_of = |id: &str| {
        events
            .iter()
            .position(|e| from_ready(e) && e.ticket.as_ref().is_some_and(|t| t.as_str() == id))
    };
    assert!(
        merge_read(&events, TICKET) < dispatch_of("DEMO-2"),
        "{report}"
    );

    // The widening found before delivery holds DEMO-3 at Verify, its
    // verified commit kept.
    let held: Vec<&Event> = events
        .iter()
        .filter(|e| e.data.get("delivery") == Some(&json!("held")))
        .collect();
    assert_eq!(held.len(), 1, "{report}");
    assert_eq!(held[0].ticket.as_ref().unwrap().as_str(), "DEMO-3");
    assert_eq!(
        (&held[0].data["tickets"], &held[0].data["resource"]),
        (&json!(["DEMO-1"]), &app)
    );
    let widened: Vec<&Event> = events
        .iter()
        .filter(|e| e.data.get("held") == Some(&json!("widened")))
        .filter(|e| e.ticket.as_ref().unwrap().as_str() == "DEMO-3")
        .collect();
    assert_eq!(widened.len(), 1, "{report}");
    assert_eq!(
        widened[0].data["files"],
        json!(["GREETING.md", "app/x.md", "docs/y.md"])
    );
    let waited = collisions(&events, "watch");
    assert!(
        waited.contains(&("DEMO-3".to_owned(), json!(["DEMO-1"]), app.clone())),
        "{waited:?}\n{report}"
    );
    // The ticket says it once, whatever the passes it waits (OWL-419); one
    // that waits at Ready, DEMO-2, is told nothing there.
    let waiting = |id: &str| -> Vec<String> {
        bench
            .comments_on(id)
            .into_iter()
            .filter(|c| c.starts_with("[owlshift] WAITING\n"))
            .collect()
    };
    let shown = waiting("DEMO-3");
    assert_eq!(shown.len(), 1, "{shown:?}\n{report}");
    assert!(
        shown[0].contains("This ticket waits: DEMO-1 holds zone `app`"),
        "{}",
        shown[0]
    );
    assert!(shown[0].contains("goes on by itself"), "{}", shown[0]);
    assert_eq!(waiting("DEMO-2"), Vec::<String>::new(), "{report}");

    // DEMO-1 delivers first, then DEMO-3, once DEMO-1's pull request is
    // merged, with no Verify pass after its hold.
    let opened: Vec<String> = events
        .iter()
        .filter(|e| e.kind == EventKind::TrackerWrite && e.data["action"] == json!("pull_request"))
        .map(|e| e.ticket.as_ref().unwrap().as_str().to_owned())
        .collect();
    assert_eq!(opened, ["DEMO-1", "DEMO-3"], "{report}");
    let verifies = |id: &str| {
        bench
            .runs()
            .iter()
            .filter(|(role, ticket)| *role == Role::Verify && ticket == id)
            .count()
    };
    assert_eq!((verifies("DEMO-1"), verifies("DEMO-3")), (1, 1), "{report}");
    let verified: Vec<&Event> = events
        .iter()
        .filter(|e| e.data.get("review") == Some(&json!("verified")))
        .collect();
    assert_eq!(verified.len(), 1, "{report}");
    assert_eq!(
        verified[0].data["commit"], held[0].data["commit"],
        "{report}"
    );

    // What DEMO-3's ref keeps of what it held: its declared zone, the files
    // its branch changed, the times, and no mark once delivered.
    let kept = bench.record_of("DEMO-3").unwrap().held.unwrap();
    assert_eq!(kept.zones, ["docs"]);
    assert_eq!(kept.files, ["GREETING.md", "app/x.md", "docs/y.md"]);
    assert!(kept.dispatched_at.is_some() && kept.delivered_at.is_some());
    assert_eq!(kept.widened_at.len(), 1);
    assert_eq!(kept.verified, None);
    let first = bench.record_of(TICKET).unwrap().held.unwrap();
    assert!(
        first.dispatched_at < kept.dispatched_at,
        "{first:?} {kept:?}"
    );
}

/// The `drain` = `waits` decisions of `events` for a collision: each
/// ticket, the event's holder fields and the resource.
fn collisions_with(events: &[Event]) -> Vec<(String, Value, Value)> {
    events
        .iter()
        .filter(|e| e.kind == EventKind::Decision && e.data.get("drain") == Some(&json!("waits")))
        .filter(|e| e.data["reason"] == json!("collision"))
        .map(|e| {
            let holder = ["tickets", "person", "pull_request"]
                .into_iter()
                .filter_map(|key| e.data.get(key).map(|value| (key.to_owned(), value.clone())))
                .collect::<serde_json::Map<_, _>>();
            (
                e.ticket.as_ref().unwrap().as_str().to_owned(),
                Value::Object(holder),
                e.data["resource"].clone(),
            )
        })
        .collect()
}

/// Scenario S5, "Collision with work in flight", its parts for a pull
/// request and a person's ticket (OWL-410). DEMO-1 declares `app`. A
/// person's pull request #7 changes `app/x.md`, and DEMO-9, which a person
/// works on, carries `zone:app`: DEMO-1 waits for the pull request, then,
/// once it is closed, for DEMO-9, then, once DEMO-9 left the working state,
/// is dispatched and delivered. DEMO-8, worked on with neither a pull
/// request nor a `zone:` label, is not counted, and told once.
#[test]
fn scenario_s5_a_persons_pull_request_and_ticket() {
    let bench = Bench::draining("maintainer");
    bench.put_ticket(
        "DEMO-9",
        "maintainer",
        "In Progress",
        &["zone:app"],
        "",
        "By hand.",
    );
    bench.put_ticket(
        "DEMO-8",
        "maintainer",
        "In Progress",
        &[],
        "",
        "By hand too.",
    );
    bench.github.others.lock().unwrap().push((
        7,
        "someone/topic".to_owned(),
        vec!["app/x.md".to_owned()],
    ));
    let replies = vec![
        bench.reply(None, Some(&intake_declaring(&["app"], &[]))),
        bench.reply(Some("Hello"), Some(DONE)),
    ];
    let (closed, left) = (Cell::new(false), Cell::new(false));
    let (outcomes, lines, printed) = bench.drain_during(replies, Vec::new(), 8, |_| {
        let told = collisions_with(&bench.events());
        if !closed.get()
            && told
                .iter()
                .any(|(_, holder, _)| holder["pull_request"] == 7)
        {
            bench.github.others.lock().unwrap().clear();
            closed.set(true);
        }
        if !left.get()
            && told
                .iter()
                .any(|(_, holder, _)| holder["tickets"] == json!(["DEMO-9"]))
        {
            bench.put_ticket(
                "DEMO-9",
                "maintainer",
                "Done",
                &["zone:app"],
                "",
                "By hand.",
            );
            left.set(true);
        }
    });
    let report = format!("{outcomes:?}\n{lines}\n{printed}");
    let events = bench.events();
    assert_eq!(
        collisions_with(&events),
        [
            (
                TICKET.to_owned(),
                json!({ "pull_request": 7 }),
                json!({ "zone": "app/x.md" })
            ),
            (
                TICKET.to_owned(),
                json!({ "tickets": ["DEMO-9"], "person": true }),
                json!({ "zone": "app" })
            ),
        ],
        "{report}"
    );
    assert!(
        lines.contains("DEMO-1 waits: pull request #7 holds zone `app/x.md`"),
        "{lines}"
    );
    assert!(
        lines.contains("DEMO-1 waits: DEMO-9, which a person works on, holds zone `app`"),
        "{lines}"
    );
    let unknown: Vec<&str> = events
        .iter()
        .filter(|e| e.kind == EventKind::Warning && e.data["what"] == json!("zones_unknown"))
        .map(|e| e.ticket.as_ref().unwrap().as_str())
        .collect();
    assert_eq!(unknown, ["DEMO-8"], "{report}");
    assert_eq!(dispatched(&events), [TICKET], "{report}");
    let record = bench.record_of(TICKET).unwrap();
    assert!(
        record.delivery.is_some_and(|delivery| delivery.followed()),
        "{report}"
    );
}

/// A pass whose read of the work in flight outside Owlshift fails starts
/// nothing that the collision rule checks (OWL-410). The listing of the
/// open pull requests fails once as DEMO-1 is ready: it waits, then is
/// dispatched. A pull request's files fail to read while its Build works:
/// its delivery is held at Verify, its verified commit kept. The next
/// listing fails too: its stage step waits. Once the reads answer, it
/// delivers the verified commit with no second Verify pass.
#[test]
fn an_unread_in_flight_set_holds_dispatches_stage_steps_and_deliveries() {
    let bench = Bench::draining("maintainer");
    bench.github.others.lock().unwrap().push((
        7,
        "someone/docs".to_owned(),
        vec!["docs/z.md".to_owned()],
    ));
    let replies = vec![
        bench.reply(None, Some(&intake_declaring(&["app"], &[]))),
        bench.reply(Some("Hello"), Some(DONE)),
    ];
    let github = bench.github.clone();
    let during: Vec<During> = vec![(2, Box::new(move || *github.fail_files.lock().unwrap() = 1))];
    let (listing, stepping) = (Cell::new(false), Cell::new(false));
    let (outcomes, lines, printed) = bench.drain_during(replies, during, 8, |pass| {
        if pass == 1 && !listing.get() {
            *bench.github.fail_pulls.lock().unwrap() = 1;
            listing.set(true);
        }
        let held = bench
            .events()
            .iter()
            .any(|e| e.data.get("delivery") == Some(&json!("held")));
        if held && !stepping.get() {
            *bench.github.fail_pulls.lock().unwrap() = 1;
            stepping.set(true);
        }
    });
    let report = format!("{outcomes:?}\n{lines}\n{printed}");
    let events = bench.events();
    let unread = |key: &str| {
        events
            .iter()
            .filter(|e| e.data.get(key).is_some())
            .filter(|e| e.data.get("reason") == Some(&json!("in_flight_unread")))
            .count()
    };
    assert_eq!(
        (unread("drain"), unread("delivery"), unread("watch")),
        (1, 1, 1),
        "{report}"
    );
    assert!(
        lines.contains("nothing that would touch work in flight starts while the open pull requests could not be read"),
        "{lines}"
    );
    let held = events
        .iter()
        .find(|e| e.data.get("delivery") == Some(&json!("held")))
        .unwrap();
    assert!(held.data.get("tickets").is_none(), "{held:?}");
    let verifies = bench
        .runs()
        .iter()
        .filter(|(role, _)| *role == Role::Verify)
        .count();
    assert_eq!(verifies, 1, "{report}");
    let record = bench.record_of(TICKET).unwrap();
    assert!(
        record.delivery.is_some_and(|delivery| delivery.followed()),
        "{report}"
    );
}

/// Scenario S6, "The human is the bottleneck", with `max_open_prs = 1`. The
/// drain's Build of DEMO-1 stops at a usage limit, which tells the operator,
/// and leaves DEMO-1 at Build: DEMO-2, taken in, waits for the one slot
/// until the reset, after which watch resumes DEMO-1's Build itself, at
/// round 0 as at any round (OWL-367), and delivers it, with nothing typed.
/// DEMO-2 then waits at the cap. In a new `watch --drain`, DEMO-1's open pull request holds
/// DEMO-2's Build at the cap, told once, while nothing is notified; the pass
/// that reads DEMO-1 merged builds DEMO-2.
#[test]
fn scenario_s6_the_human_is_the_bottleneck() {
    let bench = Bench::build_admitting(
        "maintainer",
        "assignee = \"maintainer\"\nlabels = [\"agent\"]\n",
        "max_open_prs = 1\n",
        ", parked = \"Parked\"",
        "admit = { label = \"agent\" }\nadmitters = [\"maintainer\"]",
    );
    bench.put_ticket(
        "DEMO-2",
        "maintainer",
        "Todo",
        &["agent"],
        "",
        "Greet once more.",
    );

    // DEMO-1's Build cut by the usage limit; DEMO-2 waits for the slot
    // DEMO-1 holds; after the reset, watch resumes DEMO-1's Build.
    let intake = || bench.reply(None, Some(&intake_result("done", None, "")));
    let reset: Timestamp = "2026-10-02T12:00:00Z".parse().unwrap();
    let mut limited = bench.reply(None, None);
    limited.usage_limit = Some(reset);
    let resumed = bench.reply(Some("Hello"), Some(DONE));
    let ends = RefCell::new(Vec::new());
    let replies = vec![intake(), intake(), limited, resumed];
    let (held, lines, printed) = bench.drain(replies, 4, |pass| {
        ends.borrow_mut().push(bench.events().len());
        if pass == 3 {
            bench.clock.set(reset);
        }
    });
    assert_eq!(held.len(), 4, "{held:?}\n{lines}\n{printed}");
    let stop = held[2].as_ref().expect_err("the usage limit");
    assert!(matches!(stop, Stop::UsageLimit { .. }), "{stop}");
    // The runner told it, once (OWL-404): the command's line says nothing.
    assert_eq!(notify::blocker_line(&ticket(), stop), None);
    assert_eq!(bench.notified.0.borrow().len(), 1);
    assert!(held[3].is_ok(), "{held:?}\n{lines}");
    let at = TicketState::try_from(&bench.record().state).unwrap();
    assert_eq!(at.status(), Status::Active(Stage::Watch));
    let events = bench.events();
    let passes = by_pass(&events, 0, &ends.borrow());
    assert_eq!(
        waits(&passes[2]),
        [("DEMO-2".to_owned(), json!("slot"), json!(["DEMO-1"]))],
        "{lines}"
    );
    // No dispatch from Ready: the resume's is a `continue`'s.
    let from_ready = |pass: &[Event]| {
        pass.iter()
            .filter(|e| e.kind == EventKind::Dispatch && e.data["command"] == json!("drain"))
            .count()
    };
    assert!(passes[2..].iter().all(|pass| from_ready(pass) == 0));
    let resumes: Vec<Value> = data_of(&events, EventKind::Decision, "watch")
        .into_iter()
        .filter(|data| data["stage"] == json!("build"))
        .collect();
    assert_eq!(resumes.len(), 1, "{resumes:?}");

    // At the cap, then released by the merge.
    let before = bench.events().len();
    let ends = RefCell::new(Vec::new());
    let build = bench.reply(Some("Hello there"), Some(&done_for("DEMO-2")));
    let (drained, lines, printed) = bench.drain(vec![build], 3, |pass| {
        ends.borrow_mut().push(bench.events().len());
        if pass == 2 {
            bench.merge("DEMO-1");
        }
    });
    assert_eq!(drained.len(), 1, "{drained:?}\n{lines}\n{printed}");
    assert!(drained[0].is_ok(), "{drained:?}");
    let events = bench.events();
    let capped: Vec<Value> = events[before..]
        .iter()
        .filter(|e| e.kind == EventKind::Decision && e.data.get("drain") == Some(&json!("pr_cap")))
        .map(|e| {
            json!([
                e.ticket.as_ref().map(|t| t.as_str().to_owned()),
                e.data["open_prs"],
                e.data["max_open_prs"]
            ])
        })
        .collect();
    assert_eq!(capped, [json!(["DEMO-2", 1, 1])], "{lines}");
    assert_eq!(
        lines.matches("reached their cap (1 of 1)").count(),
        1,
        "{lines}"
    );
    let passes = by_pass(&events, before, &ends.borrow());
    let per_pass: Vec<Vec<String>> = passes.iter().map(|pass| dispatched(pass)).collect();
    assert_eq!(per_pass, [vec![], vec![], vec!["DEMO-2".to_owned()]]);
    let read = merge_read(&passes[2], "DEMO-1").expect("the merge read");
    let dispatch = passes[2]
        .iter()
        .position(|e| e.kind == EventKind::Dispatch)
        .unwrap();
    assert!(read < dispatch);

    // No intake that ends done, no delivery and no cap told the operator.
    for outcome in held[..2].iter().chain(&drained) {
        if let Err(stop) = outcome {
            assert_eq!(notify::blocker_line(&ticket(), stop), None, "{stop}");
        }
    }
    assert_eq!(
        bench.runs(),
        [
            (Role::Intake, "DEMO-1".to_owned()),
            (Role::Intake, "DEMO-2".to_owned()),
            (Role::Build, "DEMO-1".to_owned()),
            (Role::Build, "DEMO-1".to_owned()),
            (Role::Verify, "DEMO-1".to_owned()),
            (Role::Build, "DEMO-2".to_owned()),
            (Role::Verify, "DEMO-2".to_owned()),
        ]
    );
}

/// OWL-397: a `do` typed by hand has a ticket ref from its first run's end,
/// so a running `watch --drain` takes it as it takes its own tickets. DEMO-1's
/// Build, cut by a usage limit whose reset has passed, is left at Build: watch
/// resumes it, where it saw no ref before; the resume is cut again, and
/// although its cost moved DEMO-1's ref, watch holds it for 10 minutes as a
/// resume that changed nothing else. Meanwhile DEMO-1's ref holds the one
/// slot: DEMO-2, taken in, waits for it, where it was built before. Once the
/// hold ends, watch resumes DEMO-1 and delivers it; DEMO-2 then waits for
/// DEMO-1's open pull request, which holds the whole repository (OWL-403).
#[test]
fn a_do_typed_by_hand_is_resumed_by_watch_and_holds_the_slot() {
    let bench = Bench::draining("maintainer");
    bench.put_ticket(
        "DEMO-2",
        "maintainer",
        "Todo",
        &["agent"],
        "",
        "Greet once more.",
    );
    // Passed when it is read: the limit pauses nothing.
    let cut = || {
        let mut reply = bench.reply(None, None);
        reply.usage_limit = Some("2026-10-01T00:00:00Z".parse().unwrap());
        reply.cost_micros = Some(1_000_000);
        reply
    };
    let (stopped, printed) = bench.run(vec![cut()], None);
    assert!(
        matches!(&stopped, Err(Stop::UsageLimit { .. })),
        "{stopped:?}\n{printed}"
    );
    let at = TicketState::try_from(&bench.record().state).unwrap();
    assert_eq!(at.status(), Status::Active(Stage::Build));
    assert_eq!(bench.stage(), "In Progress");
    let spent = bench.ref_commit();

    let before = bench.events().len();
    let intake = bench.reply(None, Some(&intake_result("done", None, "")));
    let replies = vec![cut(), intake, bench.reply(Some("Hello"), Some(DONE))];
    let ends = RefCell::new(Vec::new());
    let after_cut = RefCell::new(None);
    let (outcomes, lines, printed) = bench.drain(replies, 3, |pass| {
        ends.borrow_mut().push(bench.events().len());
        if pass == 1 {
            *after_cut.borrow_mut() = Some((bench.ref_commit(), bench.record()));
        }
        if pass == 2 {
            bench.waited.set(SignedDuration::from_mins(25));
        }
    });
    let events = bench.events();
    let passes = by_pass(&events, before, &ends.borrow());
    // Watch resumed DEMO-1's Build twice, the second once the hold ended.
    let resumes: Vec<usize> = passes
        .iter()
        .enumerate()
        .filter(|(_, pass)| {
            pass.iter().any(|e| {
                e.kind == EventKind::Decision
                    && e.ticket.as_ref().is_some_and(|t| t.as_str() == "DEMO-1")
                    && e.data.get("watch") == Some(&json!("continue"))
                    && e.data["stage"] == json!("build")
            })
        })
        .map(|(n, _)| n)
        .collect();
    assert_eq!(resumes, [0, 2], "{lines}\n{printed}");
    assert!(
        matches!(&outcomes[0], Err(Stop::UsageLimit { .. })),
        "{outcomes:?}"
    );
    // Cut again: the ref moved by the run's cost alone, and watch held it.
    let (commit, record) = after_cut.into_inner().expect("read after pass 1");
    assert_ne!(commit, spent);
    assert_eq!(TicketState::try_from(&record.state).unwrap(), at);
    assert_eq!(record.cost.map(|cost| cost.runs), Some(2));
    assert!(
        lines.contains("DEMO-1 is at Build as it was before that resume"),
        "{lines}"
    );
    // DEMO-2 waited for the slot DEMO-1's ref held; then, DEMO-1 delivered,
    // for DEMO-1's open pull request, which holds the whole repository: a
    // `do` with no intake declares no zone (OWL-403).
    assert_eq!(
        waits(&passes[1]),
        [("DEMO-2".to_owned(), json!("slot"), json!(["DEMO-1"]))],
        "{lines}"
    );
    assert_eq!(
        collisions(&events, "drain"),
        [(
            "DEMO-2".to_owned(),
            json!(["DEMO-1"]),
            json!({ "zone": "" })
        )],
        "{lines}"
    );
    assert!(
        lines.contains("DEMO-2 waits: DEMO-1 holds the whole repository"),
        "{lines}"
    );
    assert_eq!(
        bench.runs()[1..],
        [
            (Role::Build, "DEMO-1".to_owned()),
            (Role::Intake, "DEMO-2".to_owned()),
            (Role::Build, "DEMO-1".to_owned()),
            (Role::Verify, "DEMO-1".to_owned()),
        ],
        "{lines}"
    );
    assert_eq!(
        outcomes.iter().filter(|o| o.is_ok()).count(),
        1,
        "{outcomes:?}"
    );
}

impl Bench {
    /// What the ticket `id` holds on the tracker: its `ticket.md` and its
    /// comments.
    fn on_tracker(&self, id: &str) -> (String, Vec<String>) {
        let dir = self.remote.checkout.join("tickets").join(id);
        let file = fs::read_to_string(dir.join("ticket.md")).unwrap();
        let comments = if dir.join("comments").exists() {
            self.comments_on(id)
        } else {
            Vec::new()
        };
        (file, comments)
    }

    /// A person rewrites the `labels` line of `id` to `labels`.
    fn label(&self, id: &str, labels: &[&str]) {
        let file = self
            .remote
            .checkout
            .join("tickets")
            .join(id)
            .join("ticket.md");
        let text = fs::read_to_string(&file).unwrap();
        let line = text.lines().find(|l| l.starts_with("labels = ")).unwrap();
        fs::write(&file, text.replace(line, &format!("labels = {labels:?}"))).unwrap();
    }

    /// A person moves `id` to `stage`.
    fn move_to(&self, id: &str, stage: &str) {
        MarkdownTracker::new(&self.remote.checkout)
            .set_stage(&TicketId::new(id).unwrap(), stage)
            .unwrap();
    }
}

/// Scenario S12, "The human keeps control", with the exclusion label
/// `manual`. DEMO-2 carries it from the start: never taken in. DEMO-1,
/// taken in, gets it from a person: never dispatched, told once. DEMO-3
/// lacks the admission label, DEMO-4's gesture is a stranger's, and a
/// person moves DEMO-6 out of the column before its turn: none gets a
/// comment, a stage or a ticket ref. A person moves DEMO-5 out once it is
/// taken in: nothing more is written on it.
#[test]
fn scenario_s12_the_human_keeps_control() {
    let bench = Bench::build_admitting(
        "maintainer",
        "assignee = \"maintainer\"\nlabels = [\"agent\"]\n",
        "",
        ", parked = \"Parked\"",
        "admit = { label = \"agent\" }\nadmitters = [\"maintainer\"]\nexclude = \"manual\"",
    );
    let admitted = ["agent"];
    bench.put_ticket(
        "DEMO-2",
        "maintainer",
        "Todo",
        &["agent", "manual"],
        "",
        "Mine.",
    );
    bench.put_ticket("DEMO-3", "maintainer", "Todo", &[], "", "Not for agents.");
    bench.put_ticket("DEMO-4", "stranger", "Todo", &admitted, "", "Please do it.");
    bench.put_ticket(
        "DEMO-5",
        "maintainer",
        "Todo",
        &admitted,
        "",
        "Greet twice.",
    );
    bench.put_ticket(
        "DEMO-6",
        "maintainer",
        "Todo",
        &admitted,
        "",
        "Greet thrice.",
    );
    let untouched = ["DEMO-2", "DEMO-3", "DEMO-4", "DEMO-6"];
    let kept = RefCell::new(Vec::new());
    let intake = || bench.reply(None, Some(&intake_result("done", None, "")));
    let (outcomes, lines, printed) = bench.drain(vec![intake(), intake()], 4, |pass| match pass {
        1 => {
            bench.label(TICKET, &["agent", "manual"]);
            bench.move_to("DEMO-6", "In Progress");
            for id in untouched {
                kept.borrow_mut().push(bench.on_tracker(id));
            }
        }
        2 => {
            bench.move_to("DEMO-5", "In Progress");
            kept.borrow_mut().push(bench.on_tracker("DEMO-5"));
        }
        _ => {}
    });
    assert_eq!(outcomes.len(), 2, "{outcomes:?}\n{lines}\n{printed}");
    assert_eq!(
        bench.runs(),
        [
            (Role::Intake, "DEMO-1".to_owned()),
            (Role::Intake, "DEMO-5".to_owned()),
        ]
    );
    let events = bench.events();
    assert!(dispatched(&events).is_empty(), "{lines}");

    // Nothing written on the tickets Owlshift did not take, nor after a
    // person took one back.
    let kept = kept.borrow();
    for (id, before) in untouched.iter().chain(&["DEMO-5"]).zip(kept.iter()) {
        assert_eq!(&bench.on_tracker(id), before, "{id}");
    }
    for id in untouched {
        assert!(bench.on_tracker(id).1.is_empty(), "{id}");
        assert!(bench.record_of(id).is_none(), "{id}");
    }
    assert_eq!(bench.shown("DEMO-5").stage, "In Progress");
    // Only the two intakes wrote on the tracker, and only their comments.
    let written: Vec<(String, Value)> = events
        .iter()
        .filter(|e| e.kind == EventKind::TrackerWrite)
        .map(|e| {
            (
                e.ticket.as_ref().unwrap().as_str().to_owned(),
                e.data["kind"].clone(),
            )
        })
        .collect();
    assert_eq!(
        written,
        [
            ("DEMO-1".to_owned(), json!("INTAKE")),
            ("DEMO-5".to_owned(), json!("INTAKE")),
        ]
    );
    assert_eq!(bench.stage(), "Todo");

    // Each exclusion told once, in watch's log only; the stranger warned of
    // once.
    let excluded: Vec<_> = waits(&events)
        .into_iter()
        .filter(|(_, reason, _)| *reason == json!("excluded"))
        .collect();
    assert_eq!(
        excluded,
        [
            ("DEMO-2".to_owned(), json!("excluded"), Value::Null),
            ("DEMO-1".to_owned(), json!("excluded"), Value::Null),
        ],
        "{lines}"
    );
    let warned: Vec<(String, Value)> = events
        .iter()
        .filter(|e| e.kind == EventKind::Warning && e.data["what"] == json!("not_admitted"))
        .map(|e| {
            (
                e.ticket.as_ref().unwrap().as_str().to_owned(),
                e.data["account"].clone(),
            )
        })
        .collect();
    assert_eq!(warned, [("DEMO-4".to_owned(), json!("stranger"))]);
}

/// A Build `done` delivering DEMO-1 with seven follow-ups: one that needs
/// DEMO-1's code; one titled as an open ticket; one titled as another
/// parent's declined proposal; two more; the first again in other words of
/// the same fingerprint; and one past the three per parent.
fn proposing() -> String {
    let followup = |title: &str, why: &str, source: &str| json!({ "title": title, "why": why, "source": source, "done_when": format!("{title}: done.") });
    let mut first = followup("Translate the greeting", "Readers abroad.", "agent");
    first["evidence"] = json!(["GREETING.md:1"]);
    first["blocked_by_parent"] = json!(true);
    let mut result: Value = serde_json::from_str(DONE).unwrap();
    result["followups"] = json!([
        first,
        followup("Lint the docs", "Typos slip in.", "reviewer"),
        followup("Cache the greeting", "It is read often.", "agent"),
        followup(
            "Document the greeting",
            "Nobody knows it is there.",
            "reviewer"
        ),
        followup("translate the greeting!", "Readers abroad, again.", "agent"),
        followup("Test the greeting", "It could break unseen.", "ci"),
        followup("Style the greeting", "It looks plain.", "agent"),
    ]);
    result.to_string()
}

/// Scenario S13, "Proposed follow-ups", through the Markdown tracker's own
/// inbox. DEMO-1's delivery files its follow-ups as tickets at `Triage`,
/// authored `owlshift`, related to DEMO-1 and blocked by it when they need
/// its code, each standing on its own; a duplicate of an open ticket or of
/// a declined proposal is not filed, a follow-up given twice is filed once,
/// and past three the rest are only named in the DELIVERY report. No
/// proposal is taken in at `Triage`, at `Backlog`, nor in the column while
/// `owlshift` is its author; once a person makes DEMO-9 theirs, it is, and
/// waits for DEMO-1.
#[test]
fn scenario_s13_proposed_follow_ups() {
    let bench = Bench::draining("maintainer");
    bench.inbox.borrow_mut().markdown = true;
    let tickets = bench.remote.checkout.join("tickets");
    for (id, front, description) in [
        (
            "DEMO-7",
            "title = \"Lint the docs\"\nauthor = \"maintainer\"\nstage = \"Todo\"\n",
            "A person's own ticket.".to_owned(),
        ),
        (
            "DEMO-8",
            "title = \"Cache the greeting\"\nauthor = \"owlshift\"\nstage = \"Canceled\"\n",
            format!(
                "Declined.\n\n[owlshift] PROPOSAL · DEMO-5 · fingerprint {}",
                owlshift_runner::proposals::fingerprint("Cache the greeting")
            ),
        ),
    ] {
        fs::create_dir_all(tickets.join(id)).unwrap();
        fs::write(
            tickets.join(id).join("ticket.md"),
            format!("+++\n{front}+++\n\n{description}\n"),
        )
        .unwrap();
    }
    let proposals = ["DEMO-9", "DEMO-10", "DEMO-11"];
    let filed = RefCell::new(Vec::new());
    let left = RefCell::new(Vec::new());
    let replies = vec![
        bench.reply(None, Some(&intake_result("done", None, ""))),
        bench.reply(Some("Hello"), Some(&proposing())),
        bench.reply(None, Some(&intake_result("done", None, ""))),
    ];
    let ends = RefCell::new(Vec::new());
    let file_9 = tickets.join("DEMO-9").join("ticket.md");
    let rewrite = |from: &str, to: &str| {
        let text = fs::read_to_string(&file_9).unwrap();
        assert!(text.contains(from), "{text}");
        fs::write(&file_9, text.replace(from, to)).unwrap();
    };
    let (outcomes, lines, printed) = bench.drain(replies, 6, |pass| {
        ends.borrow_mut().push(bench.events().len());
        match pass {
            2 => {
                for id in proposals {
                    filed.borrow_mut().push(bench.shown(id));
                }
            }
            3 => {
                // DEMO-9 put in the column by a person who leaves it
                // Owlshift's; DEMO-11 accepted into the backlog.
                bench.move_to("DEMO-9", "Todo");
                rewrite(
                    "author = \"owlshift\"\n",
                    "author = \"owlshift\"\nassignee = \"maintainer\"\nlabels = [\"agent\"]\n",
                );
                bench.move_to("DEMO-11", "Backlog");
                for id in &proposals[1..] {
                    left.borrow_mut().push(bench.on_tracker(id));
                }
            }
            4 => {
                // The person makes it theirs, and watch's hold has passed.
                rewrite("author = \"owlshift\"", "author = \"maintainer\"");
                let later = bench.clock.get().checked_add(SignedDuration::from_mins(11));
                bench.clock.set(later.unwrap());
            }
            _ => {}
        }
    });
    assert_eq!(outcomes.len(), 3, "{outcomes:?}\n{lines}\n{printed}");
    assert!(outcomes[1].is_ok(), "the delivery: {outcomes:?}");

    // Filed at Triage, each standing on its own; nothing past three.
    let filed = filed.borrow();
    for (proposal, (title, blocked)) in filed.iter().zip([
        ("Translate the greeting", true),
        ("Document the greeting", false),
        ("Test the greeting", false),
    ]) {
        let parent = vec![ticket()];
        assert_eq!(proposal.title, title);
        assert_eq!(
            (proposal.stage.as_str(), proposal.author.as_str()),
            ("Triage", "owlshift")
        );
        assert_eq!(proposal.related, parent, "{title}");
        assert_eq!(proposal.blocked_by, if blocked { parent } else { vec![] });
        assert!(proposal.labels.is_empty() && proposal.assignee.is_none());
        for part in [
            "**Why:** ",
            "**Found by:** ",
            "**Done when:** ",
            "**Proposed by** Owlshift's delivery run on DEMO-1",
        ] {
            assert!(
                proposal.description.contains(part),
                "{part}\n{}",
                proposal.description
            );
        }
        let footer = format!(
            "[owlshift] PROPOSAL · DEMO-1 · fingerprint {}",
            owlshift_runner::proposals::fingerprint(title)
        );
        assert!(proposal.description.trim_end().ends_with(&footer));
    }
    assert!(filed[0].description.contains("`GREETING.md:1`"));
    assert!(!tickets.join("DEMO-12").exists());
    let id = |s: &str| TicketId::new(s).unwrap();
    assert_eq!(
        proposal_outcomes(&bench),
        [
            ProposalOutcome::Filed {
                ticket: id("DEMO-9")
            },
            ProposalOutcome::Duplicate {
                ticket: id("DEMO-7")
            },
            ProposalOutcome::Duplicate {
                ticket: id("DEMO-8")
            },
            ProposalOutcome::Filed {
                ticket: id("DEMO-10")
            },
            ProposalOutcome::Filed {
                ticket: id("DEMO-11")
            },
            ProposalOutcome::OverLimit,
        ]
    );
    let report = bench
        .comments()
        .into_iter()
        .find(|c| c.starts_with("[owlshift] DELIVERY"))
        .unwrap();
    for line in [
        "- Proposed follow-up **Translate the greeting**, filed as DEMO-9 for triage: Readers abroad.",
        "- Proposed follow-up **Lint the docs**, not filed, already proposed as DEMO-7: Typos slip in.",
        "- Proposed follow-up **Cache the greeting**, not filed, already proposed as DEMO-8: It is \
         read often.",
        "- Proposed follow-up **translate the greeting!**, filed as DEMO-9 for triage: Readers \
         abroad, again.",
        "- Proposed follow-up **Style the greeting**, not filed past the three per ticket: It \
         looks plain.",
    ] {
        assert!(report.contains(line), "{line}\n---\n{report}");
    }

    // No proposal taken in before a person made it theirs.
    let events = bench.events();
    let passes = by_pass(&events, 0, &ends.borrow());
    assert!(
        passes[2..4]
            .iter()
            .all(|pass| pass.iter().all(|e| e.kind != EventKind::RunStarted))
    );
    let warned: Vec<(String, Value)> = passes[3]
        .iter()
        .filter(|e| e.kind == EventKind::Warning && e.data["what"] == json!("not_admitted"))
        .map(|e| {
            (
                e.ticket.as_ref().unwrap().as_str().to_owned(),
                e.data["account"].clone(),
            )
        })
        .collect();
    assert_eq!(warned, [("DEMO-9".to_owned(), json!("owlshift"))]);
    assert_eq!(
        bench.runs(),
        [
            (Role::Intake, "DEMO-1".to_owned()),
            (Role::Build, "DEMO-1".to_owned()),
            (Role::Verify, "DEMO-1".to_owned()),
            (Role::Intake, "DEMO-9".to_owned()),
        ]
    );
    let record = bench.record_of("DEMO-9").unwrap().intake.unwrap();
    assert_eq!(record.gesture.account, "maintainer");
    assert!(
        waits(&passes[5]).contains(&("DEMO-9".to_owned(), json!("blocked_by"), json!(["DEMO-1"]))),
        "{lines}"
    );
    // The others are left as the person left them.
    for (id, before) in proposals[1..].iter().zip(left.borrow().iter()) {
        assert_eq!(&bench.on_tracker(id), before, "{id}");
        assert!(bench.record_of(id).is_none(), "{id}");
    }
}

impl Bench {
    /// What the quarantine told `id` so far (OWL-368): the operator's
    /// notifications, the `waits` events for the quarantine and the WAITING
    /// comments on `id`.
    fn quarantine_told(&self, id: &str) -> (usize, usize, usize) {
        let waits = self
            .events()
            .iter()
            .filter(|e| {
                e.kind == EventKind::Decision
                    && e.ticket.as_ref().is_some_and(|t| t.as_str() == id)
                    && e.data.get("reason") == Some(&json!("quarantine"))
            })
            .count();
        let comments = self
            .comments_on(id)
            .iter()
            .filter(|c| c.starts_with("[owlshift] WAITING\n"))
            .count();
        (self.notified.0.borrow().len(), waits, comments)
    }

    /// `owlshift unquarantine`, the project's name typed.
    pub(super) fn unquarantine(&self) -> quarantine::Cleared {
        let mut out = Vec::new();
        let mut sink = EventSink::new(REPO, EventLog::in_dir(&self.data), &mut out);
        let mut ask = |_: &str| Ok(format!("{REPO}\n"));
        let env = self.env.clone();
        let git = Git::with_setup("git", move |command| env.apply(command));
        quarantine::clear(&git, &self.dirs(), &mut ask, &mut sink).unwrap()
    }
}

/// OWL-368's acceptance, under `watch --drain`. DEMO-1's intake asks its
/// decider; DEMO-2, taken in, is dispatched, and its Build writes in the
/// main checkout. The runner notifies the quarantine once, as it writes the
/// marker, in fixed words, and the park adds nothing; DEMO-2's PARKED
/// comment says that the whole project is refused. The decider's answer on
/// DEMO-1 then counts: two quarantined passes tell it once, one event and
/// one WAITING comment, and notify nothing more, and what `owlshift doctor`
/// reads names the run and the breach. Once `owlshift unquarantine` clears
/// it, the next pass continues DEMO-1 and builds it.
#[test]
fn a_quarantine_is_told_once_to_the_operator_and_to_the_ticket_it_holds() {
    let bench = Bench::draining("maintainer");
    bench.put_ticket(
        "DEMO-2",
        "maintainer",
        "Todo",
        &["agent"],
        "",
        "Greet once more.",
    );
    let question = r#"[{"id":"Q1","category":"scope",
        "context":"The ticket does not say the greeting's language.",
        "text":"Which language?","recommendation":{"why":"The simplest reading of the ticket."}}]"#;
    // DEMO-2, parked, keeps what it holds (OWL-403): its zone and its file
    // are not DEMO-1's.
    let mut breach = bench.reply(None, Some(&done_for("DEMO-2")));
    breach.files.insert(
        RelativePath::new("docs/again.md").unwrap(),
        "Hello again.\n".to_owned(),
    );
    breach.commit = Some("Greet again".to_owned());
    breach
        .main_checkout
        .insert(RelativePath::new("planted.txt").unwrap(), "x".to_owned());
    let replies = vec![
        bench.reply(None, Some(&intake_result("questions", None, question))),
        bench.reply(None, Some(&intake_declaring(&["docs"], &[]))),
        breach,
        bench.reply(None, Some(&check(&[("Q1", "answered", "English.")]))),
        bench.reply(None, Some(&intake_declaring(&["app"], &[]))),
        bench.reply(Some("Hello"), Some(&done_for("DEMO-1"))),
    ];
    let (outcomes, lines, printed) = bench.drain(replies, 6, |pass| match pass {
        // DEMO-2's Build broke isolation: notified once; DEMO-1 waits.
        3 => {
            assert_eq!(bench.quarantine_told("DEMO-1"), (1, 0, 0));
            bench.answer("Q1: English, please.");
        }
        // Told once, over two passes, and nothing more notified.
        4 => assert_eq!(bench.quarantine_told("DEMO-1"), (1, 1, 1)),
        5 => {
            assert_eq!(bench.quarantine_told("DEMO-1"), (1, 1, 1));
            let state = quarantine::state(&bench.dirs()).unwrap();
            let Some((_, text)) = state.quarantine else {
                panic!("{state:?}");
            };
            assert!(text.contains("of DEMO-2 broke isolation"), "{text}");
            assert!(text.contains("planted.txt"), "{text}");
            assert!(matches!(
                bench.unquarantine(),
                quarantine::Cleared::Cleared { .. }
            ));
        }
        _ => {}
    });
    let notified = bench.notified.0.borrow().clone();
    assert_eq!(notified.len(), 1, "{notified:?}\n{lines}\n{printed}");
    assert!(
        notified[0].starts_with(&format!("{REPO} is quarantined: run ")),
        "{notified:?}"
    );
    assert!(
        notified[0].contains(
            " of DEMO-2 broke isolation (the main checkout changed). No ticket of the project \
             runs until a person clears it with owlshift unquarantine"
        ),
        "{notified:?}"
    );
    assert!(!notified[0].contains("planted"), "{notified:?}");
    // The breach's park notifies nothing more from the command's outcome.
    let parked: Vec<&Stop> = outcomes
        .iter()
        .filter_map(|outcome| outcome.as_ref().err())
        .filter(|stop| {
            matches!(
                stop,
                Stop::Parked {
                    reason: ParkReason::IsolationBreach,
                    ..
                } | Stop::NotKept {
                    landed: Landed::Quarantined,
                    ..
                }
            )
        })
        .collect();
    assert_eq!(parked.len(), 1, "{outcomes:?}");
    let demo_2 = TicketId::new("DEMO-2").unwrap();
    assert_eq!(notify::blocker_line(&demo_2, parked[0]), None);
    let park = bench
        .comments_on("DEMO-2")
        .into_iter()
        .find(|c| c.starts_with("[owlshift] PARKED"))
        .expect("DEMO-2's PARKED comment");
    assert!(
        park.contains("Owlshift refuses the whole project on that machine"),
        "{park}"
    );
    assert!(park.contains("`owlshift unquarantine`"), "{park}");
    // Cleared, DEMO-1 is continued and built; DEMO-2 stays parked.
    assert_eq!(bench.dirs().quarantine().unwrap(), None);
    assert!(
        !bench.dirs().obligations_stand(),
        "cleared with the quarantine"
    );
    assert!(outcomes.last().unwrap().is_ok(), "{outcomes:?}\n{lines}");
    assert_eq!(
        bench.runs(),
        [
            (Role::Intake, "DEMO-1".to_owned()),
            (Role::Intake, "DEMO-2".to_owned()),
            (Role::Build, "DEMO-2".to_owned()),
            (Role::AnswerCheck, "DEMO-1".to_owned()),
            (Role::Intake, "DEMO-1".to_owned()),
            (Role::Build, "DEMO-1".to_owned()),
            (Role::Verify, "DEMO-1".to_owned()),
        ]
    );
    assert_eq!(bench.quarantine_told("DEMO-1"), (1, 1, 1));
    assert!(
        lines.contains("watch: DEMO-1 waits for the quarantine to be cleared"),
        "{lines}"
    );
}

/// A marker no command notified, as a cut-off run leaves, found by `watch
/// --drain` (OWL-368): its first pass notifies it, and the ticket the drain
/// would dispatch is told once, over two passes, that it waits; nothing is
/// built. The quarantined checkout is only read: a file-system monitor
/// planted in its configuration never runs, and no ref of it moves.
#[test]
fn a_cut_off_run_is_notified_once_and_the_ready_ticket_told_once() {
    let bench = Bench::draining("maintainer");
    let checkout = bench.dirs().checkout();
    let fired = bench.tmp.path().join("fsmonitor-fired");
    let refs = || bench.env.run(&checkout, &["for-each-ref"]).unwrap();
    let before = RefCell::new(Vec::new());
    let replies = vec![bench.reply(None, Some(&intake_result("done", None, "")))];
    let (outcomes, lines, printed) = bench.drain(replies, 3, |pass| {
        if pass == 1 {
            let monitor = bench.tmp.path().join("fsmonitor.sh");
            fs::write(&monitor, format!("#!/bin/sh\n: > '{}'\n", fired.display())).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&monitor, fs::Permissions::from_mode(0o755)).unwrap();
            }
            let monitor = monitor.to_string_lossy().into_owned();
            bench
                .env
                .run(&checkout, &["config", "core.fsmonitor", &monitor])
                .unwrap();
            fs::write(
                bench.dirs().quarantine_file(),
                "Run r1 of DEMO-1, started at 2026-10-19T00:00:00Z, ended before its isolation \
                 check passed (a crash or a stop): nobody checked what it changed.\n",
            )
            .unwrap();
            *before.borrow_mut() = refs();
        } else {
            assert_eq!(bench.quarantine_told(TICKET), (1, 1, 1), "{pass}");
        }
    });
    assert_eq!(outcomes.len(), 1, "{outcomes:?}\n{lines}\n{printed}");
    assert_eq!(
        bench.notified.0.borrow()[..],
        [quarantine::found_line(REPO)]
    );
    let told: Vec<(String, Value, Value)> = waits(&bench.events())
        .into_iter()
        .filter(|(_, reason, _)| *reason == json!("quarantine"))
        .collect();
    assert_eq!(
        told,
        [(TICKET.to_owned(), json!("quarantine"), Value::Null)],
        "{lines}"
    );
    let comment = bench
        .comments_on(TICKET)
        .into_iter()
        .find(|c| c.starts_with("[owlshift] WAITING"))
        .unwrap();
    assert!(comment.contains("This ticket is ready"), "{comment}");
    assert!(dispatched(&bench.events()).is_empty());
    assert_eq!(bench.runs(), [(Role::Intake, TICKET.to_owned())]);
    assert!(!fired.exists(), "the planted file-system monitor ran");
    assert_eq!(refs(), *before.borrow());
}
