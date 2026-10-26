//! Resources: what two tickets in flight cannot hold at once (architecture
//! section 5, "Parallel", "What a ticket holds").
//!
//! A ticket holds zones (folders of the repository), named resources (the
//! keys of `stack.resources`, each a [`Pattern`] over repository paths) and
//! files, each the zone of its own path. A ticket takes them at its Build's
//! admission (OWL-448): its plan's files ([`planned`]), or intake's zones and
//! named resources when no plan names files; [`held`] adds the zones its
//! `zone:` labels declare and the named resources its zones reach; [`added`]
//! is what an admission adds to what a ticket holds; [`closed`] gives the
//! collision rule everything a ticket holds, the files its branch changed
//! included (OWL-403).

use std::collections::BTreeMap;
use std::fmt;

/// Something a ticket in flight holds exclusively.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Resource {
    /// A code zone: a directory of the repository, relative to its root, with
    /// `/` separators (`leases/`, `backend/billing`). The empty zone is the
    /// whole repository. Zones come from validated relative paths, so `..`
    /// never appears.
    Zone(String),
    /// A named resource the project declares, such as a migration chain.
    Named(String),
    /// The machine's single browser slot.
    Browser,
}

impl Resource {
    /// Whether two tickets holding these resources would collide.
    ///
    /// Two zones collide when one contains the other; they compare segment
    /// by segment, ignoring empty and `.` segments and ASCII case, so a
    /// case-insensitive file system never hides a collision. Named resources
    /// collide by name, the browser slot with itself; resources of different
    /// kinds never collide.
    pub fn conflicts_with(&self, other: &Resource) -> bool {
        match (self, other) {
            (Resource::Zone(a), Resource::Zone(b)) => {
                let (a, b) = (segments(a), segments(b));
                contains(&a, &b) || contains(&b, &a)
            }
            (Resource::Named(a), Resource::Named(b)) => a == b,
            (Resource::Browser, Resource::Browser) => true,
            _ => false,
        }
    }

    /// Whether holding this resource holds `other` already: a zone holds
    /// every zone inside it, itself included, compared as
    /// [`Self::conflicts_with`] compares them; a named resource and the
    /// browser slot hold themselves.
    pub fn contains(&self, other: &Resource) -> bool {
        match (self, other) {
            (Resource::Zone(a), Resource::Zone(b)) => contains(&segments(a), &segments(b)),
            _ => self == other,
        }
    }
}

/// How a reason names a resource: a zone by its path, a file being the zone
/// of its own path; the empty zone as the whole repository; a named resource
/// by its name.
impl fmt::Display for Resource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Resource::Zone(zone) if segments(zone).is_empty() => {
                f.write_str("the whole repository")
            }
            Resource::Zone(zone) => write!(f, "zone `{zone}`"),
            Resource::Named(name) => write!(f, "resource `{name}`"),
            Resource::Browser => f.write_str("the browser slot"),
        }
    }
}

/// Everything a ticket holds, as the collision rule weighs it (architecture
/// section 5, "What a ticket holds"): each of `zones`, the whole repository
/// when there is none; each of `files`, the zone of its own path; each of
/// `resources`; then each named resource of `named` that a zone's path can
/// reach ([`Pattern::reaches`]) or a file matches ([`Pattern::matches`]), so a
/// named resource conflicts with any zone or file its pattern reaches. Each
/// once, in that order.
pub fn closed(
    zones: &[String],
    files: &[String],
    resources: &[String],
    named: &NamedResources,
) -> Vec<Resource> {
    let mut held: Vec<Resource> = Vec::new();
    let push = |resource: Resource, held: &mut Vec<Resource>| {
        if !held.contains(&resource) {
            held.push(resource);
        }
    };
    if zones.is_empty() {
        push(Resource::Zone(String::new()), &mut held);
    }
    for zone in zones.iter().chain(files) {
        push(Resource::Zone(zone.clone()), &mut held);
    }
    for name in resources {
        push(Resource::Named(name.clone()), &mut held);
    }
    for (name, pattern) in named.iter() {
        let reached = zones.is_empty()
            || zones.iter().any(|zone| pattern.reaches(zone))
            || files.iter().any(|file| pattern.matches(file));
        if reached {
            push(Resource::Named(name.to_owned()), &mut held);
        }
    }
    held
}

