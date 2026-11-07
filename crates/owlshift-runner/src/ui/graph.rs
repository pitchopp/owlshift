//! The drain's wait graph, as the web UI's graph page draws it (P8 part 5,
//! OWL-460, architecture section 5, "The local web UI"): a pure function of
//! the document `owlshift status --waits --json` prints ([`StatusWaits`]) and,
//! per project, what its `ready.json` gave ([`ReadyInput`]). The server
//! serialises the [`Graph`]; the page only places and draws what it holds.
//!
//! A project has two layers:
//!
//! - *live*: a node for each ticket of its `StatusWaits`, and an edge from
//!   each waiting ticket to each thing its wait names behind it
//!   ([`owlshift_contracts::waits::Wait::behind`]), labelled with the zone,
//!   the named resource, the browser, or else the wait's reason. A target
//!   that is not a listed ticket (a ticket out of flight, a person's ticket,
//!   a pull request) is a ghost node. The tickets that wait for a slot
//!   (reasons `slot` and `run_cap`) hold no edge: they are the project's
//!   [`QueueEntry`]s;
//! - *blockers*: the tracker's blockers, from the snapshot, among the ready
//!   column and the tickets in flight.
//!
//! Every node says its state, one of [`NODE_STATES`]; every field a page may
//! read is always there, `null` standing for what is not known. The layout is
//! computed here, so it is tested: a node's `layer` is its longest path to a
//! node that waits on nothing in the layer (a holder is in layer 0, what
//! waits behind it to its right), its `row` its place in its layer.

use std::collections::{BTreeMap, HashMap, HashSet};

use jiff::Timestamp;
use owlshift_contracts::ready::{BlockerState, Outcome, ReadySnapshot};
use owlshift_contracts::waits::{
    Behind, HeldRun, Paused, ProjectWaits, RefStatus, RunSlots, StatusWaits, TicketWaits,
    WaitResource,
};
use owlshift_core::schedule::natural_cmp;
use owlshift_core::vocab::{Priority, Stage};
use serde::Serialize;

/// Every state a node can take, as the page colours it: `running` (a step
/// holds a run slot), `resource` (a resource another holds), `ticket` (a
/// ticket, its blockers), `slot` (the project's capacity or the machine's
/// run slots), `decider` (a person's answer), `intake` (Owlshift's own
/// intake), `usage_limit`, `other` (any other wait), `not_started` (admitted,
/// no ticket ref), `idle` (no wait), `ready` (in the ready column, not in
/// the waits), `ghost` (a blocker outside both).
pub const NODE_STATES: [&str; 12] = [
    "running",
    "resource",
    "ticket",
    "slot",
    "decider",
    "intake",
    "usage_limit",
    "other",
    "not_started",
    "idle",
    "ready",
    "ghost",
];

/// The states of a blocker the tracker shows that still hold a ticket: the
/// `blocker_state` of a ghost node.
pub const BLOCKER_STATES: [&str; 3] = ["open", "canceled", "unknown"];

/// The label of an edge whose wait names the whole repository.
pub const WHOLE_REPOSITORY: &str = "whole repository";

/// The drain of every project asked about, at one time.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Graph {
    /// When the waits were read.
    pub at: Timestamp,
    /// What could not be read.
    pub notes: Vec<String>,
    pub machine_pause: Option<Paused>,
    pub runs: Option<RunSlots>,
    pub projects: Vec<ProjectGraph>,
}

/// One project's drain.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProjectGraph {
    pub repository: String,
    pub served: bool,
    pub max_in_flight: Option<u8>,
    pub in_flight: u32,
    pub paused: Option<Paused>,
    pub quarantined: bool,
    /// The live waits.
    pub live: Layer,
    /// The tickets that wait for a slot, the project's capacity first, then
    /// the machine's run slots.
    pub queue: Vec<QueueEntry>,
    /// The tracker's blockers; `None` without a ready snapshot that reads.
    pub blockers: Option<Layer>,
    /// The snapshot the blockers come from.
    pub ready: Option<ReadyInfo>,
    /// What could not be read of this project's graph.
    pub notes: Vec<String>,
}

/// A ready snapshot's age.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ReadyInfo {
    /// When the drain wrote it.
    pub at: Timestamp,
    /// The watch's interval between two idle passes, in milliseconds.
    pub interval_ms: u64,
    /// Older than two of those intervals.
    pub stale: bool,
}

/// What a project's `ready.json` gave.
#[derive(Clone, Debug)]
pub enum ReadyInput {
    /// There is none.
    Missing,
    /// It does not read: why.
    Unread(String),
    /// It reads; `stale` tells whether it is older than two intervals.
    Read {
        snapshot: ReadySnapshot,
        stale: bool,
    },
}

/// Nodes and the edges between them.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Layer {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

