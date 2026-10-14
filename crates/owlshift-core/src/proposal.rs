//! Follow-up proposals (architecture section 5, "Follow-ups", "Not twice"):
//! the title a proposal's fingerprint is taken from. Hashing stays out of the
//! core: the runner takes the SHA-256 of [`normalize_title`].

/// A title as its fingerprint reads it: lowercased, with every run of
/// characters other than ASCII letters and digits made one space, and the
/// spaces at either end dropped (OWL-253), so that "Add X." and "add x"
/// are one proposal. Every other character is such a run: "Résumé" reads
/// `r sum`.
pub fn normalize_title(title: &str) -> String {
    title
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_is_normalized_to_its_ascii_words() {
        for (title, normalized) in [
            ("Cap the retries", "cap the retries"),
            ("  Cap   the RETRIES.  ", "cap the retries"),
            ("cap-the_retries!", "cap the retries"),
            ("Résumé 2 étapes", "r sum 2 tapes"),
            ("Add `owlshift doctor` line", "add owlshift doctor line"),
            ("...", ""),
            ("", ""),
        ] {
            assert_eq!(normalize_title(title), normalized, "{title:?}");
        }
    }
}
