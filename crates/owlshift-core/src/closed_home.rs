//! What a confined command's output says the sandbox refused in the home
//! (OWL-597; build plan, "P9 tasks & exit gate", "OWL-597").
//!
//! Neither system keeps a record of the sandbox's refusals that can be read
//! afterwards (macOS's unified log holds no deny line for a `sandbox-exec`
//! profile, checked live on 2026-11-18), so the command's own output is read:
//! [`scan`] finds the paths under the home that a line refuses, and
//! [`folder_of`] the folder of the home each one names, the one a project
//! would declare in `stack.caches`. Whether that folder is on the machine,
//! and whether the sandbox opens it already, is the runner's to tell.
//!
//! A path is refused when its line holds a refusal: `EPERM`, `EACCES`,
//! `EROFS`, "Operation not permitted", "Permission denied" or "Read-only file
//! system" (macOS, and an allowed cache written on Linux), or `ENOENT` and "No
//! such file or directory" (a closed folder on Linux, whose private home holds
//! nothing). A line that names an error's `path` or `dest`, as npm and Node
//! print an error's fields one to a line, also takes the refusal of the three
//! lines before it.

use crate::agent_env::{CredentialRelation, check_home_folders, credential_relation};

/// The folders of the home that only gather others: the folder named under
/// one is one part deeper (`.cache/uv`, `Library/pnpm`).
const GATHERING: &[&str] = &[
    ".cache",
    ".config",
    ".local",
    ".local/share",
    ".local/state",
    "Library",
    "Library/Caches",
    "Library/Application Support",
    "Library/Preferences",
    "Library/Containers",
    "Library/Group Containers",
];

/// The names of macOS's gathering folders that hold a space, read across it
/// in a path no quote closes: `(first word, rest)`.
const SPACED: &[(&str, &str)] = &[("Application", " Support"), ("Group", " Containers")];

/// The refusals a line may hold, any case for the phrases.
const DENIED_CODES: &[&str] = &["EPERM", "EACCES", "EROFS"];
const DENIED_PHRASES: &[&str] = &[
    "operation not permitted",
    "permission denied",
    "read-only file system",
];
const MISSING_CODES: &[&str] = &["ENOENT"];
const MISSING_PHRASES: &[&str] = &["no such file or directory"];

/// How many lines before an error's `path` line hold its refusal.
const FIELD_LINES: usize = 3;

/// What becomes of a folder named.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A folder a project may declare as a cache.
    Offer,
    /// It is, lies in or holds a credential path at every depth: never
    /// opened.
    Credential,
    /// The home's first-level folder that is not hidden (`Projects`), or a
    /// folder that only gathers others (`.cache`): too wide to open.
    Wide,
}

/// A folder of the home a failing command's output says was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    /// The folder, from the home, `/` between its parts.
    pub folder: String,
    /// The first refused path that named it, from the home.
    pub path: String,
    pub kind: Kind,
    /// Whether every line that named it said only that a file is missing:
    /// a refusal on Linux only when the folder is on the machine.
    pub missing_only: bool,
}

/// What a line says.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Sign {
    Missing,
    Denied,
}

/// The folders of the home `output` says were refused, each once, in the
/// order first met. `homes` are the home as the commands saw it and its
/// real path, absolute, without a trailing `/`; `~/` is always read as the
/// home too.
pub fn scan(output: &str, homes: &[&str]) -> Vec<Found> {
    let lines: Vec<&str> = output.lines().collect();
    let signs: Vec<Option<Sign>> = lines.iter().map(|line| sign(line)).collect();
    let mut found: Vec<Found> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let mut said = signs[i];
        if is_field_line(line) {
            said = signs[i.saturating_sub(FIELD_LINES)..=i]
                .iter()
                .copied()
                .max()
                .flatten();
        }
        let Some(said) = said else { continue };
        for path in home_paths(line, homes) {
            let Some((folder, kind)) = folder_of(&path) else {
                continue;
            };
            let missing = said == Sign::Missing;
            match found
                .iter_mut()
                .find(|seen| seen.folder.eq_ignore_ascii_case(&folder))
            {
                Some(seen) => seen.missing_only &= missing,
                None => found.push(Found {
                    folder,
                    path,
                    kind,
                    missing_only: missing,
                }),
            }
        }
    }
    found
}

