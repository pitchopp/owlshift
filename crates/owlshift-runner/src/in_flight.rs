//! The work in flight that Owlshift does not run (OWL-410; architecture
//! section 5, "Parallel", "The in-flight set", 2 and 3): the repository's
//! other open pull requests, by the files each changes, and people's tickets
//! in the working state, by their `zone:` labels. Owlshift's own open pull
//! requests' files join their tickets, since a person may push to them.
//!
//! The core's collision rule weighs each outside holder as a ticket whose
//! pull request is open ([`InFlight::delivered`]): it holds every ticket not
//! yet delivered and never waits itself, which is the rule against a
//! person's pull request or ticket. Holders are named here, never in the
//! core: a pull request by `#<number>`, which no ticket identifier can
//! start with, a person's ticket by its identifier.
//!
//! What cannot be read is never taken as free: a failed read of the open
//! pull requests, of one's files or of the working state is an error, and
//! the caller then starts nothing that the rule would check.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;

use serde_json::{Value, json};

use owlshift_adapters::forge::github::GitHubForge;
use owlshift_adapters::forge::{CommitId, OpenPullRequest, PullFiles};
use owlshift_adapters::tracker::{Author, Ticket, Tracker};
use owlshift_contracts::Priority;
use owlshift_contracts::config::ProjectConfig;
use owlshift_contracts::format::Format;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::step::{OutsidePull, OutsideTicket, StepOutside};
use owlshift_core::collision::{Collision, Dispatched, InFlight};
use owlshift_core::decider::declared_zones;
use owlshift_core::resource::{self, NamedResources, Resource};
use owlshift_core::schedule::Wait;

use crate::held;
use crate::ticket_ref::Stored;

/// The files of the pull requests read, by number, each kept with the head
/// commit, the base branch and the base commit it was read at: read again
/// when its pull request moved or was retargeted.
#[derive(Debug, Default)]
pub struct PullCache(HashMap<u64, (Key, PullFiles)>);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Key {
    head: CommitId,
    base_branch: String,
    base: CommitId,
}

impl Key {
    fn of(pull: &OpenPullRequest) -> Self {
        Self {
            head: pull.head.clone(),
            base_branch: pull.base_branch.clone(),
            base: pull.base.clone(),
        }
    }
}

/// An open pull request and the files it changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pull {
    pub number: u64,
    pub head_branch: String,
    pub files: PullFiles,
}

/// One read of the work outside Owlshift: the open pull requests with their
/// files, and the tickets of the working state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outside {
    pub pulls: Vec<Pull>,
    pub working: Vec<Ticket>,
}

impl Outside {
    /// This read as a pass hands it to the step of a dispatch it starts
    /// (OWL-401, `outside-<attempt>.json`).
    pub fn to_step(&self) -> StepOutside {
        StepOutside {
            format: Format,
            pulls: self
                .pulls
                .iter()
                .map(|pull| OutsidePull {
                    number: pull.number,
                    head_branch: pull.head_branch.clone(),
                    files: match &pull.files {
                        PullFiles::Listed(files) => Some(files.clone()),
                        PullFiles::Whole => None,
                    },
                })
                .collect(),
            working: self
                .working
                .iter()
                .map(|ticket| OutsideTicket {
                    id: ticket.id.clone(),
                    labels: ticket.labels.clone(),
                })
                .collect(),
        }
    }

    /// The read a pass handed to a step, as the collision rule reads it: a
    /// working ticket's identifier and labels alone.
    pub fn from_step(step: StepOutside) -> Self {
        Self {
            pulls: step
                .pulls
                .into_iter()
                .map(|pull| Pull {
                    number: pull.number,
                    head_branch: pull.head_branch,
                    files: pull.files.map_or(PullFiles::Whole, PullFiles::Listed),
                })
                .collect(),
            working: step
                .working
                .into_iter()
                .map(|ticket| Ticket {
                    id: ticket.id,
                    title: String::new(),
                    description: String::new(),
                    priority: Priority::Unset,
                    assignee: None,
                    labels: ticket.labels,
                    author: Author::unknown(),
                })
                .collect(),
        }
    }
}

