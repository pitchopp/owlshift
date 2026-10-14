//! Intake (OWL-248; architecture section 5, "Intake" and "Back to the ready
//! column"): the intake role reads an admitted ticket, its thread, its
//! decider and the project's rules, read-only and without network, and ends
//! `done` or `questions`, each with its record: the variant, the routing
//! verdict and the dependencies it found. The runner keeps the record in the
//! ticket ref (`intake.json`, with the text's hash and the admission
//! gesture) and shows it in an INTAKE comment. Questions go through the
//! resolver, then to the decider in a round whose return stage is Ready; the
//! record is kept with the round, so answering does not run intake again.
//! Before a verdict is kept, its dependencies become `blocks` relations on
//! the tracker, or say why not (OWL-249, "Dependencies to relations").
//! An intake that ends at Ready moves the ticket back to the ready column
//! while the tracker still shows the state the runner set.
//!
//! The drain (part G) calls [`OnDemand::intake`] with the gesture it checked;
//! `continue` runs intake again on a ticket at Intake.

use std::collections::HashMap;

use owlshift_adapters::tracker::{ErrorKind, Gesture as TrackerGesture, StateType};
use owlshift_contracts::config::Admit;
use owlshift_contracts::drain::{DependencyOutcome, Gesture, GestureKind, KeptDependency};
use owlshift_contracts::result::IntakeVerdict;
use owlshift_core::dependency::{self, Node};

use super::*;
use crate::ticket_ref::text_sha256;
use crate::writer::IntakeComment;

/// The admission gesture of `ticket` as the drain records it (architecture
/// section 5, "By whom"): the account the tracker reads as having made
/// `admit`, which must be a person's account listed in `admitters`, matched
/// on the tracker's identifier exactly. An app user, a bot or an
/// integration never admits, nor does anyone when `admitters` is absent. A
/// tracker that keeps no history gives no time: the gesture's time is then
/// `now`, when it was read.
pub fn admission(
    tracker: &dyn Tracker,
    admit: &Admit,
    admitters: Option<&[String]>,
    ticket: &TicketId,
    now: Timestamp,
) -> Result<Gesture, String> {
    let (kind, name) = match admit {
        Admit::Label(label) => (GestureKind::Label, label),
        Admit::State(state) => (GestureKind::State, state),
        Admit::Delegation => {
            return Err(
                "the `delegation` admission gesture is not built: admit with a label or a state"
                    .to_owned(),
            );
        }
    };
    let Some(admitters) = admitters else {
        return Err(
            "`[tracker].admitters` in owlshift.toml lists no account, so no gesture admits"
                .to_owned(),
        );
    };
    let TrackerGesture { account, at } = tracker
        .gesture(ticket, admit)
        .map_err(|e| format!("reading the admission gesture of {ticket}: {e}"))?;
    let account = match account {
        TrackerAuthor::Account(person) => person.id,
        TrackerAuthor::Other { name } => {
            return Err(format!(
                "the admission gesture on {ticket} was made by {name:?}, not a person's account: \
                 only an account `[tracker].admitters` lists admits"
            ));
        }
    };
    if !admitters.contains(&account) {
        return Err(format!(
            "the admission gesture on {ticket} was made by {account:?}, whom \
             `[tracker].admitters` does not list"
        ));
    }
    Ok(Gesture {
        kind,
        name: name.clone(),
        account,
        at: at.unwrap_or(now),
    })
}

/// Why a result's dependencies are refused (OWL-248): each must name its
/// ticket in its quote, and the ticket's text, its title, description or a
/// comment of its thread, must name it too. The text is the source of a
/// dependency, never the model: an identifier the text does not hold is a
/// guess, or an instruction the text smuggled in. `None` when every one is
/// founded.
fn unfounded(verdict: &IntakeVerdict, found: &Ticket, comments: &[Comment]) -> Option<String> {
    let in_text = |id: &str| {
        found.title.contains(id)
            || found.description.contains(id)
            || comments.iter().any(|comment| comment.body.contains(id))
    };
    verdict.dependencies.iter().find_map(|dependency| {
        let id = dependency.ticket.as_str();
        if !dependency.quote.contains(id) {
            Some(format!("the quote of dependency {id} does not name it"))
        } else if !in_text(id) {
            Some(format!(
                "dependency {id} is named nowhere in the ticket or its thread"
            ))
        } else {
            None
        }
    })
}

