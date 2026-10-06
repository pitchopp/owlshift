//! `owlshift hello [NAME]`: greets the user.

/// The greeting for `name`: a missing, empty or whitespace-only name greets
/// the world.
pub fn greeting(name: Option<&str>) -> String {
    match name {
        Some(name) if !name.trim().is_empty() => format!("Hello, {name}!"),
        _ => "Hello, world!".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::greeting;

    #[test]
    fn no_name_greets_the_world() {
        assert_eq!(greeting(None), "Hello, world!");
    }

    #[test]
    fn a_blank_name_greets_the_world() {
        assert_eq!(greeting(Some("")), "Hello, world!");
        assert_eq!(greeting(Some("  \t")), "Hello, world!");
    }

    #[test]
    fn a_name_is_printed_as_given() {
        assert_eq!(greeting(Some("Ada")), "Hello, Ada!");
        assert_eq!(greeting(Some(" Ada ")), "Hello,  Ada !");
    }
}
