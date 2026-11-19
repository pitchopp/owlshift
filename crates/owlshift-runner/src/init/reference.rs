//! OWL-605: the configuration reference, `docs/reference/configuration.md`,
//! kept in step with the project file `owlshift init` writes and with the
//! personal file's schema.
//!
//! The page's project-file keys are rendered from [`super::project_file`]'s
//! template, its comments as their text, between two markers; a test fails
//! when the page differs. Run
//! `OWLSHIFT_UPDATE_DOCS=1 cargo test -p owlshift-runner --lib init::reference`
//! to rewrite that part. The personal file's keys are written by hand, and a
//! test fails when a key of its schema has no entry.

use std::path::PathBuf;

use semver::Version;

use super::tests::{TRACKERS, file_keys, schema_keys, toml_leaves, toml_line, written};
use crate::config::OWLSHIFT_VERSION;
use owlshift_contracts::config::TrackerKind;

/// The page, from the repository's root.
const PAGE: &str = "docs/reference/configuration.md";
/// The line before the rendered part of the page.
const BEGIN: &str = "<!-- BEGIN generated from the file `owlshift init` writes: \
                     OWLSHIFT_UPDATE_DOCS=1 cargo test -p owlshift-runner --lib init::reference -->";
/// The line after it.
const END: &str = "<!-- END generated -->";
/// What the page shows for the version `requires` names, which `init` takes
/// from the Owlshift that writes the file.
const VERSION_SHOWN: &str = "requires = \">=<major>.<minor>\"";

/// One block of the template: a comment, as paragraphs, and the lines it
/// describes, set or commented out.
#[derive(Debug, Default)]
struct Block {
    /// The table the block starts in, dotted; empty at the top level.
    table: String,
    /// The comment's lines, `#` taken off; an empty one parts paragraphs.
    prose: Vec<String>,
    /// The TOML lines, as written.
    lines: Vec<String>,
    /// The keys the key lines set, dotted under their table.
    keys: Vec<String>,
}

impl Block {
    fn is_empty(&self) -> bool {
        self.prose.is_empty() && self.lines.is_empty()
    }

    /// The table it documents: the first it opens, else the one it starts
    /// in (a comment above `# [forge]` is the forge's).
    fn documents(&self) -> String {
        self.headers()
            .into_iter()
            .next()
            .unwrap_or_else(|| self.table.clone())
    }

    /// The table headers among its lines.
    fn headers(&self) -> Vec<String> {
        self.lines
            .iter()
            .filter_map(|line| match toml_line(line) {
                Some((true, parsed)) => Some(table_of(&parsed)),
                _ => None,
            })
            .collect()
    }
}

/// The dotted name of the table a parsed header line opens.
fn table_of(parsed: &toml::Table) -> String {
    let mut found = Vec::new();
    toml_leaves(parsed, "", &mut found);
    found.pop().unwrap()
}

/// `key` under `table`.
fn dotted(table: &str, key: &str) -> String {
    if table.is_empty() {
        key.to_owned()
    } else {
        format!("{table}.{key}")
    }
}

/// The blocks of a project file. A comment that follows TOML lines starts
/// a block, unless the block so far is table headers alone
/// (`[tracker.states]` above its own comment); a blank line ends one;
/// header, key and continuation lines join it.
fn blocks(text: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut table = String::new();
    let mut block = Block::default();
    // Whether the line before was a TOML line rather than a comment.
    let mut after_lines = false;
    for line in text.lines() {
        let parsed = toml_line(line);
        if block.is_empty() && !line.trim().is_empty() {
            block.table = match &parsed {
                Some((true, header)) => table_of(header),
                _ => table.clone(),
            };
        }
        match parsed {
            Some((true, header)) => {
                table = table_of(&header);
                block.lines.push(line.to_owned());
                after_lines = true;
            }
            Some((false, keys)) => {
                block
                    .keys
                    .extend(keys.keys().map(|key| dotted(&table, key)));
                block.lines.push(line.to_owned());
                after_lines = true;
            }
            None if line.starts_with('#') => {
                let headers_only = block.keys.is_empty() && block.prose.is_empty();
                if after_lines && !headers_only {
                    out.push(std::mem::take(&mut block));
                    block.table = table.clone();
                }
                block
                    .prose
                    .push(line.trim_start_matches('#').trim().to_owned());
                after_lines = false;
            }
            None if line.trim().is_empty() => {
                if !block.is_empty() {
                    out.push(std::mem::take(&mut block));
                }
                after_lines = false;
            }
            // An array's next line, such as a gate command.
            None => {
                block.lines.push(line.to_owned());
                after_lines = true;
            }
        }
    }
    if !block.is_empty() {
        out.push(block);
    }
    out
}