impl OnDemand<'_> {
    /// Runs intake on `ticket` for the drain, `gesture` being the admission
    /// the drain checked ([`admission`]). The ticket ref must be absent, at
    /// Intake, at Ready, or at Watch with its following ended; the ticket is
    /// put back at Intake keeping its round count. Returns the record kept
    /// once intake ends at Ready; a round of questions, a park or anything
    /// else that stops it is the stop.
    pub fn intake(
        &self,
        ticket: &TicketId,
        gesture: Gesture,
        sink: &mut EventSink<'_>,
    ) -> Result<TicketIntake, Stop> {
        let mut p = self.prepare(ticket, Command::Intake)?;
        let round = match &p.stored {
            None => 0,
            Some(stored) => {
                let state = TicketState::try_from(&stored.record.state).map_err(core_error)?;
                let ended = stored
                    .record
                    .delivery
                    .as_ref()
                    .is_none_or(|delivery| !delivery.followed());
                match state.status() {
                    Status::Active(Stage::Intake | Stage::Ready) => {}
                    Status::Active(Stage::Watch) if ended => {}
                    status => {
                        return Err(Stop::Refused(format!(
                            "intake runs on a ticket at Intake, at Ready, or at Watch once its \
                             pull request is no longer followed; {ticket} is {status:?}"
                        )));
                    }
                }
                state.round()
            }
        };
        let state = TicketState::restore(Status::Active(Stage::Intake), round, 0, 0, 0, 0)
            .map_err(core_error)?;
        self.run_intake(&mut p, state, gesture, sink)
    }

    /// `continue` on a ticket at Intake: intake runs again, under the kept
    /// gesture while the record holds the ticket's current text, else under
    /// the gesture read again ([`admission`]). Ends with the stop that says
    /// where the ticket is.
    pub(super) fn intake_again(
        &self,
        p: &mut Prepared,
        state: TicketState,
        sink: &mut EventSink<'_>,
    ) -> Stop {
        let text = text_sha256(&p.found.title, &p.found.description);
        let kept = p
            .intake
            .as_ref()
            .filter(|record| record.text_sha256 == text)
            .map(|record| record.gesture.clone());
        let gesture = match kept {
            Some(gesture) => gesture,
            None => {
                let admitters =
                    match project::admitters_at(&self.executor.git, &p.checkout, &p.base) {
                        Ok(admitters) => admitters,
                        Err(why) => return Stop::Refused(why),
                    };
                match admission(
                    self.tracker,
                    &self.config.tracker.admit,
                    admitters.as_deref(),
                    &p.ticket,
                    (self.clock)(),
                ) {
                    Ok(gesture) => gesture,
                    Err(why) => return Stop::Refused(why),
                }
            }
        };
        match self.run_intake(p, state, gesture, sink) {
            Ok(_) => Stop::Ready {
                ticket: p.ticket.clone(),
            },
            Err(stop) => stop,
        }
    }

    /// Runs the intake role from `state`, at Intake, until it ends at Ready
    /// or stops. A failed run, a refused result among them, gets one more
    /// run before the core parks the ticket; `blocked` and a breach park it;
    /// a usage limit stops it at Intake. Questions go through the resolver
    /// ([`Self::resolve`]): all decided, intake runs again; what is left
    /// opens a round, the record kept with it.
    fn run_intake(
        &self,
        p: &mut Prepared,
        mut state: TicketState,
        gesture: Gesture,
        sink: &mut EventSink<'_>,
    ) -> Result<TicketIntake, Stop> {
        // The drain checked the gesture; the record names who admitted, so
        // the account is checked again against the base's admitters.
        let admitters = project::admitters_at(&self.executor.git, &p.checkout, &p.base)
            .map_err(Stop::Refused)?;
        if !admitters
            .as_deref()
            .is_some_and(|admitters| admitters.contains(&gesture.account))
        {
            return Err(Stop::Refused(format!(
                "the admission gesture of {} names {:?}, whom `[tracker].admitters` on {} does \
                 not list",
                p.ticket, gesture.account, p.base.remote_ref
            )));
        }
        let current = person(p.current()?, &p.found);
        let rules =
            rules::project_rules(&self.executor.git, &p.checkout, &p.base, &self.config.stack)
                .map_err(|e| refused("the project's rules", e))?;
        // The text the briefs hold, read once: an edit made while intake
        // runs is a text intake did not read, which the next pass takes in.
        let text = text_sha256(&p.found.title, &p.found.description);
        let mut attempt = 0u32;
        let mut settled = 0u32;
        loop {
            attempt += 1;
            let comments = self.comments(&p.ticket)?;
            let brief = self.brief(p, Role::Intake, &comments, &rules, None, &current);
            let (ran, mut event) = self.run_role_once(
                p,
                self.intake,
                &state,
                &brief,
                attempt,
                &mut Gathered::default(),
                sink,
            )?;
            let mut result = ran.result();
            if let Some(found) = result {
                // A false premise waits for P5 (S8), and a dependency the
                // ticket's text does not name is refused: either is a failed
                // run.
                let refusal = if found.status == result::Status::PremiseFalse {
                    Some("intake records no false premise before P5".to_owned())
                } else {
                    found
                        .intake
                        .as_ref()
                        .and_then(|verdict| unfounded(verdict, &p.found, &comments))
                };
                if let Some(reason) = refusal {
                    sink.emit(
                        &p.ticket,
                        Some(&ran.run),
                        EventKind::Warning,
                        data([("what", json!("intake_refused")), ("reason", json!(reason))]),
                    );
                    event = Event::RunFailed;
                    result = None;
                }
            }
            let verdict = result.and_then(|result| result.intake.as_ref());
            if event == Event::Questions
                && let (Some(result), Some(verdict)) = (result, verdict)
            {
                match self.resolve(
                    p,
                    &state,
                    result,
                    &[],
                    &rules,
                    &current,
                    settled,
                    &ran.run,
                    sink,
                )? {
                    Routing::Settled => {
                        settled += 1;
                        continue;
                    }
                    // The verdict is kept with the round: its dependencies
                    // become relations now, and a failure is a failed run.
                    Routing::Ask(round) => match self.relate_dependencies(p, verdict, &ran, sink) {
                        Ok(dependencies) => {
                            let record = self.keep_intake(
                                p,
                                &text,
                                &gesture,
                                verdict,
                                dependencies,
                                &ran,
                                sink,
                            )?;
                            let asked = match state.apply(PIPELINE, Event::Questions) {
                                Ok(Transition::To(asked)) => asked,
                                other => return Err(core_error(other)),
                            };
                            p.intake = Some(record);
                            return Err(self.ask(
                                p,
                                &asked,
                                result.status,
                                &result.summary,
                                round,
                                &ran.run,
                                sink,
                            ));
                        }
                        Err(()) => event = Event::RunFailed,
                    },
                }
            }
            // A `done` verdict is kept: its dependencies become relations
            // first, and a failure is a failed run.
            let mut dependencies = None;
            if event == Event::Completed
                && let Some(verdict) = verdict
            {
                match self.relate_dependencies(p, verdict, &ran, sink) {
                    Ok(related) => dependencies = Some(related),
                    Err(()) => event = Event::RunFailed,
                }
            }
            match state.apply(PIPELINE, event) {
                Ok(Transition::To(next)) => state = next,
                Ok(Transition::Parked {
                    state: parked,
                    reason,
                }) => {
                    // The park makes the ticket's ref when it has none, so
                    // `continue` restarts intake.
                    let questions = p.questions();
                    return Err(self.park(
                        p,
                        &parked,
                        reason,
                        &ran,
                        Some(questions),
                        Vec::new(),
                        sink,
                    ));
                }
                other => return Err(core_error(other)),
            }
            match state.status() {
                Status::Active(Stage::Intake) => {
                    self.keep(p, &state)?;
                    if let Outcome::UsageLimit { resets_at } = ran.report.outcome {
                        return Err(Stop::UsageLimit { resets_at });
                    }
                }
                Status::Active(Stage::Ready) => {
                    let (Some(verdict), Some(dependencies)) = (verdict, dependencies) else {
                        return Err(Stop::Refused(
                            "an intake run done without its record".to_owned(),
                        ));
                    };
                    let record =
                        self.keep_intake(p, &text, &gesture, verdict, dependencies, &ran, sink)?;
                    p.intake = Some(record.clone());
                    let questions = p.questions();
                    if let Err(error) = self.store(p, &state, questions) {
                        return Err(Stop::NotKept {
                            landed: Landed::Intake,
                            message: format!(
                                "the INTAKE comment of {} is on the ticket, but keeping its \
                                 record in the ticket's ref failed: {error}; once the ref can be \
                                 written, the drain runs intake again",
                                p.ticket
                            ),
                        });
                    }
                    // Only once the record is kept: a ticket back in the
                    // column is one the drain dispatches.
                    self.back_to_ready(&p.ticket, Some(&ran.run), sink);
                    return Ok(record);
                }
                _ => return Err(core_error(event)),
            }
        }
    }

    /// The record of an intake run whose `verdict` is accepted, with its
    /// `dependencies` as [`Self::relate_dependencies`] left them, posted as
    /// the INTAKE comment; the caller keeps it.
    #[allow(clippy::too_many_arguments)]
    fn keep_intake(
        &self,
        p: &Prepared,
        text: &str,
        gesture: &Gesture,
        verdict: &IntakeVerdict,
        dependencies: Vec<KeptDependency>,
        ran: &Ran,
        sink: &mut EventSink<'_>,
    ) -> Result<TicketIntake, Stop> {
        let record = TicketIntake {
            format: Format,
            text_sha256: text.to_owned(),
            gesture: gesture.clone(),
            variant: verdict.variant,
            routing: verdict.routing,
            dependencies,
        };
        record
            .validate()
            .map_err(|e| refused("the intake record", e))?;
        sink.emit(
            &p.ticket,
            Some(&ran.run),
            EventKind::Gate,
            data([
                ("intake", json!("recorded")),
                ("variant", json!(record.variant)),
                ("routing", json!(record.routing)),
                ("dependencies", json!(record.dependencies.len())),
            ]),
        );
        let comment = IntakeComment {
            ticket: p.ticket.clone(),
            variant: record.variant,
            routing: record.routing,
            dependencies: record.dependencies.clone(),
            run: Some(ran.run.clone()),
        };
        let posted = Writer::new(self.tracker)
            .post_intake(&comment)
            .map_err(|e| refused("posting the INTAKE comment on the ticket", e))?;
        sink.emit(
            &p.ticket,
            Some(&ran.run),
            EventKind::TrackerWrite,
            comment_written("INTAKE", &posted),
        );
        Ok(record)
    }

    /// Turns the dependencies of the `verdict` intake keeps into relations
    /// (OWL-249, architecture section 5, "Dependencies to relations"): each
    /// one, once, gets the outcome [`dependency::check`] gives on the
    /// tracker's reads, and a `blocks` relation from it to the ticket,
    /// written through the Writer, when the outcome is to write one, with a
    /// `tracker_write` event. A read or a write that fails otherwise than
    /// with the tracker's "not found" stops the check with a `warning`
    /// (`relations_not_written`): the caller counts the run failed. The
    /// relations written before it stay, and the next run finds them there.
    fn relate_dependencies(
        &self,
        p: &Prepared,
        verdict: &IntakeVerdict,
        ran: &Ran,
        sink: &mut EventSink<'_>,
    ) -> Result<Vec<KeptDependency>, ()> {
        let related = self.related(&p.ticket, verdict, ran, sink);
        related.map_err(|reason| {
            sink.emit(
                &p.ticket,
                Some(&ran.run),
                EventKind::Warning,
                data([
                    ("what", json!("relations_not_written")),
                    ("reason", json!(reason)),
                ]),
            );
        })
    }

    /// [`Self::relate_dependencies`], failing with the reason.
    fn related(
        &self,
        ticket: &TicketId,
        verdict: &IntakeVerdict,
        ran: &Ran,
        sink: &mut EventSink<'_>,
    ) -> Result<Vec<KeptDependency>, String> {
        let mut read = Reads::new(self.tracker);
        let own = self
            .tracker
            .relations(ticket)
            .map_err(|e| format!("reading the relations of {ticket}: {e}"))?;
        let blockers: Vec<TicketId> = own.blockers.iter().map(|b| b.id.clone()).collect();
        let mut kept: Vec<KeptDependency> = Vec::new();
        for named in &verdict.dependencies {
            if kept.iter().any(|known| known.ticket == named.ticket) {
                continue;
            }
            let outcome = dependency::check(&own.id, &blockers, &named.ticket, |id| read.node(id))?;
            let outcome = match outcome {
                dependency::Outcome::Write => {
                    // The tracker's own identifier, as the check compared it.
                    let blocker = read.id(&named.ticket);
                    Writer::new(self.tracker)
                        .relate(&blocker, &own.id)
                        .map_err(|e| format!("writing that {blocker} blocks {}: {e}", own.id))?;
                    sink.emit(
                        ticket,
                        Some(&ran.run),
                        EventKind::TrackerWrite,
                        data([
                            ("target", json!("tracker")),
                            ("action", json!("relation")),
                            ("type", json!("blocks")),
                            ("from", json!(blocker.as_str())),
                            ("to", json!(own.id.as_str())),
                        ]),
                    );
                    DependencyOutcome::Written
                }
                dependency::Outcome::Existed => DependencyOutcome::Existed,
                dependency::Outcome::Done => DependencyOutcome::Done,
                dependency::Outcome::Unknown => DependencyOutcome::Unknown,
                dependency::Outcome::Itself => DependencyOutcome::Itself,
                dependency::Outcome::Cycle => DependencyOutcome::Cycle,
            };
            kept.push(KeptDependency {
                ticket: named.ticket.clone(),
                quote: named.quote.clone(),
                outcome,
            });
        }
        Ok(kept)
    }

    /// Moves the ticket back to the ready column after an intake that ends
    /// at Ready (architecture section 5, "Back to the ready column"), only
    /// while the tracker still shows the state the runner set, needs input
    /// or parked: a ticket already in the column gets no write, and a stage
    /// a person chose meanwhile is kept. A stage read or move that fails is a
    /// `stage_not_moved` warning; `continue` on the ticket makes the move
    /// again.
    pub(super) fn back_to_ready(
        &self,
        ticket: &TicketId,
        run: Option<&str>,
        sink: &mut EventSink<'_>,
    ) {
        let states = &self.config.tracker.states;
        let moved = self
            .tracker
            .stage(ticket)
            .map_err(|e| format!("reading its stage: {e}"))
            .and_then(|shown| {
                let set_by_runner = [VisibleStage::NeedsInput, VisibleStage::Parked]
                    .iter()
                    .any(|stage| stage.state(states) == shown);
                if set_by_runner && shown != states.ready {
                    self.move_stage(ticket, run, VisibleStage::Ready, sink)
                } else {
                    Ok(())
                }
            });
        if let Err(reason) = moved {
            sink.emit(
                ticket,
                run,
                EventKind::Warning,
                data([
                    ("what", json!("stage_not_moved")),
                    ("stage", json!(VisibleStage::Ready.key())),
                    ("state", json!(VisibleStage::Ready.state(states))),
                    ("reason", json!(reason)),
                ]),
            );
        }
    }
}