/// The zones a plan's `files` hold at its Build's admission (OWL-448;
/// architecture section 5, "Resources admitted at Build"): each entry the
/// zone of its own path, so a file holds itself, read fail-closed. Blanks
/// around it, leading `./` and trailing `/` are dropped, and a part holding
/// `*`, `?`, `[` or `]` is a pattern that cuts the path to the folder before
/// it, the whole repository when it is the first: a Next.js dynamic segment
/// (`[id]`) holds its parent folder. Parentheses are part of a path, as a
/// Next.js route group's (`(app)`) are (OWL-458). `None`, so that intake's
/// zones hold as for a plan without files, when the plan names no file or
/// an entry is no path from the repository's root by the rules of a `zone:`
/// label ([`check_zone`](crate::decider::check_zone)), or holds a blank or
/// one of `` ` ' " { } , < > | ; ``: a list or a note written in its place.
/// More than `max` entries hold the whole repository, as a widening past
/// its bound does. The whole repository is no zone: `Some` of none.
pub fn planned(files: &[String], max: usize) -> Option<Vec<String>> {
    const NOT_A_PATH: &str = "`'\"{},<>|;";
    const PATTERN: &[char] = &['*', '?', '[', ']'];
    if files.is_empty() {
        return None;
    }
    let mut zones: Vec<String> = Vec::new();
    for entry in files {
        let mut path = entry.trim();
        while let Some(rest) = path.strip_prefix("./") {
            path = rest;
        }
        let path = path.trim_end_matches('/');
        if crate::decider::check_zone(path).is_err()
            || path
                .chars()
                .any(|c| c.is_whitespace() || NOT_A_PATH.contains(c))
        {
            return None;
        }
        let kept: Vec<&str> = path
            .split('/')
            .take_while(|part| !part.contains(PATTERN))
            .collect();
        if kept.is_empty() {
            return Some(Vec::new());
        }
        let zone = kept.join("/");
        if !zones.contains(&zone) {
            zones.push(zone);
        }
    }
    if zones.len() > max {
        return Some(Vec::new());
    }
    Some(zones)
}

/// The resources of `wanted` that nothing of `held` holds already
/// ([`Resource::contains`]), in their order: what a Build's admission adds
/// to what a ticket holds, and checks (OWL-448).
pub fn added(held: &[Resource], wanted: &[Resource]) -> Vec<Resource> {
    wanted
        .iter()
        .filter(|resource| !held.iter().any(|kept| kept.contains(resource)))
        .cloned()
        .collect()
}

/// A zone's segments, without the empty and `.` ones.
pub(crate) fn segments(zone: &str) -> Vec<&str> {
    zone.split('/')
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .collect()
}

/// Whether the zone of segments `outer` contains the zone of segments
/// `inner`, itself included, comparing segments in any ASCII case.
pub(crate) fn contains(outer: &[&str], inner: &[&str]) -> bool {
    outer.len() <= inner.len()
        && outer
            .iter()
            .zip(inner)
            .all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// Whether two zones are the same zone: each contains the other, compared
/// part by part in any ASCII case, so `Web/`, `./web` and `web` are one.
pub(crate) fn same_zone(a: &str, b: &str) -> bool {
    let (a, b) = (segments(a), segments(b));
    contains(&a, &b) && contains(&b, &a)
}

/// One part of a [`Pattern`].
#[derive(Clone, Debug, PartialEq, Eq)]
enum Part {
    /// This part, in any ASCII case.
    Literal(String),
    /// `*`: exactly one part.
    One,
    /// `**`: any number of parts, none included.
    Any,
}

/// A named resource's pattern (`stack.resources`): a path from the
/// repository's root, `/` between its parts, where a part `*` stands for
/// exactly one part and a part `**` for any number of parts, none included.
/// Parts compare in any ASCII case, as zones do. A pattern describes paths:
/// `db/*` matches `db/schema.sql`, never `db/main/0001.sql`; write
/// `db/*/**` for everything below `db`'s folders.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    text: String,
    parts: Vec<Part>,
}