/// `key` as the page names it: each part a key a person chooses stands
/// for (`*` in the schema) written `<name>`.
fn shown(key: &str, schema: &[String]) -> String {
    let parts: Vec<&str> = key.split('.').collect();
    let leaf = schema
        .iter()
        .map(|leaf| leaf.split('.').collect::<Vec<_>>())
        .find(|leaf| {
            leaf.len() >= parts.len() && leaf.iter().zip(&parts).all(|(l, p)| *l == "*" || l == p)
        })
        .unwrap_or_else(|| panic!("{key} is no key of the project file"));
    parts
        .iter()
        .zip(&leaf)
        .map(|(part, l)| if *l == "*" { "<name>" } else { part })
        .collect::<Vec<_>>()
        .join(".")
}

/// A block as Markdown: a heading naming the keys it sets (none when it
/// sets none: its section's heading names its table), its comment as
/// paragraphs, then its lines.
fn block_markdown(block: &Block, schema: &[String]) -> String {
    let mut names: Vec<String> = Vec::new();
    for key in &block.keys {
        let name = format!("`{}`", shown(key, schema));
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let mut parts = Vec::new();
    if !names.is_empty() {
        parts.push(format!("#### {}", names.join(", ")));
    }
    for paragraph in block.prose.split(String::is_empty) {
        if !paragraph.is_empty() {
            parts.push(paragraph.join(" "));
        }
    }
    if !block.lines.is_empty() {
        parts.push(format!("```toml\n{}\n```", block.lines.join("\n")));
    }
    parts.join("\n\n")
}

/// The name of a tracker in a section's title.
fn tracker_name(kind: TrackerKind) -> &'static str {
    match kind {
        TrackerKind::Linear => "Linear",
        TrackerKind::Github => "GitHub Issues",
        TrackerKind::Markdown => "the Markdown tracker",
    }
}

/// The file `init` writes for `kind` with its version shown as
/// [`VERSION_SHOWN`], cut where `[stack]` starts: the part that differs
/// between trackers, then the rest.
fn file_parts(kind: TrackerKind) -> (String, String) {
    let version = Version::parse(OWLSHIFT_VERSION).unwrap();
    let requires = format!("requires = \">={}.{}\"", version.major, version.minor);
    let text = written(kind);
    assert!(text.contains(&requires), "{text}");
    let text = text.replace(&requires, VERSION_SHOWN);
    let at = text.find("\n[stack]\n").expect("the file has a [stack]") + 1;
    (text[..at].to_owned(), text[at..].to_owned())
}

/// The rendered part of the page: the top of the file, `[tracker]` for each
/// tracker, then each other table, every block as [`block_markdown`] says.
fn render() -> String {
    let schema = schema_keys("project-config");
    let mut sections: Vec<(String, Vec<Block>)> = Vec::new();
    let (head, rest) = file_parts(TRACKERS[0]);
    let top = blocks(&head)
        .into_iter()
        .filter(|block| block.documents().is_empty())
        .collect();
    sections.push(("Top level".to_owned(), top));
    for kind in TRACKERS {
        let (head, other_rest) = file_parts(kind);
        assert_eq!(
            other_rest, rest,
            "{kind:?}: the file past [tracker] differs"
        );
        let tracker = blocks(&head)
            .into_iter()
            .filter(|block| !block.documents().is_empty())
            .collect();
        sections.push((format!("`[tracker]` for {}", tracker_name(kind)), tracker));
    }
    for block in blocks(&rest) {
        let table = block.documents().split('.').next().unwrap().to_owned();
        let title = format!("`[{table}]`");
        match sections.last_mut() {
            Some((last, list)) if *last == title => list.push(block),
            _ => sections.push((title, vec![block])),
        }
    }
    let mut parts = Vec::new();
    for (title, list) in &sections {
        parts.push(format!("### {title}"));
        parts.extend(list.iter().map(|block| block_markdown(block, &schema)));
    }
    parts.join("\n\n") + "\n"
}

/// The page, its line endings read as `\n`.
fn page_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(PAGE)
}

fn page() -> String {
    std::fs::read_to_string(page_path())
        .unwrap()
        .replace("\r\n", "\n")
}

/// The section of `page` under the `## ` heading that starts with `title`,
/// up to the next one.
fn section<'a>(page: &'a str, title: &str) -> &'a str {
    let start = page
        .find(&format!("\n## {title}"))
        .unwrap_or_else(|| panic!("{PAGE} has no section `## {title}`"))
        + 1;
    let end = page[start + 3..]
        .find("\n## ")
        .map_or(page.len(), |at| start + 3 + at);
    &page[start..end]
}