/// A ticket, or what a wait names behind a ticket.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Node {
    /// The ticket's identifier, `pull request #N` for a pull request, or,
    /// for a blocker the tracker names, the blocker's own text.
    pub id: String,
    /// `ticket` or `pull_request`.
    pub kind: &'static str,
    /// One of [`NODE_STATES`].
    pub state: &'static str,
    /// Whether the ticket is one of the document's tickets.
    pub in_flight: bool,
    /// A ticket a person works on.
    pub person: bool,
    /// A pull request's number.
    pub number: Option<u64>,
    /// A ghost of the blockers layer: `open`, `canceled` or `unknown`.
    pub blocker_state: Option<&'static str>,
    pub stage: Option<Stage>,
    pub ref_status: Option<RefStatus>,
    pub holds_capacity: Option<bool>,
    /// The run slot it holds.
    pub run: Option<HeldRun>,
    /// Its wait's reason key.
    pub reason: Option<String>,
    /// Its wait, in words.
    pub text: Option<String>,
    /// When its wait was told.
    pub since: Option<Timestamp>,
    /// Its title, from the ready snapshot.
    pub title: Option<String>,
    pub priority: Option<Priority>,
    /// What the drain made of it: `taken_in`, `running`, `dispatched`,
    /// `waits` or `not_admitted`.
    pub drain: Option<&'static str>,
    /// The reason of a drain outcome that waits.
    pub drain_reason: Option<String>,
    /// Its column in the drawing: 0 for what waits on nothing.
    pub layer: u32,
    /// Its place in its column.
    pub row: u32,
}

/// An edge from a ticket to what it waits on.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    /// The zone, the resource or the reason; the state of a blocker.
    pub label: String,
    /// The wait in words, for a tooltip.
    pub text: String,
}

/// A ticket that waits for a slot.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct QueueEntry {
    pub ticket: String,
    /// `slot` (the project's capacity) or `run_cap` (the machine's run
    /// slots).
    pub reason: String,
    pub since: Option<Timestamp>,
    pub text: String,
    /// What it waits behind, by name.
    pub behind: Vec<String>,
}

/// The state a wait's reason key gives a node. An unknown key is `other`.
pub fn reason_state(reason: &str) -> &'static str {
    match reason {
        "collision" => "resource",
        "blocked_by" | "blocker_canceled" | "blocker_unknown" => "ticket",
        "slot" | "run_cap" => "slot",
        "needs_input" | "no_decider" => "decider",
        "no_intake" | "stale_intake" => "intake",
        "usage_limit" => "usage_limit",
        _ => "other",
    }
}

fn live_state(row: &TicketWaits) -> &'static str {
    if row.run.is_some() {
        return "running";
    }
    match &row.waits {
        Some(wait) => reason_state(&wait.reason),
        None if row.ref_status == RefStatus::NoRef => "not_started",
        None => "idle",
    }
}

/// What `behind` is called: a ticket's identifier, `pull request #N`.
fn behind_name(behind: &Behind) -> String {
    match behind {
        Behind::Ticket { id, .. } => id.as_str().to_owned(),
        Behind::PullRequest { number } => format!("pull request #{number}"),
    }
}

/// What labels the edge of a wait: its resource, else its reason.
fn edge_label(wait: &owlshift_contracts::waits::Wait) -> String {
    match &wait.resource {
        Some(WaitResource::Zone { path }) if path.is_empty() => WHOLE_REPOSITORY.to_owned(),
        Some(WaitResource::Zone { path }) => path.clone(),
        Some(WaitResource::Named { name }) => name.clone(),
        Some(WaitResource::Browser) => "browser".to_owned(),
        None => wait.reason.clone(),
    }
}

fn blank(id: String, kind: &'static str, state: &'static str) -> Node {
    Node {
        id,
        kind,
        state,
        in_flight: false,
        person: false,
        number: None,
        blocker_state: None,
        stage: None,
        ref_status: None,
        holds_capacity: None,
        run: None,
        reason: None,
        text: None,
        since: None,
        title: None,
        priority: None,
        drain: None,
        drain_reason: None,
        layer: 0,
        row: 0,
    }
}

/// The node of a ticket of the document.
fn ticket_node(row: &TicketWaits) -> Node {
    let mut node = blank(row.ticket.as_str().to_owned(), "ticket", live_state(row));
    node.in_flight = true;
    node.stage = row.stage;
    node.ref_status = Some(row.ref_status);
    node.holds_capacity = Some(row.holds_capacity);
    node.run = row.run;
    if let Some(wait) = &row.waits {
        node.reason = Some(wait.reason.clone());
        node.text = Some(wait.text.clone());
        node.since = wait.since;
    }
    node
}

/// The graph of `waits`, each project's snapshot being `ready` under its
/// repository (a project with no entry has none).
pub fn of(waits: &StatusWaits, ready: &BTreeMap<String, ReadyInput>) -> Graph {
    Graph {
        at: waits.at,
        notes: waits.notes.clone(),
        machine_pause: waits.machine_pause.clone(),
        runs: waits.runs,
        projects: waits
            .projects
            .iter()
            .map(|project| of_project(project, ready.get(&project.repository)))
            .collect(),
    }
}