/// Reads the repository's open pull requests and their files through
/// `forge`, the files from `cache` while a pull request has not moved, and
/// the tickets in `config`'s working state through `tracker`. An error, in
/// Owlshift's words with the forge's or tracker's, when any read fails.
pub fn read(
    forge: &GitHubForge,
    tracker: &dyn Tracker,
    config: &ProjectConfig,
    cache: &mut PullCache,
) -> Result<Outside, String> {
    let open = forge
        .open_pull_requests()
        .map_err(|e| format!("the open pull requests could not be read: {e}"))?;
    let listed: HashSet<u64> = open.iter().map(|pull| pull.number).collect();
    cache.0.retain(|number, _| listed.contains(number));
    let mut pulls = Vec::with_capacity(open.len());
    for pull in &open {
        let key = Key::of(pull);
        let files = match cache.0.get(&pull.number) {
            Some((kept, files)) if *kept == key => files.clone(),
            _ => {
                let files = forge.pull_request_files(pull).map_err(|e| {
                    format!(
                        "the files of pull request #{} could not be read: {e}",
                        pull.number
                    )
                })?;
                cache.0.insert(pull.number, (key, files.clone()));
                files
            }
        };
        pulls.push(Pull {
            number: pull.number,
            head_branch: pull.head_branch.clone(),
            files,
        });
    }
    let tracker_config = &config.tracker;
    let working = tracker
        .working(
            tracker_config.team.as_deref(),
            &tracker_config.states.working,
        )
        .map_err(|e| format!("the tickets in the working state could not be read: {e}"))?;
    Ok(Outside { pulls, working })
}

/// What holds a resource, as a reason names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// One of Owlshift's tickets of the project.
    Ticket,
    /// Another open pull request of the repository.
    PullRequest(u64),
    /// A ticket a person works on.
    Person,
}

/// A holder named for an event or a line.
#[derive(Clone, Copy, Debug)]
pub struct Named<'a> {
    pub id: &'a str,
    pub kind: Kind,
}

impl Named<'_> {
    /// The event's fields naming it: `tickets` with the identifier, and
    /// `person` for a person's ticket; `pull_request` with its number.
    pub fn data(&self) -> Vec<(&'static str, Value)> {
        match self.kind {
            Kind::Ticket => vec![("tickets", json!([self.id]))],
            Kind::Person => vec![("tickets", json!([self.id])), ("person", json!(true))],
            Kind::PullRequest(number) => vec![("pull_request", json!(number))],
        }
    }
}

impl fmt::Display for Named<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            Kind::Ticket => f.write_str(self.id),
            Kind::Person => write!(f, "{}, which a person works on,", self.id),
            Kind::PullRequest(number) => write!(f, "pull request #{number}"),
        }
    }
}

/// The outside holders of a pass, as the collision rule weighs them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Holders {
    /// The other open pull requests and the people's tickets with zones,
    /// each holding everything it changes or declares.
    pub others: Vec<InFlight<String>>,
    /// The files of each Owlshift ticket's own open pull request.
    pub own: HashMap<String, PullFiles>,
    /// How each of `others` is named.
    pub kinds: HashMap<String, Kind>,
    /// The people's tickets with neither a pull request nor a `zone:`
    /// label, left out: each is warned of (`zones_unknown`).
    pub unknown: Vec<TicketId>,
    /// The labels of each of Owlshift's own tickets in the working state,
    /// from which watch reads the `zone:` labels a Build's admission takes
    /// (OWL-448).
    pub labels: HashMap<String, Vec<String>>,
}

impl Holders {
    /// How the holder `id` is named: an Owlshift ticket unless it is one of
    /// `others`.
    pub fn named<'a>(&self, id: &'a str) -> Named<'a> {
        named(&self.kinds, id)
    }

    /// Adds to each of `in_flight`, the project's tickets, the files of its
    /// own open pull request, with the named resources of `named` they
    /// match; then appends the outside holders.
    pub fn join(&self, in_flight: &mut Vec<InFlight<String>>, named: &NamedResources) {
        for entry in in_flight.iter_mut() {
            if let Some(files) = self.own.get(&entry.id) {
                for resource in files_held(files, named) {
                    if !entry.held.contains(&resource) {
                        entry.held.push(resource);
                    }
                }
            }
        }
        in_flight.extend(self.others.iter().cloned());
    }
}

