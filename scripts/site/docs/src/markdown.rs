//! One Markdown page of the documentation turned into the HTML of its text,
//! with GitHub's anchors and the site's link rules (build plan, "The public
//! site", "The documentation", "One source" and "Links and anchors").

use std::collections::HashMap;
use std::path::Path;

use pulldown_cmark::{
    CowStr, Event, HeadingLevel, LinkType, Options, Parser, Tag, TagEnd, TextMergeStream,
};

/// The repository's address on GitHub: a link to a file that is not served
/// goes to it there.
pub const REPOSITORY: &str = "https://github.com/pitchopp/owlshift";

/// The anchor GitHub gives a heading of text `text`: lower case, everything
/// but letters, digits, `-`, `_` and spaces dropped, each space a hyphen.
pub fn github_anchor(text: &str) -> String {
    text.trim()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

/// The anchors of one page: a heading's anchor, then `-1`, `-2` added to its
/// repeats, as GitHub does.
#[derive(Debug, Default)]
struct Anchors(HashMap<String, usize>);

impl Anchors {
    fn unique(&mut self, base: String) -> String {
        let seen = self.0.entry(base.clone()).or_insert(0);
        let anchor = if *seen == 0 {
            base
        } else {
            format!("{base}-{seen}")
        };
        *seen += 1;
        anchor
    }
}

/// A part of a page that opens with a heading: what the search finds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// 1 for the page's title, 2 for a section, and so on.
    pub level: u8,
    /// The heading's id, GitHub's anchor.
    pub anchor: String,
    /// The heading's text.
    pub heading: String,
    /// The section's text up to the next heading, code included, its
    /// whitespace folded.
    pub text: String,
}

/// A page of the documentation, rendered.
#[derive(Debug, Clone)]
pub struct Page {
    /// The text of its first heading, `# …`.
    pub title: String,
    /// The HTML of its text.
    pub html: String,
    /// Its sections, in order; the first is the title's.
    pub sections: Vec<Section>,
}

/// Where a page's links lead.
pub struct Links<'a> {
    /// The repository's root, where the link targets are looked for.
    pub root: &'a Path,
    /// The served pages, as paths from the repository's root
    /// (`docs/adopt/linear.md`).
    pub served: &'a [String],
}

fn parser(markdown: &str) -> TextMergeStream<'_, Parser<'_>> {
    TextMergeStream::new(Parser::new_ext(markdown, Options::ENABLE_TABLES))
}

/// The ids of a Markdown text's headings, as the documentation's pages give
/// them and as GitHub does.
pub fn heading_ids(markdown: &str) -> Vec<String> {
    let mut events: Vec<Event> = parser(markdown).collect();
    headings(&mut events)
        .into_iter()
        .map(|section| section.anchor)
        .collect()
}

