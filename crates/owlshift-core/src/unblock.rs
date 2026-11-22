//! A `blocked` through the resolver (OWL-620; architecture section 4, "A
//! `blocked` through the resolver"): which verdicts a resolver run may give
//! on a run that stopped `blocked`.
//!
//! The runner still schedules from plain data: the resolver answers one
//! question about one ticket, with one verdict of a closed list, each an act
//! the runner already makes ([`UnblockVerdict`]). This module says which of
//! them are open; the runner makes the act.

use crate::vocab::{DelegationLevel, Stage, UnblockVerdict};

/// Where a run stopped `blocked`, and which acts its stage allows there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unblocking {
    /// The stage the run ran at: the bounds are a ticket's and a stage's.
    pub stage: Stage,
    /// Whether the branch can go to the rebase run: a Build's at Build, its
    /// branch not delivered, clean, and behind the base; since OWL-634 a
    /// fix's, a correction's or a person's results' run at Watch, the
    /// delivery behind the base and short of its repairs' bound.
    pub rebase: bool,
    /// Whether a step of the plan can be owed to a person: a Build's at
    /// Build, with a given plan that holds an agent's step, outside a review
    /// loop.
    pub person_step: bool,
}

/// Whether the project's delegation level sends a `blocked` to the resolver:
/// from `discoverable` up. Below it nothing changes, and the ticket parks.
pub fn delegated(level: DelegationLevel) -> bool {
    level >= DelegationLevel::Discoverable
}

/// The verdicts open to the resolver on a `blocked` at `at.stage`, in
/// [`UnblockVerdict::ALL`]'s order, given those the runner already took at
/// that stage of the ticket (`taken`). `retry` and `rebase` are taken once
/// each a ticket and stage, and a `blocked` after either parks: once one of
/// them was taken there, `park` alone is open. `park` is always open.
pub fn open(at: Unblocking, taken: &[UnblockVerdict]) -> Vec<UnblockVerdict> {
    if taken
        .iter()
        .any(|verdict| matches!(verdict, UnblockVerdict::Retry | UnblockVerdict::Rebase))
    {
        return vec![UnblockVerdict::Park];
    }
    UnblockVerdict::ALL
        .into_iter()
        .filter(|verdict| match verdict {
            UnblockVerdict::Retry | UnblockVerdict::Park => true,
            UnblockVerdict::Rebase => at.rebase,
            UnblockVerdict::PersonStep => at.person_step,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use UnblockVerdict::{Park, PersonStep, Rebase, Retry};

    const AT_BUILD: Unblocking = Unblocking {
        stage: Stage::Build,
        rebase: true,
        person_step: true,
    };

    /// Each verdict is open where its act is, `park` always; once `retry`
    /// or `rebase` was taken at the stage, `park` alone; a `person_step`
    /// closes nothing. At Watch, `rebase` is open where the delivery can be
    /// rebased (OWL-634), with the same bound.
    #[test]
    fn the_open_verdicts_follow_the_stage_and_what_was_taken() {
        const AT_WATCH: Unblocking = Unblocking {
            stage: Stage::Watch,
            rebase: true,
            person_step: false,
        };
        let cases: [(Unblocking, &[UnblockVerdict], &[UnblockVerdict]); 10] = [
            (AT_BUILD, &[], &[Retry, Rebase, PersonStep, Park]),
            (
                Unblocking {
                    rebase: false,
                    ..AT_BUILD
                },
                &[],
                &[Retry, PersonStep, Park],
            ),
            (
                Unblocking {
                    stage: Stage::Watch,
                    rebase: false,
                    person_step: false,
                },
                &[],
                &[Retry, Park],
            ),
            (AT_BUILD, &[Retry], &[Park]),
            (AT_BUILD, &[Rebase], &[Park]),
            (AT_BUILD, &[PersonStep], &[Retry, Rebase, PersonStep, Park]),
            (AT_BUILD, &[Park, PersonStep, Retry], &[Park]),
            (AT_WATCH, &[], &[Retry, Rebase, Park]),
            (AT_WATCH, &[Rebase], &[Park]),
            (AT_WATCH, &[Retry], &[Park]),
        ];
        for (at, taken, expected) in cases {
            assert_eq!(open(at, taken), expected, "{at:?} after {taken:?}");
        }
    }

    /// From `discoverable` up, a `blocked` goes to the resolver.
    #[test]
    fn a_blocked_reaches_the_resolver_from_discoverable_up() {
        let sent: Vec<bool> = DelegationLevel::ALL.into_iter().map(delegated).collect();
        assert_eq!(sent, [false, false, true, true, true]);
    }
}