/// How the holder `id` is named, from the `kinds` of a pass's outside
/// holders.
pub fn named<'a>(kinds: &HashMap<String, Kind>, id: &'a str) -> Named<'a> {
    Named {
        id,
        kind: kinds.get(id).copied().unwrap_or(Kind::Ticket),
    }
}

impl Outside {
    /// The outside holders against the project's ticket refs `refs`, each
    /// with its ref, `None` for one that cannot be read; closed with
    /// `named`.
    ///
    /// A pull request is Owlshift's own when a ref that holds
    /// ([`held::holds`]) delivered it: its files join that ticket. Any other
    /// is another pull request, one a ticket delivered and no longer follows
    /// included. A ticket of the working state is a person's unless a ref
    /// that holds, or one that cannot be read, is its own; it holds its
    /// labels' zones, the whole repository for a malformed `zone:` label,
    /// and with no label it is counted only through a pull request whose
    /// head branch names it, or left out and listed in
    /// [`Holders::unknown`].
    pub fn holders<'a>(
        &self,
        refs: impl IntoIterator<Item = (&'a TicketId, Option<&'a Stored>)>,
        named: &NamedResources,
    ) -> Holders {
        let mut ours = HashSet::new();
        let mut own_pulls = HashMap::new();
        for (ticket, stored) in refs {
            match stored {
                Some(stored) if !held::holds(stored) => {}
                Some(stored) => {
                    ours.insert(ticket.as_str().to_owned());
                    let delivered = stored
                        .record
                        .delivery
                        .as_ref()
                        .and_then(|delivery| delivery.pull_request.as_ref());
                    if let Some(pull) = delivered {
                        own_pulls.insert(pull.number, ticket.as_str().to_owned());
                    }
                }
                None => {
                    ours.insert(ticket.as_str().to_owned());
                }
            }
        }
        let mut holders = Holders::default();
        for pull in &self.pulls {
            if let Some(ticket) = own_pulls.get(&pull.number) {
                holders.own.insert(ticket.clone(), pull.files.clone());
                continue;
            }
            let id = format!("#{}", pull.number);
            holders
                .kinds
                .insert(id.clone(), Kind::PullRequest(pull.number));
            holders
                .others
                .push(outside(id, files_held(&pull.files, named)));
        }
        for ticket in &self.working {
            let id = ticket.id.as_str();
            if ours.contains(id) {
                holders.labels.insert(id.to_owned(), ticket.labels.clone());
                continue;
            }
            let held = match declared_zones(ticket.labels.iter().map(String::as_str)) {
                Ok(zones) if zones.is_empty() => {
                    let has_pull = self
                        .pulls
                        .iter()
                        .any(|pull| branch_names(&pull.head_branch, id));
                    if !has_pull {
                        holders.unknown.push(ticket.id.clone());
                    }
                    continue;
                }
                Ok(zones) => {
                    let zones: Vec<String> = zones
                        .into_iter()
                        .filter_map(|zone| match zone {
                            Resource::Zone(zone) => Some(zone),
                            _ => None,
                        })
                        .collect();
                    resource::closed(&zones, &[], &[], named)
                }
                Err(_) => resource::closed(&[], &[], &[], named),
            };
            holders.kinds.insert(id.to_owned(), Kind::Person);
            holders.others.push(outside(id.to_owned(), held));
        }
        holders
    }
}

/// An outside holder: it holds every ticket not yet delivered and never
/// waits ([`InFlight::delivered`]), and is never an overlap's sibling
/// ([`InFlight::outside`], OWL-449).
fn outside(id: String, held: Vec<Resource>) -> InFlight<String> {
    InFlight {
        id,
        dispatched: Dispatched::Unrecorded,
        delivered: true,
        outside: true,
        held,
    }
}

/// What a pull request's files hold: each file, the zone of its own path,
/// and the named resources of `named` a file matches; none for no file.
/// [`PullFiles::Whole`] holds the whole repository.
pub fn files_held(files: &PullFiles, named: &NamedResources) -> Vec<Resource> {
    let files = match files {
        PullFiles::Listed(files) => files,
        PullFiles::Whole => return resource::closed(&[], &[], &[], named),
    };
    let mut held: Vec<Resource> = files
        .iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|file| Resource::Zone(file.clone()))
        .collect();
    for (name, pattern) in named.iter() {
        if files.iter().any(|file| pattern.matches(file)) {
            held.push(Resource::Named(name.to_owned()));
        }
    }
    held
}

