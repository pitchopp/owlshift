//! Whether a delivered pull request's conflict with its base is trivial
//! (OWL-210, architecture section 5, "Conflicts and the rebase run"): read
//! from `git merge-tree --write-tree -z <onto> <head>` in the
//! dedicated checkout after the fetch. A trivial conflict gets the rebase
//! role on the fast tier; any other goes to the Build routing at once.
//!
//! The output, documented from git 2.38.0 and stable at 2.39.0, Owlshift's
//! floor (OWL-199): the tree, then one `<mode> <object> <stage>\t<path>`
//! entry per stage of each conflicted path, then an empty field; then one
//! record per message, `<count>NUL<path>…<type>NUL<message>NUL`, its type a
//! stable string. Exit status 0 is a clean merge, 1 a conflict.
//! Checked live with git 2.43.0 on 2026-10-11: a contents conflict, a
//! modify/delete and a binary file each give their own record, the binary
//! one a `CONFLICT (binary)` before its `CONFLICT (contents)`. Both sides
//! adding the same path is typed `CONFLICT (contents)` too (only its message
//! says add/add), but its path has no stage 1, the merge base's version:
//! add/add is not trivial (OWL-230).

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
pub fn classify(
    git: &Git,
    checkout: &Path,
    onto: &str,
    head: &str,
    conflicting: bool,
) -> Classified {
    let out = git.output(
        checkout,
        &[
            "merge-tree",
            "--write-tree",
            "-z",
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
/// every record other than `Auto-merging` is `CONFLICT (contents)`, every
/// conflicted path has a stage-1 entry (an add/add has none), and at most
/// [`TRIVIAL_PATHS`] paths conflict. Any other exit status, or output that
/// does not read as documented, is non-trivial.
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
    // The tree, then one entry per stage of each conflicted path up to an
    // empty field, a path's stages next to each other.
    if fields.next().is_none() {
        return non_trivial(Vec::new());
    }
    let mut paths: Vec<String> = Vec::new();
    let mut every_path_in_base = true;
    let mut in_base = false;
    loop {
        let entry = match fields.next() {
            Some(field) if field.is_empty() => break,
            Some(field) => field,
            None => return non_trivial(paths),
        };
        let Some((stage, path)) = stage_entry(&entry) else {
            return non_trivial(paths);
        };
        if paths.last().map(String::as_str) != Some(path) {
            every_path_in_base &= paths.is_empty() || in_base;
            in_base = false;
            paths.push(path.to_owned());
        }
        in_base |= stage == 1;
    }
    every_path_in_base &= in_base;
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
        trivial: only_contents
            && every_path_in_base
            && records > 0
            && !paths.is_empty()
            && paths.len() <= TRIVIAL_PATHS,
        paths,
    }
}

/// The stage and path of a `<mode> <object> <stage>\t<path>` entry; `None`
/// unless the stage is 1, 2 or 3 and the path is not empty.
fn stage_entry(entry: &str) -> Option<(u8, &str)> {
    let (head, path) = entry.split_once('\t')?;
    let mut parts = head.split(' ');
    let (Some(_mode), Some(_object), Some(stage), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let stage = match stage {
        "1" => 1,
        "2" => 2,
        "3" => 3,
        _ => return None,
    };
    (!path.is_empty()).then_some((stage, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TREE: &str = "4602d8d66c7dec5be06ff4c1c70a608116d76ec";

    /// Output in git 2.43.0's shape: the tree, stages 1 to 3 of each path,
    /// then records of one path each.
    fn output(paths: &[&str], records: &[(&str, &str)]) -> Vec<u8> {
        let mut out = format!("{TREE}\0");
        for path in paths {
            for stage in 1..=3 {
                out.push_str(&format!("100644 {TREE} {stage}\t{path}\0"));
            }
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
            "CONFLICT (rename/delete)",
            "CONFLICT (file/directory)",
        ] {
            let out = output(&["f"], &[(kind, kind), ("f", "CONFLICT (contents)")]);
            assert!(!parse(Some(1), &out, true).trivial, "{kind}");
        }
    }

    /// Recorded with git 2.43.0 on 2026-10-11 from `git merge-tree
    /// --write-tree -z main side`: both sides edit a line of `mod.txt` and
    /// each adds its own `new.txt`.
    const ADD_ADD: &[u8] = b"ab183118ac867b8ba0cb89742e9ae66d19167017\0\
100644 de980441c3ab03a8c07dda1ad27b8a11f39deb1e 1\tmod.txt\0\
100644 af703352c64a2d88d4f62818fa68e6ae91241dfd 2\tmod.txt\0\
100644 f794161ca7f359f1bc311e2276a9a3d89a5bbec8 3\tmod.txt\0\
100644 ba2906d0666cf726c7eaadd2cd3db615dedfdf3a 2\tnew.txt\0\
100644 2299c37978265a95cbe835a4b0f0bbf15aad5549 3\tnew.txt\0\
\0\
1\0mod.txt\0Auto-merging\0Auto-merging mod.txt\n\0\
1\0mod.txt\0CONFLICT (contents)\0CONFLICT (content): Merge conflict in mod.txt\n\0\
1\0new.txt\0Auto-merging\0Auto-merging new.txt\n\0\
1\0new.txt\0CONFLICT (contents)\0CONFLICT (add/add): Merge conflict in new.txt\n\0";

    #[test]
    fn a_path_with_no_base_version_is_not_trivial() {
        assert_eq!(
            parse(Some(1), ADD_ADD, true),
            Classified {
                trivial: false,
                paths: vec!["mod.txt".to_owned(), "new.txt".to_owned()]
            }
        );
        // The same output with a base version of `new.txt` is a contents
        // conflict on both paths.
        let based = String::from_utf8_lossy(ADD_ADD).replace(
            "100644 ba29",
            "100644 0000000000000000000000000000000000000000 1\tnew.txt\x00100644 ba29",
        );
        assert!(parse(Some(1), based.as_bytes(), true).trivial);
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
        let records = "1\0f\0CONFLICT (contents)\0m\0";
        let entry = format!("100644 {TREE} 1\tf\0");
        let bad = format!("{TREE}\0{entry}\0x\0f\0CONFLICT (contents)\0m\0").into_bytes();
        assert!(!parse(Some(1), &bad, true).trivial);
        // Stage entries not in the documented shape: a bare path, as
        // `--name-only` writes, a stage outside 1 to 3, an empty path.
        for entry in [
            "f".to_owned(),
            format!("100644 {TREE} 0\tf"),
            format!("100644 {TREE} 1\t"),
        ] {
            let bad = format!("{TREE}\0{entry}\0\0{records}").into_bytes();
            assert!(!parse(Some(1), &bad, true).trivial, "{entry}");
        }
        // The documented shape is trivial.
        let good = format!("{TREE}\0{entry}\0{records}").into_bytes();
        assert!(parse(Some(1), &good, true).trivial);
    }
}
