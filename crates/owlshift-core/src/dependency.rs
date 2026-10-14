//! Dependencies to relations (architecture section 5, "Intake"): what the
//! runner does with each dependency intake found. It writes a `blocks`
//! relation from the dependency to the ticket unless one exists, the
//! identifier is unknown or the ticket itself, the dependency is already
//! done, or the ticket already blocks it, directly or through its own
//! blockers up to [`MAX_DEPTH`] deep: a cycle, which would hold both forever.
//! The runner reads the tracker through the closure it passes; nothing here
//! performs I/O.

use std::collections::BTreeSet;

use crate::ticket::text_blockers;

/// How deep the cycle walk follows a dependency's blockers: its own are at
/// depth 1.
pub const MAX_DEPTH: usize = 10;

/// What becomes of a dependency.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Outcome {
    /// Write the relation.
    Write,
    /// The ticket's tracker relations already name it as a blocker.
    Existed,
    /// It is already done: the relation would release the ticket at once.
    Done,
    /// The tracker answered that no such ticket exists.
    Unknown,
    /// It names the ticket itself.
    Itself,
    /// The ticket already blocks it, directly or through its blockers.
    Cycle,
}

/// A ticket as the walk reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node<K> {
    /// The tracker's own identifier of the ticket, which a request may have
    /// spelled otherwise (a Linear issue's UUID for its `OWL-n`).
    pub id: K,
    /// Whether the tracker shows it done (architecture section 5,
    /// "Blockers"): a canceled ticket is not.
    pub done: bool,
    /// What blocks it, by relation or by its "Blocked by" line
    /// ([`blockers_of`]).
    pub blockers: Vec<K>,
}

/// The outcome for `dependency` of the ticket `ticket`, the tracker's own
/// identifier, whose tracker relations name the blockers `related`. `read`
/// reads a ticket by identifier: `Ok(None)` only when the tracker answered
/// it knows no such ticket, an error for any other failure, which stops the
/// check. Each ticket is read at most once; the tickets at the walk's last
/// depth are only compared, never read.
pub fn check<K: Clone + Ord, E>(
    ticket: &K,
    related: &[K],
    dependency: &K,
    mut read: impl FnMut(&K) -> Result<Option<Node<K>>, E>,
) -> Result<Outcome, E> {
    if dependency == ticket {
        return Ok(Outcome::Itself);
    }
    let Some(node) = read(dependency)? else {
        return Ok(Outcome::Unknown);
    };
    if node.id == *ticket {
        return Ok(Outcome::Itself);
    }
    if related.contains(&node.id) || related.contains(dependency) {
        return Ok(Outcome::Existed);
    }
    if node.done {
        return Ok(Outcome::Done);
    }
    if reaches(ticket, node, &mut read)? {
        Ok(Outcome::Cycle)
    } else {
        Ok(Outcome::Write)
    }
}

/// Whether `ticket` is among the blockers of `start`, or of theirs, up to
/// [`MAX_DEPTH`] deep, breadth first. A ticket the tracker does not know
/// blocks nothing; one already seen is not read again.
fn reaches<K: Clone + Ord, E>(
    ticket: &K,
    start: Node<K>,
    read: &mut impl FnMut(&K) -> Result<Option<Node<K>>, E>,
) -> Result<bool, E> {
    let mut seen = BTreeSet::from([start.id]);
    let mut frontier = start.blockers;
    for depth in 1..=MAX_DEPTH {
        if frontier.contains(ticket) {
            return Ok(true);
        }
        if depth == MAX_DEPTH {
            break;
        }
        let mut next = Vec::new();
        for id in frontier {
            if !seen.insert(id.clone()) {
                continue;
            }
            let Some(node) = read(&id)? else {
                continue;
            };
            // Asked by another spelling of the ticket's own identifier.
            if node.id == *ticket {
                return Ok(true);
            }
            seen.insert(node.id);
            next.extend(node.blockers.into_iter().filter(|b| !seen.contains(b)));
        }
        frontier = next;
    }
    Ok(false)
}