/// Who holds what a collision names, as a reason says it: "DEMO-1 holds",
/// or under `rebase`, a path two tickets hold already, "DEMO-1 and DEMO-2
/// hold" (OWL-449); and who must let it go, the holder or either of them.
pub fn holding(kinds: &HashMap<String, Kind>, found: &Collision<String>) -> (String, String) {
    let holder = named(kinds, &found.holder);
    match &found.sharing {
        Some(sharing) => {
            let sharing = named(kinds, sharing);
            (
                format!("{holder} and {sharing} hold"),
                format!("{holder} or {sharing}"),
            )
        }
        None => (format!("{holder} holds"), holder.to_string()),
    }
}

/// The event's fields naming who holds what a collision names: the
/// holder's ([`Named::data`]), and under `rebase` both tickets sharing the
/// path in `tickets` (OWL-449).
pub fn holding_data(
    kinds: &HashMap<String, Kind>,
    found: &Collision<String>,
) -> Vec<(&'static str, Value)> {
    let mut fields = named(kinds, &found.holder).data();
    if found.sharing.is_some() {
        fields.retain(|(key, _)| *key != "tickets");
        fields.push(("tickets", json!(found.holders())));
    }
    fields
}

/// What a waiting reason says: on watch's line for a ticket of the drain,
/// and since OWL-401 in the refusal of a person's `do` or `continue` that
/// the admission holds back, in the words `owlshift why` uses. `exclude` is
/// the project's exclusion label, `kinds` how the outside holders are named.
pub fn said(wait: &Wait<String>, exclude: Option<&str>, kinds: &HashMap<String, Kind>) -> String {
    let list = |ids: &[String]| ids.join(", ");
    match wait {
        Wait::Duplicate => "the column lists it twice; the first is taken".to_owned(),
        Wait::NotAdmitted => {
            "its admission gesture is not from an account `[tracker].admitters` lists".to_owned()
        }
        Wait::Excluded => format!(
            "it carries the exclusion label {:?}",
            exclude.unwrap_or_default()
        ),
        Wait::NoDecider => {
            "it has no decider: give it an assignee, or a `zone:` label whose zone has an owner"
                .to_owned()
        }
        Wait::NoIntake => "its intake has not run yet".to_owned(),
        Wait::StaleIntake => "its text changed since its intake, which runs again".to_owned(),
        Wait::NotReady => {
            "it is being worked, waits for an answer, is parked, or its pull request is followed"
                .to_owned()
        }
        Wait::BlockerCanceled(ids) => format!(
            "its blocker {} was canceled: it waits until a person removes the relation or the \
             line",
            list(ids)
        ),
        Wait::BlockerUnknown(ids) => format!(
            "its \"Blocked by\" line names {}, which the tracker does not know: it waits until a \
             person corrects the line",
            list(ids)
        ),
        Wait::BlockedBy(ids) => format!("blocked by {}", list(ids)),
        Wait::PrCap => "the open pull requests reached their cap".to_owned(),
        Wait::Slot(ids) => format!(
            "the project's slot is held by {} (`owlshift doctor` says how to clear a stale one)",
            list(ids)
        ),
        Wait::Collision(found) => {
            let (holding, who) = holding(kinds, found);
            format!(
                "{holding} {}, which it would touch: it goes once {who} no longer holds it",
                found.resource
            )
        }
        Wait::RunCap => "every run of this machine is taken".to_owned(),
    }
}

/// Whether the branch `branch` names the ticket `id`: `id` in any ASCII
/// case, neither preceded nor followed by a letter or a digit, as
/// `owl-410-count` or `someone/OWL-410` name `OWL-410` and `owl-4100` does
/// not.
pub fn branch_names(branch: &str, id: &str) -> bool {
    let branch = branch.to_ascii_lowercase();
    let id = id.to_ascii_lowercase();
    if id.is_empty() {
        return false;
    }
    let bytes = branch.as_bytes();
    branch.match_indices(&id).any(|(at, _)| {
        let before = at.checked_sub(1).map(|i| bytes[i]);
        let after = bytes.get(at + id.len()).copied();
        !before.is_some_and(|b| b.is_ascii_alphanumeric())
            && !after.is_some_and(|b| b.is_ascii_alphanumeric())
    })
}

