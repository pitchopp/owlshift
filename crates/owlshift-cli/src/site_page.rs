//! OWL-576: the public site's page stays true to the repository, as
//! `doc_commands` keeps the documentation true to the binary. Read from the
//! page's source, `site/index.html` and `site/site.css` (build plan, "The
//! public site", "Kept true by CI"):
//!
//! - its install commands are the README's quick start, character for
//!   character;
//! - each `owlshift` command it shows parses with the binary's own [`Cli`];
//! - each link to a file of the repository names a file that exists, each
//!   link into the served documentation (OWL-607) names a page the site
//!   serves, and each anchor resolves, on the page or in the Markdown file
//!   the link points into;
//! - nothing it loads is absolute, and everything it loads exists, in `site/`
//!   or among the files `scripts/site/build.sh` copies beside it;
//! - its Content-Security-Policy is the build plan's, word for word, and it
//!   holds nothing that policy refuses, such as an inline script;
//! - the README's two images of its timeline were exported from the timeline
//!   it has now (OWL-579, `scripts/site/export-timeline.sh`).
//!
//! The rules the documentation's pages share with the page, GitHub's anchors
//! and the site's floors, are the documentation renderer's
//! (`scripts/site/docs`), read here as a dev-dependency.
//!
//! [`Cli`]: super::Cli

use std::path::{Path, PathBuf};

use owlshift_site_docs::html::{attributes, decode, floor_failures, is_absolute, loads, tags};
use owlshift_site_docs::markdown::{REPOSITORY, heading_ids};
use owlshift_site_docs::page_source;
use sha2::{Digest, Sha256};

use super::doc_commands::{commands_of_line, refusal};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(path: &str) -> String {
    std::fs::read_to_string(root().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn page() -> String {
    read("site/index.html")
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
    let mut served = 0;
    let anchor_in = |path: &str, fragment: &str| -> bool {
        let text = std::fs::read_to_string(root.join(path)).unwrap_or_default();
        heading_ids(&text).iter().any(|anchor| anchor == fragment)
    };
    let links = tags(&html, "a");
    for href in links.iter().flat_map(|a| attributes(a, "href")) {
        if let Some(id) = href.strip_prefix('#') {
            if !ids.iter().any(|known| known == id) {
                failures.push(format!("{href}: no element of the page has this id"));
            }
            continue;
        }
        // A page of the served documentation (OWL-607), rendered from a
        // Markdown file whose headings give its anchors.
        if !is_absolute(&href) {
            let (path, fragment) = href.split_once('#').unwrap_or((&href, ""));
            served += 1;
            if path == "docs/index.html" && fragment.is_empty() {
                continue;
            }
            match page_source(path) {
                None => failures.push(format!("{href}: not a page the site serves")),
                Some(file) if !fragment.is_empty() && !anchor_in(&file, fragment) => {
                    failures.push(format!("{href}: {file} has no heading #{fragment}"));
                }
                Some(_) => {}
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
    // The bar, the band, the roadmap and the foot.
    assert!(
        files >= 8,
        "{files} links into the repository, at least 8 expected"
    );
    // The bar's Docs and the three tracker cards.
    assert!(
        served >= 4,
        "{served} links into the served documentation, at least 4 expected"
    );
}

#[test]
fn the_page_loads_nothing_absolute_and_everything_it_loads_exists() {
    let root = root();
    let html = page();
    let css = read("site/site.css");
    let build = read("scripts/site/build.sh");
    let mut loads = loads(&html);
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
        if is_absolute(url) {
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
    let failures = floor_failures(&html, &meta);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The timeline's markup as `scripts/site/export-timeline.sh` hashes it: the
/// lines from the first holding `<ol class="timeline">` to the first holding
/// `</ol>` after it, each ended by a newline.
fn timeline_markup(html: &str) -> String {
    let mut markup = String::new();
    for line in html
        .lines()
        .skip_while(|l| !l.contains("<ol class=\"timeline\">"))
    {
        markup.push_str(line);
        markup.push('\n');
        if line.contains("</ol>") {
            return markup;
        }
    }
    panic!("site/index.html has no complete timeline");
}

#[test]
fn the_readmes_timeline_images_are_exported_from_the_pages_timeline() {
    let digest = Sha256::digest(timeline_markup(&page()).as_bytes());
    let actual: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let recorded = read("assets/readme/timeline.sha256");
    assert_eq!(
        recorded.trim(),
        actual,
        "the timeline in site/index.html changed since the README's images were \
         exported: run scripts/site/export-timeline.sh and commit assets/readme/"
    );
    let readme = read("README.md");
    for theme in ["light", "dark"] {
        let image = format!("assets/readme/timeline-{theme}.png");
        let bytes = std::fs::read(root().join(&image)).unwrap_or_else(|e| panic!("{image}: {e}"));
        assert!(bytes.starts_with(b"\x89PNG"), "{image} is not a PNG");
        assert!(readme.contains(&image), "the README does not show {image}");
    }
}
