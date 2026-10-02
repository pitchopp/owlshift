//! Deciders: who answers a ticket's questions (architecture, section 4).
//!
//! The decider is the ticket's assignee whenever it has one. A ticket
//! without an assignee falls back on the owners the project names for its
//! code zones: when the owned zones it declared all belong to one person,
//! that person decides; otherwise nobody does, and [`NoDecider`] says why.
//! Accounts are the tracker's identifiers, compared as written.
//!
//! Until intake declares zones (P6), a ticket declares them with labels
//! `zone:<folder>` ([`declared_zones`]).

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::resource::{Resource, contains, segments};

/// The prefix of a label that declares a zone, such as
/// `zone:backend/billing`; its case does not matter.
pub const ZONE_LABEL_PREFIX: &str = "zone:";

/// The zones a ticket declares with its labels: each label `zone:<folder>`,
/// the prefix in any ASCII case, gives the folder that follows it, without
/// the spaces around it. Other labels are left out. A `zone:` label whose
/// folder is not one by the rules of an owned zone ([`ZoneOwners::new`]) is
/// refused rather than left out, since a dropped declaration would change
/// who decides unseen.
pub fn declared_zones<'a>(
    labels: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<Resource>, ZoneLabelError> {
    let mut zones = Vec::new();
    for label in labels {
        let Some(prefix) = label.get(..ZONE_LABEL_PREFIX.len()) else {
            continue;
        };
        if !prefix.eq_ignore_ascii_case(ZONE_LABEL_PREFIX) {
            continue;
        }
        let zone = label[ZONE_LABEL_PREFIX.len()..].trim();
        check_zone(zone).map_err(|reason| ZoneLabelError {
            label: label.to_owned(),
            reason,
        })?;
        zones.push(Resource::Zone(zone.to_owned()));
    }
    Ok(zones)
}

/// A `zone:` label that names no folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZoneLabelError {
    pub label: String,
    pub reason: &'static str,
}

impl fmt::Display for ZoneLabelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the label {:?} declares a zone that {}",
            self.label, self.reason
        )
    }
}

impl std::error::Error for ZoneLabelError {}

/// The owners a project names for its code zones (`[zones]` in the project
/// file): each zone a folder from the repository's root, owned by one
/// tracker account.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ZoneOwners {
    /// Each owned zone as written, with its owner.
    owned: Vec<(String, String)>,
}

impl ZoneOwners {
    /// Checks and keeps `(zone, owner)` entries. A zone is a folder from the
    /// repository's root, `/` between its parts and at most one `/` at the
    /// end; no part may be empty, `.` or `..`, so the whole repository cannot
    /// be owned. Two entries may not name the same zone, compared part by
    /// part in any ASCII case. An owner may be neither blank nor padded with
    /// spaces, since it must equal a tracker account's identifier.
    pub fn new<Z: Into<String>, O: Into<String>>(
        entries: impl IntoIterator<Item = (Z, O)>,
    ) -> Result<Self, ZoneOwnersError> {
        let mut owned: Vec<(String, String)> = Vec::new();
        for (zone, owner) in entries {
            let (zone, owner) = (zone.into(), owner.into());
            if let Err(reason) = check_zone(&zone) {
                return Err(ZoneOwnersError::BadZone { zone, reason });
            }
            if owner.trim().is_empty() || owner.trim() != owner {
                return Err(ZoneOwnersError::BadOwner { zone, owner });
            }
            let same = segments(&zone);
            if let Some((first, _)) = owned.iter().find(|(other, _)| {
                let other = segments(other);
                contains(&other, &same) && contains(&same, &other)
            }) {
                return Err(ZoneOwnersError::SameZone {
                    first: first.clone(),
                    second: zone,
                });
            }
            owned.push((zone, owner));
        }
        Ok(Self { owned })
    }