#[cfg(test)]
mod tests {
    use owlshift_adapters::tracker::{Author, Ticket};
    use owlshift_contracts::Priority;
    use owlshift_contracts::refs::{PersistedState, TicketDelivery};
    use owlshift_core::collision::collision;
    use owlshift_core::state::{Status, TicketState};
    use owlshift_core::vocab::Stage;

    use super::*;
    use crate::ticket_ref::blank;

    fn id(text: &str) -> TicketId {
        TicketId::new(text).unwrap()
    }

    fn zone(path: &str) -> Resource {
        Resource::Zone(path.to_owned())
    }

    fn person(text: &str, labels: &[&str]) -> Ticket {
        Ticket {
            id: id(text),
            title: "t".to_owned(),
            description: String::new(),
            priority: Priority::Unset,
            assignee: None,
            labels: labels.iter().map(|l| (*l).to_owned()).collect(),
            author: Author::Other {
                name: "someone".to_owned(),
            },
        }
    }

    fn pull(number: u64, branch: &str, files: &[&str]) -> Pull {
        Pull {
            number,
            head_branch: branch.to_owned(),
            files: PullFiles::Listed(files.iter().map(|f| (*f).to_owned()).collect()),
        }
    }

    /// A ref at `status`, delivered as pull request `number` when given.
    fn stored(status: Status, number: Option<u64>) -> Stored {
        let round = u32::from(status.stage() != Stage::Ready);
        let state = TicketState::restore(status, round, 0, 0, 0, 0).unwrap();
        let mut record = blank(None);
        record.state = PersistedState::from(&state);
        record.delivery = number.map(|number| {
            let mut delivery = TicketDelivery::parse(include_str!(
                "../../owlshift-contracts/tests/fixtures/ticket-delivery.json"
            ))
            .unwrap();
            delivery.pull_request.as_mut().unwrap().number = number;
            delivery
        });
        Stored {
            record,
            commit: "c".repeat(40),
            proposals: None,
        }
    }

