//! The push of an agent's commits with closing keywords in their messages
//! broken (OWL-558; build plan, "The GitHub Issues adapter", Delivery): a
//! delivery and every repair push the neutral image of the commits they
//! bring ([`crate::neutral_commits`]), never rewriting a commit on the base
//! or one Owlshift pushed before, and the ticket's branch follows the image,
//! so that the next run, its lease and its checks see one history.

use owlshift_platform::confined::read_confined;

use super::*;
use crate::neutral_commits::{self, Neutral};

/// A push event's details, its `commit` the image `neutral` pushed, told
/// the gated commit `built` and how many commits were rewritten when any
/// was: so the events say why the pushed id is not the gate's.
pub(super) fn tell(event: &mut Data, built: &CommitId, neutral: &Neutral) {
    if !neutral.rewritten.is_empty() {
        event.insert("built".to_owned(), json!(built.as_str()));
        event.insert("neutralised".to_owned(), json!(neutral.rewritten.len()));
    }
}

impl OnDemand<'_> {
    /// The neutral image of `commit`, the commit a push of `p`'s branch
    /// would send. Its bases are the base commit `p` was dispatched on, which
    /// must be in the checkout, and `also` (the base's head on the remote at
    /// the push, the lease, a rebase's base), each when the checkout has it:
    /// commits the runner read itself, never a remote-tracking ref, which a
    /// run can move (OWL-51). When a commit was rewritten, the Build ledger's
    /// ids follow, then the ticket's branch moves from `commit` to the image
    /// through the run file, both before the caller keeps the push under
    /// way: a push made again after a crash computes the same image and
    /// finds the branch there. A branch at neither is refused, nothing
    /// pushed.
    pub(super) fn neutral_push(
        &self,
        p: &Prepared,
        commit: &CommitId,
        also: &[&str],
        sink: &mut EventSink<'_>,
    ) -> Result<Neutral, Stop> {
        let failed =
            |why: String| Stop::Delivery(format!("making {commit}'s messages safe to push: {why}"));
        let bases = self.push_bases(p, also).map_err(failed)?;
        let git = &self.executor.git;
        let neutral =
            neutral_commits::neutral_history(git, &p.checkout, commit, &bases).map_err(failed)?;
        if neutral.rewritten.is_empty() {
            return Ok(neutral);
        }
        let image = neutral.head.commit().as_str();
        let head = self.read_branch_head(p).map_err(failed)?;
        if head.as_deref() != Some(image) && head.as_deref() != Some(commit.as_str()) {
            return Err(failed(format!(
                "the ticket's branch {} is at {}, neither at that commit nor at its image {image}",
                p.branch,
                head.as_deref().unwrap_or("no commit")
            )));
        }
        self.follow_ledger(p, &neutral, sink);
        if head.as_deref() == Some(commit.as_str()) {
            let name = format!("refs/heads/{}", p.branch);
            ticket_run::move_branch(
                git,
                self.dirs,
                &p.checkout,
                &p._ticket_lock,
                &p.ticket,
                Some(image),
                || {
                    git.run(
                        &p.checkout,
                        &[
                            "update-ref",
                            "-m",
                            "owlshift: closing keywords broken (OWL-558)",
                            &name,
                            image,
                            commit.as_str(),
                        ],
                    )
                },
            )
            .map_err(|fault| self.run_fault(&p.ticket, fault))?
            .map_err(|e| failed(format!("moving {name} to {image}: {e}")))?;
        }
        Ok(neutral)
    }

    /// The bases of a push of `p`'s branch ([`Self::neutral_push`]).
    fn push_bases(&self, p: &Prepared, also: &[&str]) -> Result<Vec<CommitId>, String> {
        let resolve = |name: &str| {
            let read = self
                .executor
                .git
                .output(
                    &p.checkout,
                    &[
                        "rev-parse",
                        "--verify",
                        "--quiet",
                        "--end-of-options",
                        &format!("{name}^{{commit}}"),
                    ],
                    None,
                )
                .ok()?;
            let id = String::from_utf8_lossy(&read.stdout).trim().to_owned();
            read.success().then(|| CommitId::new(id).ok()).flatten()
        };
        let dispatched = resolve(&p.base.commit).ok_or_else(|| {
            format!(
                "{}, the base commit the ticket was dispatched on, is not in Owlshift's checkout",
                p.base.commit
            )
        })?;
        let mut bases = vec![dispatched];
        for &name in also {
            if let Some(id) = resolve(name)
                && !bases.contains(&id)
            {
                bases.push(id);
            }
        }
        Ok(bases)
    }

    /// The Build ledger's commit ids made those of `neutral`'s images
    /// ([`neutral_commits::remap_ledger`]), through no link; a ledger that
    /// does not read changes nothing, and one that cannot be written is
    /// warned of: the next Build run may then take a step for not done.
    fn follow_ledger(&self, p: &Prepared, neutral: &Neutral, sink: &mut EventSink<'_>) {
        let path = format!("{RUN_DIR}/ledger.json");
        let Some(ledger) = read_confined(&p.worktree, &path, crate::artifact::MAX_ARTIFACT_BYTES)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
        else {
            return;
        };
        let Some(followed) = neutral_commits::remap_ledger(&ledger, &neutral.rewritten) else {
            return;
        };
        if let Err(error) = crate::executor::write_run_file(&p.worktree, &path, followed.as_bytes())
        {
            sink.emit(
                &p.ticket,
                None,
                EventKind::Warning,
                data([
                    ("what", json!("ledger_unfollowed")),
                    ("reason", json!(error.to_string())),
                ]),
            );
        }
    }

    /// The image of the kept Build's `commit` that a delivery of `p` pushes,
    /// computed without writing anything: where the branch is once that
    /// delivery moved it ([`Self::neutral_push`]).
    pub(super) fn neutral_image(&self, p: &Prepared, commit: &CommitId) -> Option<CommitId> {
        let bases = self.push_bases(p, &[]).ok()?;
        neutral_commits::image(&self.executor.git, &p.checkout, commit, &bases).ok()
    }
}
