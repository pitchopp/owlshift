//! Owlshift's published releases, as `owlshift self-update` reads them
//! (OWL-541; build plan, "P9 · Open-source readiness", part 11): the version
//! of the latest one, from where `<base>/releases/latest` redirects, and its
//! files, from `<base>/releases/download/v<version>/<file>`.
//!
//! Nothing read here is trusted: the redirect, the files and the host that
//! serves them are checked by the signature of `SHA256SUMS`, which the caller
//! verifies with the key built into the binary. This module only keeps what
//! it reads from reaching a URL or a path in an unexpected shape, and bounds
//! what it downloads.

use std::time::Duration;

use ureq::http::Uri;

/// GitHub's page of Owlshift's repository, under which its releases are.
pub const DEFAULT_BASE: &str = "https://github.com/pitchopp/owlshift";

/// The most a small file of a release (`SHA256SUMS`, its signature) may weigh.
pub const SMALL_FILE_LIMIT: u64 = 64 * 1024;

/// The most an archive may weigh: a release's archive weighs about 10 MB.
pub const ARCHIVE_LIMIT: u64 = 64 * 1024 * 1024;

/// What `<base>/releases/latest` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Latest {
    /// The latest published release, `v<version>`.
    Version(String),
    /// No release is published yet: GitHub redirects to the releases page.
    NoneYet,
}

/// The releases under one base: GitHub's by default, a local server in the
/// tests, or a mirror.
pub struct Releases {
    base: String,
    probe: ureq::Agent,
    download: ureq::Agent,
}

impl std::fmt::Debug for Releases {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Releases")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl Releases {
    /// The releases under `base`, which is HTTPS, or plain HTTP to this
    /// machine only (`127.0.0.1`, `localhost`, `[::1]`).
    pub fn new(base: &str) -> Result<Self, String> {
        let base = base.trim_end_matches('/').to_owned();
        let https = check_base(&base)?;
        let download = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(300)))
            .http_status_as_error(false)
            // GitHub's asset URLs redirect to its storage, on another host.
            .max_redirects(10)
            .https_only(https)
            .user_agent(concat!("owlshift/", env!("CARGO_PKG_VERSION")))
            .build();
        Ok(Self {
            base,
            // Follows no redirect: the one `releases/latest` answers is read.
            probe: crate::forge::github::agent(Duration::from_secs(30)),
            download: ureq::Agent::new_with_config(download),
        })
    }

    /// The base the releases are read under.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// The version of the latest published release.
    pub fn latest(&self) -> Result<Latest, String> {
        let url = format!("{}/releases/latest", self.base);
        let response = self
            .probe
            .get(&url)
            .call()
            .map_err(|error| format!("could not reach {url}: {error}"))?;
        let status = response.status();
        if !status.is_redirection() {
            return Err(format!(
                "{url} answered {status}, not a redirect to the latest release"
            ));
        }
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| format!("{url} redirects nowhere"))?;
        latest_of(location)
            .ok_or_else(|| format!("{url} redirects to {location}, which names no release version"))
    }

    /// The release file `name` of `version`, at most `limit` bytes.
    pub fn file(&self, version: &str, name: &str, limit: u64) -> Result<Vec<u8>, String> {
        if !is_version(version) {
            return Err(format!("\"{version}\" is not a release version"));
        }
        let url = format!("{}/releases/download/v{version}/{name}", self.base);
        let mut response = self
            .download
            .get(&url)
            .call()
            .map_err(|error| format!("could not download {url}: {error}"))?;
        let status = response.status();
        if status != 200 {
            return Err(format!("could not download {url}: it answered {status}"));
        }
        response
            .body_mut()
            .with_config()
            .limit(limit)
            .read_to_vec()
            .map_err(|error| format!("could not download {url}: {error}"))
    }
}