fn level_number(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Gives each heading of `events` its id and returns the sections, each
/// with its text up to the next heading.
fn headings(events: &mut [Event]) -> Vec<Section> {
    let mut anchors = Anchors::default();
    let mut sections: Vec<Section> = Vec::new();
    let mut heading: Option<(usize, String)> = None;
    for at in 0..events.len() {
        match &events[at] {
            Event::Start(Tag::Heading { .. }) => heading = Some((at, String::new())),
            Event::End(TagEnd::Heading(level)) => {
                let level = *level;
                let (start, text) = heading.take().expect("a heading's end follows its start");
                let text = fold(&text);
                let anchor = anchors.unique(github_anchor(&text));
                events[start] = Event::Start(Tag::Heading {
                    level,
                    id: Some(CowStr::from(anchor.clone())),
                    classes: Vec::new(),
                    attrs: Vec::new(),
                });
                sections.push(Section {
                    level: level_number(level),
                    anchor,
                    heading: text,
                    text: String::new(),
                });
            }
            Event::Text(text) | Event::Code(text) => match (&mut heading, sections.last_mut()) {
                (Some((_, heading)), _) => heading.push_str(text),
                (None, Some(section)) => {
                    section.text.push_str(text);
                }
                (None, None) => {}
            },
            Event::SoftBreak
            | Event::HardBreak
            | Event::End(
                TagEnd::Paragraph | TagEnd::Item | TagEnd::CodeBlock | TagEnd::TableCell,
            ) => match (&mut heading, sections.last_mut()) {
                (Some((_, heading)), _) => heading.push(' '),
                (None, Some(section)) => section.text.push(' '),
                (None, None) => {}
            },
            _ => {}
        }
    }
    for section in &mut sections {
        section.text = fold(&section.text);
    }
    sections
}

/// `text` with each run of whitespace one space, and none at its ends.
fn fold(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The path of `target`, a relative link of the file `source` (both from the
/// repository's root, `/`-separated), with `.` and `..` resolved; `None` when
/// it climbs above the root. A target that starts with `/` is taken from the
/// root, as GitHub takes it.
fn resolve(source: &str, target: &str) -> Option<String> {
    let mut parts: Vec<&str> = if target.starts_with('/') {
        Vec::new()
    } else {
        let mut folder: Vec<&str> = source.split('/').collect();
        folder.pop();
        folder
    };
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

/// The link from the file `from` to the file `to`, both paths from the same
/// root, `/`-separated.
pub fn relative(from: &str, to: &str) -> String {
    let from: Vec<&str> = from.split('/').collect();
    let to: Vec<&str> = to.split('/').collect();
    let from_folder = &from[..from.len() - 1];
    let common = from_folder
        .iter()
        .zip(&to)
        .take_while(|(a, b)| a == b)
        .count();
    let mut link = "../".repeat(from_folder.len() - common);
    link.push_str(&to[common..].join("/"));
    link
}

/// The served page an `.md` path names, as its `.html` path.
pub fn html_path(markdown_path: &str) -> String {
    format!(
        "{}.html",
        markdown_path.strip_suffix(".md").unwrap_or(markdown_path)
    )
}

/// Where the link `destination` of the page `source` leads on the site.
fn link(destination: &str, source: &str, links: &Links) -> Result<String, String> {
    let trimmed = destination.trim_matches(|c: char| c.is_ascii_whitespace() || c.is_control());
    if trimmed.starts_with('#') {
        return Ok(trimmed.to_owned());
    }
    if trimmed.starts_with("//") {
        return Err(format!("{destination}: a link without its scheme"));
    }
    if let Some((scheme, _)) = trimmed.split_once(':')
        && !scheme.is_empty()
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return match scheme.to_ascii_lowercase().as_str() {
            "https" | "http" | "mailto" => Ok(trimmed.to_owned()),
            _ => Err(format!(
                "{destination}: a link whose scheme is not https, http or mailto"
            )),
        };
    }
    let (path, fragment) = match trimmed.split_once('#') {
        Some((path, fragment)) => (path, format!("#{fragment}")),
        None => (trimmed, String::new()),
    };
    let Some(target) = resolve(source, path) else {
        return Err(format!("{destination}: a link above the repository's root"));
    };
    if links.served.contains(&target) {
        return Ok(format!(
            "{}{fragment}",
            relative(&html_path(source), &html_path(&target))
        ));
    }
    let on_disk = target
        .split('/')
        .filter(|part| !part.is_empty())
        .fold(links.root.to_path_buf(), |path, part| path.join(part));
    let kind = if target.is_empty() || on_disk.is_dir() {
        "tree"
    } else if on_disk.is_file() {
        "blob"
    } else {
        return Err(format!("{destination}: {target} does not exist"));
    };
    let target = if target.is_empty() {
        String::new()
    } else {
        format!("/{target}")
    };
    Ok(format!("{REPOSITORY}/{kind}/main{target}{fragment}"))
}

/// The end of a bare address in `text`, which starts with one: up to the
/// first space or `<`, less the punctuation that ends a sentence and a `)`
/// the address does not open.
fn address_end(text: &str) -> usize {
    let mut end = text
        .find(|c: char| c.is_whitespace() || c == '<')
        .unwrap_or(text.len());
    loop {
        let address = &text[..end];
        let Some(last) = address.chars().last() else {
            return end;
        };
        let unbalanced = last == ')' && address.matches(')').count() > address.matches('(').count();
        if matches!(
            last,
            '.' | ',' | ':' | ';' | '!' | '?' | '\'' | '"' | '*' | '_' | '~'
        ) || unbalanced
        {
            end -= last.len_utf8();
        } else {
            return end;
        }
    }
}

/// `text` with its bare `https://` and `http://` addresses linked, as GitHub
/// links them.
fn autolink<'a>(text: CowStr<'a>, out: &mut Vec<Event<'a>>) {
    let mut rest: &str = &text;
    let mut any = false;
    while let Some(at) = ["https://", "http://"]
        .iter()
        .filter_map(|scheme| rest.find(scheme))
        .min()
    {
        let end = at + address_end(&rest[at..]);
        if end <= at + "https://".len() {
            break;
        }
        any = true;
        if at > 0 {
            out.push(Event::Text(CowStr::from(rest[..at].to_owned())));
        }
        let address = CowStr::from(rest[at..end].to_owned());
        out.push(Event::Start(Tag::Link {
            link_type: LinkType::Autolink,
            dest_url: address.clone(),
            title: CowStr::from(""),
            id: CowStr::from(""),
        }));
        out.push(Event::Text(address));
        out.push(Event::End(TagEnd::Link));
        rest = &rest[end..];
    }
    if !any {
        out.push(Event::Text(text));
    } else if !rest.is_empty() {
        out.push(Event::Text(CowStr::from(rest.to_owned())));
    }
}

/// `html`, raw HTML of a page, less its comments; an error when anything but
/// whitespace is left, or a comment is not closed.
fn comments_only(html: &str) -> Result<(), String> {
    let mut rest = html;
    let mut left = String::new();
    while let Some(start) = rest.find("<!--") {
        left.push_str(&rest[..start]);
        let Some(end) = rest[start..].find("-->") else {
            return Err(format!(
                "an HTML comment that is not closed: {}",
                html.trim()
            ));
        };
        rest = &rest[start + end + 3..];
    }
    left.push_str(rest);
    if left.trim().is_empty() {
        Ok(())
    } else {
        Err(format!(
            "raw HTML, which the site does not serve: {}",
            left.trim()
        ))
    }
}

/// The page `markdown`, the file `source` of the repository (`docs/faq.md`).
/// Errors name the file: raw HTML other than comments, an image, a link whose
/// scheme is not `https`, `http` or `mailto`, a relative link to nothing, a
/// page that does not open with its `#` title.
pub fn page(markdown: &str, source: &str, links: &Links) -> Result<Page, Vec<String>> {
    let mut errors = Vec::new();
    let mut events: Vec<Event> = Vec::new();
    let mut code_block = false;
    let mut in_link = 0usize;
    let mut html_block: Option<String> = None;
    let mut image = false;
    for event in parser(markdown) {
        match event {
            // The text of an image is dropped with it.
            Event::End(TagEnd::Image) => image = false,
            _ if image => {}
            Event::Start(Tag::HtmlBlock) => html_block = Some(String::new()),
            Event::End(TagEnd::HtmlBlock) => {
                if let Err(e) = comments_only(&html_block.take().unwrap_or_default()) {
                    errors.push(e);
                }
            }
            Event::Html(html) => match html_block.as_mut() {
                Some(block) => block.push_str(&html),
                None => {
                    if let Err(e) = comments_only(&html) {
                        errors.push(e);
                    }
                }
            },
            Event::InlineHtml(html) => {
                if let Err(e) = comments_only(&html) {
                    errors.push(e);
                }
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                errors.push(format!(
                    "{dest_url}: an image, which the site does not serve"
                ));
                image = true;
            }
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }) => {
                in_link += 1;
                let dest_url = match link(&dest_url, source, links) {
                    Ok(url) => CowStr::from(url),
                    Err(e) => {
                        errors.push(e);
                        dest_url
                    }
                };
                events.push(Event::Start(Tag::Link {
                    link_type,
                    dest_url,
                    title,
                    id,
                }));
            }
            Event::End(TagEnd::Link) => {
                in_link -= 1;
                events.push(Event::End(TagEnd::Link));
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                code_block = true;
                events.push(Event::Start(Tag::CodeBlock(kind)));
            }
            Event::End(TagEnd::CodeBlock) => {
                code_block = false;
                events.push(Event::End(TagEnd::CodeBlock));
            }
            // A table wider than the text scrolls inside its own box.
            Event::Start(Tag::Table(alignments)) => {
                events.push(Event::Html(CowStr::from("<div class=\"table\">")));
                events.push(Event::Start(Tag::Table(alignments)));
            }
            Event::End(TagEnd::Table) => {
                events.push(Event::End(TagEnd::Table));
                events.push(Event::Html(CowStr::from("</div>\n")));
            }
            Event::Text(text) if !code_block && in_link == 0 => autolink(text, &mut events),
            event => events.push(event),
        }
    }
    let sections = headings(&mut events);
    let title = match (events.first(), sections.first()) {
        (
            Some(Event::Start(Tag::Heading {
                level: HeadingLevel::H1,
                ..
            })),
            Some(first),
        ) => first.heading.clone(),
        _ => {
            errors.push("the page does not open with its title, a `#` heading".to_owned());
            String::new()
        }
    };
    if !errors.is_empty() {
        return Err(errors
            .into_iter()
            .map(|e| format!("{source}: {e}"))
            .collect());
    }
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, events.into_iter());
    Ok(Page {
        title,
        html,
        sections,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_heading_anchor_is_githubs() {
        assert_eq!(github_anchor("Adopt a project"), "adopt-a-project");
        assert_eq!(github_anchor("What's different"), "whats-different");
        assert_eq!(
            github_anchor("8. Policy, security & trust"),
            "8-policy-security--trust"
        );
        assert_eq!(github_anchor("owlshift do once"), "owlshift-do-once");
        // docs/troubleshooting.md: a heading holding a symbol.
        assert_eq!(
            heading_ids("## `owlshift doctor` ends with `✗ Not ready`"),
            ["owlshift-doctor-ends-with--not-ready"]
        );
    }

    #[test]
    fn a_repeated_heading_takes_a_number() {
        assert_eq!(
            heading_ids("# A\n\n## Usage\n\n## Usage\n\n### Usage\n"),
            ["a", "usage", "usage-1", "usage-2"]
        );
    }

    #[test]
    fn a_bare_address_is_linked_without_the_sentence_around_it() {
        for (text, address) in [
            ("see https://x.dev/a_b.", "https://x.dev/a_b"),
            (
                "(https://git-scm.com/downloads), then",
                "https://git-scm.com/downloads",
            ),
            ("https://x.dev/a_(b)!", "https://x.dev/a_(b)"),
        ] {
            let at = text.find("http").unwrap();
            assert_eq!(&text[at..at + address_end(&text[at..])], address, "{text}");
        }
    }

    #[test]
    fn links_are_resolved_from_the_page() {
        assert_eq!(
            resolve("docs/adopt/linear.md", "../faq.md").as_deref(),
            Some("docs/faq.md")
        );
        assert_eq!(resolve("docs/faq.md", "../../x.md"), None);
        assert_eq!(
            resolve("docs/faq.md", "/README.md").as_deref(),
            Some("README.md")
        );
        assert_eq!(
            relative("docs/adopt/linear.html", "docs/faq.html"),
            "../faq.html"
        );
        assert_eq!(
            relative("docs/faq.html", "docs/adopt/linear.html"),
            "adopt/linear.html"
        );
        assert_eq!(relative("docs/faq.html", "docs/faq.html"), "faq.html");
    }
}