/// The folder of the home a refused `path` (from the home, `/` between its
/// parts, none empty, `.` or `..`) names, and what becomes of it: the first
/// part, one deeper under a folder that only gathers others (`.cache`,
/// `.local/share`, `Library/Caches`, …), and deeper while it would hold a
/// credential path. `None` for a path no project could declare (a
/// part starting with `~`, a backslash, a control character) and for the
/// home itself.
pub fn folder_of(path: &str) -> Option<(String, Kind)> {
    let parts: Vec<&str> = path.split('/').collect();
    let plain = |part: &&str| {
        !part.is_empty()
            && !part.starts_with('~')
            && !part.contains('\\')
            && !part.chars().any(char::is_control)
    };
    if !parts.iter().all(plain) {
        return None;
    }
    let gathered = GATHERING
        .iter()
        .map(|folder| folder.split('/').collect::<Vec<_>>())
        .filter(|folder| {
            folder.len() <= parts.len()
                && folder
                    .iter()
                    .zip(&parts)
                    .all(|(a, b)| a.eq_ignore_ascii_case(b))
        })
        .map(|folder| folder.len())
        .max();
    let first = gathered.map_or(1, |depth| depth + 1);
    if parts.len() < first {
        return Some((path.to_owned(), Kind::Wide));
    }
    if gathered.is_none() && !parts[0].starts_with('.') {
        return Some((parts[0].to_owned(), Kind::Wide));
    }
    for depth in first..=parts.len() {
        let folder = parts[..depth].join("/");
        match credential_relation(&folder) {
            Some(CredentialRelation::Within) => return Some((folder, Kind::Credential)),
            Some(CredentialRelation::Holds) => {}
            None => {
                return check_home_folders(&[folder.as_str()])
                    .ok()
                    .map(|()| (folder, Kind::Offer));
            }
        }
    }
    Some((path.to_owned(), Kind::Credential))
}

/// The strongest refusal `line` holds.
fn sign(line: &str) -> Option<Sign> {
    let lower = line.to_ascii_lowercase();
    let holds = |codes: &[&str], phrases: &[&str]| {
        codes.iter().any(|code| has_word(line, code))
            || phrases.iter().any(|phrase| lower.contains(phrase))
    };
    if holds(DENIED_CODES, DENIED_PHRASES) {
        Some(Sign::Denied)
    } else if holds(MISSING_CODES, MISSING_PHRASES) {
        Some(Sign::Missing)
    } else {
        None
    }
}

/// Whether `word` stands in `line` with no letter or digit beside it.
fn has_word(line: &str, word: &str) -> bool {
    line.match_indices(word).any(|(at, _)| {
        let before = line[..at].chars().next_back();
        let after = line[at + word.len()..].chars().next();
        !before.is_some_and(|c| c.is_ascii_alphanumeric())
            && !after.is_some_and(|c| c.is_ascii_alphanumeric())
    })
}

/// Whether `line` names an error's `path` or `dest` field, as npm (`npm
/// error path /…`, `npm ERR! path /…`) and Node (`path: '/…'`) print it.
fn is_field_line(line: &str) -> bool {
    let mut rest = line.trim_start();
    for prefix in ["npm error ", "npm ERR! "] {
        if let Some(after) = rest.strip_prefix(prefix) {
            rest = after.trim_start();
        }
    }
    ["path", "dest"].iter().any(|field| {
        rest.strip_prefix(field)
            .is_some_and(|after| after.starts_with(|c: char| c == ':' || c.is_whitespace()))
    })
}