/// Whether `base` may be read, and whether it is HTTPS: HTTPS, or plain HTTP
/// to a loopback host; no user, query or fragment.
fn check_base(base: &str) -> Result<bool, String> {
    let refused = |why: &str| Err(format!("{base} cannot serve releases: {why}"));
    let Ok(uri) = base.parse::<Uri>() else {
        return refused("not a URL");
    };
    let Some(authority) = uri.authority() else {
        return refused("not an absolute URL");
    };
    if authority.as_str().contains('@') {
        return refused("a URL with a user name");
    }
    if uri.query().is_some() || base.contains('#') {
        return refused("a URL with a query or a fragment");
    }
    match uri.scheme_str() {
        Some("https") => Ok(true),
        Some("http") if matches!(authority.host(), "127.0.0.1" | "localhost" | "[::1]") => {
            Ok(false)
        }
        Some("http") => refused("plain HTTP is allowed to this machine only"),
        _ => refused("only https is allowed"),
    }
}

/// What a redirect of `releases/latest` to `location` says: a path ending in
/// `/releases/tag/v<version>`, or in `/releases` when nothing is published.
fn latest_of(location: &str) -> Option<Latest> {
    let uri = location.parse::<Uri>().ok()?;
    let path = uri.path().trim_end_matches('/');
    if path.ends_with("/releases") {
        return Some(Latest::NoneYet);
    }
    let (rest, version) = path.rsplit_once("/v")?;
    (rest.ends_with("/releases/tag") && is_version(version))
        .then(|| Latest::Version(version.to_owned()))
}

/// Whether `version` is a release version such as `1.2.3`, with an optional
/// pre-release or build suffix of ASCII letters, digits, `.` and `-`: the
/// rule `install.sh` applies before a version reaches a URL or a path.
pub fn is_version(version: &str) -> bool {
    let (core, suffix) = match version.find(['-', '+']) {
        Some(at) => (&version[..at], Some(&version[at + 1..])),
        None => (version, None),
    };
    let numbers: Vec<&str> = core.split('.').collect();
    numbers.len() == 3
        && numbers
            .iter()
            .all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        && suffix.is_none_or(|s| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_base_is_https_or_http_to_this_machine() {
        for (base, https) in [
            ("https://github.com/pitchopp/owlshift", true),
            ("http://127.0.0.1:8080/case", false),
            ("http://localhost:1/x", false),
            ("http://[::1]:1", false),
        ] {
            assert_eq!(check_base(base), Ok(https), "{base}");
        }
        for base in [
            "http://github.com/pitchopp/owlshift",
            "http://127.0.0.1@evil.example/",
            "https://user@github.com/x",
            "ftp://127.0.0.1/",
            "file:///tmp/releases",
            "https://github.com/x?y=1",
            "github.com/pitchopp/owlshift",
        ] {
            assert!(check_base(base).is_err(), "{base}");
        }
    }

    #[test]
    fn the_latest_release_is_read_from_the_redirects_path() {
        for (location, latest) in [
            (
                "https://github.com/pitchopp/owlshift/releases/tag/v0.2.0",
                Some(Latest::Version("0.2.0".into())),
            ),
            (
                "/case/releases/tag/v1.0.0-rc.1",
                Some(Latest::Version("1.0.0-rc.1".into())),
            ),
            (
                "https://github.com/pitchopp/owlshift/releases",
                Some(Latest::NoneYet),
            ),
            // Observed on 2026-11-12, with no release published.
            (
                "https://github.com/pitchopp/owlshift/releases/",
                Some(Latest::NoneYet),
            ),
            ("https://github.com/x/releases/tag/0.2.0", None),
            ("https://github.com/x/releases/tag/v0.2", None),
            ("https://github.com/x/releases/tag/v0.2.0/..", None),
            ("https://evil.example/?x=/releases/tag/v1.0.0", None),
            ("https://github.com/x/tag/v1.0.0", None),
        ] {
            assert_eq!(latest_of(location), latest, "{location}");
        }
    }

    #[test]
    fn a_version_is_three_numbers_and_an_optional_suffix() {
        for good in ["0.1.0", "10.20.30", "1.0.0-rc.1", "1.0.0+build-5"] {
            assert!(is_version(good), "{good}");
        }
        for bad in [
            "", "0.1", "0.1.0.0", "v0.1.0", "0.1.0-", "0.1.0/..", "0.1.0\n", "a.b.c",
        ] {
            assert!(!is_version(bad), "{bad:?}");
        }
    }
}
