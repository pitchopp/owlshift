//! The commits Owlshift pushes, with closing keywords in their messages
//! broken (OWL-558; build plan, "The GitHub Issues adapter", Delivery).
//!
//! GitHub closes an issue from a closing keyword ("Fixes #5") in any commit
//! message that reaches the default branch: through a merge commit, a
//! rebase, or a squash whose message GitHub builds from the commits, which
//! is what a person's merge button proposes and what Owlshift's own merge
//! on green gets (live check of 2026-11-14,
//! `docs/design/live-checks/owl-558.sh`). So before each push the runner
//! sends the *neutral image* of the commits that push brings: every commit
//! reachable from the pushed one and not from the push's bases (the base,
//! the commit Owlshift last pushed), rewritten byte for byte with its
//! message passed through [`neutralise_closing_keywords`], its parents
//! replaced by their images, and the headers a rewrite invalidates
//! (`gpgsig`, `gpgsig-sha256`, `mergetag`) dropped. Its tree, author,
//! committer and dates are kept. An image is thus a function of the commit
//! and its ancestry alone: a push made again after a crash sends the same
//! ids, and a commit with no keyword whose parents are unchanged keeps its
//! id, no object written.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use owlshift_adapters::forge::CommitId;

use crate::executor::Git;
use crate::writer::neutralise_closing_keywords;

/// A commit whose history, down to the bases of its push, holds no live
/// closing keyword: the only commit the Writer pushes. Only
/// [`neutral_history`] makes one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NeutralHead(CommitId);

impl NeutralHead {
    pub fn commit(&self) -> &CommitId {
        &self.0
    }

    /// A head taken as neutral, for a test of what the Writer does with it.
    #[cfg(test)]
    pub(crate) fn assumed(commit: CommitId) -> Self {
        Self(commit)
    }
}

/// What making a push's history neutral did.
#[derive(Clone, Debug)]
pub struct Neutral {
    /// The commit to push.
    pub head: NeutralHead,
    /// Each commit rewritten, the agent's id then its image's, parents
    /// first. Empty when every message was already neutral.
    pub rewritten: Vec<(CommitId, CommitId)>,
}

impl Neutral {
    /// The image of `commit`: itself unless it was rewritten.
    pub fn image(&self, commit: &CommitId) -> CommitId {
        self.rewritten
            .iter()
            .find(|(agent, _)| agent == commit)
            .map_or_else(|| commit.clone(), |(_, image)| image.clone())
    }
}

/// The neutral image of `head`'s history down to `bases`, its rewritten
/// commits written to the object store of `checkout`.
pub(crate) fn neutral_history(
    git: &Git,
    checkout: &Path,
    head: &CommitId,
    bases: &[CommitId],
) -> Result<Neutral, String> {
    let (head, rewritten) = rewrite(git, checkout, head, bases, true)?;
    Ok(Neutral {
        head: NeutralHead(head),
        rewritten,
    })
}

/// The id [`neutral_history`] gives `head`, computed without writing any
/// object: for a check that only reads.
pub(crate) fn image(
    git: &Git,
    checkout: &Path,
    head: &CommitId,
    bases: &[CommitId],
) -> Result<CommitId, String> {
    rewrite(git, checkout, head, bases, false).map(|(head, _)| head)
}