/// The paths under the home `line` names, each from the home, `/` between
/// its parts: after one of `homes` or `~`, standing alone (not inside a
/// longer path) and followed by `/`; to the matching quote when a quote
/// opens it, else to a blank or a sign that ends a path in a message. A path
/// with a `..` part is left out; `.` and empty parts are dropped.
fn home_paths(line: &str, homes: &[&str]) -> Vec<String> {
    let mut paths = Vec::new();
    for home in homes.iter().copied().chain(["~"]) {
        if home.is_empty() {
            continue;
        }
        for (at, _) in line.match_indices(home) {
            let before = line[..at].chars().next_back();
            if before.is_some_and(|c| c.is_ascii_alphanumeric() || "._~/-".contains(c)) {
                continue;
            }
            let start = at + home.len();
            if !line[start..].starts_with('/') {
                continue;
            }
            let quote = before.filter(|c| "'\"`".contains(*c));
            let raw = match quote {
                Some(quote) => line[start..].split(quote).next().unwrap_or_default(),
                None => unquoted(&line[start..]),
            };
            if let Some(path) = from_home(raw)
                && !paths.contains(&path)
            {
                paths.push(path);
            }
        }
    }
    paths
}

/// The path at the start of `rest`, no quote around it: to a blank or one of
/// the signs a message puts after a path, read across the space of a
/// [`SPACED`] folder name.
fn unquoted(rest: &str) -> &str {
    let stop = |c: char| c.is_whitespace() || "'\"`,;:()[]{}<>|".contains(c);
    let mut end = rest.find(stop).unwrap_or(rest.len());
    while let Some(more) = SPACED.iter().find_map(|(word, more)| {
        let last = rest[..end].rsplit('/').next().unwrap_or_default();
        (last == *word && rest[end..].starts_with(more)).then_some(more.len())
    }) {
        end += more;
        end += rest[end..].find(stop).unwrap_or(rest.len() - end);
    }
    &rest[..end]
}