fn of_project(project: &ProjectWaits, ready: Option<&ReadyInput>) -> ProjectGraph {
    let (mut live, queue) = live_layer(project);
    let mut notes = Vec::new();
    let (blockers, info) = match ready {
        Some(ReadyInput::Read { snapshot, stale }) => {
            for node in &mut live.nodes {
                if let Some(ticket) = snapshot
                    .tickets
                    .iter()
                    .find(|ticket| ticket.ticket.as_str().eq_ignore_ascii_case(&node.id))
                {
                    ready_fields(node, ticket);
                }
            }
            (
                Some(blockers_layer(project, snapshot)),
                Some(ReadyInfo {
                    at: snapshot.at,
                    interval_ms: snapshot.interval_ms,
                    stale: *stale,
                }),
            )
        }
        Some(ReadyInput::Unread(why)) => {
            notes.push(format!(
                "{}: its ready snapshot cannot be read: {why}",
                project.repository
            ));
            (None, None)
        }
        Some(ReadyInput::Missing) | None => {
            notes.push(format!(
                "{}: the drain has written no ready snapshot yet, so the blockers are not drawn",
                project.repository
            ));
            (None, None)
        }
    };
    ProjectGraph {
        repository: project.repository.clone(),
        served: project.served,
        max_in_flight: project.max_in_flight,
        in_flight: project.in_flight,
        paused: project.paused.clone(),
        quarantined: project.quarantined,
        live,
        queue,
        blockers,
        ready: info,
        notes,
    }
}

/// What the ready snapshot says of a ticket, put on its node.
fn ready_fields(node: &mut Node, ticket: &owlshift_contracts::ready::ReadyTicket) {
    node.title = Some(ticket.title.clone());
    node.priority = Some(ticket.priority);
    let (drain, reason) = match &ticket.drain {
        Outcome::TakenIn => ("taken_in", None),
        Outcome::Running => ("running", None),
        Outcome::Dispatched => ("dispatched", None),
        Outcome::Waits { wait } => ("waits", Some(wait.reason.clone())),
        Outcome::NotAdmitted { wait } => ("not_admitted", Some(wait.reason.clone())),
    };
    node.drain = Some(drain);
    node.drain_reason = reason;
}

/// How a blocker's state is worded on an edge and a ghost.
fn blocker_word(state: BlockerState) -> &'static str {
    match state {
        BlockerState::Open => "open",
        BlockerState::Completed => "completed",
        BlockerState::Canceled => "canceled",
        BlockerState::Unknown => "unknown",
    }
}

/// The tracker's blockers: a node for each ticket of the ready column and
/// each ticket of the waits, an edge from a ticket to each blocker that still
/// holds it (not `completed`, not recorded merged), and a dashed ghost for a
/// blocker that is neither. An identifier is compared in any ASCII case.
fn blockers_layer(project: &ProjectWaits, snapshot: &ReadySnapshot) -> Layer {
    let rows: HashMap<String, &TicketWaits> = project
        .tickets
        .iter()
        .map(|row| (row.ticket.as_str().to_ascii_lowercase(), row))
        .collect();
    let mut nodes: Vec<Node> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for ticket in &snapshot.tickets {
        let key = ticket.ticket.as_str().to_ascii_lowercase();
        if index.contains_key(&key) {
            continue;
        }
        let mut node = match rows.get(&key) {
            Some(row) => ticket_node(row),
            None => blank(ticket.ticket.as_str().to_owned(), "ticket", "ready"),
        };
        ready_fields(&mut node, ticket);
        index.insert(key, nodes.len());
        nodes.push(node);
    }
    for row in &project.tickets {
        let key = row.ticket.as_str().to_ascii_lowercase();
        if let std::collections::hash_map::Entry::Vacant(slot) = index.entry(key) {
            slot.insert(nodes.len());
            nodes.push(ticket_node(row));
        }
    }
    let mut edges: Vec<Edge> = Vec::new();
    let mut drawn: HashSet<(usize, usize)> = HashSet::new();
    for ticket in &snapshot.tickets {
        let Some(&from) = index.get(&ticket.ticket.as_str().to_ascii_lowercase()) else {
            continue;
        };
        for blocker in &ticket.blockers {
            if blocker.state == BlockerState::Completed || blocker.merged {
                continue;
            }
            let word = blocker_word(blocker.state);
            let to = *index
                .entry(blocker.ticket.to_ascii_lowercase())
                .or_insert_with(|| {
                    let mut ghost = blank(blocker.ticket.clone(), "ticket", "ghost");
                    ghost.blocker_state = Some(word);
                    nodes.push(ghost);
                    nodes.len() - 1
                });
            if drawn.insert((from, to)) {
                edges.push(Edge {
                    from: nodes[from].id.clone(),
                    to: nodes[to].id.clone(),
                    label: word.to_owned(),
                    text: format!("{} is held by {}: {word}", nodes[from].id, nodes[to].id),
                });
            }
        }
    }
    layout(&mut nodes, &edges);
    Layer { nodes, edges }
}

/// Whether a wait for `reason` is a place in a queue, not an edge.
fn queued(reason: &str) -> bool {
    matches!(reason, "slot" | "run_cap")
}

