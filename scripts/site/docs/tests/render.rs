//! The documentation as the site serves it (build plan, "The public site",
//! "The documentation", "Kept true by CI"): rendered into a temporary folder
//! and read back. URLs are joined with `/` only, so the test reads the same on
//! macOS, Linux and Windows.

use std::path::{Path, PathBuf};

use owlshift_site_docs::html::{attributes, floor_failures, frame, is_absolute, loads, tags};
use owlshift_site_docs::{Group, PAGES, Source, render, search_index, served_files};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn index() -> String {
    std::fs::read_to_string(root().join("site").join("index.html")).expect("site/index.html")
}

/// `path`, `/`-separated from `base`, as a path of the system.
fn join(base: &Path, path: &str) -> PathBuf {
    path.split('/')
        .fold(base.to_path_buf(), |path, part| path.join(part))
}

/// The site path a relative `url` of the page `page` names, both from the
/// site's root; `None` when it climbs above it.
fn resolve(page: &str, url: &str) -> Option<String> {
    let mut parts: Vec<&str> = page.split('/').collect();
    parts.pop();
    for part in url.split('/') {
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

/// Every file of `folder`, as paths from it, `/`-separated.
fn files(folder: &Path, prefix: &str, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(folder).unwrap().flatten() {
        let name = format!("{prefix}{}", entry.file_name().to_string_lossy());
        if entry.path().is_dir() {
            files(&entry.path(), &format!("{name}/"), out);
        } else {
            out.push(name);
        }
    }
}

#[test]
fn the_served_documentation_keeps_the_sites_rules() {
    let root = root();
    let index = index();
    let policy = frame(&index).expect("the page's frame").policy;
    let build = std::fs::read_to_string(root.join("scripts/site/build.sh")).unwrap();
    let out = tempfile::tempdir().unwrap();
    let pages = render(
        &Source {
            root: &root,
            pages: PAGES,
            index: &index,
        },
        out.path(),
    )
    .unwrap_or_else(|errors| panic!("{}", errors.join("\n")));
    assert_eq!(pages, 11);

    let mut written = Vec::new();
    files(out.path(), "", &mut written);
    let html: Vec<&String> = written.iter().filter(|f| f.ends_with(".html")).collect();
    assert_eq!(html.len(), 12, "the 11 pages and their index: {written:?}");
    let ids_of = |site_path: &str| -> Option<Vec<String>> {
        let text = if site_path.starts_with("docs/") {
            std::fs::read_to_string(join(out.path(), site_path)).ok()?
        } else {
            std::fs::read_to_string(join(&root.join("site"), site_path)).ok()?
        };
        Some(attributes(&text, "id"))
    };

    let mut failures = Vec::new();
    let mut links = 0;
    for page in html {
        let text = std::fs::read_to_string(join(out.path(), page)).unwrap();
        for failure in floor_failures(&text, &policy) {
            failures.push(format!("{page}: {failure}"));
        }
        for url in loads(&text) {
            let Some(path) = resolve(page, &url) else {
                failures.push(format!("{page}: {url} leaves the site"));
                continue;
            };
            let name = path.rsplit('/').next().unwrap();
            let found = if path.starts_with("docs/") {
                written.contains(&path)
            } else if path.starts_with("fonts/") || path.starts_with("brand/") {
                build.contains(name)
            } else {
                join(&root.join("site"), &path).is_file()
            };
            if !found {
                failures.push(format!("{page}: loads {url}, which the site does not hold"));
            }
        }
        let own_ids = attributes(&text, "id");
        let anchors = tags(&text, "a");
        for href in anchors.iter().flat_map(|a| attributes(a, "href")) {
            if is_absolute(&href) {
                continue;
            }
            links += 1;
            let (url, fragment) = href.split_once('#').unwrap_or((&href, ""));
            let target = if url.is_empty() {
                Some(page.clone())
            } else {
                resolve(page, url)
            };
            let Some(target) = target else {
                failures.push(format!("{page}: {href} leaves the site"));
                continue;
            };
            let ids = if target == *page {
                Some(own_ids.clone())
            } else {
                ids_of(&target)
            };
            match ids {
                None => failures.push(format!("{page}: {href} names no file of the site")),
                Some(ids) if !fragment.is_empty() && !ids.iter().any(|id| id == fragment) => {
                    failures.push(format!("{page}: {href}: {target} has no id #{fragment}"));
                }
                Some(_) => {}
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    // Each page links the 12 of the list, the bar and the foot.
    assert!(links > 200, "{links} links within the site, more expected");

    let css = std::fs::read_to_string(out.path().join("docs").join("docs.css")).unwrap();
    assert!(!css.contains("url("), "docs.css loads nothing");

    let index_js =
        std::fs::read_to_string(out.path().join("docs").join("search-index.js")).unwrap();
    let entries = read_index(&index_js);
    for group in PAGES {
        for page in group.pages {
            let html = page.replace(".md", ".html");
            assert!(
                entries.iter().any(|entry| entry["page"] == html.as_str()),
                "the search finds no section of {page}"
            );
        }
    }
}

#[test]
fn the_pages_list_names_the_served_files() {
    let mut listed: Vec<String> = PAGES
        .iter()
        .flat_map(|group| group.pages.iter().map(|page| page.to_string()))
        .collect();
    listed.sort();
    assert_eq!(listed, served_files(&root()));
}

/// The index's entries, read back from `search-index.js`.
fn read_index(script: &str) -> Vec<serde_json::Value> {
    let json = script
        .strip_prefix("window.owlshiftSearchIndex = ")
        .and_then(|rest| rest.strip_suffix(";\n"))
        .expect("one assignment");
    serde_json::from_str::<Vec<serde_json::Value>>(json).expect("a JSON array")
}

const FIXTURE: &[Group] = &[Group {
    name: "Start",
    pages: &["a.md"],
}];

/// Renders a documentation of one page, `docs/a.md` holding `markdown`, into
/// `out/` of a temporary repository holding `others` too: a file, or a folder
/// when its path ends with `/`.
fn render_fixture(
    markdown: &str,
    others: &[&str],
) -> (tempfile::TempDir, Result<usize, Vec<String>>) {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("docs")).unwrap();
    std::fs::write(root.path().join("docs").join("a.md"), markdown).unwrap();
    for other in others {
        match other.strip_suffix('/') {
            Some(folder) => std::fs::create_dir_all(join(root.path(), folder)).unwrap(),
            None => std::fs::write(join(root.path(), other), "# Other\n").unwrap(),
        }
    }
    let out = root.path().join("out");
    let index = index();
    let result = render(
        &Source {
            root: root.path(),
            pages: FIXTURE,
            index: &index,
        },
        &out,
    );
    (root, result)
}

#[test]
fn what_the_site_does_not_serve_fails_the_build() {
    for (markdown, others, error) in [
        ("# A\n\n<div>x</div>\n", &[][..], "raw HTML"),
        ("# A\n\nText <span>x</span>.\n", &[], "raw HTML"),
        ("# A\n\n<!-- open\n", &[], "not closed"),
        ("# A\n\n![x](a.png)\n", &[], "an image"),
        ("# A\n\n[x](javascript:alert(1))\n", &[], "scheme"),
        ("# A\n\n[x](java&#115;cript:alert(1))\n", &[], "scheme"),
        ("# A\n\n[x](//example.com/a)\n", &[], "without its scheme"),
        ("# A\n\n[x](nope.md)\n", &[], "does not exist"),
        (
            "# A\n\n[x](../../up.md)\n",
            &[],
            "above the repository's root",
        ),
        ("Text first.\n\n# A\n", &[], "does not open with its title"),
        ("# A\n", &["docs/b.md"], "missing from the pages' list"),
    ] {
        let (_root, result) = render_fixture(markdown, others);
        let errors = result.expect_err(markdown).join("\n");
        assert!(errors.contains(error), "{markdown:?}: {errors}");
    }
}

#[test]
fn links_lead_to_the_served_page_or_to_github() {
    // A comment over two lines is dropped, as a one-line one is.
    let (root, result) = render_fixture(
        "# A\n\n<!-- one\ntwo -->\n\nSee [the readme](../README.md#quick-start), [the folder](design/), \
         [a mail](mailto:a@b.c) and https://example.com/a_b.\n",
        &["README.md", "docs/design/"],
    );
    result.unwrap_or_else(|errors| panic!("{}", errors.join("\n")));
    let page =
        std::fs::read_to_string(root.path().join("out").join("docs").join("a.html")).unwrap();
    for href in [
        "https://github.com/pitchopp/owlshift/blob/main/README.md#quick-start",
        "https://github.com/pitchopp/owlshift/tree/main/docs/design",
        "mailto:a@b.c",
        "https://example.com/a_b",
    ] {
        assert!(
            page.contains(&format!("href=\"{href}\"")),
            "{href} missing from {page}"
        );
    }
}

#[test]
fn the_index_reads_back_as_written() {
    let (root, result) = render_fixture(
        "# Quotes \"q\" and `</script>`\n\nA backslash \\\\, `</script>` and \"quotes\".\n",
        &[],
    );
    result.unwrap_or_else(|errors| panic!("{}", errors.join("\n")));
    let script =
        std::fs::read_to_string(root.path().join("out").join("docs").join("search-index.js"))
            .unwrap();
    assert!(!script.contains('<'), "{script}");
    let entries = read_index(&script);
    assert_eq!(entries[0]["heading"], "Quotes \"q\" and </script>");
    assert_eq!(
        entries[0]["text"],
        "A backslash \\, </script> and \"quotes\"."
    );

    // The line ends of older JavaScript, which the folded page text never
    // keeps, are escaped all the same.
    let tricky = "a\u{2028}b\u{2029}c</script>\"\\";
    let script = search_index(&serde_json::json!([{ "text": tricky }]));
    assert!(!script.contains(['<', '\u{2028}', '\u{2029}']), "{script}");
    assert_eq!(read_index(&script)[0]["text"], tricky);
}
