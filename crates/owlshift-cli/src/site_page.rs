//! OWL-576: the public site's page stays true to the repository, as
//! `doc_commands` keeps the documentation true to the binary. Read from the
//! page's source, `site/index.html` and `site/site.css` (build plan, "The
//! public site", "Kept true by CI"):
//!
//! - its install commands are the README's quick start, character for
//!   character;
//! - each `owlshift` command it shows parses with the binary's own [`Cli`];
//! - each link to a file of the repository names a file that exists, and each
//!   anchor resolves, on the page or in the Markdown file it points into;
//! - nothing it loads is absolute, and everything it loads exists, in `site/`
//!   or among the files `scripts/site/build.sh` copies beside it;
//! - its Content-Security-Policy is the build plan's, word for word, and it
//!   holds no inline script and no style attribute, which that policy refuses.
//!
//! The page is written by hand with double-quoted attributes, so plain string
//! scanning reads it; a page that stopped being so would fail the floors.
//!
//! [`Cli`]: super::Cli

use std::path::{Path, PathBuf};

use super::doc_commands::{commands_of_line, refusal};

/// The repository's address on GitHub, the prefix of the page's links into it.
const REPOSITORY: &str = "https://github.com/pitchopp/owlshift";

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(path: &str) -> String {
    std::fs::read_to_string(root().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn page() -> String {
    read("site/index.html")
}

/// The text of `value` with the page's character references replaced.
fn decode(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// `html` without its tags.
fn strip_tags(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// The values of every `name="…"` attribute in `html`.
fn attributes(html: &str, name: &str) -> Vec<String> {
    let pattern = format!("{name}=\"");
    html.match_indices(&pattern)
        .filter(|(at, _)| html[..*at].ends_with(char::is_whitespace))
        .map(|(at, _)| {
            let value = &html[at + pattern.len()..];
            decode(&value[..value.find('"').expect("a closed attribute")])
        })
        .collect()
}

/// Every opening tag `<tag …>` of `html`, whole.
fn tags<'a>(html: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}");
    html.match_indices(&open)
        .filter(|(at, _)| {
            html[at + open.len()..].starts_with(|c: char| c.is_whitespace() || c == '>')
        })
        .map(|(at, _)| &html[at..at + html[at..].find('>').expect("a closed tag") + 1])
        .collect()
}

/// The decoded text of every `<element>…</element>` of `html`.
fn contents(html: &str, element: &str) -> Vec<String> {
    let close = format!("</{element}>");
    tags(html, element)
        .into_iter()
        .map(|tag| {
            let start = tag.as_ptr() as usize - html.as_ptr() as usize + tag.len();
            let end = start + html[start..].find(&close).expect("a closed element");
            decode(&strip_tags(&html[start..end]))
        })
        .collect()
}

/// The anchor GitHub gives a Markdown heading: lower case, punctuation
/// dropped, each space a hyphen.
fn slug(heading: &str) -> String {
    heading
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

/// The anchors of a Markdown text's headings, code blocks left out.
fn heading_anchors(markdown: &str) -> Vec<String> {
    let mut fenced = false;
    let mut anchors = Vec::new();
    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        let hashes = line.chars().take_while(|c| *c == '#').count();
        if !fenced && (1..=6).contains(&hashes) && line[hashes..].starts_with(' ') {
            anchors.push(slug(&line[hashes..]));
        }
    }
    anchors
}

#[test]
fn the_install_commands_are_the_readmes() {
    let readme = read("README.md");
    let quick_start: Vec<&str> = readme
        .lines()
        .skip_while(|line| *line != "## Quick start")
        .skip_while(|line| !line.starts_with("```bash"))
        .skip(1)
        .take_while(|line| !line.starts_with("```"))
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .collect();
    let html = page();
    let blocks = contents(&html, "pre");
    let on_page: Vec<&str> = blocks
        .iter()
        .flat_map(|block| block.lines())
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert!(
        quick_start.len() >= 4,
        "README.md: the quick start's install block was not found: {quick_start:?}"
    );
    assert_eq!(
        on_page, quick_start,
        "the page's install commands are not the README's quick start"
    );
}

#[test]
fn each_owlshift_command_on_the_page_parses() {
    let html = page();
    let mut failures = Vec::new();
    let mut found = 0;
    for code in contents(&html, "code") {
        for line in code.lines() {
            for command in commands_of_line(line) {
                found += 1;
                if let Some(why) = refusal(&command) {
                    failures.push(format!("`{}`: {why}", command.join(" ")));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    // `owlshift doctor` after the install, `owlshift init` on a tracker card.
    assert!(
        found >= 2,
        "{found} commands found on the page, at least 2 expected"
    );
}

#[test]
fn each_repository_link_names_a_file_and_each_anchor_resolves() {
    let root = root();
    let html = page();
    let ids = attributes(&html, "id");
    let mut failures = Vec::new();
    let mut files = 0;
    let anchor_in = |path: &str, fragment: &str| -> bool {
        let text = std::fs::read_to_string(root.join(path)).unwrap_or_default();
        heading_anchors(&text)
            .iter()
            .any(|anchor| anchor == fragment)
    };
    for href in attributes(&html, "href") {
        if let Some(id) = href.strip_prefix('#') {
            if !ids.iter().any(|known| known == id) {
                failures.push(format!("{href}: no element of the page has this id"));
            }
            continue;
        }
        let Some(rest) = href.strip_prefix(REPOSITORY) else {
            continue;
        };
        let (path, fragment) = rest.split_once('#').unwrap_or((rest, ""));
        let file = match path {
            // GitHub redirects it to the latest release: no file to check.
            "/releases/latest" => continue,
            // A pull request, such as the timeline's: no file to check.
            pull if pull.strip_prefix("/pull/").is_some_and(|number| {
                !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
            }) =>
            {
                continue;
            }
            // The repository's page shows its README, anchors included.
            "" => "README.md",
            _ => match path.strip_prefix("/blob/main/") {
                Some(file) => file,
                None => {
                    failures.push(format!(
                        "{href}: not a file of the repository's main branch"
                    ));
                    continue;
                }
            },
        };
        if !root.join(file).is_file() {
            failures.push(format!("{href}: {file} does not exist"));
            continue;
        }
        files += 1;
        if !fragment.is_empty() && !anchor_in(file, fragment) {
            failures.push(format!("{href}: {file} has no heading #{fragment}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    // The bar, the band, the tracker cards, the roadmap and the foot.
    assert!(
        files >= 10,
        "{files} links into the repository, at least 10 expected"
    );
}

#[test]
fn the_page_loads_nothing_absolute_and_everything_it_loads_exists() {
    let root = root();
    let html = page();
    let css = read("site/site.css");
    let build = read("scripts/site/build.sh");
    let mut loads = attributes(&html, "src");
    for link in tags(&html, "link") {
        loads.extend(attributes(link, "href"));
    }
    for (at, _) in css.match_indices("url(") {
        let value = &css[at + 4..];
        loads.push(
            value[..value.find(')').unwrap()]
                .trim_matches(['"', '\''])
                .to_owned(),
        );
    }
    // The social preview is named by its full URL, the one the build must
    // still write beside the page.
    let preview = html
        .lines()
        .find(|line| line.contains("property=\"og:image\""))
        .and_then(|line| attributes(line, "content").pop())
        .expect("the page names its social preview");
    let preview = preview.rsplit('/').next().unwrap().to_owned();
    let mut failures = Vec::new();
    let mut copied = vec![
        format!("brand/{preview}"),
        "fonts/OFL-BricolageGrotesque.txt".to_owned(),
        "fonts/OFL-JetBrainsMono.txt".to_owned(),
    ];
    for url in &loads {
        if url.contains(':') || url.starts_with('/') {
            failures.push(format!(
                "{url}: absolute; the page is served under a path prefix"
            ));
        } else if url.starts_with("fonts/") || url.starts_with("brand/") {
            copied.push(url.clone());
        } else if !root.join("site").join(url).is_file() {
            failures.push(format!("{url}: not in site/"));
        }
    }
    for url in &copied {
        let (folder, name) = url.split_once('/').unwrap();
        let source = match folder {
            "fonts" => root
                .join("crates/owlshift-runner/src/ui/assets/fonts")
                .join(name),
            _ => root.join("assets/brand").join(name),
        };
        if !source.is_file() {
            failures.push(format!("{url}: {} does not exist", source.display()));
        }
        if !build.contains(name) {
            failures.push(format!("{url}: scripts/site/build.sh does not copy {name}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    // The stylesheet, the script, three fonts, two lockups and the favicon.
    assert!(
        loads.len() >= 8,
        "{} loads found, at least 8 expected: {loads:?}",
        loads.len()
    );
}

#[test]
fn the_policy_is_the_build_plans_and_the_page_runs_no_inline_code() {
    let plan = read("docs/design/build-plan.md");
    let policies: Vec<&str> = plan
        .lines()
        .flat_map(|line| line.split('`').skip(1).step_by(2))
        .filter(|span| span.starts_with("default-src"))
        .collect();
    assert_eq!(
        policies.len(),
        1,
        "docs/design/build-plan.md should state the page's policy once: {policies:?}"
    );
    let html = page();
    let meta = format!(
        "<meta http-equiv=\"Content-Security-Policy\" content=\"{}\">",
        policies[0]
    );
    assert!(
        html.contains(&meta),
        "the page lacks, word for word: {meta}"
    );
    let scripts = tags(&html, "script");
    assert!(!scripts.is_empty());
    for script in scripts {
        assert!(script.contains(" src=\""), "an inline script: {script}");
    }
    assert!(attributes(&html, "style").is_empty(), "a style attribute");
    assert!(tags(&html, "style").is_empty(), "a <style> element");
}

#[test]
fn a_heading_anchor_is_githubs() {
    assert_eq!(slug("Adopt a project"), "adopt-a-project");
    assert_eq!(slug("What's different"), "whats-different");
    assert_eq!(
        slug("8. Policy, security & trust"),
        "8-policy-security--trust"
    );
    assert_eq!(slug("`owlshift do` once"), "owlshift-do-once");
}