fn live_layer(project: &ProjectWaits) -> (Layer, Vec<QueueEntry>) {
    let mut nodes: Vec<Node> = project.tickets.iter().map(ticket_node).collect();
    let mut index: HashMap<String, usize> = nodes
        .iter()
        .enumerate()
        .map(|(at, node)| (node.id.clone(), at))
        .collect();
    let mut edges = Vec::new();
    let mut slot_queue: Vec<&TicketWaits> = Vec::new();
    let mut run_queue: Vec<&TicketWaits> = Vec::new();
    for row in &project.tickets {
        let Some(wait) = &row.waits else { continue };
        match wait.reason.as_str() {
            "slot" => slot_queue.push(row),
            "run_cap" => run_queue.push(row),
            _ => {}
        }
        if queued(&wait.reason) {
            continue;
        }
        let label = edge_label(wait);
        for behind in &wait.behind {
            let name = behind_name(behind);
            let at = *index.entry(name.clone()).or_insert_with(|| {
                let ghost = match behind {
                    Behind::Ticket { .. } => blank(name.clone(), "ticket", "idle"),
                    Behind::PullRequest { number } => {
                        let mut ghost = blank(name.clone(), "pull_request", "idle");
                        ghost.number = Some(*number);
                        ghost
                    }
                };
                nodes.push(ghost);
                nodes.len() - 1
            });
            if let Behind::Ticket { person: true, .. } = behind {
                nodes[at].person = true;
            }
            edges.push(Edge {
                from: row.ticket.as_str().to_owned(),
                to: name,
                label: label.clone(),
                text: wait.text.clone(),
            });
        }
    }
    slot_queue.sort_by_key(|row| row.waits.as_ref().and_then(|wait| wait.since));
    let queue = slot_queue
        .into_iter()
        .chain(run_queue)
        .filter_map(|row| {
            let wait = row.waits.as_ref()?;
            Some(QueueEntry {
                ticket: row.ticket.as_str().to_owned(),
                reason: wait.reason.clone(),
                since: wait.since,
                text: wait.text.clone(),
                behind: wait.behind.iter().map(behind_name).collect(),
            })
        })
        .collect();
    layout(&mut nodes, &edges);
    (Layer { nodes, edges }, queue)
}

/// Gives every node its `layer` and `row`.
///
/// A node's layer is the longest path from it along its edges to a node with
/// none, so what a node waits on is in a column to its left. A cycle has no
/// such path: the edges a depth-first walk, in the nodes' natural order,
/// finds closing a cycle (a back edge) are left out, so the walk ends and
/// each node is drawn once. The row is the node's place among its layer's:
/// by the first edge that reaches it, then by natural order.
pub fn layout(nodes: &mut [Node], edges: &[Edge]) {
    let by_id: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(at, node)| (node.id.as_str(), at))
        .collect();
    let mut order: Vec<usize> = (0..nodes.len()).collect();
    order.sort_by(|&a, &b| natural_cmp(&nodes[a].id, &nodes[b].id));
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    let mut first_edge: Vec<usize> = vec![usize::MAX; nodes.len()];
    for (at, edge) in edges.iter().enumerate() {
        let (Some(&from), Some(&to)) = (by_id.get(edge.from.as_str()), by_id.get(edge.to.as_str()))
        else {
            continue;
        };
        out[from].push(to);
        for end in [from, to] {
            first_edge[end] = first_edge[end].min(at);
        }
    }
    // The depth-first walk, with its own stack: a graph of any depth.
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        New,
        Open,
        Done,
    }
    let mut mark = vec![Mark::New; nodes.len()];
    let mut back: HashSet<(usize, usize)> = HashSet::new();
    for &start in &order {
        if mark[start] != Mark::New {
            continue;
        }
        let mut stack = vec![(start, 0usize)];
        mark[start] = Mark::Open;
        while let Some((node, next)) = stack.pop() {
            if let Some(&target) = out[node].get(next) {
                stack.push((node, next + 1));
                match mark[target] {
                    Mark::New => {
                        mark[target] = Mark::Open;
                        stack.push((target, 0));
                    }
                    Mark::Open => {
                        back.insert((node, target));
                    }
                    Mark::Done => {}
                }
            } else {
                mark[node] = Mark::Done;
            }
        }
    }
    // What is left is acyclic: a node is one past the deepest it waits on.
    // The walk's finishing order puts a node's targets before it.
    let mut finished: Vec<usize> = Vec::with_capacity(nodes.len());
    {
        let mut seen = vec![false; nodes.len()];
        for &start in &order {
            if seen[start] {
                continue;
            }
            let mut stack = vec![(start, 0usize)];
            seen[start] = true;
            while let Some((node, next)) = stack.pop() {
                if let Some(&target) = out[node].get(next) {
                    stack.push((node, next + 1));
                    if !back.contains(&(node, target)) && !seen[target] {
                        seen[target] = true;
                        stack.push((target, 0));
                    }
                } else {
                    finished.push(node);
                }
            }
        }
    }
    let mut layer = vec![0u32; nodes.len()];
    for &node in &finished {
        layer[node] = out[node]
            .iter()
            .filter(|&&target| !back.contains(&(node, target)))
            .map(|&target| layer[target] + 1)
            .max()
            .unwrap_or(0);
    }
    let mut columns: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
    for (at, &column) in layer.iter().enumerate() {
        columns.entry(column).or_default().push(at);
    }
    for (column, members) in &mut columns {
        members.sort_by(|&a, &b| {
            first_edge[a]
                .cmp(&first_edge[b])
                .then_with(|| natural_cmp(&nodes[a].id, &nodes[b].id))
        });
        for (row, &at) in members.iter().enumerate() {
            nodes[at].layer = *column;
            nodes[at].row = u32::try_from(row).unwrap_or(u32::MAX);
        }
    }
}

