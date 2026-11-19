//! The site's HTML read as plain text, and the floors every page of the site
//! holds (build plan, "The public site", "Kept true by CI").
//!
//! The page is written by hand with double-quoted attributes, and the
//! documentation's pages are written by this crate, whose text goes through
//! pulldown-cmark's escaping: a `<` or a `"` of the text never reaches the
//! HTML raw. So every `<` and every `name="` of a page is markup, and string
//! scanning reads it.

/// The text of `value` with the page's character references replaced.
pub fn decode(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// The values of every `name="…"` attribute in `html`, decoded.
pub fn attributes(html: &str, name: &str) -> Vec<String> {
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
pub fn tags<'a>(html: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}");
    html.match_indices(&open)
        .filter(|(at, _)| {
            html[at + open.len()..].starts_with(|c: char| c.is_whitespace() || c == '>')
        })
        .map(|(at, _)| &html[at..at + html[at..].find('>').expect("a closed tag") + 1])
        .collect()
}

/// Whether a URL leaves the folder it is read from: a scheme, or a path from
/// the host's root. The site is served under a path prefix, so everything it
/// loads, and every link within it, is relative.
pub fn is_absolute(url: &str) -> bool {
    url.contains(':') || url.starts_with('/')
}

/// Every URL `html` loads: each `src`, and each `<link>`'s `href`.
pub fn loads(html: &str) -> Vec<String> {
    let mut loads = attributes(html, "src");
    for link in tags(html, "link") {
        loads.extend(attributes(link, "href"));
    }
    loads
}

/// The names of the attributes of every opening tag of `html`.
fn attribute_names(html: &str) -> Vec<String> {
    let mut names = Vec::new();
    let bytes = html.as_bytes();
    let mut at = 0;
    while let Some(found) = html[at..].find('<') {
        let start = at + found + 1;
        at = start;
        if !bytes.get(start).is_some_and(u8::is_ascii_alphabetic) {
            continue;
        }
        // The tag's name, then its attributes up to the closing `>`.
        let mut i = start;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
            i += 1;
        }
        loop {
            while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
                i += 1;
            }
            if i >= bytes.len() || bytes[i] == b'>' {
                break;
            }
            let name_start = i;
            while i < bytes.len()
                && !matches!(bytes[i], b'=' | b'>' | b'/')
                && !bytes[i].is_ascii_whitespace()
            {
                i += 1;
            }
            names.push(html[name_start..i].to_ascii_lowercase());
            if bytes.get(i) == Some(&b'=') {
                i += 1;
                match bytes.get(i) {
                    Some(&quote @ (b'"' | b'\'')) => {
                        i += 1;
                        while i < bytes.len() && bytes[i] != quote {
                            i += 1;
                        }
                        i += 1;
                    }
                    _ => {
                        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>'
                        {
                            i += 1;
                        }
                    }
                }
            }
        }
        at = i.min(bytes.len());
    }
    names
}

/// What `html` breaks of the site's floors: the policy line `policy`, a
/// `<meta>` element written word for word; no inline script, `<style>`
/// element, style attribute, event attribute or frame, which the policy
/// refuses or which would load another document; nothing loaded from an
/// absolute URL. Empty when it holds them all.
pub fn floor_failures(html: &str, policy: &str) -> Vec<String> {
    let mut failures = Vec::new();
    if !html.contains(policy) {
        failures.push(format!("lacks the policy line, word for word: {policy}"));
    }
    for script in tags(html, "script") {
        if !script.contains(" src=\"") {
            failures.push(format!("an inline script: {script}"));
        }
    }
    if !tags(html, "style").is_empty() {
        failures.push("a <style> element".to_owned());
    }
    for name in attribute_names(html) {
        if name == "style" {
            failures.push("a style attribute".to_owned());
        } else if name.starts_with("on") {
            failures.push(format!("an event attribute: {name}"));
        }
    }
    for frame in ["iframe", "frame", "object", "embed"] {
        if !tags(html, frame).is_empty() {
            failures.push(format!("a <{frame}> element"));
        }
    }
    for url in loads(html) {
        if is_absolute(&url) {
            failures.push(format!(
                "{url}: loaded from an absolute URL; the site is served under a path prefix"
            ));
        }
    }
    failures
}

