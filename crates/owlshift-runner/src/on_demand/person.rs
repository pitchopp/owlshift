//! A person's steps at Watch (OWL-619; architecture section 4, "A person's
//! steps", *At delivery*): the steps of the plan only a person can do,
//! due before the merge, asked of the ticket's decider in one round at
//! Watch, and the run that follows their answers.
//!
//! - *The round.* [`OnDemand::person_steps`], from watch's step
//!   ([`OnDemand::person_steps_seen`]) and from `continue` at Watch, on a
//!   read of the pull request that asks for no repair: the runner's own
//!   round, through no resolver, one question a step under
//!   [`PERSON_STEP`], which is always the decider's; `plan.json` keeps the
//!   steps it asked (`checks`) in the write that keeps the round. Merge on
//!   green waits for it, as for any question left to a person.
//! - *The answers.* Read by the answer check, as any round's; once the
//!   round is answered, [`OnDemand::settle_person_steps`] keeps each in the
//!   RESUME's write: the option [`PASSED`] (a thumbs-up's included) is the
//!   step's result as the plan names it, anything else a defect.
//! - *The run.* The next read that asks for no repair gives the answers to
//!   one run of the build role at Watch (decided by the maintainer on
//!   2026-11-22): it fixes what each defect reports, and builds the agent's
//!   steps owed until the person's steps before them passed. Its push is a
//!   correction's, landed with `Corrected` (no fix pass, no repair; its
//!   failed runs bound it); once it lands, or once a run given defects
//!   alone found nothing to change, the steps answered as a defect are
//!   asked again and the agent's steps it built leave `owed`
//!   ([`TicketPlan::settle_steps_run`]). A step answered as passed is never
//!   asked again.

use super::*;
use owlshift_contracts::brief::{AnsweredStep, MAX_REPLY_BYTES, PersonStepsBrief};
use owlshift_contracts::result::{QuestionOption, Recommendation};
use owlshift_contracts::review::{PlanStep, StepAnswer, StepsRun};
use owlshift_core::gate::PERSON_STEP;

/// The option of a person's step's question that says the step gave the
/// result the plan names, the recommendation.
const PASSED: &str = "A";

/// Whether `pushing` is the run after a person's steps': a correction's
/// push whose report names steps (OWL-619).
pub(crate) fn steps_push(pushing: &Pushing) -> bool {
    pushing.kind == PushKind::Correction
        && pushing
            .report
            .as_ref()
            .is_some_and(|report| !report.steps.is_empty())
}

/// What a ticket whose ref keeps `record` owes its person's steps at
/// Watch, read from the ref alone (OWL-619): a run (its push under way
/// included) or a round, `None` when it owes neither.
pub(crate) fn steps_due(record: &TicketRecord) -> Option<StepsDue> {
    let plan = record.plan.as_ref()?;
    let under_way = record
        .delivery
        .as_ref()
        .and_then(|delivery| delivery.pushing.as_ref())
        .is_some_and(steps_push);
    if under_way || plan.steps_run().is_some() {
        return Some(StepsDue::Run);
    }
    (!plan.steps_to_ask().is_empty()).then_some(StepsDue::Ask)
}

/// What a ticket's person's steps owe at Watch ([`steps_due`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StepsDue {
    /// The round that asks the decider.
    Ask,
    /// The run that follows the answers, a model's.
    Run,
}

