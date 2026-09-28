//! Resources: what two tickets in flight cannot hold at once.
//!
//! Provisional until P6, which checks collisions in the ready set: the
//! conflict rule below is the model's, and P6 may refine it once real zones
//! are declared at intake.

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
                let shared = a.len().min(b.len());
                a[..shared]
                    .iter()
                    .zip(&b[..shared])
                    .all(|(x, y)| x.eq_ignore_ascii_case(y))
            }
            (Resource::Named(a), Resource::Named(b)) => a == b,
            (Resource::Browser, Resource::Browser) => true,
            _ => false,
        }
    }
}

fn segments(zone: &str) -> Vec<&str> {
    zone.split('/')
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .collect()
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
}