/// What blocks the ticket `own` (architecture section 5, "Blockers"): its
/// tracker relations' blockers and the identifiers of its description's
/// "Blocked by" line ([`text_blockers`]), each once, its own identifier left
/// out, since a ticket is never its own prerequisite.
pub fn blockers_of<'a>(
    own: &str,
    relations: impl IntoIterator<Item = &'a str>,
    description: &'a str,
) -> Vec<&'a str> {
    let mut found: Vec<&str> = Vec::new();
    for id in relations.into_iter().chain(text_blockers(description)) {
        if id != own && !found.contains(&id) {
            found.push(id);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    use super::*;

    /// A tracker of open tickets: each name maps to its blockers. `done`
    /// tickets are done, `broken` ones fail to read, `alias` maps another
    /// spelling to a name; any other name is unknown.
    struct Graph {
        blockers: BTreeMap<&'static str, Vec<&'static str>>,
        done: Vec<&'static str>,
        broken: Vec<&'static str>,
        alias: Vec<(&'static str, &'static str)>,
        reads: RefCell<Vec<&'static str>>,
    }

    impl Graph {
        fn new(edges: &[(&'static str, &[&'static str])]) -> Self {
            Self {
                blockers: edges.iter().map(|(id, b)| (*id, b.to_vec())).collect(),
                done: Vec::new(),
                broken: Vec::new(),
                alias: Vec::new(),
                reads: RefCell::new(Vec::new()),
            }
        }

        fn read(&self, id: &&'static str) -> Result<Option<Node<&'static str>>, String> {
            self.reads.borrow_mut().push(id);
            if self.broken.contains(id) {
                return Err(format!("{id}: the tracker failed"));
            }
            let id = self
                .alias
                .iter()
                .find(|(alias, _)| alias == id)
                .map_or(*id, |(_, name)| name);
            Ok(self.blockers.get(id).map(|blockers| Node {
                id,
                done: self.done.contains(&id),
                blockers: blockers.clone(),
            }))
        }

        fn check(&self, related: &[&'static str], dependency: &'static str) -> Outcome {
            check(&"T", related, &dependency, |id| self.read(id)).unwrap()
        }
    }

    /// A chain T <- X1 <- ... <- Xn: each Xi blocked by the one before, X1
    /// by T, and D blocked by Xn.
    fn chain(n: usize) -> Graph {
        const NAMES: [&str; 11] = [
            "T", "X1", "X2", "X3", "X4", "X5", "X6", "X7", "X8", "X9", "X10",
        ];
        let mut graph = Graph::new(&[("T", &[]), ("D", &[])]);
        for i in 1..=n {
            graph.blockers.insert(NAMES[i], vec![NAMES[i - 1]]);
        }
        graph.blockers.insert("D", vec![NAMES[n]]);
        graph
    }

    #[test]
    fn each_outcome_in_its_order() {
        let mut graph = Graph::new(&[("T", &[]), ("D", &[]), ("C", &["T"])]);
        graph.done.push("D");
        graph.alias.push(("uuid-of-t", "T"));
        assert_eq!(graph.check(&[], "T"), Outcome::Itself);
        assert!(graph.reads.borrow().is_empty(), "itself needs no read");
        assert_eq!(graph.check(&[], "uuid-of-t"), Outcome::Itself);
        assert_eq!(graph.check(&[], "NOPE-1"), Outcome::Unknown);
        // A relation already there wins over done.
        assert_eq!(graph.check(&["D"], "D"), Outcome::Existed);
        assert_eq!(graph.check(&[], "D"), Outcome::Done);
        assert_eq!(graph.check(&[], "C"), Outcome::Cycle);
        graph.blockers.insert("E", vec!["NOPE-2"]);
        assert_eq!(graph.check(&[], "E"), Outcome::Write);
    }

    #[test]
    fn the_cycle_walk_stops_ten_deep() {
        // D's blockers are at depth 1, so T sits at depth n + 1.
        assert_eq!(chain(9).check(&[], "D"), Outcome::Cycle);
        let deep = chain(10);
        assert_eq!(deep.check(&[], "D"), Outcome::Write);
        // D, then X10 down to X2: the last depth is compared, never read.
        assert_eq!(deep.reads.borrow().len(), 10);
    }

    #[test]
    fn a_loop_elsewhere_is_read_once_and_an_unknown_blocks_nothing() {
        let graph = Graph::new(&[("D", &["A", "GONE-1"]), ("A", &["B"]), ("B", &["A", "D"])]);
        assert_eq!(graph.check(&[], "D"), Outcome::Write);
        let mut reads = graph.reads.borrow().clone();
        reads.sort_unstable();
        assert_eq!(reads, ["A", "B", "D", "GONE-1"]);
    }

    #[test]
    fn a_failed_read_stops_the_check() {
        let mut graph = Graph::new(&[("D", &["A"]), ("A", &[])]);
        graph.broken.push("A");
        let failed = check(&"T", &[], &"D", |id| graph.read(id));
        assert_eq!(failed, Err("A: the tracker failed".to_owned()));
    }

    #[test]
    fn blockers_are_the_relations_and_the_line_without_the_ticket_itself() {
        let description = "Some text.\n- Blocked by OWL-2, OWL-1 and OWL-3.\n";
        assert_eq!(
            blockers_of("OWL-1", ["OWL-3", "OWL-4"], description),
            ["OWL-3", "OWL-4", "OWL-2"]
        );
    }
}