    /// Owlshift's own pull request joins its ticket; another, one an ended
    /// ref delivered included, holds its files; people's tickets hold their
    /// labels' zones, the whole repository for a malformed label, and with
    /// none are counted through a pull request naming them or left out.
    #[test]
    fn the_outside_holders_and_owlshifts_own_pull_requests() {
        let outside = Outside {
            pulls: vec![
                pull(1, "owl-1-own", &["app/own.rs"]),
                pull(7, "someone/topic", &["docs/a.md", "db/m.sql"]),
                pull(8, "owl-6-gone", &[]),
                pull(9, "fix/DEMO-21-typo", &["web/x.ts"]),
            ],
            working: vec![
                person("DEMO-1", &[]),
                person("DEMO-20", &["zone:app", "bug"]),
                person("DEMO-21", &[]),
                person("DEMO-22", &[]),
                person("DEMO-23", &["zone:../x"]),
                person("DEMO-5", &[]),
                person("DEMO-6", &[]),
                person("DEMO-7", &["zone:web"]),
            ],
        };
        let (one, five, six) = (id("DEMO-1"), id("DEMO-5"), id("DEMO-6"));
        let held = stored(Status::Active(Stage::Watch), Some(1));
        let ended = stored(Status::Active(Stage::Ready), Some(8));
        // In Design, dispatched and holding nothing (OWL-448): Owlshift's
        // all the same, never a person's, its labels kept for its Build.
        let seven = id("DEMO-7");
        let mut designing = stored(Status::Active(Stage::Design), None);
        designing.record.held = Some(owlshift_contracts::refs::TicketHeld::dispatched(
            jiff::Timestamp::UNIX_EPOCH,
        ));
        let named = NamedResources::new([("migrations", "db/**")]).unwrap();
        let holders = outside.holders(
            [
                (&one, Some(&held)),
                (&five, None),
                (&six, Some(&ended)),
                (&seven, Some(&designing)),
            ],
            &named,
        );
        assert_eq!(holders.labels["DEMO-7"], ["zone:web"]);

        assert_eq!(
            holders.own,
            HashMap::from([(
                "DEMO-1".to_owned(),
                PullFiles::Listed(vec!["app/own.rs".to_owned()])
            )])
        );
        let others: Vec<(&str, &[Resource])> = holders
            .others
            .iter()
            .map(|entry| (entry.id.as_str(), entry.held.as_slice()))
            .collect();
        assert_eq!(
            others,
            [
                (
                    "#7",
                    &[
                        zone("db/m.sql"),
                        zone("docs/a.md"),
                        Resource::Named("migrations".to_owned())
                    ][..]
                ),
                ("#8", &[][..]),
                ("#9", &[zone("web/x.ts")][..]),
                ("DEMO-20", &[zone("app")][..]),
                (
                    "DEMO-23",
                    &[zone(""), Resource::Named("migrations".to_owned())][..]
                ),
            ]
        );
        assert_eq!(holders.unknown, [id("DEMO-22"), id("DEMO-6")]);
        assert!(holders.others.iter().all(|entry| entry.delivered));
        assert_eq!(holders.named("#7").to_string(), "pull request #7",);
        assert_eq!(
            holders.named("DEMO-20").data(),
            [("tickets", json!(["DEMO-20"])), ("person", json!(true))]
        );
        assert_eq!(holders.named("DEMO-1").to_string(), "DEMO-1");

        // Joined: the own pull request's files widen DEMO-1, and the
        // outside holders hold a ticket dispatched now.
        let mut in_flight = vec![held::in_flight(&one, &held, &named)];
        holders.join(&mut in_flight, &named);
        assert!(in_flight[0].held.contains(&zone("app/own.rs")));
        let ready = InFlight {
            id: "DEMO-30".to_owned(),
            dispatched: Dispatched::Now,
            delivered: false,
            outside: false,
            held: vec![zone("app/sub")],
        };
        let found = collision(&ready, &in_flight).unwrap();
        assert_eq!(found.holder, "DEMO-1");
    }

    /// The forge as GitHub answers: the open pull requests, from a list the
    /// test changes, and each one's files, counted.
    struct Forge {
        pulls: std::sync::Arc<std::sync::Mutex<Vec<(u64, char)>>>,
        files_reads: std::sync::Arc<std::sync::Mutex<u32>>,
    }

    impl owlshift_adapters::graphql::Transport for Forge {
        fn send(&self, _request: &Value) -> Result<owlshift_adapters::graphql::Response, String> {
            let nodes: Vec<Value> = self
                .pulls
                .lock()
                .unwrap()
                .iter()
                .map(|(number, head)| {
                    json!({ "number": number, "isDraft": false, "changedFiles": 1,
                        "headRefName": "topic", "headRefOid": head.to_string().repeat(40),
                        "baseRefName": "main", "baseRefOid": "b".repeat(40) })
                })
                .collect();
            let body = json!({ "data": { "repository": { "pullRequests": {
                "totalCount": nodes.len(),
                "pageInfo": { "hasNextPage": false, "endCursor": null },
                "nodes": nodes,
            } } } });
            Ok(owlshift_adapters::graphql::Response {
                status: 200,
                body: body.to_string(),
                ..Default::default()
            })
        }

        fn get(&self, path: &str) -> Result<owlshift_adapters::graphql::RestAnswer, String> {
            *self.files_reads.lock().unwrap() += 1;
            assert!(path.contains("/files?"), "{path}");
            Ok(owlshift_adapters::graphql::RestAnswer::new(
                200,
                json!([{ "filename": "app/x.rs" }]).to_string(),
            ))
        }
    }

    /// The working state, or a failure to read it.
    struct Working(Option<Vec<Ticket>>);