/// `raw`, a path after the home's `/`, from the home: trailing `.` and `/`
/// trimmed, empty and `.` parts dropped; `None` for a `..` part or nothing
/// left.
fn from_home(raw: &str) -> Option<String> {
    let trimmed = raw.trim_end_matches(['.', '/']);
    let parts: Vec<&str> = trimmed
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    if parts.is_empty() || parts.contains(&"..") {
        return None;
    }
    Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/Users/op";

    /// A folder named, what becomes of it, and whether only a missing file
    /// named it.
    type Named = (String, Kind, bool);

    fn folders(output: &str) -> Vec<Named> {
        scan(output, &[HOME])
            .into_iter()
            .map(|found| (found.folder, found.kind, found.missing_only))
            .collect()
    }

    fn offer(folder: &str) -> Named {
        (folder.to_owned(), Kind::Offer, false)
    }

    /// The refusals recorded live on 2026-11-18 (build plan, "OWL-597"), the
    /// home written as the tools printed it, and lines that must name nothing.
    #[test]
    fn the_recorded_refusals_name_their_folders() {
        let cases: &[(&str, &str, Vec<Named>)] = &[
            (
                "npm 11.12.1, the cache's path two lines below its code",
                "npm error code EPERM\nnpm error syscall open\n\
                 npm error path /Users/op/.npm/_cacache/content-v2/sha512/e3/50/a27e\n\
                 npm error errno -1\nnpm error\n\
                 npm error Your cache folder contains root-owned files, due to a bug in previous \
                 versions of npm which has since been addressed.\n\
                 npm error   sudo chown -R 501:20 \"/Users/op/.npm\"\n\
                 npm error Log files were not written due to an error writing to the directory: \
                 /Users/op/.npm/_logs",
                vec![offer(".npm")],
            ),
            (
                "pnpm 10.33.2, its store allowed read-only",
                " EPERM  EPERM: operation not permitted, symlink '../../../../../../../private/var/\
                 folders/x/T/run/wt' -> '/Users/op/Library/pnpm/store/v10/projects/49c858b6'",
                vec![offer("Library/pnpm")],
            ),
            (
                "pip 26.0.1, --user",
                "WARNING: The directory '/Users/op/Library/Caches/pip' or its parent directory is \
                 not owned or is not writable by the current user. The cache has been disabled.\n\
                 ERROR: Could not install packages due to an OSError: [Errno 1] Operation not \
                 permitted: '/Users/op/.local/lib'",
                vec![offer(".local/lib")],
            ),
            (
                "uv, in backquotes",
                "error: Failed to initialize cache at `/Users/op/.cache/uv`\n  Caused by: failed to \
                 open file `/Users/op/.cache/uv/sdists-v9/.git`: Operation not permitted (os error 1)",
                vec![offer(".cache/uv")],
            ),
            (
                "macOS's ls and mkdir, the path ended by a colon",
                "ls: /Users/op/.cache: Operation not permitted\n\
                 mkdir: /Users/op/.cache/demo: Operation not permitted",
                vec![
                    (".cache".to_owned(), Kind::Wide, false),
                    offer(".cache/demo"),
                ],
            ),
            (
                "Linux, an allowed cache written, and a closed folder read",
                "touch: cannot touch '/Users/op/.cache/allowed/new': Read-only file system\n\
                 cat: /Users/op/.cache/closed/file: No such file or directory\n\
                 ls: cannot access '~/.cache/closed': No such file or directory",
                vec![
                    offer(".cache/allowed"),
                    (".cache/closed".to_owned(), Kind::Offer, true),
                ],
            ),
            (
                "a test's panic beside a backtrace frame in the tool chains",
                "thread 'x' panicked at src/lib.rs:3:5: Permission denied (os error 13)\n\
                 at /Users/op/.cargo/registry/src/index/serde-1.0.0/src/de.rs:12",
                vec![],
            ),
            (
                "a longer path, a sibling home, a home alone and a parent part",
                "EPERM /private/Users/op/.npm/x and /Users/opx/.npm and /Users/op: denied, \
                 EACCES /Users/op/.npm/../.ssh/id_ed25519",
                vec![],
            ),
            (
                "an unquoted macOS folder with a space",
                "open /Users/op/Library/Application Support/Foo/cache.db: Operation not permitted",
                vec![offer("Library/Application Support/Foo")],
            ),
        ];
        for (what, output, want) in cases {
            assert_eq!(&folders(output), want, "{what}");
        }
    }

    /// The folder named: under a gathering folder one part deeper, past a
    /// folder that holds a credential path, never a credential path itself,
    /// and too wide for the home's own folders and the gathering ones.
    #[test]
    fn a_folder_is_named_wide_of_no_credential() {
        let cases: &[(&str, Option<(&str, Kind)>)] = &[
            (".npm/_cacache/tmp/x", Some((".npm", Kind::Offer))),
            (".yarn/berry/cache/a.zip", Some((".yarn", Kind::Offer))),
            (
                "Library/Caches/Yarn/v6",
                Some(("Library/Caches/Yarn", Kind::Offer)),
            ),
            (
                ".local/share/uv/python",
                Some((".local/share/uv", Kind::Offer)),
            ),
            (
                ".gradle/caches/modules-2",
                Some((".gradle/caches", Kind::Offer)),
            ),
            (".m2/repository/org", Some((".m2/repository", Kind::Offer))),
            (".ssh/id_ed25519", Some((".ssh", Kind::Credential))),
            (".SSH/id_ed25519", Some((".SSH", Kind::Credential))),
            (
                ".config/gh/hosts.yml",
                Some((".config/gh", Kind::Credential)),
            ),
            (".npmrc", Some((".npmrc", Kind::Credential))),
            (".cargo", Some((".cargo", Kind::Credential))),
            (".cache", Some((".cache", Kind::Wide))),
            (
                "Library/Application Support",
                Some(("Library/Application Support", Kind::Wide)),
            ),
            ("Projects/web/.env", Some(("Projects", Kind::Wide))),
            ("~odd/x", None),
            (".cache/a\\b", None),
        ];
        for (path, want) in cases {
            let got = folder_of(path);
            let got = got.as_ref().map(|(folder, kind)| (folder.as_str(), *kind));
            assert_eq!(got, *want, "{path}");
        }
    }
}