    /// The owner of `zone`: the owner of the deepest owned zone that contains
    /// it, itself included. A zone that only contains owned zones has none.
    pub fn owner_of(&self, zone: &str) -> Option<&str> {
        let zone = segments(zone);
        self.owned
            .iter()
            .map(|(owned, owner)| (segments(owned), owner))
            .filter(|(owned, _)| contains(owned, &zone))
            .max_by_key(|(owned, _)| owned.len())
            .map(|(_, owner)| owner.as_str())
    }
}

/// Checks the path of an owned zone, see [`ZoneOwners::new`].
fn check_zone(zone: &str) -> Result<(), &'static str> {
    if zone.chars().any(char::is_control) {
        return Err("holds a control character: name a folder by its path");
    }
    if zone.contains('\\') {
        return Err("holds `\\`: separate folders with `/`");
    }
    if zone.starts_with('/') {
        return Err("starts with `/`: name a folder from the repository's root");
    }
    let folder = zone.strip_suffix('/').unwrap_or(zone);
    if folder.is_empty() {
        return Err("is the whole repository, which cannot be owned: name a folder");
    }
    if folder
        .split('/')
        .any(|part| matches!(part, "" | "." | ".."))
    {
        return Err("is not a folder's path: no part of it may be empty, `.` or `..`");
    }
    Ok(())
}

/// Why zone owners cannot be kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ZoneOwnersError {
    /// A zone that is not a folder's path from the repository's root.
    BadZone { zone: String, reason: &'static str },
    /// An owner that is blank or padded with spaces.
    BadOwner { zone: String, owner: String },
    /// Two entries naming the same zone, such as `Billing` and `billing/`.
    SameZone { first: String, second: String },
}

impl fmt::Display for ZoneOwnersError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadZone { zone, reason } => write!(f, "{zone:?} {reason}"),
            Self::BadOwner { zone, owner } => write!(
                f,
                "{zone:?}: the owner {owner:?} is blank or has a space at either end: write \
                 the tracker account's identifier exactly"
            ),
            Self::SameZone { first, second } => {
                write!(f, "{first:?} and {second:?} name the same zone")
            }
        }
    }
}

impl std::error::Error for ZoneOwnersError {}

/// Who decides a ticket, and by which rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Decider<'a> {
    /// The ticket's assignee.
    Assignee(&'a str),
    /// No assignee: the one owner of every owned zone the ticket declared.
    ZoneOwner(&'a str),
}

impl<'a> Decider<'a> {
    /// The decider's tracker account identifier.
    pub const fn account(self) -> &'a str {
        match self {
            Self::Assignee(account) | Self::ZoneOwner(account) => account,
        }
    }

    /// The rule that made them the decider.
    pub const fn rule(self) -> DeciderRule {
        match self {
            Self::Assignee(_) => DeciderRule::Assignee,
            Self::ZoneOwner(_) => DeciderRule::ZoneOwner,
        }
    }
}

/// The rule that made someone a ticket's decider, as a ticket ref records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeciderRule {
    /// The ticket's assignee.
    Assignee,
    /// The one owner of the owned zones a ticket without an assignee declared.
    ZoneOwner,
}

/// Why a ticket has no decider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoDecider<'a> {
    /// No assignee, and none of the zones the ticket declared has an owner.
    NoOwnedZone,
    /// No assignee, and the owned zones the ticket declared belong to
    /// several people: their identifiers, sorted, each once.
    SeveralOwners(Vec<&'a str>),
}

impl fmt::Display for NoDecider<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoOwnedZone => write!(
                f,
                "the ticket has no assignee and touches no zone with an owner: assign it to \
                 the person who decides its questions"
            ),
            Self::SeveralOwners(owners) => write!(
                f,
                "the ticket has no assignee and touches zones of several owners ({}): assign \
                 it to the one who decides its questions",
                owners.join(", ")
            ),
        }
    }
}