    impl Tracker for Working {
        fn capabilities(&self) -> &'static [owlshift_adapters::tracker::Capability] {
            &[]
        }
        fn ticket(&self, _: &TicketId) -> Result<Ticket, owlshift_adapters::tracker::Error> {
            unreachable!()
        }
        fn comments(
            &self,
            _: &TicketId,
        ) -> Result<Vec<owlshift_adapters::tracker::Comment>, owlshift_adapters::tracker::Error>
        {
            unreachable!()
        }
        fn post_comment(
            &self,
            _: &TicketId,
            _: &str,
        ) -> Result<owlshift_adapters::tracker::Comment, owlshift_adapters::tracker::Error>
        {
            unreachable!()
        }
        fn set_stage(
            &self,
            _: &TicketId,
            _: &str,
        ) -> Result<(), owlshift_adapters::tracker::Error> {
            unreachable!()
        }
        fn working(
            &self,
            team: Option<&str>,
            state: &str,
        ) -> Result<Vec<Ticket>, owlshift_adapters::tracker::Error> {
            assert_eq!((team, state), (Some("DEMO"), "In Progress"));
            self.0.clone().ok_or_else(|| {
                owlshift_adapters::tracker::Error::new(
                    owlshift_adapters::tracker::ErrorKind::Other,
                    "down",
                )
            })
        }
    }

    /// A pull request's files are read once while it stays at its head,
    /// base branch and base commit, again once it moved, and forgotten once
    /// it is no longer open; a failed read of the working state is an
    /// error.
    #[test]
    fn the_files_are_read_again_only_when_a_pull_request_moved() {
        let pulls = std::sync::Arc::new(std::sync::Mutex::new(vec![(7, 'a')]));
        let reads = std::sync::Arc::new(std::sync::Mutex::new(0));
        let forge = GitHubForge::with_transport(
            Forge {
                pulls: pulls.clone(),
                files_reads: reads.clone(),
            },
            owlshift_adapters::forge::Repo::parse("demo/project").unwrap(),
        );
        let config = ProjectConfig::parse(
            "requires = \">=0.0\"\n[tracker]\nkind = \"linear\"\nteam = \"DEMO\"\n\
             admit = \"delegation\"\nstates = { ready = \"Todo\", working = \"In Progress\", \
             needs_input = \"c\", review = \"d\" }\n[stack]\ngate = []\n[pipeline]\n\
             default = \"trivial\"\nplan_approval = \"never\"\n[models]\n[policy]\n\
             always_human = []\n",
        )
        .unwrap();
        let tracker = Working(Some(vec![person("DEMO-9", &["zone:app"])]));
        let mut cache = PullCache::default();
        let first = read(&forge, &tracker, &config, &mut cache).unwrap();
        assert_eq!(first.pulls, [pull(7, "topic", &["app/x.rs"])],);
        assert_eq!(first.working.len(), 1);
        read(&forge, &tracker, &config, &mut cache).unwrap();
        assert_eq!(*reads.lock().unwrap(), 1);
        pulls.lock().unwrap()[0].1 = 'c';
        read(&forge, &tracker, &config, &mut cache).unwrap();
        assert_eq!(*reads.lock().unwrap(), 2);
        pulls.lock().unwrap().clear();
        assert!(
            read(&forge, &tracker, &config, &mut cache)
                .unwrap()
                .pulls
                .is_empty()
        );
        assert!(cache.0.is_empty());
        let error = read(&forge, &Working(None), &config, &mut cache).unwrap_err();
        assert!(
            error.contains("working state could not be read: down"),
            "{error}"
        );
    }

    #[test]
    fn a_branch_names_a_ticket_by_its_whole_identifier() {
        for branch in ["owl-410-count", "someone/OWL-410", "OWL-410", "x_owl-410.y"] {
            assert!(branch_names(branch, "OWL-410"), "{branch}");
        }
        for branch in ["owl-4100", "owl-41", "bowl-410", "main"] {
            assert!(!branch_names(branch, "OWL-410"), "{branch}");
        }
    }

    /// Files hold themselves and the named resources they match, none for
    /// no file; a list not read whole holds the whole repository.
    #[test]
    fn a_pull_requests_files_hold_themselves() {
        let named = NamedResources::new([("lock", "Cargo.lock")]).unwrap();
        assert_eq!(
            files_held(&PullFiles::Listed(vec!["Cargo.lock".to_owned()]), &named),
            [zone("Cargo.lock"), Resource::Named("lock".to_owned())]
        );
        assert!(files_held(&PullFiles::Listed(vec![]), &named).is_empty());
        assert_eq!(
            files_held(&PullFiles::Whole, &named),
            [zone(""), Resource::Named("lock".to_owned())]
        );
    }
}