impl OnDemand<'_> {
    /// A person's steps for `owlshift watch`: run only while the ticket ref,
    /// read under the project's lock, is still `seen`, the commit watch read
    /// it at, and keeps a ticket at Watch. `None`, with nothing asked, run,
    /// kept or recorded, when the project is busy, or the ref moved or no
    /// longer keeps a ticket at Watch: the next pass reads it again.
    pub fn person_steps_seen(
        &self,
        ticket: &TicketId,
        seen: &str,
        following: &mut Following,
        sink: &mut EventSink<'_>,
    ) -> Option<Result<Delivered, Stop>> {
        let mut p = match self.prepare_at_watch(ticket, seen, Command::Fix, sink)? {
            Ok(p) => p,
            Err(stop) => return Some(Err(stop)),
        };
        Some(self.person_steps(&mut p, following, sink))
    }

    /// The person's steps of a ticket at Watch, under the project's lock,
    /// after the fetch: the pull request read again, and only a read that
    /// asks for no repair goes on (green, checks running, or a red the
    /// base's), anything else told as a look; then the ticket's stage
    /// ([`repairs_in`]). The run after the answers comes first (a push of
    /// its with no verdict pushed again without a run), else the round that
    /// asks the steps due, else a look. Its end moves a stage its run left
    /// at working back to review ([`Self::back_from_repair`]).
    pub(super) fn person_steps(
        &self,
        p: &mut Prepared,
        following: &mut Following,
        sink: &mut EventSink<'_>,
    ) -> Result<Delivered, Stop> {
        let outcome = self.person_steps_due(p, following, sink);
        self.back_from_repair(p, &outcome, sink);
        outcome
    }

    /// [`Self::person_steps`] before its end moves the stage.
    fn person_steps_due(
        &self,
        p: &mut Prepared,
        following: &mut Following,
        sink: &mut EventSink<'_>,
    ) -> Result<Delivered, Stop> {
        let ticket = p.ticket.clone();
        let stored = p
            .stored
            .clone()
            .ok_or_else(|| nothing_to_continue(&ticket))?;
        let delivery = stored
            .record
            .delivery
            .clone()
            .ok_or_else(|| nothing_to_continue(&ticket))?;
        let (Some(pr), Some(pushed)) = (delivery.pull_request.clone(), delivery.pushed.clone())
        else {
            return Err(nothing_to_continue(&ticket));
        };
        let state = TicketState::try_from(&stored.record.state).map_err(core_error)?;
        let fresh = follow::read(
            self.forge,
            &delivery,
            pr.number,
            &pushed,
            |_| None,
            None,
            (self.clock)(),
        )
        .map_err(|e| refused("reading the pull request again", e))?;
        let look = || Stop::Watched {
            ticket: ticket.clone(),
            seen: Box::new(fresh.seen(pr.number, &pr.url)),
            merge: None,
        };
        let due = steps_due(&stored.record);
        if due.is_none()
            || state.status() != Status::Active(Stage::Watch)
            || !fresh.decision.asks_no_repair()
        {
            return Err(look());
        }
        let stage = &self
            .tracker
            .stage(&ticket)
            .map_err(|e| refused(&format!("reading the stage of {ticket}"), e))?;
        if !repairs_in(stage, &self.config.tracker.states) {
            return Err(Stop::Refused(format!(
                "{ticket} shows {stage:?} on the tracker, which is not a state Owlshift works \
                 in: the steps for a person of pull request #{} wait while it stays there",
                pr.number
            )));
        }
        // A yielded ticket, restarted, is admitted again first (OWL-621).
        self.admit_yielded(p, &state, sink)?;
        match due {
            Some(StepsDue::Run) => {
                self.run_steps(p, state, &delivery, &pr, &pushed, following, sink)
            }
            Some(StepsDue::Ask) => Err(self.ask_steps(p, &state, &pr, &pushed, sink)),
            None => Err(look()),
        }
    }

    /// The round that asks the decider the person's steps due before the
    /// merge (OWL-619): one question a step, Q1 to Qn in the plan's order,
    /// kept as the plan's checks in the write that keeps the round. Returns
    /// the stop that says so, as [`Self::ask`] does.
    fn ask_steps(
        &self,
        p: &mut Prepared,
        state: &TicketState,
        pr: &DeliveredPullRequest,
        pushed: &str,
        sink: &mut EventSink<'_>,
    ) -> Stop {
        let Some(plan) = p.plan.as_ref() else {
            return Stop::Refused("the ticket's ref keeps no plan".to_owned());
        };
        let asked = plan.steps_to_ask();
        let questions = step_questions(plan, &asked, pr, pushed);
        let waiting = match state.apply(PIPELINE, Event::Questions) {
            Ok(Transition::To(waiting)) => waiting,
            other => return core_error(other),
        };
        let Some(round) = NonZeroU32::new(waiting.round()) else {
            return Stop::Refused("no question round is open".to_owned());
        };
        let ids: Vec<(NonZeroU32, QuestionId)> = asked
            .iter()
            .zip(&questions)
            .map(|((number, _), question)| (*number, question.id.clone()))
            .collect();
        if let Some(plan) = p.plan.as_mut() {
            plan.ask_steps(round, &ids);
        }
        let steps: Vec<u32> = ids.iter().map(|(number, _)| number.get()).collect();
        sink.emit(
            &p.ticket,
            None,
            EventKind::Decision,
            data([
                ("watch", json!("person_steps")),
                ("outcome", json!("ask")),
                ("steps", json!(steps)),
            ]),
        );
        let summary = format!(
            "Owlshift delivered pull request #{} and built the agent's steps of the plan. {} only \
             a person can do, and merge on green waits for {}: do {}, then answer with what {} \
             gave.",
            pr.number,
            match asked.len() {
                1 => "One step is".to_owned(),
                n => format!("{n} steps are"),
            },
            if asked.len() == 1 {
                "its result"
            } else {
                "their results"
            },
            if asked.len() == 1 { "it" } else { "each" },
            if asked.len() == 1 { "it" } else { "each" },
        );
        let round_of = Round {
            questions: questions
                .into_iter()
                .map(|question| (question.id.clone(), question))
                .collect(),
            decided: 0,
            fallback: None,
            reopened: Vec::new(),
            labels: BTreeMap::new(),
        };
        self.ask(
            p,
            &waiting,
            result::Status::Questions,
            &summary,
            round_of,
            None,
            sink,
        )
    }

    /// Keeps, in the RESUME's write, how the decider answered the person's
    /// steps round `round` asked, read from the verdicts of every ask of
    /// the round, the latest on each question (OWL-619): the option
    /// [`PASSED`] is the step's result as the plan names it, any other
    /// answer a defect the next run at Watch follows.
    pub(super) fn settle_person_steps(
        &self,
        p: &mut Prepared,
        round: NonZeroU32,
        questions: &TicketQuestions,
        sink: &mut EventSink<'_>,
    ) {
        let Some(plan) = p.plan.as_mut() else {
            return;
        };
        if !plan.checks.iter().any(|check| check.round == round) {
            return;
        }
        let understood = answer_check::understood(questions.round_asks(round));
        let answer = |id: &QuestionId| {
            understood
                .iter()
                .find(|(question, _)| question.id == *id)
                .and_then(|(_, verdict)| verdict.as_ref())
                .filter(|verdict| verdict.class == AnswerClass::Answered)
                .map(|verdict| match verdict.option.as_deref() {
                    Some(PASSED) => StepAnswer::Passed,
                    _ => StepAnswer::Defect,
                })
        };
        if !plan.answer_round(round, answer) {
            return;
        }
        let answered = |kind: StepAnswer| -> Vec<u32> {
            plan.checks
                .iter()
                .filter(|check| check.round == round && check.answer == Some(kind))
                .map(|check| check.step.get())
                .collect()
        };
        sink.emit(
            &p.ticket,
            None,
            EventKind::Decision,
            data([
                ("watch", json!("person_steps")),
                ("outcome", json!("answered")),
                ("passed", json!(answered(StepAnswer::Passed))),
                ("defects", json!(answered(StepAnswer::Defect))),
            ]),
        );
    }

    /// The run of the build role at Watch after a person's steps were
    /// answered (OWL-619), on Owlshift's last push `pushed`: the answers
    /// and the agent's steps they release in its brief's `person_steps`. A
    /// push of its with no verdict is pushed again without a run. A `done`
    /// on a new commit is pushed as a correction's; a `done` on `pushed`
    /// found nothing to change, which only a run given defects alone may
    /// say: one given the agent's steps to build that commits nothing is a
    /// failed run.
    #[allow(clippy::too_many_arguments)]
    fn run_steps(
        &self,
        p: &mut Prepared,
        state: TicketState,
        delivery: &TicketDelivery,
        pr: &DeliveredPullRequest,
        pushed: &str,
        following: &mut Following,
        sink: &mut EventSink<'_>,
    ) -> Result<Delivered, Stop> {
        let pushed_id = CommitId::new(pushed).map_err(|e| refused("the pushed commit", e))?;
        let local = self.fix_branch(p, &pushed_id)?;
        if let Some(pushing) = delivery
            .pushing
            .as_ref()
            .filter(|pushing| steps_push(pushing))
            && local.as_deref() == Some(pushing.commit.as_str())
        {
            let commit = CommitId::new(pushing.commit.as_str())
                .map_err(|e| refused("the commit under way", e))?;
            let steps = pushing
                .report
                .as_ref()
                .map(|report| report.steps.clone())
                .unwrap_or_default();
            let (repaired, followups) = Repaired::again(commit, pushing, "correction");
            p.repairing = true;
            return self.push_steps(
                p, state, delivery, &pushed_id, repaired, steps, followups, following, sink,
            );
        }
        let run = p
            .plan
            .as_ref()
            .and_then(TicketPlan::steps_run)
            .ok_or_else(|| Stop::Refused("no run after a person's steps is due".to_owned()))?;
        let given = person_brief(&p.questions(), &run, pr, pushed)?;
        let mut steps: Vec<NonZeroU32> = run.defects.iter().chain(&run.build).copied().collect();
        steps.sort_unstable();
        let dispatched = self.dispatch(p, Command::Fix, sink)?;
        let harness = self.build.harness(dispatched.build);
        let build = |_: u32| &*harness;
        let decorate = |brief: &mut Brief, questions: &TicketQuestions| {
            brief.person_steps = Some(given.clone());
            tell_build(brief, questions);
        };
        let builds = !run.build.is_empty();
        let accept = |commit: &CommitId| {
            if builds && *commit == pushed_id {
                Err(format!(
                    "the run ended done without a commit on top of {pushed}, which was to build \
                     the agent's steps the person's results released"
                ))
            } else {
                Ok(())
            }
        };
        // A `blocked` on the base goes to the rebase run (OWL-634): the run
        // stays due, so the next pass runs it on the rebased head.
        let ran = self.repair_or_rebase(
            p,
            state,
            &RepairRun {
                role: Role::Build,
                what: "person's steps",
                pull_request: pr.number,
                rules: &dispatched.rules,
                harness: &build,
                before: &|_, _, _| Ok(()),
                decorate: &decorate,
                accept: &accept,
                cut: None,
                rebase: Some(&OnDelivery {
                    delivery,
                    pr,
                    pushed,
                }),
            },
            following,
            sink,
        )?;
        let (done, followups) = match ran {
            Repair::Done(done, followups) => (*done, followups),
            Repair::Rebased(delivered) => return Ok(delivered),
        };
        if done.repaired.commit == pushed_id {
            return self.steps_unchanged(
                p, delivery, &pushed_id, done, steps, followups, following, sink,
            );
        }
        self.push_steps(
            p,
            done.state,
            delivery,
            &pushed_id,
            done.repaired,
            steps,
            followups,
            following,
            sink,
        )
    }

    /// Pushes the run's commit as a correction's ([`Self::push_repair`]),
    /// which settles what it followed in the write that keeps it landed,
    /// and records the outcome once the ref says the push landed.
    #[allow(clippy::too_many_arguments)]
    fn push_steps(
        &self,
        p: &mut Prepared,
        state: TicketState,
        delivery: &TicketDelivery,
        pushed: &CommitId,
        repaired: Repaired,
        steps: Vec<NonZeroU32>,
        followups: Vec<Followup>,
        following: &mut Following,
        sink: &mut EventSink<'_>,
    ) -> Result<Delivered, Stop> {
        let commit = repaired.commit.to_string();
        let kind = RepairKind::Steps {
            steps: steps.clone(),
        };
        let outcome = self.push_repair(
            p, state, delivery, pushed, repaired, kind, followups, following, sink,
        );
        let landed = p
            .stored
            .as_ref()
            .and_then(|stored| stored.record.delivery.as_ref())
            .is_some_and(|kept| kept.pushed.as_deref() == Some(commit.as_str()));
        if landed {
            steps_event(p, "pushed", &steps, sink);
        }
        outcome
    }

    /// A run given defects alone that found nothing to change: `Corrected`,
    /// what it followed settled (its steps asked again), and a DELIVERY
    /// report saying so, owed from the write that keeps it, as a
    /// correction's that changed nothing is (OWL-236).
    #[allow(clippy::too_many_arguments)]
    fn steps_unchanged(
        &self,
        p: &mut Prepared,
        delivery: &TicketDelivery,
        pushed: &CommitId,
        done: RepairDone,
        steps: Vec<NonZeroU32>,
        followups: Vec<Followup>,
        following: &mut Following,
        sink: &mut EventSink<'_>,
    ) -> Result<Delivered, Stop> {
        let state = match done.state.apply(PIPELINE, Event::Corrected) {
            Ok(Transition::To(next)) => next,
            other => return Err(core_error(other)),
        };
        if let Some(plan) = p.plan.as_mut() {
            plan.settle_steps_run();
        }
        let owed = OwedReport {
            kind: PushKind::Correction,
            count: u32::try_from(steps.len().max(1)).unwrap_or(u32::MAX),
            report: Some(done.repaired.kept(
                &RepairKind::Steps {
                    steps: steps.clone(),
                },
                &followups,
            )),
            unchanged: true,
        };
        let questions = p.questions();
        let kept = self.owe_unchanged(
            p,
            delivery,
            &state,
            questions,
            owed,
            "keeping what the run after a person's steps settled",
            sink,
        )?;
        steps_event(p, "unchanged", &steps, sink);
        self.post_unchanged(
            p,
            kept,
            pushed,
            &state,
            "the run after a person's steps found nothing to change, and the steps answered as a \
             defect are asked again",
            following,
            sink,
        )
    }
}