/// The tickets the dependency check read, each read once over all the
/// dependencies of a verdict.
struct Reads<'t> {
    tracker: &'t dyn Tracker,
    nodes: HashMap<TicketId, Option<Node<TicketId>>>,
}

impl<'t> Reads<'t> {
    fn new(tracker: &'t dyn Tracker) -> Self {
        Self {
            tracker,
            nodes: HashMap::new(),
        }
    }

    /// The ticket `id` as the check walks it: what blocks it by relation or
    /// by its "Blocked by" line, and whether it is done; `None` when the
    /// tracker answered it knows no such ticket.
    fn node(&mut self, id: &TicketId) -> Result<Option<Node<TicketId>>, String> {
        if let Some(node) = self.nodes.get(id) {
            return Ok(node.clone());
        }
        let node = match self.tracker.relations(id) {
            Ok(relations) => {
                let blockers = dependency::blockers_of(
                    relations.id.as_str(),
                    relations.blockers.iter().map(|b| b.id.as_str()),
                    &relations.description,
                )
                .into_iter()
                .filter_map(|id| TicketId::new(id).ok())
                .collect();
                Some(Node {
                    done: relations.state == Some(StateType::Completed),
                    blockers,
                    id: relations.id,
                })
            }
            Err(e) if e.kind == ErrorKind::NotFound => None,
            Err(e) => return Err(format!("reading {id}: {e}")),
        };
        self.nodes.insert(id.clone(), node.clone());
        Ok(node)
    }