/// The parts of the page every page of the documentation is set in.
#[derive(Debug, Clone)]
pub struct Frame {
    /// The page's `<meta http-equiv="Content-Security-Policy" …>`, word for word.
    pub policy: String,
    /// The page's bar, `<header class="bar">…</header>`, its URLs as the page has them.
    pub bar: String,
    /// The page's foot, `<footer class="foot">…</footer>`.
    pub foot: String,
}

fn once(html: &str, needle: &str) -> Result<usize, String> {
    let found: Vec<usize> = html.match_indices(needle).map(|(at, _)| at).collect();
    match found[..] {
        [at] => Ok(at),
        _ => Err(format!(
            "site/index.html: `{needle}` found {} times, once expected",
            found.len()
        )),
    }
}

/// The element of `html` from `open` to `close`, both found exactly once.
fn element(html: &str, open: &str, close: &str) -> Result<String, String> {
    let start = once(html, open)?;
    let end = once(html, close)?;
    if end < start {
        return Err(format!("site/index.html: `{close}` comes before `{open}`"));
    }
    Ok(html[start..end + close.len()].to_owned())
}

/// The frame of the page `index`, the text of `site/index.html`.
pub fn frame(index: &str) -> Result<Frame, String> {
    let open = "<meta http-equiv=\"Content-Security-Policy\"";
    let start = once(index, open)?;
    let end = start
        + index[start..]
            .find('>')
            .ok_or("site/index.html: the policy line is not closed")?;
    Ok(Frame {
        policy: index[start..=end].to_owned(),
        bar: element(index, "<header class=\"bar\">", "</header>")?,
        foot: element(index, "<footer class=\"foot\">", "</footer>")?,
    })
}

/// `part` with every `href` and `src` rebased for a page `prefix` below the
/// site's root (`prefix` being `../` once per folder): a fragment leads back
/// to the page's own, a relative URL is taken from the root, an absolute one
/// stays as it is.
pub fn rebase(part: &str, prefix: &str) -> String {
    let mut out = String::with_capacity(part.len());
    let mut rest = part;
    loop {
        let next = ["href=\"", "src=\""]
            .iter()
            .filter_map(|pattern| {
                rest.match_indices(pattern)
                    .find(|(at, _)| rest[..*at].ends_with(char::is_whitespace))
                    .map(|(at, _)| (at, pattern.len()))
            })
            .min();
        let Some((at, len)) = next else {
            out.push_str(rest);
            return out;
        };
        let value_start = at + len;
        let value_end = value_start + rest[value_start..].find('"').expect("a closed attribute");
        let value = &rest[value_start..value_end];
        out.push_str(&rest[..value_start]);
        if value.starts_with('#') {
            out.push_str(prefix);
            out.push_str("index.html");
            out.push_str(value);
        } else if is_absolute(value) {
            out.push_str(value);
        } else {
            out.push_str(prefix);
            out.push_str(value);
        }
        rest = &rest[value_end..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_floors_find_what_the_policy_refuses() {
        let policy = "<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'\">";
        let good =
            format!("<head>{policy}<script src=\"a.js\"></script></head><p>on=\"x\" style</p>");
        assert!(
            floor_failures(&good, policy).is_empty(),
            "{:?}",
            floor_failures(&good, policy)
        );
        let bad = "<script>x</script><style></style><p style=\"a\"><img onerror=\"x\" src=\"https://x/a.png\"><iframe src=\"a\">";
        let failures = floor_failures(bad, policy);
        assert_eq!(failures.len(), 7, "{failures:#?}");
    }

    #[test]
    fn the_frame_is_rebased_for_a_page_below_the_root() {
        let bar = "<a class=\"x\" href=\"#top\"><img src=\"brand/a.svg\"></a><a href=\"docs/index.html\">Docs</a><a href=\"https://github.com/x\">";
        assert_eq!(
            rebase(bar, "../../"),
            "<a class=\"x\" href=\"../../index.html#top\"><img src=\"../../brand/a.svg\"></a><a href=\"../../docs/index.html\">Docs</a><a href=\"https://github.com/x\">"
        );
    }

    #[test]
    fn a_frame_part_found_twice_is_refused() {
        let index = "<meta http-equiv=\"Content-Security-Policy\" content=\"x\"><header class=\"bar\"></header><footer class=\"foot\"></footer><footer class=\"foot\"></footer>";
        assert!(frame(index).unwrap_err().contains("2 times"));
    }
}