/// Rewrites, parents first, each commit `git rev-list` lists from `head`
/// less `bases`, replace objects ignored; `write` puts the images in the
/// object store, otherwise only their ids are computed. `git hash-object`
/// checks each image as git checks a commit.
fn rewrite(
    git: &Git,
    checkout: &Path,
    head: &CommitId,
    bases: &[CommitId],
    write: bool,
) -> Result<(CommitId, Vec<(CommitId, CommitId)>), String> {
    let mut list = vec![
        "--no-replace-objects".to_owned(),
        "rev-list".to_owned(),
        "--reverse".to_owned(),
        "--topo-order".to_owned(),
        "--end-of-options".to_owned(),
        head.to_string(),
    ];
    list.extend(bases.iter().map(|base| format!("^{base}")));
    let listed = git.run(checkout, &list).map_err(|e| e.to_string())?;
    let mut hash = vec!["hash-object", "-t", "commit"];
    if write {
        hash.push("-w");
    }
    hash.push("--stdin");
    let mut images: HashMap<String, String> = HashMap::new();
    let mut rewritten = Vec::new();
    for agent in String::from_utf8_lossy(&listed).lines().map(str::trim) {
        let raw = git
            .run(
                checkout,
                &["--no-replace-objects", "cat-file", "commit", agent],
            )
            .map_err(|e| e.to_string())?;
        let Some(object) = neutral_object(&raw, &images) else {
            continue;
        };
        let made = git
            .run_with_input(checkout, &hash, &object)
            .map_err(|e| e.to_string())?;
        let made = String::from_utf8_lossy(&made).trim().to_owned();
        let pair = (
            CommitId::new(agent).map_err(|e| e.to_string())?,
            CommitId::new(made.as_str()).map_err(|e| e.to_string())?,
        );
        images.insert(agent.to_owned(), made);
        rewritten.push(pair);
    }
    let head = match images.get(head.as_str()) {
        Some(image) => CommitId::new(image.as_str()).map_err(|e| e.to_string())?,
        None => head.clone(),
    };
    Ok((head, rewritten))
}

/// The raw commit object `raw` with its message's closing keywords broken
/// and each parent named in `images` replaced by its image; `None` when
/// neither changes anything. A message that is not UTF-8 is read lossily,
/// and only when that reading holds a keyword is it written back, in UTF-8,
/// its `encoding` header dropped. Once anything changes, the signatures and
/// the merged tags' headers, which no longer match, are dropped.
fn neutral_object(raw: &[u8], images: &HashMap<String, String>) -> Option<Vec<u8>> {
    let (headers, message) = match raw.windows(2).position(|pair| pair == b"\n\n") {
        Some(at) => (&raw[..at], &raw[at + 2..]),
        None => (raw.strip_suffix(b"\n").unwrap_or(raw), &[][..]),
    };
    let (broken, lossy) = match std::str::from_utf8(message) {
        Ok(text) => {
            let broken = neutralise_closing_keywords(text);
            ((broken != text).then(|| broken.into_bytes()), false)
        }
        Err(_) => {
            let text = String::from_utf8_lossy(message);
            let broken = neutralise_closing_keywords(&text);
            let changed = broken != text;
            (changed.then(|| broken.into_bytes()), changed)
        }
    };
    // Each header with its continuation lines, which start with a space.
    let mut entries: Vec<Vec<u8>> = Vec::new();
    for line in headers.split(|&byte| byte == b'\n') {
        match (line.first(), entries.last_mut()) {
            (Some(b' '), Some(entry)) => {
                entry.push(b'\n');
                entry.extend_from_slice(line);
            }
            _ => entries.push(line.to_vec()),
        }
    }
    let mut moved = false;
    for entry in &mut entries {
        let image = entry
            .strip_prefix(b"parent ")
            .and_then(|id| std::str::from_utf8(id).ok())
            .and_then(|id| images.get(id));
        if let Some(image) = image {
            *entry = format!("parent {image}").into_bytes();
            moved = true;
        }
    }
    if broken.is_none() && !moved {
        return None;
    }
    entries.retain(|entry| {
        let name = entry.split(|&byte| byte == b' ').next().unwrap_or_default();
        !matches!(name, b"gpgsig" | b"gpgsig-sha256" | b"mergetag") && !(lossy && name == b"encoding")
    });
    let mut object = entries.join(&b'\n');
    object.extend_from_slice(b"\n\n");
    object.extend_from_slice(broken.as_deref().unwrap_or(message));
    Some(object)
}