impl Pattern {
    /// Checks and keeps `text`. A pattern may not be empty, start or end
    /// with `/`, hold `\` or a control character, or have an empty, `.` or
    /// `..` part: `db/` would describe one path, not the folder's. A `*` is a whole
    /// part, `*` or `**`: `*.sql` or `a**` is refused, since `*` never stands
    /// for part of a name. Adjacent `**` parts are kept as one.
    pub fn new(text: &str) -> Result<Self, &'static str> {
        if text.chars().any(char::is_control) {
            return Err("holds a control character");
        }
        if text.contains('\\') {
            return Err("holds `\\`: separate parts with `/`");
        }
        if text.is_empty() {
            return Err("is empty: write a path from the repository's root");
        }
        if text.starts_with('/') {
            return Err("starts with `/`: write a path from the repository's root");
        }
        if text.ends_with('/') {
            return Err(
                "ends with `/`: a pattern describes paths, so write `db/**` for \
                        everything in the folder `db`",
            );
        }
        let mut parts: Vec<Part> = Vec::new();
        for part in text.split('/') {
            let part = match part {
                "" | "." | ".." => return Err("has an empty, `.` or `..` part"),
                "*" => Part::One,
                "**" => Part::Any,
                _ if part.contains('*') => {
                    return Err(
                        "has a part holding `*` that is neither `*` nor `**`: a `*` \
                                stands for whole parts",
                    );
                }
                _ => Part::Literal(part.to_owned()),
            };
            if !(part == Part::Any && parts.last() == Some(&Part::Any)) {
                parts.push(part);
            }
        }
        Ok(Self {
            text: text.to_owned(),
            parts,
        })
    }

    /// The pattern as written.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Whether the path matches the pattern, whole. The path's empty and `.`
    /// parts are left out, as a zone's are.
    pub fn matches(&self, path: &str) -> bool {
        self.walk(&segments(path), false)
    }

    /// Whether the pattern can reach the zone: some path inside it, the zone
    /// itself included, matches. The whole repository (the empty zone)
    /// reaches every pattern.
    pub fn reaches(&self, zone: &str) -> bool {
        self.walk(&segments(zone), true)
    }

    /// Walks `path` through the parts. `prefix`: the path need only lead to
    /// a match, any parts after it free. Each state (part, path index) is
    /// visited once, so chained `**` cost no more than one.
    fn walk(&self, path: &[&str], prefix: bool) -> bool {
        let (parts, len) = (&self.parts, path.len());
        // reached[i][j]: parts[..i] can consume path[..j].
        let mut reached = vec![vec![false; len + 1]; parts.len() + 1];
        reached[0][0] = true;
        for i in 0..=parts.len() {
            for j in 0..=len {
                if !reached[i][j] {
                    continue;
                }
                if prefix && j == len {
                    // The rest of the parts can always be met by some path.
                    return true;
                }
                let Some(part) = parts.get(i) else { continue };
                match part {
                    Part::Any => {
                        reached[i + 1][j] = true;
                        if j < len {
                            reached[i][j + 1] = true;
                        }
                    }
                    Part::One if j < len => reached[i + 1][j + 1] = true,
                    Part::Literal(literal) if j < len && literal.eq_ignore_ascii_case(path[j]) => {
                        reached[i + 1][j + 1] = true;
                    }
                    _ => {}
                }
            }
        }
        reached[parts.len()][len]
    }
}

/// Checks a named resource's name: not empty, and only ASCII letters,
/// digits, `.`, `_` and `-`, so that it reads the same in a comment, a
/// brief and a refusal.
pub fn check_resource_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("is empty: name the resource");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err("may hold only ASCII letters, digits, `.`, `_` and `-`");
    }
    Ok(())
}

/// The named resources a project declares (`stack.resources`), each name
/// checked by [`check_resource_name`] and each pattern by [`Pattern::new`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NamedResources {
    entries: BTreeMap<String, Pattern>,
}

impl NamedResources {
    /// Checks and keeps `(name, pattern)` entries; a name given twice keeps
    /// its last pattern.
    pub fn new<N: Into<String>, P: AsRef<str>>(
        entries: impl IntoIterator<Item = (N, P)>,
    ) -> Result<Self, NamedResourceError> {
        let mut kept = BTreeMap::new();
        for (name, pattern) in entries {
            let name = name.into();
            let checked = check_resource_name(&name)
                .and_then(|()| Pattern::new(pattern.as_ref()))
                .map_err(|reason| NamedResourceError {
                    name: name.clone(),
                    reason,
                })?;
            kept.insert(name, checked);
        }
        Ok(Self { entries: kept })
    }

    /// Whether the project declares a resource of this name, written
    /// exactly.
    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// Each resource with its pattern, by name.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Pattern)> {
        self.entries.iter().map(|(name, p)| (name.as_str(), p))
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A named resource that cannot be kept: its name and what is wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamedResourceError {
    pub name: String,
    pub reason: &'static str,
}

impl fmt::Display for NamedResourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?}: the name or its pattern {}",
            self.name, self.reason
        )
    }
}

impl std::error::Error for NamedResourceError {}

