//! Whether a delivered pull request's conflict with its base is trivial
//! (OWL-210, architecture section 5, "Conflicts and the rebase run"): read
//! from `git merge-tree --write-tree -z --name-only <onto> <head>` in the
//! dedicated checkout after the fetch. A trivial conflict gets the rebase
//! role on the fast tier; any other goes to the Build routing at once.
//!
//! The output, documented from git 2.38.0 and stable at 2.39.0, Owlshift's
//! floor (OWL-199): the tree, then each conflicted path, then an empty
//! field; then one record per message, `<count>NUL<path>…<type>NUL<message>NUL`,
//! its type a stable string. Exit status 0 is a clean merge, 1 a conflict.
//! Checked live with git 2.43.0 on 2026-10-11: a contents conflict, a
//! modify/delete and a binary file each give their own record, the binary
//! one a `CONFLICT (binary)` before its `CONFLICT (contents)`.

use std::path::Path;

use crate::executor::Git;

/// The most paths a trivial conflict touches.
pub const TRIVIAL_PATHS: usize = 3;

/// What the merge of the head into the base found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Classified {
    /// The fast tier is enough.
    pub trivial: bool,
    /// The conflicted paths, in git's order; empty on a clean merge or when
    /// the output could not be read.
    pub paths: Vec<String>,
}

/// Classifies the merge of `head` into `onto`, both commits the checkout
/// holds. `conflicting` is whether the forge reports a conflict: a clean
/// merge then counts as non-trivial, since git and the forge disagree; a
/// clean merge with no conflict reported, a refresh's, is trivial.
pub fn classify(git: &Git, checkout: &Path, onto: &str, head: &str, conflicting: bool) -> Classified {
    let out = git.output(
        checkout,
        &[
            "merge-tree",
            "--write-tree",
            "-z",
            "--name-only",
            "--end-of-options",
            onto,
            head,
        ],
        None,
    );
    match out {
        Ok(out) => parse(out.code, &out.stdout, conflicting),
        Err(_) => Classified {
            trivial: false,
            paths: Vec::new(),
        },
    }
}

/// [`classify`] from git's exit status and standard output: trivial when
/// every record other than `Auto-merging` is `CONFLICT (contents)` and at
/// most [`TRIVIAL_PATHS`] paths conflict. Any other exit status, or output
/// that does not read as documented, is non-trivial.
pub fn parse(code: Option<i32>, stdout: &[u8], conflicting: bool) -> Classified {
    let non_trivial = |paths| Classified {
        trivial: false,
        paths,
    };
    match code {
        Some(0) => {
            return Classified {
                trivial: !conflicting,
                paths: Vec::new(),
            };
        }
        Some(1) => {}
        _ => return non_trivial(Vec::new()),
    }
    let mut fields = stdout.split(|b| *b == 0).map(String::from_utf8_lossy);
    // The tree, then the conflicted paths up to an empty field.
    if fields.next().is_none() {
        return non_trivial(Vec::new());
    }
    let mut paths = Vec::new();
    loop {
        match fields.next() {
            Some(field) if field.is_empty() => break,
            Some(field) => paths.push(field.into_owned()),
            None => return non_trivial(paths),
        }
    }
    let mut only_contents = true;
    let mut records = 0;
    loop {
        let Some(count) = fields.next() else {
            return non_trivial(paths);
        };
        // The output ends with the last record's NUL: one empty field.
        if count.is_empty() && fields.next().is_none() {
            break;
        }
        let Ok(count) = count.parse::<usize>() else {
            return non_trivial(paths);
        };
        for _ in 0..count {
            if fields.next().is_none() {
                return non_trivial(paths);
            }
        }
        let (Some(kind), Some(_message)) = (fields.next(), fields.next()) else {
            return non_trivial(paths);
        };
        records += 1;
        if kind != "Auto-merging" && kind != "CONFLICT (contents)" {
            only_contents = false;
        }
    }
    Classified {
        trivial: only_contents && records > 0 && !paths.is_empty() && paths.len() <= TRIVIAL_PATHS,
        paths,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TREE: &str = "4602d8d66c7dec5be06ff4c1c70a608116d76ec";

    /// Output in git 2.43.0's shape: the tree, the paths, then records of
    /// one path each.
    fn output(paths: &[&str], records: &[(&str, &str)]) -> Vec<u8> {
        let mut out = format!("{TREE}\0");
        for path in paths {
            out.push_str(&format!("{path}\0"));
        }
        out.push('\0');
        for (path, kind) in records {
            out.push_str(&format!("1\0{path}\0{kind}\0{kind}: in {path}\n\0"));
        }
        out.into_bytes()
    }

    #[test]
    fn contents_conflicts_on_three_paths_at_most_are_trivial() {
        let one = output(
            &["f"],
            &[("f", "Auto-merging"), ("f", "CONFLICT (contents)")],
        );
        assert_eq!(
            parse(Some(1), &one, true),
            Classified {
                trivial: true,
                paths: vec!["f".to_owned()]
            }
        );
        let three: Vec<(&str, &str)> = ["a", "b", "c"]
            .iter()
            .map(|p| (*p, "CONFLICT (contents)"))
            .collect();
        assert!(parse(Some(1), &output(&["a", "b", "c"], &three), true).trivial);
        let four: Vec<(&str, &str)> = ["a", "b", "c", "d"]
            .iter()
            .map(|p| (*p, "CONFLICT (contents)"))
            .collect();
        let four = parse(Some(1), &output(&["a", "b", "c", "d"], &four), true);
        assert!(!four.trivial);
        assert_eq!(four.paths.len(), 4);
    }

    #[test]
    fn any_other_conflict_is_not_trivial() {
        for kind in [
            "CONFLICT (modify/delete)",
            "CONFLICT (binary)",
            "CONFLICT (add/add)",
            "CONFLICT (rename/delete)",
        ] {
            let out = output(&["f"], &[(kind, kind), ("f", "CONFLICT (contents)")]);
            assert!(!parse(Some(1), &out, true).trivial, "{kind}");
        }
    }

    #[test]
    fn a_clean_merge_is_trivial_only_without_a_reported_conflict() {
        let clean = format!("{TREE}\0").into_bytes();
        // A refresh: nothing conflicts, and the forge reports nothing.
        assert!(parse(Some(0), &clean, false).trivial);
        // The forge reports a conflict git does not find.
        assert!(!parse(Some(0), &clean, true).trivial);
    }

    #[test]
    fn another_exit_or_unreadable_output_is_not_trivial() {
        assert!(!parse(Some(128), b"fatal: bad revision\n", true).trivial);
        assert!(!parse(None, b"", true).trivial);
        assert!(!parse(Some(1), b"", true).trivial);
        // A record cut short.
        let mut cut = output(&["f"], &[("f", "CONFLICT (contents)")]);
        cut.truncate(cut.len() - 12);
        assert!(!parse(Some(1), &cut, true).trivial);
        // A count that is not one.
        let bad = format!("{TREE}\0f\0\0x\0f\0CONFLICT (contents)\0m\0").into_bytes();
        assert!(!parse(Some(1), &bad, true).trivial);
    }
}
