//! The public site's documentation (OWL-607; build plan, "The public site",
//! "The documentation"): the repository's user documentation, its Markdown
//! files, rendered into the site under `docs/`, each page set in the page's
//! own frame, with the list of pages and a search that needs no request out
//! of the site.
//!
//! The library also holds the rules the page's own test reads
//! (`owlshift-cli`'s `site_page`): GitHub's anchors, the site's floors, and
//! which pages are served.

use std::path::{Path, PathBuf};

pub mod html;
pub mod markdown;

use markdown::{Links, Page, html_path, relative};

/// A group of the pages' list: its name and its pages, as paths from `docs/`.
#[derive(Debug, Clone, Copy)]
pub struct Group {
    pub name: &'static str,
    pub pages: &'static [&'static str],
}

/// The served pages, in their order: the list is kept here and nowhere else.
/// A served file missing from it, or an entry without its file, fails the
/// build.
pub const PAGES: &[Group] = &[
    Group {
        name: "Start",
        pages: &["concepts.md", "faq.md"],
    },
    Group {
        name: "Adopt a project",
        pages: &[
            "adopt/linear.md",
            "adopt/github-issues.md",
            "adopt/markdown.md",
            "adopt/team.md",
        ],
    },
    Group {
        name: "Guides",
        pages: &["recipes.md", "troubleshooting.md", "markdown-tracker.md"],
    },
    Group {
        name: "Reference",
        pages: &["reference/cli.md", "reference/configuration.md"],
    },
];

/// The folders whose Markdown files are served, from `docs/`: not their
/// subfolders.
const SERVED_FOLDERS: &[&str] = &["", "adopt", "reference"];

/// The stylesheet and the search's script, written beside the pages.
const DOCS_CSS: &str = include_str!("../assets/docs.css");
const SEARCH_JS: &str = include_str!("../assets/search.js");

/// `path`, `/`-separated from `root`, as a path of the system.
fn join(root: &Path, path: &str) -> PathBuf {
    path.split('/')
        .filter(|part| !part.is_empty())
        .fold(root.to_path_buf(), |path, part| path.join(part))
}

/// The Markdown files the site serves, as paths from `docs/`, sorted: those
/// of `docs/`, `docs/adopt/` and `docs/reference/`.
pub fn served_files(root: &Path) -> Vec<String> {
    let mut files = Vec::new();
    for folder in SERVED_FOLDERS {
        let Ok(entries) = std::fs::read_dir(join(&root.join("docs"), folder)) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".md") && entry.path().is_file() {
                files.push(if folder.is_empty() {
                    name
                } else {
                    format!("{folder}/{name}")
                });
            }
        }
    }
    files.sort();
    files
}

/// The Markdown file, from the repository's root, that the site serves at
/// `path`, a path from the site's root (`docs/adopt/linear.html`); `None`
/// when no page of [`PAGES`] is served there. `docs/index.html`, the list of
/// pages, has no file.
pub fn page_source(path: &str) -> Option<String> {
    let page = path.strip_prefix("docs/")?.strip_suffix(".html")?;
    let source = format!("{page}.md");
    PAGES
        .iter()
        .flat_map(|group| group.pages)
        .any(|listed| *listed == source)
        .then(|| format!("docs/{source}"))
}

/// What the documentation is rendered from.
pub struct Source<'a> {
    /// The repository's root: `docs/` and the links' targets.
    pub root: &'a Path,
    /// The pages' list.
    pub pages: &'a [Group],
    /// The page's text, `site/index.html`, whose frame every page takes.
    pub index: &'a str,
}

/// `text` escaped for HTML text and attributes.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// The search's index: one assignment of a JSON array, each element a section
/// of a page. serde_json escapes quotes, backslashes and control characters;
/// the text pass after it escapes `<`, so no `</script>` can close anything,
/// and U+2028 and U+2029, which older JavaScript reads as line ends. They
/// occur only inside the JSON's strings, where `\u` escapes read back as
/// written.
pub fn search_index(entries: &serde_json::Value) -> String {
    let json = serde_json::to_string(entries).expect("JSON values serialize");
    let json = json
        .replace('<', "\\u003c")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");
    format!("window.owlshiftSearchIndex = {json};\n")
}