/// What a ticket holds, from its intake on ([`held`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Held {
    /// The zones, each as first written; empty: the whole repository.
    pub zones: Vec<String>,
    /// The named resources, intake's first, in its order, then those its
    /// zones reach, by name.
    pub resources: Vec<String>,
}

/// What a ticket whose intake declared `intake_zones` and
/// `intake_resources` holds, with `label_zones` from its `zone:` labels and
/// the project's `named` resources (architecture section 5, "Declared at
/// intake"). An intake that names no zone holds the whole repository, which
/// labels cannot narrow. Otherwise the zones are intake's and the labels',
/// each once, a zone another contains left out. The resources are intake's
/// that the project still names, then each named resource whose pattern
/// reaches a held zone: all of them for the whole repository.
pub fn held(
    intake_zones: &[String],
    label_zones: &[String],
    intake_resources: &[String],
    named: &NamedResources,
) -> Held {
    let mut zones: Vec<String> = Vec::new();
    if !intake_zones.is_empty() {
        let candidates: Vec<&String> = intake_zones.iter().chain(label_zones).collect();
        for (i, zone) in candidates.iter().enumerate() {
            let inside_another = candidates.iter().enumerate().any(|(j, other)| {
                let (outer, inner) = (segments(other), segments(zone));
                i != j && contains(&outer, &inner) && !contains(&inner, &outer)
            });
            if !inside_another && !zones.iter().any(|kept| same_zone(kept, zone)) {
                zones.push((*zone).clone());
            }
        }
    }
    let mut resources: Vec<String> = Vec::new();
    for name in intake_resources {
        // A resource the project no longer names is held by nobody else.
        if named.contains(name) && !resources.contains(name) {
            resources.push(name.clone());
        }
    }
    for (name, pattern) in named.iter() {
        let reached = zones.is_empty() || zones.iter().any(|zone| pattern.reaches(zone));
        if reached && !resources.iter().any(|kept| kept == name) {
            resources.push(name.to_owned());
        }
    }
    Held { zones, resources }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone(path: &str) -> Resource {
        Resource::Zone(path.to_owned())
    }

    fn named(name: &str) -> Resource {
        Resource::Named(name.to_owned())
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn zones_collide_when_one_contains_the_other() {
        let colliding = [
            ("leases/", "leases"),
            ("leases", "leases/models"),
            ("./leases/", "leases/models/lease.py"),
            ("Leases", "leases/models"),
            ("", "anything/at/all"),
        ];
        for (a, b) in colliding {
            assert!(zone(a).conflicts_with(&zone(b)), "{a:?} vs {b:?}");
            assert!(zone(b).conflicts_with(&zone(a)), "{b:?} vs {a:?}");
        }
        let apart = [
            ("leases", "lease"),
            ("leases", "leasesx"),
            ("backend/billing", "backend/leases"),
        ];
        for (a, b) in apart {
            assert!(!zone(a).conflicts_with(&zone(b)), "{a:?} vs {b:?}");
        }
    }

    #[test]
    fn named_resources_and_the_browser_collide_only_with_themselves() {
        assert!(named("migrations").conflicts_with(&named("migrations")));
        assert!(!named("migrations").conflicts_with(&named("ports")));
        assert!(Resource::Browser.conflicts_with(&Resource::Browser));

        let kinds = [zone("migrations"), named("migrations"), Resource::Browser];
        for (i, a) in kinds.iter().enumerate() {
            for (j, b) in kinds.iter().enumerate() {
                if i != j {
                    assert!(!a.conflicts_with(b), "{a:?} vs {b:?}");
                }
            }
        }
    }

    #[test]
    fn a_pattern_matches_whole_paths_part_by_part() {
        let pattern = Pattern::new("backend/*/migrations/**").unwrap();
        for path in [
            "backend/billing/migrations",
            "backend/billing/migrations/0001.py",
            "Backend/Billing/MIGRATIONS/a/b.py",
            "./backend//billing/migrations/x",
        ] {
            assert!(pattern.matches(path), "{path}");
        }
        for path in [
            "backend/migrations/0001.py",
            "backend/a/b/migrations/0001.py",
            "backend/billing",
            "frontend/billing/migrations/x",
        ] {
            assert!(!pattern.matches(path), "{path}");
        }
        // `**` takes no part at either end too; `*` exactly one.
        let any = Pattern::new("**/schema.sql").unwrap();
        assert!(any.matches("schema.sql") && any.matches("a/b/schema.sql"));
        assert!(!any.matches("a/schema.sql/x"));
        let one = Pattern::new("db/*").unwrap();
        assert_eq!(one.as_str(), "db/*");
        assert!(one.matches("db/main") && !one.matches("db") && !one.matches("db/main/x"));
        assert!(Pattern::new("**").unwrap().matches(""));
    }

    #[test]
    fn a_pattern_reaches_a_zone_some_path_inside_it_matches() {
        let pattern = Pattern::new("backend/*/migrations/**").unwrap();
        for zone in [
            "",
            "backend",
            "Backend/billing/",
            "backend/billing/migrations",
            "backend/billing/migrations/0001.py",
        ] {
            assert!(pattern.reaches(zone), "{zone:?}");
        }
        for zone in ["frontend", "backend/billing/models", "backendx"] {
            assert!(!pattern.reaches(zone), "{zone:?}");
        }
        // A pattern describes paths: a zone below a path `db/*` matches is
        // not reached, as the files under it would not match.
        let one = Pattern::new("db/*").unwrap();
        assert!(one.reaches("db/main"));
        assert!(!one.reaches("db/main/sub"));
    }

    #[test]
    fn chained_globs_are_walked_in_linear_states() {
        let pattern = Pattern::new("a/**/**/**/**/**/**/x").unwrap();
        let deep = format!("a/{}", vec!["p"; 200].join("/"));
        assert!(!pattern.matches(&deep));
        assert!(pattern.reaches(&deep));
        assert!(pattern.matches(&format!("{deep}/x")));
        assert!(
            !Pattern::new("a/**/**/b/**/**/c/**/**/d")
                .unwrap()
                .matches(&deep)
        );
    }

    #[test]
    fn patterns_and_names_that_cannot_describe_paths_are_refused() {
        for (text, reason) in [
            ("", "is empty"),
            ("/", "starts with `/`"),
            ("db/migrations/", "ends with `/`"),
            ("/db", "starts with `/`"),
            ("db\\x", "`\\`"),
            ("db\nx", "control character"),
            ("db//x", "empty, `.` or `..`"),
            ("./db", "empty, `.` or `..`"),
            ("db/..", "empty, `.` or `..`"),
            ("db/*.sql", "neither `*` nor `**`"),
            ("db/a**", "neither `*` nor `**`"),
            ("***", "neither `*` nor `**`"),
        ] {
            let error = Pattern::new(text).unwrap_err();
            assert!(error.contains(reason), "{text:?}: {error}");
        }
        for name in ["migrations", "db.migrations", "Ports_2-x"] {
            assert_eq!(check_resource_name(name), Ok(()), "{name}");
        }
        for name in ["", "two words", "db/migrations", "é", "a:b"] {
            assert!(check_resource_name(name).is_err(), "{name:?}");
        }
        let error = NamedResources::new([("ok", "db/**"), ("bad", "db/*.sql")]).unwrap_err();
        assert_eq!(error.name, "bad");
        assert!(error.to_string().starts_with("\"bad\": "), "{error}");
        assert!(NamedResources::new([("a b", "db")]).is_err());
        let kept = NamedResources::new([("b", "x"), ("a", "y/**")]).unwrap();
        let names: Vec<&str> = kept.iter().map(|(name, _)| name).collect();
        assert_eq!(names, ["a", "b"]);
        assert!(kept.contains("a") && !kept.contains("A"));
    }

    #[test]
    fn the_held_set_unites_intake_labels_and_the_resources_zones_reach() {
        let named = NamedResources::new([
            ("migrations", "db/*/migrations/**"),
            ("assets", "static/**"),
            ("lock", "Cargo.lock"),
        ])
        .unwrap();

        // Intake's zones and the labels', each once, a contained one left
        // out; the resources intake named, then those the zones reach.
        let found = held(
            &strings(&["db/main", "web/cart", "Web/"]),
            &strings(&["web", "docs", "db/main/migrations"]),
            &strings(&["assets", "assets"]),
            &named,
        );
        assert_eq!(found.zones, ["db/main", "Web/", "docs"]);
        assert_eq!(found.resources, ["assets", "migrations"]);

        // No zone from intake: the whole repository, labels or not, and so
        // every named resource.
        let whole = held(&[], &strings(&["docs"]), &strings(&["lock"]), &named);
        assert_eq!(whole.zones, Vec::<String>::new());
        assert_eq!(whole.resources, ["lock", "assets", "migrations"]);

        // A resource the project no longer names is not held.
        let renamed = held(&strings(&["docs"]), &[], &strings(&["old", "lock"]), &named);
        assert_eq!(renamed.resources, ["lock"]);

        // Nothing reached, nothing named: no resource.
        let narrow = held(&strings(&["docs"]), &[], &[], &named);
        assert_eq!(
            narrow,
            Held {
                zones: strings(&["docs"]),
                resources: Vec::new(),
            }
        );
        assert_eq!(
            held(&[], &[], &[], &NamedResources::default()),
            Held::default()
        );
    }

    /// OWL-403: what the collision rule weighs. A file reaches a named
    /// resource only when it matches its pattern, a zone when a path inside
    /// it can; no zone is the whole repository, which reaches them all.
    #[test]
    fn the_closed_set_adds_the_named_resources_zones_reach_and_files_match() {
        let project =
            NamedResources::new([("migrations", "db/*/migrations/**"), ("lock", "Cargo.lock")])
                .unwrap();
        // `db/main` as a file is no folder: nothing below it can match.
        let found = closed(
            &strings(&["web"]),
            &strings(&["db/main", "Cargo.lock", "web/a.rs"]),
            &strings(&["assets"]),
            &project,
        );
        assert_eq!(
            found,
            [
                zone("web"),
                zone("db/main"),
                zone("Cargo.lock"),
                zone("web/a.rs"),
                named("assets"),
                named("lock"),
            ]
        );
        let reached = closed(&strings(&["db/main"]), &[], &[], &project);
        assert_eq!(reached, [zone("db/main"), named("migrations")]);
        let whole = closed(&[], &[], &[], &project);
        assert_eq!(whole, [zone(""), named("lock"), named("migrations")]);
        assert_eq!(whole[0].to_string(), "the whole repository");
        assert_eq!(reached[0].to_string(), "zone `db/main`");
        assert_eq!(reached[1].to_string(), "resource `migrations`");
    }

    /// OWL-448: a plan's files as its Build's admission holds them, read
    /// fail-closed: a pattern holds the folder it lies in, one from the root
    /// the whole repository, as do more entries than the bound; a list
    /// written in place of a path, a note or a path outside the repository
    /// leaves the plan's files unread, and so does a plan naming none.
    #[test]
    fn a_plans_files_are_read_as_zones_fail_closed() {
        let read = |files: &[&str]| planned(&strings(files), 3);
        assert_eq!(
            read(&[" ./src/lib.rs ", "docs/", "src/lib.rs", "a/*/b.rs"]),
            Some(strings(&["src/lib.rs", "docs", "a"]))
        );
        assert_eq!(read(&["src/a.rs", "**/*.rs"]), Some(Vec::new()));
        assert_eq!(read(&["a", "b", "c", "d"]), Some(Vec::new()));
        for unread in [
            &[][..],
            &["src/lib.rs (new)"],
            &["`src/lib.rs`"],
            &["src/{a,b}.rs"],
            &["/etc/passwd"],
            &["../x.rs"],
            &["a\\b.rs"],
            &["./"],
        ] {
            assert_eq!(read(unread), None, "{unread:?}");
        }
    }

    /// OWL-458: Next.js route paths, found on Locary's LOC-1030, whose
    /// route group made the whole plan unread: a group's parentheses are
    /// part of the path, and a dynamic segment's brackets, read as a
    /// pattern, hold the folder before it.
    #[test]
    fn a_plans_nextjs_route_paths_are_read() {
        let read = |files: &[&str]| planned(&strings(files), 3);
        assert_eq!(
            read(&[
                "frontend/src/app/(app)/dashboard/leases/import/page.js",
                "frontend/src/app/(app)/dashboard/leases/[id]/page.js",
                "src/a?.rs",
            ]),
            Some(strings(&[
                "frontend/src/app/(app)/dashboard/leases/import/page.js",
                "frontend/src/app/(app)/dashboard/leases",
                "src",
            ]))
        );
        assert_eq!(read(&["[id]/page.js"]), Some(Vec::new()));
    }

    /// OWL-448: what an admission adds is what nothing held holds already,
    /// a zone holding every zone inside it.
    #[test]
    fn an_admission_adds_only_what_is_not_held_already() {
        let held = [zone("app"), named("migrations"), zone("docs/a.md")];
        let wanted = [
            zone("app/x.rs"),
            zone("docs"),
            named("migrations"),
            named("lock"),
            zone("App"),
        ];
        assert_eq!(added(&held, &wanted), [zone("docs"), named("lock")]);
        assert_eq!(
            added(&[zone("")], &wanted),
            [named("migrations"), named("lock")]
        );
    }
}