/// Whether `part` is a part of a key as the page names one: lowercase
/// letters, digits and `_`, or a placeholder such as `<name>`.
fn is_key_part(part: &str) -> bool {
    let word = |w: &str| {
        !w.is_empty()
            && w.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    word(part)
        || part
            .strip_prefix('<')
            .and_then(|p| p.strip_suffix('>'))
            .is_some_and(word)
}

/// The keys the headings of `text` name, in inline code.
fn heading_keys(text: &str) -> Vec<String> {
    let mut keys = Vec::new();
    for line in text.lines().filter(|line| line.starts_with("###")) {
        for (index, span) in line.split('`').enumerate() {
            if index % 2 == 1 && span.split('.').all(is_key_part) {
                keys.push(span.to_owned());
            }
        }
    }
    keys
}

/// Whether the page's `named` covers `key`: `key` is it or lies under it, a
/// placeholder standing for any part, and a key a person chooses (`*`)
/// matched by a placeholder only.
fn covers(named: &str, key: &str) -> bool {
    let named: Vec<&str> = named.split('.').collect();
    let key: Vec<&str> = key.split('.').collect();
    named.len() <= key.len()
        && named
            .iter()
            .zip(&key)
            .all(|(n, k)| n.starts_with('<') || (*k != "*" && n == k))
}

/// The keys of `keys` no key of `named` covers.
fn missing(keys: &[String], named: &[String]) -> Vec<String> {
    keys.iter()
        .filter(|key| !named.iter().any(|n| covers(n, key)))
        .cloned()
        .collect()
}

/// Every key of the file `init` writes, for any tracker.
fn project_keys() -> Vec<String> {
    let mut keys: Vec<String> = TRACKERS
        .iter()
        .flat_map(|k| file_keys(&written(*k)))
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

/// The page shows the file `init` writes, as [`render`] renders it; set
/// `OWLSHIFT_UPDATE_DOCS=1` to rewrite that part of the page.
#[test]
fn the_page_shows_the_file_init_writes() {
    let page = page();
    let begin = page
        .find(BEGIN)
        .unwrap_or_else(|| panic!("{PAGE} lacks the line {BEGIN}"));
    let end = page
        .find(END)
        .unwrap_or_else(|| panic!("{PAGE} lacks the line {END}"));
    let shown = &page[begin..end + END.len()];
    let rendered = format!("{BEGIN}\n\n{}\n{END}", render());
    if std::env::var_os("OWLSHIFT_UPDATE_DOCS").is_some_and(|v| v == "1") {
        std::fs::write(page_path(), page.replace(shown, &rendered)).unwrap();
        return;
    }
    assert!(
        shown == rendered,
        "{PAGE} no longer shows the file `owlshift init` writes. Run \
         `OWLSHIFT_UPDATE_DOCS=1 cargo test -p owlshift-runner --lib init::reference` \
         and review the diff.\n--- rendered ---\n{rendered}"
    );
}

/// Every key the file sets says what happens without it: its default, that
/// it is required, or what leaving it out does.
#[test]
fn every_key_of_the_file_says_its_default() {
    for kind in TRACKERS {
        for block in blocks(&written(kind)) {
            if block.keys.is_empty() {
                continue;
            }
            let prose = block.prose.join(" ");
            assert!(
                ["Default", "Required", "Left out"]
                    .iter()
                    .any(|word| prose.contains(word)),
                "{kind:?}, {:?}: {prose}",
                block.keys
            );
        }
    }
}

/// OWL-605's acceptance: a key of the file `init` writes, for any tracker,
/// or of the personal file's schema, missing from the page fails here.
#[test]
fn every_key_is_on_the_page() {
    let page = page();
    let project = heading_keys(section(&page, "The project file"));
    let personal = heading_keys(section(&page, "The personal file"));
    // An extraction that finds nothing would fail every key; these floors
    // say it found the entries.
    assert!(project.len() >= 40, "{project:?}");
    assert!(personal.len() >= 20, "{personal:?}");
    let keys = project_keys();
    assert!(keys.len() >= 40, "{keys:?}");
    let left = missing(&keys, &project);
    assert!(
        left.is_empty(),
        "project file keys missing from {PAGE}: {left:?}"
    );
    let keys = schema_keys("personal-config");
    let left = missing(&keys, &personal);
    assert!(
        left.is_empty(),
        "personal file keys missing from {PAGE}: {left:?}"
    );
}

/// The check above sees one entry taken out of the page.
#[test]
fn a_key_taken_out_of_the_page_is_missing() {
    let page = page();
    let personal = section(&page, "The personal file");
    let without = personal.replace("### `web_tools`", "### Web tools");
    assert_ne!(without, personal);
    let keys = schema_keys("personal-config");
    assert_eq!(missing(&keys, &heading_keys(&without)), ["web_tools"]);
    let project = section(&page, "The project file");
    let without = project.replace(
        "#### `policy.max_open_prs`",
        "#### The cap on pull requests",
    );
    assert_ne!(without, project);
    assert_eq!(
        missing(&project_keys(), &heading_keys(&without)),
        ["policy.max_open_prs"]
    );
    // A placeholder stands for any part; a key a person chooses needs one.
    assert!(covers("zones.<name>.owner", "zones.backend/billing.owner"));
    assert!(covers(
        "repositories.<repository>.takes",
        "repositories.*.takes"
    ));
    assert!(!covers("repositories.takes", "repositories.*.takes"));
    assert!(covers("models.deep", "models.deep.claude"));
}