/// `ledger`, a Build ledger's text (`roles/build.md`, "Planning": steps,
/// each with the `commit` it landed in), with each step's `commit` that
/// names a rewritten commit, by its full id or an abbreviation of at least
/// 7 digits that names that one alone, naming its image instead, at the
/// same length: so the next Build run finds its done steps on the branch.
/// Only those values change, the rest of the text as it was. `None` when
/// the text is no JSON with `steps`, or names no rewritten commit.
pub(crate) fn remap_ledger(ledger: &str, rewritten: &[(CommitId, CommitId)]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(ledger).ok()?;
    let steps = value.get("steps")?.as_array()?;
    let mut text = ledger.to_owned();
    let mut seen = HashSet::new();
    for named in steps
        .iter()
        .filter_map(|step| step.get("commit")?.as_str())
    {
        let abbreviation = (7..=64).contains(&named.len())
            && named.bytes().all(|byte| byte.is_ascii_hexdigit());
        if !abbreviation || !seen.insert(named) {
            continue;
        }
        let lower = named.to_ascii_lowercase();
        let mut matches = rewritten
            .iter()
            .filter(|(agent, _)| agent.as_str().starts_with(&lower));
        if let (Some((_, image)), None) = (matches.next(), matches.next())
            && let Some(image) = image.as_str().get(..named.len())
        {
            text = replace_commit_values(&text, named, image);
        }
    }
    (text != ledger).then_some(text)
}