/// The list of pages, linked from the page `current` (`docs/…`), the current
/// one marked.
fn page_list(groups: &[Group], titles: &[(String, String)], current: &str) -> String {
    let mut list = String::new();
    for group in groups {
        list.push_str(&format!(
            "<p class=\"pages-group\">{}</p>\n<ul class=\"pages-list\">\n",
            escape(group.name)
        ));
        for page in group.pages {
            let path = format!("docs/{}", html_path(page));
            let title = titles
                .iter()
                .find(|(listed, _)| listed == page)
                .map_or("", |(_, title)| title.as_str());
            let mark = if path == current {
                " aria-current=\"page\""
            } else {
                ""
            };
            list.push_str(&format!(
                "<li><a href=\"{}\"{mark}>{}</a></li>\n",
                relative(current, &path),
                escape(title)
            ));
        }
        list.push_str("</ul>\n");
    }
    list
}

/// A whole page of the documentation at `path` (`docs/…`), its `text` set
/// in the page's frame.
fn document(
    frame: &html::Frame,
    path: &str,
    title: &str,
    list: &str,
    text: &str,
    toc: &str,
) -> String {
    let prefix = "../".repeat(path.matches('/').count());
    let bar = html::rebase(&frame.bar, &prefix);
    let foot = html::rebase(&frame.foot, &prefix);
    format!(
        "<!doctype html>
<html lang=\"en\">
<head>
<meta charset=\"utf-8\">
{policy}
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">
<title>{title} · Owlshift documentation</title>
<link rel=\"icon\" type=\"image/svg+xml\" href=\"{prefix}brand/owlshift-mark.svg\">
<link rel=\"stylesheet\" href=\"{prefix}site.css\">
<link rel=\"stylesheet\" href=\"{prefix}docs/docs.css\">
<script src=\"{prefix}theme.js\"></script>
<script src=\"{prefix}docs/search.js\" defer></script>
</head>
<body id=\"top\">

<!-- Rendered from the repository's Markdown by scripts/site/docs (OWL-607):
     edit the Markdown file, not this page. -->

{bar}

<div class=\"wrap docs\">
<aside class=\"docs-side\">
<div class=\"search\" hidden>
<label class=\"search-label\" for=\"search-box\">Search the documentation</label>
<input class=\"search-box\" id=\"search-box\" type=\"search\" autocomplete=\"off\" spellcheck=\"false\" placeholder=\"Search the documentation\">
<p class=\"search-status\" aria-live=\"polite\"></p>
<ol class=\"search-results\"></ol>
</div>
<details class=\"pages-narrow\">
<summary>Pages</summary>
<nav aria-label=\"Documentation pages\">
{list}</nav>
</details>
<nav class=\"pages-wide\" aria-label=\"Documentation\">
{list}</nav>
</aside>
<main class=\"docs-text\">
{text}</main>
{toc}</div>

{foot}

</body>
</html>
",
        policy = frame.policy,
        title = escape(title),
    )
}

/// The list of a page's sections, the third column: its `##` headings, when
/// it has two or more.
fn sections_list(page: &Page) -> String {
    let sections: Vec<_> = page.sections.iter().filter(|s| s.level == 2).collect();
    if sections.len() < 2 {
        return String::new();
    }
    let mut toc = String::from(
        "<nav class=\"docs-toc\" aria-label=\"On this page\">\n<p class=\"toc-title\">On this page</p>\n<ul>\n",
    );
    for section in sections {
        toc.push_str(&format!(
            "<li><a href=\"#{}\">{}</a></li>\n",
            escape(&section.anchor),
            escape(&section.heading)
        ));
    }
    toc.push_str("</ul>\n</nav>\n");
    toc
}