#[cfg(test)]
mod tests {
    use owlshift_contracts::format::Format;
    use owlshift_contracts::ids::TicketId;
    use owlshift_contracts::ready::{ReadyBlocker, ReadyTicket};
    use owlshift_contracts::waits::Wait;

    use super::*;

    fn id(ticket: &str) -> TicketId {
        TicketId::new(ticket).unwrap()
    }

    fn wait(reason: &str, behind: Vec<Behind>) -> Wait {
        Wait {
            reason: reason.to_owned(),
            since: None,
            behind,
            resource: None,
            harness: None,
            until: None,
            text: format!("{reason} in words"),
        }
    }

    fn ticket(name: &str, waits: Option<Wait>) -> TicketWaits {
        TicketWaits {
            ticket: id(name),
            stage: Some(Stage::Build),
            ref_status: RefStatus::Active,
            holds_capacity: true,
            run: None,
            waits,
        }
    }

    fn behind(name: &str) -> Behind {
        Behind::Ticket {
            id: id(name),
            person: false,
        }
    }

    fn project(tickets: Vec<TicketWaits>) -> ProjectWaits {
        ProjectWaits {
            repository: "o/r".to_owned(),
            served: true,
            max_in_flight: Some(2),
            in_flight: 1,
            paused: None,
            quarantined: false,
            tickets,
        }
    }

    fn waits(tickets: Vec<TicketWaits>) -> StatusWaits {
        StatusWaits {
            format: Format,
            at: "2026-11-07T10:00:00Z".parse().unwrap(),
            machine_pause: None,
            runs: Some(RunSlots { in_use: 1, of: 2 }),
            projects: vec![project(tickets)],
            notes: vec!["a note".to_owned()],
        }
    }

    fn live(waits: &StatusWaits) -> (Layer, Vec<QueueEntry>) {
        let graph = of(waits, &BTreeMap::new());
        let project = graph.projects.into_iter().next().unwrap();
        (project.live, project.queue)
    }

