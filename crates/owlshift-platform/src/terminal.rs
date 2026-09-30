//! The terminal standard output writes to, for reports laid out to its width
//! (`owlshift doctor`, OWL-99).

/// The width, in columns, of the terminal standard output is: `None` when it
/// is not a terminal, when the terminal does not say, or on Windows, where
/// native runs are set aside (decision D9).
pub fn stdout_width() -> Option<usize> {
    #[cfg(unix)]
    {
        let size = rustix::termios::tcgetwinsize(std::io::stdout()).ok()?;
        // Some pseudo-terminals answer 0 columns: as good as no answer.
        (size.ws_col > 0).then_some(usize::from(size.ws_col))
    }
    #[cfg(not(unix))]
    {
        None
    }
}