/// The decider of a ticket with this assignee (its tracker account
/// identifier) and these declared resources. A blank assignee counts as
/// none; zones without an owner, named resources and the browser are left
/// out. Resolve it when a question is asked: it stays the decider of that
/// ask.
pub fn decider<'a>(
    assignee: Option<&'a str>,
    resources: &[Resource],
    owners: &'a ZoneOwners,
) -> Result<Decider<'a>, NoDecider<'a>> {
    if let Some(assignee) = assignee.filter(|account| !account.trim().is_empty()) {
        return Ok(Decider::Assignee(assignee));
    }
    let mut found: Vec<&str> = resources
        .iter()
        .filter_map(|resource| match resource {
            Resource::Zone(zone) => owners.owner_of(zone),
            Resource::Named(_) | Resource::Browser => None,
        })
        .collect();
    found.sort_unstable();
    found.dedup();
    match found.as_slice() {
        [] => Err(NoDecider::NoOwnedZone),
        [owner] => Ok(Decider::ZoneOwner(owner)),
        _ => Err(NoDecider::SeveralOwners(found)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owners(entries: &[(&str, &str)]) -> ZoneOwners {
        ZoneOwners::new(entries.iter().copied()).unwrap()
    }

    fn zones(paths: &[&str]) -> Vec<Resource> {
        paths
            .iter()
            .map(|path| Resource::Zone((*path).to_owned()))
            .collect()
    }

    #[test]
    fn the_assignee_decides_whenever_there_is_one() {
        let owned = owners(&[("billing", "bob")]);
        for (assignee, declared, expected) in [
            // Assignee only.
            (Some("ann"), vec![], Ok(Decider::Assignee("ann"))),
            // Both: the assignee wins over the zone owner.
            (
                Some("ann"),
                zones(&["billing"]),
                Ok(Decider::Assignee("ann")),
            ),
            // A blank assignee counts as none.
            (
                Some(" "),
                zones(&["billing"]),
                Ok(Decider::ZoneOwner("bob")),
            ),
            (Some(""), vec![], Err(NoDecider::NoOwnedZone)),
        ] {
            assert_eq!(
                decider(assignee, &declared, &owned),
                expected,
                "{assignee:?}, {declared:?}"
            );
        }
        assert_eq!(Decider::ZoneOwner("bob").account(), "bob");
    }

    #[test]
    fn without_an_assignee_the_one_owner_of_the_owned_zones_decides() {
        let owned = owners(&[("billing", "bob"), ("billing/tax", "tia"), ("web", "bob")]);
        for declared in [
            // Zone owner only.
            zones(&["billing/api"]),
            // Several zones, one owner, a zone repeated.
            zones(&["billing", "web/cart", "Billing/"]),
            // An unowned zone is left out.
            zones(&["billing", "docs"]),
            // So are named resources and the browser.
            [
                zones(&["./billing"]),
                vec![Resource::Named("tia".to_owned()), Resource::Browser],
            ]
            .concat(),
        ] {
            assert_eq!(
                decider(None, &declared, &owned),
                Ok(Decider::ZoneOwner("bob")),
                "{declared:?}"
            );
        }
        assert_eq!(
            decider(None, &zones(&["billing/tax/vat"]), &owned),
            Ok(Decider::ZoneOwner("tia"))
        );
    }

    #[test]
    fn without_an_assignee_or_a_single_owner_nobody_decides() {
        let owned = owners(&[("backend/billing", "bob"), ("web", "Ann"), ("app", "ann")]);
        for declared in [
            // Neither an assignee nor a zone.
            vec![],
            // Only unowned zones, or resources that are not zones.
            zones(&["docs", "backend/orders"]),
            vec![Resource::Named("web".to_owned()), Resource::Browser],
            // A zone broader than any owned zone has no owner.
            zones(&["backend"]),
        ] {
            assert_eq!(
                decider(None, &declared, &owned),
                Err(NoDecider::NoOwnedZone),
                "{declared:?}"
            );
        }
        let several = decider(None, &zones(&["web", "backend/billing", "web/a"]), &owned);
        assert_eq!(several, Err(NoDecider::SeveralOwners(vec!["Ann", "bob"])));
        assert!(
            several
                .unwrap_err()
                .to_string()
                .contains("several owners (Ann, bob)")
        );
        // Identifiers compare as written: `Ann` and `ann` are two accounts.
        assert_eq!(
            decider(None, &zones(&["web", "app"]), &owned),
            Err(NoDecider::SeveralOwners(vec!["Ann", "ann"]))
        );
    }

    #[test]
    fn the_deepest_owned_zone_that_contains_a_zone_owns_it() {
        let nested = [("billing", "bob"), ("billing/tax", "tia")];
        let reversed = [nested[1], nested[0]];
        for owned in [owners(&nested), owners(&reversed)] {
            for (zone, owner) in [
                ("billing", Some("bob")),
                ("billing/api", Some("bob")),
                ("Billing/Tax/", Some("tia")),
                ("./billing/tax/vat", Some("tia")),
                ("billingx", None),
                ("bill", None),
                ("", None),
            ] {
                assert_eq!(owned.owner_of(zone), owner, "{zone:?}");
            }
        }
    }

    #[test]
    fn zone_labels_declare_the_zones_that_choose_the_owner() {
        let declared = declared_zones([
            "Feature",
            "zone:billing",
            "ZONE: Billing/ ",
            "zones:web",
            "zone",
            "é",
        ])
        .unwrap();
        assert_eq!(declared, zones(&["billing", "Billing/"]));
        // The label's case never changes the owner, and a zone declared twice
        // is one owner, not several.
        let owned = owners(&[("billing", "bob"), ("web", "ann")]);
        assert_eq!(
            decider(None, &declared, &owned),
            Ok(Decider::ZoneOwner("bob"))
        );
        assert_eq!(Decider::ZoneOwner("bob").rule(), DeciderRule::ZoneOwner);
        assert_eq!(Decider::Assignee("ann").rule(), DeciderRule::Assignee);
        assert_eq!(declared_zones(["agent", "Bug"]), Ok(Vec::new()));

        for (label, reason) in [
            ("zone:", "the whole repository"),
            ("Zone:../secrets", "`.` or `..`"),
            ("zone:/billing", "starts with `/`"),
            ("zone:billing\\tax", "separate folders with `/`"),
        ] {
            let error = declared_zones(["zone:web", label]).unwrap_err();
            assert_eq!(error.label, label);
            let text = error.to_string();
            assert!(text.contains(reason), "{label}: {text}");
            assert!(text.contains(&format!("{label:?}")), "{text}");
        }
    }

    #[test]
    fn zone_owners_refuse_what_cannot_name_a_folder_or_an_account() {
        for (entries, error) in [
            (vec![("", "bob")], "\"\" is the whole repository"),
            (vec![("/", "bob")], "starts with `/`"),
            (vec![("/billing", "bob")], "starts with `/`"),
            (vec![("billing\\tax", "bob")], "separate folders with `/`"),
            (vec![("billing\n", "bob")], "control character"),
            (vec![("billing//tax", "bob")], "no part of it may be empty"),
            (vec![("./billing", "bob")], "`.` or `..`"),
            (vec![("billing/..", "bob")], "`.` or `..`"),
            (
                vec![("billing/", "bob"), ("ok", " ")],
                "\"ok\": the owner \" \"",
            ),
            (vec![("billing", " bob")], "has a space at either end"),
            (
                vec![("Billing", "bob"), ("billing/", "tia")],
                "\"Billing\" and \"billing/\" name the same zone",
            ),
        ] {
            let actual = ZoneOwners::new(entries.iter().copied())
                .unwrap_err()
                .to_string();
            assert!(actual.contains(error), "{entries:?}: {actual}");
        }
        assert!(ZoneOwners::new([("billing/", "bob"), ("billing/tax", "bob")]).is_ok());
    }
}