/// The `decision` event of the run after a person's steps: `outcome`
/// `pushed` or `unchanged`, and the steps it followed.
fn steps_event(p: &Prepared, outcome: &str, steps: &[NonZeroU32], sink: &mut EventSink<'_>) {
    let steps: Vec<u32> = steps.iter().map(|step| step.get()).collect();
    sink.emit(
        &p.ticket,
        None,
        EventKind::Decision,
        data([
            ("watch", json!("person_steps")),
            ("outcome", json!(outcome)),
            ("steps", json!(steps)),
        ]),
    );
}

/// One question a step of `asked`, Q1 to Qn in their order: what the
/// person does and the result the plan names, with the agent's steps of
/// `plan` that wait on it; option [`PASSED`] gives that result, the other
/// says what it gave instead.
fn step_questions(
    plan: &TicketPlan,
    asked: &[(NonZeroU32, PlanStep)],
    pr: &DeliveredPullRequest,
    pushed: &str,
) -> Vec<Question> {
    // A step the resolver handed to a person is one (OWL-620).
    let effective = plan.effective();
    asked
        .iter()
        .enumerate()
        .map(|(n, (number, step))| {
            let waiting: Vec<String> = plan
                .owed
                .iter()
                .filter(|owed| {
                    effective
                        .person_steps()
                        .take_while(|(person, _)| person < *owed)
                        .last()
                        .is_some_and(|(person, _)| person == *number)
                })
                .map(ToString::to_string)
                .collect();
            let waits = match waiting.as_slice() {
                [] => String::new(),
                [one] => format!(
                    " The agent's step {one} waits on it: Owlshift's next run at Watch builds it \
                     from your answer."
                ),
                many => format!(
                    " The agent's steps {} wait on it: Owlshift's next run at Watch builds them \
                     from your answer.",
                    many.join(", ")
                ),
            };
            Question {
                id: QuestionId::nth(u32::try_from(n + 1).unwrap_or(u32::MAX)),
                category: PERSON_STEP.to_owned(),
                context: format!(
                    "No run can do step {number}, a person's, and merge on green waits for its \
                     result.{waits} Do it on pull request #{} at head `{}`.",
                    pr.number,
                    crate::base::short(pushed),
                ),
                text: format!(
                    "Step {number}: {} What did it give? The result the plan names: {}",
                    step.what.trim(),
                    step.test.trim()
                ),
                options: vec![
                    QuestionOption {
                        label: "It gave that result".to_owned(),
                        detail: Some(step.test.trim().to_owned()),
                    },
                    QuestionOption {
                        label: "Something else".to_owned(),
                        detail: Some(
                            "Say what it gave in your reply: Owlshift's next run at Watch works \
                             from your words, then asks this step again."
                                .to_owned(),
                        ),
                    },
                ],
                recommendation: Some(Recommendation {
                    option: Some(PASSED.to_owned()),
                    why: "Choose it once you did the step and it gave the result the plan \
                          names; a thumbs-up answers so."
                        .to_owned(),
                }),
                replan: false,
            }
        })
        .collect()
}