    /// The tracker's own identifier of `id`, once read.
    fn id(&self, id: &TicketId) -> TicketId {
        match self.nodes.get(id) {
            Some(Some(node)) => node.id.clone(),
            _ => id.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use owlshift_adapters::tracker::{Capability, Error as TrackerError, ErrorKind};

    use super::*;

    /// A tracker whose admission gesture was made by `account`.
    struct Gestured(TrackerAuthor, Option<Timestamp>);

    impl Tracker for Gestured {
        fn capabilities(&self) -> &'static [Capability] {
            &[]
        }
        fn ticket(&self, _: &TicketId) -> Result<Ticket, TrackerError> {
            Err(TrackerError::new(ErrorKind::Other, "unused"))
        }
        fn comments(&self, _: &TicketId) -> Result<Vec<Comment>, TrackerError> {
            Err(TrackerError::new(ErrorKind::Other, "unused"))
        }
        fn post_comment(&self, _: &TicketId, _: &str) -> Result<Comment, TrackerError> {
            Err(TrackerError::new(ErrorKind::Other, "unused"))
        }
        fn set_stage(&self, _: &TicketId, _: &str) -> Result<(), TrackerError> {
            Err(TrackerError::new(ErrorKind::Other, "unused"))
        }
        fn gesture(&self, _: &TicketId, _: &Admit) -> Result<TrackerGesture, TrackerError> {
            Ok(TrackerGesture {
                account: self.0.clone(),
                at: self.1,
            })
        }
    }

    fn account(id: &str, name: &str) -> TrackerAuthor {
        TrackerAuthor::Account(Person {
            id: id.to_owned(),
            name: name.to_owned(),
        })
    }

    #[test]
    fn only_a_listed_account_admits_matched_on_its_identifier() {
        let ticket = TicketId::new("OWL-1").unwrap();
        let label = Admit::Label("agent".to_owned());
        let listed = ["u-ann".to_owned()];
        let now: Timestamp = "2026-10-13T10:00:00Z".parse().unwrap();
        let at: Timestamp = "2026-10-13T08:00:00Z".parse().unwrap();
        let admit = |tracker: &Gestured, admitters: Option<&[String]>| {
            admission(tracker, &label, admitters, &ticket, now)
        };

        let gesture = admit(&Gestured(account("u-ann", "Ann"), Some(at)), Some(&listed)).unwrap();
        assert_eq!(
            (
                gesture.kind,
                gesture.name.as_str(),
                gesture.account.as_str(),
                gesture.at
            ),
            (GestureKind::Label, "agent", "u-ann", at)
        );
        // A tracker that keeps no history: the time it was read.
        let gesture = admit(&Gestured(account("u-ann", "Ann"), None), Some(&listed)).unwrap();
        assert_eq!(gesture.at, now);

        for (tracker, admitters, why) in [
            (
                Gestured(account("u-bob", "u-ann"), Some(at)),
                Some(&listed[..]),
                "does not list",
            ),
            (
                Gestured(
                    TrackerAuthor::Other {
                        name: "u-ann".to_owned(),
                    },
                    Some(at),
                ),
                Some(&listed[..]),
                "not a person's account",
            ),
            (
                Gestured(account("u-ann", "Ann"), Some(at)),
                None,
                "lists no account",
            ),
        ] {
            let error = admit(&tracker, admitters).unwrap_err();
            assert!(error.contains(why), "{why}: {error}");
        }
        let delegation = admission(
            &Gestured(account("u-ann", "Ann"), Some(at)),
            &Admit::Delegation,
            Some(&listed),
            &ticket,
            now,
        );
        assert!(delegation.unwrap_err().contains("not built"));
    }
}
