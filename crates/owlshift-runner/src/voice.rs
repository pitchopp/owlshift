//! Who a text is for (OWL-529, architecture section 5, "The local web UI",
//! "Actions"): the runner's functions that word what a command did are shared
//! by the command and the web UI, and a text that names a command the UI has
//! a button for names the button when the UI shows it.

/// Whose words a shared text is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Voice {
    /// A person typing: the CLI's commands, and the watch's own lines. A
    /// text names the command to type.
    Terminal,
    /// A person clicking: the web UI's routes and screens. A text names the
    /// UI's own button where it has one, and the command where it has none.
    Ui,
}