/// The brief's `person_steps` of `run` (OWL-619): each answered step with
/// the question its round asked and the answer check's reading of the
/// decider's answer, from `questions`, and the agent's steps to build.
fn person_brief(
    questions: &TicketQuestions,
    run: &StepsRun,
    pr: &DeliveredPullRequest,
    pushed: &str,
) -> Result<PersonStepsBrief, Stop> {
    let answered: Vec<AnsweredStep> = run
        .answered
        .iter()
        .filter_map(|check| {
            let understood = answer_check::understood(questions.round_asks(check.round));
            let (question, verdict) = understood
                .into_iter()
                .find(|(question, _)| question.id == check.question)?;
            let reading = verdict.map_or_else(
                || "The answer check kept no reading of the answer.".to_owned(),
                |verdict| verdict.reason,
            );
            Some(AnsweredStep {
                step: check.step,
                round: check.round,
                question,
                defect: check.answer == Some(StepAnswer::Defect),
                reading: clip(&reading, MAX_REPLY_BYTES),
            })
        })
        .collect();
    if answered.is_empty() {
        return Err(Stop::Refused(
            "the ticket's questions keep none of the person's steps its plan says were answered"
                .to_owned(),
        ));
    }
    Ok(PersonStepsBrief {
        pull_request: pr.number,
        url: pr.url.clone(),
        head: pushed.to_owned(),
        answered,
        build: run.build.clone(),
    })
}

/// `text` cut to at most `max` bytes, on a character's boundary.
fn clip(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}