    fn node<'a>(layer: &'a Layer, name: &str) -> &'a Node {
        layer
            .nodes
            .iter()
            .find(|node| node.id == name)
            .unwrap_or_else(|| panic!("{name} missing"))
    }

    #[test]
    fn every_ticket_is_a_node_and_every_behind_of_a_non_queue_wait_an_edge() {
        let (layer, queue) = live(&waits(vec![
            ticket("DEMO-1", None),
            ticket("DEMO-2", Some(wait("blocked_by", vec![behind("DEMO-1")]))),
            ticket(
                "DEMO-3",
                Some(wait("collision", vec![behind("DEMO-1"), behind("DEMO-2")])),
            ),
            ticket("DEMO-4", Some(wait("slot", vec![behind("DEMO-1")]))),
        ]));
        let ids: Vec<&str> = layer.nodes.iter().map(|node| node.id.as_str()).collect();
        assert_eq!(ids, ["DEMO-1", "DEMO-2", "DEMO-3", "DEMO-4"]);
        let edges: Vec<(&str, &str)> = layer
            .edges
            .iter()
            .map(|edge| (edge.from.as_str(), edge.to.as_str()))
            .collect();
        assert_eq!(
            edges,
            [
                ("DEMO-2", "DEMO-1"),
                ("DEMO-3", "DEMO-1"),
                ("DEMO-3", "DEMO-2")
            ]
        );
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].ticket, "DEMO-4");
        assert_eq!(queue[0].behind, ["DEMO-1"]);
    }

    #[test]
    fn each_reason_gives_its_state() {
        let table = [
            ("blocked_by", "ticket"),
            ("blocker_canceled", "ticket"),
            ("blocker_unknown", "ticket"),
            ("collision", "resource"),
            ("slot", "slot"),
            ("run_cap", "slot"),
            ("usage_limit", "usage_limit"),
            ("quarantine", "other"),
            ("pr_cap", "other"),
            ("base_broken", "other"),
            ("in_flight_unread", "other"),
            ("to_watch", "other"),
            ("no_decider", "decider"),
            ("no_intake", "intake"),
            ("stale_intake", "intake"),
            ("not_ready", "other"),
            ("needs_input", "decider"),
            ("parked", "other"),
            ("a reason of tomorrow", "other"),
        ];
        for (reason, state) in table {
            assert_eq!(reason_state(reason), state, "{reason}");
            let (layer, _) = live(&waits(vec![ticket("DEMO-1", Some(wait(reason, vec![])))]));
            assert_eq!(node(&layer, "DEMO-1").state, state, "{reason}");
        }
        for state in table.map(|(_, state)| state) {
            assert!(NODE_STATES.contains(&state), "{state}");
        }
    }

    #[test]
    fn a_ticket_without_a_wait_is_idle_or_not_started_and_a_held_run_wins() {
        let mut not_started = ticket("DEMO-1", None);
        not_started.ref_status = RefStatus::NoRef;
        not_started.stage = None;
        let mut running = ticket("DEMO-3", Some(wait("blocked_by", vec![])));
        running.run = Some(HeldRun {
            slot: 1,
            pid: 77,
            since: "2026-11-07T09:00:00Z".parse().unwrap(),
        });
        let (layer, _) = live(&waits(vec![not_started, ticket("DEMO-2", None), running]));
        assert_eq!(node(&layer, "DEMO-1").state, "not_started");
        assert_eq!(node(&layer, "DEMO-2").state, "idle");
        let held = node(&layer, "DEMO-3");
        assert_eq!(held.state, "running");
        assert_eq!(held.run.unwrap().pid, 77);
        assert_eq!(held.reason.as_deref(), Some("blocked_by"));
        for node in &layer.nodes {
            assert!(NODE_STATES.contains(&node.state));
        }
    }

    #[test]
    fn an_edge_is_labelled_with_its_resource_else_its_reason() {
        let with = |resource: Option<WaitResource>, reason: &str| {
            let mut wait = wait(reason, vec![behind("DEMO-1")]);
            wait.resource = resource;
            let (layer, _) = live(&waits(vec![
                ticket("DEMO-1", None),
                ticket("DEMO-2", Some(wait)),
            ]));
            assert_eq!(layer.edges.len(), 1);
            layer.edges[0].label.clone()
        };
        let zone = |path: &str| {
            Some(WaitResource::Zone {
                path: path.to_owned(),
            })
        };
        assert_eq!(with(zone("app/one.rs"), "collision"), "app/one.rs");
        assert_eq!(with(zone(""), "collision"), "whole repository");
        assert_eq!(
            with(
                Some(WaitResource::Named {
                    name: "database".to_owned()
                }),
                "collision"
            ),
            "database"
        );
        assert_eq!(with(Some(WaitResource::Browser), "collision"), "browser");
        assert_eq!(with(None, "blocked_by"), "blocked_by");
    }

    #[test]
    fn the_slot_queue_is_oldest_first_and_has_no_edge() {
        let told = |reason: &str, since: &str| {
            let mut wait = wait(reason, vec![behind("DEMO-1")]);
            wait.since = Some(since.parse().unwrap());
            Some(wait)
        };
        let (layer, queue) = live(&waits(vec![
            ticket("DEMO-1", None),
            ticket("DEMO-2", told("slot", "2026-11-07T09:30:00Z")),
            ticket("DEMO-3", told("run_cap", "2026-11-07T08:00:00Z")),
            ticket("DEMO-4", told("slot", "2026-11-07T09:00:00Z")),
        ]));
        assert!(layer.edges.is_empty(), "{:?}", layer.edges);
        let order: Vec<(&str, &str)> = queue
            .iter()
            .map(|entry| (entry.ticket.as_str(), entry.reason.as_str()))
            .collect();
        assert_eq!(
            order,
            [
                ("DEMO-4", "slot"),
                ("DEMO-2", "slot"),
                ("DEMO-3", "run_cap")
            ]
        );
    }

    #[test]
    fn what_a_wait_names_that_is_no_listed_ticket_is_a_ghost_node() {
        let (layer, _) = live(&waits(vec![
            ticket(
                "DEMO-2",
                Some(wait(
                    "blocked_by",
                    vec![
                        Behind::Ticket {
                            id: id("DEMO-9"),
                            person: true,
                        },
                        Behind::PullRequest { number: 12 },
                    ],
                )),
            ),
            ticket("DEMO-3", Some(wait("blocked_by", vec![behind("DEMO-8")]))),
        ]));
        let person = node(&layer, "DEMO-9");
        assert!(person.person && !person.in_flight);
        assert_eq!((person.kind, person.state), ("ticket", "idle"));
        let pull = node(&layer, "pull request #12");
        assert_eq!((pull.kind, pull.number), ("pull_request", Some(12)));
        assert!(!node(&layer, "DEMO-8").person);
        assert!(node(&layer, "DEMO-2").in_flight);
        assert_eq!(layer.nodes.len(), 5);
        assert_eq!(layer.edges.len(), 3);
    }

    #[test]
    fn a_chain_lays_out_in_layers_and_a_cycle_terminates() {
        let (layer, _) = live(&waits(vec![
            ticket("DEMO-1", None),
            ticket("DEMO-2", Some(wait("blocked_by", vec![behind("DEMO-1")]))),
            ticket("DEMO-3", Some(wait("blocked_by", vec![behind("DEMO-2")]))),
        ]));
        let place = |name: &str| (node(&layer, name).layer, node(&layer, name).row);
        assert_eq!(place("DEMO-1"), (0, 0));
        assert_eq!(place("DEMO-2"), (1, 0));
        assert_eq!(place("DEMO-3"), (2, 0));

        let (cycle, _) = live(&waits(vec![
            ticket("DEMO-1", Some(wait("blocked_by", vec![behind("DEMO-2")]))),
            ticket("DEMO-2", Some(wait("blocked_by", vec![behind("DEMO-1")]))),
            ticket("DEMO-3", Some(wait("blocked_by", vec![behind("DEMO-3")]))),
        ]));
        assert_eq!(cycle.nodes.len(), 3);
        assert_eq!(cycle.edges.len(), 3, "every edge is kept");
        let layers: Vec<u32> = cycle.nodes.iter().map(|node| node.layer).collect();
        assert_eq!(layers, [1, 0, 0], "{:?}", cycle.nodes);
    }

    #[test]
    fn rows_follow_the_first_edge_then_natural_order() {
        let (layer, _) = live(&waits(vec![
            ticket("DEMO-10", None),
            ticket("DEMO-9", None),
            ticket("DEMO-2", Some(wait("blocked_by", vec![behind("DEMO-10")]))),
            ticket("DEMO-3", None),
        ]));
        let mut zero: Vec<&Node> = layer.nodes.iter().filter(|n| n.layer == 0).collect();
        zero.sort_by_key(|node| node.row);
        let column: Vec<&str> = zero.iter().map(|node| node.id.as_str()).collect();
        // DEMO-10 is reached by the one edge; the others follow in natural
        // order.
        assert_eq!(column, ["DEMO-10", "DEMO-3", "DEMO-9"]);
        let rows: Vec<u32> = zero.iter().map(|node| node.row).collect();
        assert_eq!(rows, [0, 1, 2]);
    }

    #[test]
    fn markup_in_a_text_is_only_data_and_every_field_is_there() {
        let mut hostile = wait("blocked_by", vec![behind("DEMO-1")]);
        hostile.text = "<script>alert(1)</script>".to_owned();
        let (layer, _) = live(&waits(vec![
            ticket("DEMO-1", None),
            ticket("DEMO-2", Some(hostile)),
        ]));
        let value = serde_json::to_value(&layer).unwrap();
        assert_eq!(
            value["nodes"][1]["text"],
            serde_json::json!("<script>alert(1)</script>")
        );
        for node in value["nodes"].as_array().unwrap() {
            for key in [
                "id",
                "kind",
                "state",
                "in_flight",
                "person",
                "number",
                "blocker_state",
                "stage",
                "ref_status",
                "holds_capacity",
                "run",
                "reason",
                "text",
                "since",
                "title",
                "priority",
                "drain",
                "drain_reason",
                "layer",
                "row",
            ] {
                assert!(node.get(key).is_some(), "{key} missing in {node}");
            }
        }
        assert!(value["nodes"][0]["text"].is_null());
        assert!(value["nodes"][0]["run"].is_null());
    }

    fn blocker(name: &str, state: BlockerState, merged: bool) -> ReadyBlocker {
        ReadyBlocker {
            ticket: name.to_owned(),
            state,
            merged,
        }
    }

    fn ready_ticket(name: &str, blockers: Vec<ReadyBlocker>, drain: Outcome) -> ReadyTicket {
        ReadyTicket {
            ticket: id(name),
            title: format!("{name} title"),
            priority: Priority::High,
            blockers,
            drain,
        }
    }

    fn snapshot(tickets: Vec<ReadyTicket>) -> ReadySnapshot {
        ReadySnapshot {
            format: Format,
            at: "2026-11-07T09:59:00Z".parse().unwrap(),
            interval_ms: 60_000,
            tickets,
        }
    }

    /// The project graph of `waits` over `snapshot`, read or not.
    fn with(waits: &StatusWaits, input: ReadyInput) -> ProjectGraph {
        let ready = BTreeMap::from([("o/r".to_owned(), input)]);
        of(waits, &ready).projects.into_iter().next().unwrap()
    }

    fn read(tickets: Vec<ReadyTicket>) -> ReadyInput {
        ReadyInput::Read {
            snapshot: snapshot(tickets),
            stale: false,
        }
    }

    fn edge_set(layer: &Layer) -> Vec<(&str, &str, &str)> {
        layer
            .edges
            .iter()
            .map(|e| (e.from.as_str(), e.to.as_str(), e.label.as_str()))
            .collect()
    }

    #[test]
    fn a_blocker_that_is_a_node_gets_an_edge_and_one_outside_a_dashed_ghost() {
        let waiting = waits(vec![
            ticket("DEMO-1", None),
            ticket("DEMO-2", Some(wait("blocked_by", vec![behind("DEMO-1")]))),
        ]);
        let graph = with(
            &waiting,
            read(vec![
                ready_ticket(
                    "DEMO-2",
                    vec![
                        blocker("DEMO-1", BlockerState::Open, false),
                        blocker("DEMO-7", BlockerState::Open, false),
                        blocker("DEMO-8", BlockerState::Canceled, false),
                        blocker("DEMO-9", BlockerState::Unknown, false),
                    ],
                    Outcome::TakenIn,
                ),
                ready_ticket("DEMO-5", vec![], Outcome::Dispatched),
            ]),
        );
        let layer = graph.blockers.unwrap();
        assert_eq!(
            edge_set(&layer),
            [
                ("DEMO-2", "DEMO-1", "open"),
                ("DEMO-2", "DEMO-7", "open"),
                ("DEMO-2", "DEMO-8", "canceled"),
                ("DEMO-2", "DEMO-9", "unknown"),
            ]
        );
        for (name, state) in [
            ("DEMO-7", "open"),
            ("DEMO-8", "canceled"),
            ("DEMO-9", "unknown"),
        ] {
            let ghost = node(&layer, name);
            assert_eq!(ghost.state, "ghost");
            assert_eq!(ghost.blocker_state, Some(state));
            assert!(!ghost.in_flight);
            assert!(BLOCKER_STATES.contains(&state));
        }
        // A ready ticket in the waits has its live state, one that is only
        // in the column is `ready`, and both say what the drain made of
        // them.
        assert_eq!(node(&layer, "DEMO-2").state, "ticket");
        assert_eq!(
            node(&layer, "DEMO-2").title.as_deref(),
            Some("DEMO-2 title")
        );
        assert_eq!(node(&layer, "DEMO-5").state, "ready");
        assert_eq!(node(&layer, "DEMO-5").drain, Some("dispatched"));
        // The live layer carries the titles too.
        assert_eq!(
            node(&graph.live, "DEMO-2").title.as_deref(),
            Some("DEMO-2 title")
        );
        assert_eq!(node(&graph.live, "DEMO-1").title, None);
        assert_eq!(layer.nodes.len(), 6);
        for node in &layer.nodes {
            assert!(NODE_STATES.contains(&node.state), "{}", node.state);
        }
    }

    #[test]
    fn a_completed_or_merged_blocker_has_no_node_and_no_edge() {
        let graph = with(
            &waits(vec![]),
            read(vec![ready_ticket(
                "DEMO-2",
                vec![
                    blocker("DEMO-1", BlockerState::Completed, false),
                    blocker("DEMO-3", BlockerState::Open, true),
                ],
                Outcome::TakenIn,
            )]),
        );
        let layer = graph.blockers.unwrap();
        assert!(layer.edges.is_empty());
        assert_eq!(layer.nodes.len(), 1);
    }

    #[test]
    fn a_blocker_is_found_in_any_case_and_a_text_that_is_no_id_is_a_ghost() {
        let graph = with(
            &waits(vec![ticket("DEMO-1", None)]),
            read(vec![ready_ticket(
                "DEMO-2",
                vec![
                    blocker("demo-1", BlockerState::Open, false),
                    blocker("see the design doc", BlockerState::Open, false),
                    blocker("SEE THE DESIGN DOC", BlockerState::Open, false),
                ],
                Outcome::TakenIn,
            )]),
        );
        let layer = graph.blockers.unwrap();
        assert_eq!(
            edge_set(&layer),
            [
                ("DEMO-2", "DEMO-1", "open"),
                ("DEMO-2", "see the design doc", "open"),
            ]
        );
        assert_eq!(layer.nodes.len(), 3);
        assert_eq!(node(&layer, "see the design doc").state, "ghost");
        // Two chains: the layout reads the blockers' edges as the live
        // layer's.
        assert_eq!(node(&layer, "DEMO-2").layer, 1);
        assert_eq!(node(&layer, "DEMO-1").layer, 0);
    }

    #[test]
    fn the_snapshots_age_is_told_and_a_missing_or_unread_one_leaves_no_layer() {
        let waiting = waits(vec![ticket("DEMO-1", None)]);
        let stale = with(
            &waiting,
            ReadyInput::Read {
                snapshot: snapshot(vec![]),
                stale: true,
            },
        );
        let info = stale.ready.unwrap();
        assert!(info.stale);
        assert_eq!(info.interval_ms, 60_000);
        assert!(stale.blockers.is_some() && stale.notes.is_empty());

        let missing = with(&waiting, ReadyInput::Missing);
        assert!(missing.blockers.is_none() && missing.ready.is_none());
        assert_eq!(missing.notes.len(), 1);
        assert!(missing.notes[0].starts_with("o/r: the drain has written no ready snapshot"));
        let unread = with(&waiting, ReadyInput::Unread("bad json".to_owned()));
        assert!(unread.blockers.is_none());
        assert_eq!(
            unread.notes,
            ["o/r: its ready snapshot cannot be read: bad json"]
        );
        // No entry at all reads as missing.
        let none = of(&waiting, &BTreeMap::new());
        assert!(none.projects[0].blockers.is_none());
    }
}