/// `text` with each JSON string `"<from>"` that is the value of a `commit`
/// key, `"commit": "<from>"`, made `"<to>"`.
fn replace_commit_values(text: &str, from: &str, to: &str) -> String {
    let quoted = format!("\"{from}\"");
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(&quoted) {
        out.push_str(&rest[..at]);
        let key = out
            .trim_end()
            .strip_suffix(':')
            .is_some_and(|before| before.trim_end().ends_with("\"commit\""));
        if key {
            out.push('"');
            out.push_str(to);
            out.push('"');
        } else {
            out.push_str(&quoted);
        }
        rest = &rest[at + quoted.len()..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use super::*;

    fn id(hex: char) -> CommitId {
        CommitId::new(hex.to_string().repeat(40)).unwrap()
    }

    fn images(pairs: &[(&CommitId, &CommitId)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(agent, image)| (agent.to_string(), image.to_string()))
            .collect()
    }

    /// The message's keywords are broken; a parent named in `images` is its
    /// image; the signatures and merged tags go once anything changes, every
    /// other header stays as it was; nothing changed gives `None`.
    #[test]
    fn an_object_keeps_all_but_its_live_keywords_parents_and_signatures() {
        let (a, b, c) = (id('a'), id('b'), id('c'));
        let raw = format!(
            "tree {t}\nparent {a}\nparent {b}\nauthor A <a@x> 1 +0000\ncommitter C <c@x> 2 +0000\n\
             mergetag object {b}\n type commit\n tag v1\ngpgsig -----BEGIN PGP SIGNATURE-----\n \n abc\n \
             -----END PGP SIGNATURE-----\n\nFixes #5\n\nSigned-off-by: A <a@x>\n",
            t = id('e')
        );
        let neutral = neutral_object(raw.as_bytes(), &images(&[(&a, &c)])).unwrap();
        assert_eq!(
            String::from_utf8(neutral).unwrap(),
            format!(
                "tree {t}\nparent {c}\nparent {b}\nauthor A <a@x> 1 +0000\ncommitter C <c@x> 2 +0000\
                 \n\nFi\u{200B}xes #5\n\nSigned-off-by: A <a@x>\n",
                t = id('e')
            )
        );
        // A parent moved alone rewrites the object, its message kept.
        let plain = format!("tree {a}\nparent {a}\nauthor A <a@x> 1 +0000\n\nAdd x\n");
        assert_eq!(
            neutral_object(plain.as_bytes(), &images(&[(&a, &c)])),
            Some(plain.replace(&format!("parent {a}"), &format!("parent {c}")).into_bytes())
        );
        assert_eq!(neutral_object(plain.as_bytes(), &HashMap::new()), None);
        assert_eq!(neutral_object(raw.replace("Fixes", "Adds").as_bytes(), &HashMap::new()), None);
    }

    /// A message that is not UTF-8 is read lossily: written back in UTF-8,
    /// its `encoding` header dropped, only when it holds a keyword.
    #[test]
    fn a_message_not_in_utf8_is_rewritten_only_for_a_keyword() {
        let head = format!("tree {}\nauthor A <a@x> 1 +0000\nencoding ISO-8859-1\n\n", id('a'));
        let mut latin = head.clone().into_bytes();
        latin.extend_from_slice(b"Caf\xe9 fixes #5\n");
        let neutral = neutral_object(&latin, &HashMap::new()).unwrap();
        assert_eq!(
            String::from_utf8(neutral).unwrap(),
            format!(
                "tree {}\nauthor A <a@x> 1 +0000\n\nCaf\u{FFFD} fi\u{200B}xes #5\n",
                id('a')
            )
        );
        let mut plain = head.into_bytes();
        plain.extend_from_slice(b"Caf\xe9\n");
        assert_eq!(neutral_object(&plain, &HashMap::new()), None);
    }

    /// Only a step's `commit` naming one rewritten commit alone changes, at
    /// its length; a text that is no ledger, or names none, gives `None`.
    #[test]
    fn a_ledger_follows_its_rewritten_commits() {
        let (a, b) = (id('a'), id('b'));
        let a2 = CommitId::new(format!("{}{}", "a".repeat(39), "1")).unwrap();
        let rewritten = [(a.clone(), id('c')), (b.clone(), id('d')), (a2, id('f'))];
        let ledger = format!(
            "{{\"steps\": [\n  {{\"step\": 1, \"done\": true, \"commit\": \"{b}\"}},\n  \
             {{\"step\": 2, \"done\": true, \"commit\": \"BBBBBBB\"}},\n  \
             {{\"step\": 3, \"done\": true, \"commit\": \"aaaaaaa\"}},\n  \
             {{\"step\": 4, \"title\": \"{b}\", \"done\": false}}\n]}}"
        );
        assert_eq!(
            remap_ledger(&ledger, &rewritten).unwrap(),
            ledger
                .replace(&format!("\"commit\": \"{b}\""), &format!("\"commit\": \"{}\"", id('d')))
                .replace("BBBBBBB", "ddddddd")
        );
        assert_eq!(remap_ledger(&ledger, &[]), None);
        assert_eq!(remap_ledger("{\"steps\": [", &rewritten), None);
        assert_eq!(remap_ledger("{\"plan\": \"x\"}", &rewritten), None);
    }

    /// A checkout with hermetic git.
    struct Repo {
        _tmp: tempfile::TempDir,
        dir: std::path::PathBuf,
        config: std::path::PathBuf,
        git: Git,
    }

    impl Repo {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let root = if cfg!(unix) {
                fs::canonicalize(tmp.path()).unwrap()
            } else {
                tmp.path().to_owned()
            };
            let config = root.join("gitconfig");
            fs::write(
                &config,
                "[user]\n\tname = Bench\n\temail = bench@example.invalid\n\
                 [init]\n\tdefaultBranch = main\n[core]\n\tautocrlf = false\n",
            )
            .unwrap();
            let dir = root.join("checkout");
            fs::create_dir_all(&dir).unwrap();
            let env = config.clone();
            let git = Git::with_setup("git", move |command| {
                command
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .env("GIT_CONFIG_GLOBAL", &env);
            });
            let repo = Self {
                _tmp: tmp,
                dir,
                config,
                git,
            };
            repo.run(&["init", "--quiet"]);
            repo
        }

        fn run(&self, args: &[&str]) -> String {
            let out = Command::new("git")
                .args(args)
                .current_dir(&self.dir)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", &self.config)
                .env("GIT_AUTHOR_DATE", "1700000000 +0000")
                .env("GIT_COMMITTER_DATE", "1700000000 +0000")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        }

        /// A commit of a file `name`, on `HEAD`.
        fn commit(&self, name: &str, message: &str) -> CommitId {
            fs::write(self.dir.join(name), name).unwrap();
            self.run(&["add", name]);
            self.run(&["commit", "--quiet", "-m", message]);
            CommitId::new(self.run(&["rev-parse", "HEAD"])).unwrap()
        }

        fn show(&self, commit: &CommitId, format: &str) -> String {
            self.run(&["show", "-s", &format!("--format={format}"), commit.as_str()])
        }
    }

    /// A branch off a base whose own commit says "Fixes #1", with an agent's
    /// "Fixes #5", a child naming no issue, and a merge of a newer base
    /// commit: the base's commits are never rewritten; each agent commit's
    /// image keeps its tree, author, committer and dates, its parents the
    /// images or the base's; the same call gives the same ids, a call on the
    /// image changes nothing, and `image` computes the head's without
    /// writing it. A history with no keyword keeps its head.
    #[test]
    fn a_history_is_rewritten_above_its_bases_alone_and_always_alike() {
        let repo = Repo::new();
        let base = repo.commit("base", "Fixes #1 on the base");
        let checkout = repo.dir.as_path();
        let git = &repo.git;
        repo.run(&["switch", "--quiet", "-c", "ticket"]);
        let fixes = repo.commit("one", "Add one\n\nFixes #5");
        let child = repo.commit("two", "Add two");
        repo.run(&["switch", "--quiet", "main"]);
        let newer = repo.commit("three", "Closes #2 on the base");
        repo.run(&["switch", "--quiet", "ticket"]);
        repo.run(&["merge", "--quiet", "--no-edit", "-m", "Merge the base", "main"]);
        let merge = CommitId::new(repo.run(&["rev-parse", "HEAD"])).unwrap();

        let neutral = neutral_history(git, checkout, &merge, &[base.clone(), newer.clone()]).unwrap();
        let agents: Vec<&CommitId> = neutral.rewritten.iter().map(|(agent, _)| agent).collect();
        assert_eq!(agents, [&fixes, &child, &merge]);
        let head = neutral.head.commit().clone();
        assert_eq!(neutral.image(&merge), head);
        assert_eq!(neutral.image(&base), base);
        assert_eq!(
            repo.show(&neutral.image(&fixes), "%B"),
            "Add one\n\nFi\u{200B}xes #5"
        );
        assert_eq!(repo.show(&neutral.image(&child), "%B"), "Add two");
        assert_eq!(
            repo.show(&head, "%P"),
            format!("{} {newer}", neutral.image(&child))
        );
        for (agent, image) in &neutral.rewritten {
            let kept = "%T %an %ae %ad %cn %ce %cd";
            assert_eq!(repo.show(agent, kept), repo.show(image, kept));
        }
        assert_eq!(repo.show(&newer, "%B"), "Closes #2 on the base");

        let again = neutral_history(git, checkout, &merge, &[base.clone(), newer.clone()]).unwrap();
        assert_eq!(again.rewritten, neutral.rewritten);
        let settled = neutral_history(git, checkout, &head, &[base.clone(), newer.clone()]).unwrap();
        assert_eq!((settled.head.commit(), settled.rewritten.len()), (&head, 0));
        assert_eq!(image(git, checkout, &merge, &[base, newer]), Ok(head));

        let plain = repo.commit("four", "Add four");
        let kept = neutral_history(git, checkout, &plain, &[merge]).unwrap();
        assert_eq!((kept.head.commit(), kept.rewritten.len()), (&plain, 0));
    }
}
