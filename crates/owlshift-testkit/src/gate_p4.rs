//! A small helper written for the P4 exit gate (OWL-255), to give the Build a
//! real, reviewable change to make.

/// Counts the occurrences of the word "owl" in `text`, ignoring case.
///
/// A word is a run of alphanumeric characters, so "owls" and "bowl" do not
/// count, while "owl." and "(owl)" do.
///
/// ```
/// use owlshift_testkit::gate_p4::owl_count;
///
/// assert_eq!(owl_count("Owl owl OWL"), 3);
/// assert_eq!(owl_count("bowl owls"), 0);
/// ```
pub fn owl_count(text: &str) -> usize {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.eq_ignore_ascii_case("owl"))
        .count()
}

#[cfg(test)]
mod tests {
    use super::owl_count;

    #[test]
    fn empty_string_has_no_owl() {
        assert_eq!(owl_count(""), 0);
    }

    #[test]
    fn counts_case_insensitively() {
        assert_eq!(owl_count("Owl owl OWL"), 3);
    }

    #[test]
    fn whole_words_only() {
        assert_eq!(owl_count("bowl owls"), 0);
    }

    #[test]
    fn punctuation_does_not_hide_the_word() {
        assert_eq!(owl_count("owl."), 1);
        assert_eq!(owl_count("(owl)"), 1);
    }
}