/// Renders the documentation of `source` into `out/docs/`: one page per
/// Markdown file, `index.html` listing them, the stylesheet, the search's
/// script and its index. Returns the number of pages written, or every
/// error found, each naming its file; nothing is written then.
pub fn render(source: &Source, out: &Path) -> Result<usize, Vec<String>> {
    let mut errors = Vec::new();
    let frame = html::frame(source.index).map_err(|e| vec![e])?;
    let listed: Vec<&str> = source
        .pages
        .iter()
        .flat_map(|group| group.pages.iter().copied())
        .collect();
    let served = served_files(source.root);
    for file in &served {
        if !listed.contains(&file.as_str()) {
            errors.push(format!(
                "docs/{file}: served but missing from the pages' list (scripts/site/docs/src/lib.rs, PAGES)"
            ));
        }
    }
    for page in &listed {
        if !served.iter().any(|file| file == page) {
            errors.push(format!(
                "docs/{page}: in the pages' list but no such file is served"
            ));
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let served_paths: Vec<String> = listed.iter().map(|page| format!("docs/{page}")).collect();
    let links = Links {
        root: source.root,
        served: &served_paths,
    };
    let mut pages = Vec::new();
    for page in &listed {
        let path = format!("docs/{page}");
        let text = match std::fs::read_to_string(join(source.root, &path)) {
            Ok(text) => text,
            Err(e) => {
                errors.push(format!("{path}: {e}"));
                continue;
            }
        };
        match markdown::page(&text, &path, &links) {
            Ok(rendered) => pages.push((*page, rendered)),
            Err(e) => errors.extend(e),
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    let titles: Vec<(String, String)> = pages
        .iter()
        .map(|(page, rendered)| (page.to_string(), rendered.title.clone()))
        .collect();
    let mut files: Vec<(String, String)> = Vec::new();
    let mut entries = Vec::new();
    for (page, rendered) in &pages {
        let path = format!("docs/{}", html_path(page));
        let list = page_list(source.pages, &titles, &path);
        files.push((
            path.clone(),
            document(
                &frame,
                &path,
                &rendered.title,
                &list,
                &rendered.html,
                &sections_list(rendered),
            ),
        ));
        for section in &rendered.sections {
            entries.push(serde_json::json!({
                "page": html_path(page),
                "title": rendered.title,
                "anchor": section.anchor,
                "heading": section.heading,
                "text": section.text,
            }));
        }
    }
    let index_path = "docs/index.html";
    let mut index = String::from(
        "<h1 id=\"documentation\">Documentation</h1>\n<p>How to install Owlshift, adopt a project and configure it: the repository's user documentation, by group.</p>\n",
    );
    for group in source.pages {
        index.push_str(&format!(
            "<h2 id=\"{}\">{}</h2>\n<ul class=\"docs-index\">\n",
            markdown::github_anchor(group.name),
            escape(group.name)
        ));
        for page in group.pages {
            let title = titles
                .iter()
                .find(|(listed, _)| listed == page)
                .map_or("", |(_, title)| title.as_str());
            index.push_str(&format!(
                "<li><a href=\"{}\">{}</a></li>\n",
                html_path(page),
                escape(title)
            ));
        }
        index.push_str("</ul>\n");
    }
    files.push((
        index_path.to_owned(),
        document(
            &frame,
            index_path,
            "Documentation",
            &page_list(source.pages, &titles, index_path),
            &index,
            "",
        ),
    ));
    files.push(("docs/docs.css".to_owned(), DOCS_CSS.to_owned()));
    files.push(("docs/search.js".to_owned(), SEARCH_JS.to_owned()));
    files.push((
        "docs/search-index.js".to_owned(),
        search_index(&serde_json::Value::Array(entries)),
    ));

    for (path, text) in &files {
        let file = join(out, path);
        let written = file
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&file, text));
        if let Err(e) = written {
            errors.push(format!("{}: {e}", file.display()));
        }
    }
    if errors.is_empty() {
        Ok(pages.len())
    } else {
        Err(errors)
    }
}
