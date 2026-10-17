//! The result a read-only run gives as its last message when it leaves no
//! result file (architecture section 6, "Harness contract", OWL-323).

/// The result in a run's last message: the whole message, trimmed, when it
/// parses as JSON; else the body of its last fenced code block, when that
/// block's closing fence ends the message, whitespace aside; else `None`.
///
/// Fences are read as CommonMark reads backtick fences, from the top, with
/// no indentation: a line that starts with at least three backticks and has
/// none in what follows them (an info string such as `json`) opens a block,
/// and a line of at least as many backticks, then only spaces, closes it.
/// The body is the lines between, taken whatever they hold: the result's
/// validation judges it. Tildes, an indented fence, a block left open and
/// text after the closing fence give nothing.
pub(super) fn last_message_result(message: &str) -> Option<&str> {
    let text = message.trim();
    if serde_json::from_str::<serde_json::Value>(text).is_ok() {
        return Some(text);
    }
    let mut open: Option<(usize, usize)> = None;
    let mut last = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        let ticks = content.bytes().take_while(|&byte| byte == b'`').count();
        let rest = &content[ticks..];
        match open {
            None if ticks >= 3 && !rest.contains('`') => open = Some((ticks, offset + line.len())),
            Some((opened, body)) if ticks >= opened && rest.trim_matches(' ').is_empty() => {
                open = None;
                let end = offset + line.len();
                last = (end == text.len()).then(|| &text[body..offset]);
            }
            _ => {}
        }
        offset += line.len();
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_result_is_the_whole_message_or_its_last_closing_block() {
        let result = "{\"format\":10,\"status\":\"blocked\",\"summary\":\"s\"}";
        for (message, expected) in [
            // Bare JSON, whitespace around it trimmed; any JSON value.
            (format!("\n  {result}\n"), Some(result.to_owned())),
            ("42".to_owned(), Some("42".to_owned())),
            // Locary's shape: a line of prose, then the block.
            (
                format!("I couldn't write `result.json`.\n\n```json\n{result}\n```\n"),
                Some(format!("{result}\n")),
            ),
            // Any info string, or none; CRLF lines; a longer closing fence.
            (
                format!("```JSON\n{result}\n```"),
                Some(format!("{result}\n")),
            ),
            (
                format!("```\n{result}\n````  "),
                Some(format!("{result}\n")),
            ),
            (
                format!("Here:\r\n```json\r\n{result}\r\n```\r\n"),
                Some(format!("{result}\r\n")),
            ),
            // The last block, even when it is not a result and an earlier one is.
            (
                format!("```json\n{result}\n```\nThen:\n```\nnot json\n```"),
                Some("not json\n".to_owned()),
            ),
            // Prose after the block, or a block left open.
            (format!("```json\n{result}\n```\nDone."), None),
            (format!("```json\n{result}"), None),
            // A closing fence shorter than the opening one does not close it.
            (format!("````json\n{result}\n```"), None),
            // A line of backticks inside the block closes it there.
            ("```json\n{\"a\":\"\n```\n\"}\n```".to_owned(), None),
            // Tildes, indented fences, a fence on the JSON's own line.
            (format!("~~~json\n{result}\n~~~"), None),
            (format!("  ```json\n  {result}\n  ```"), None),
            (format!("```json\n{result}```"), None),
            ("No result here.".to_owned(), None),
        ] {
            assert_eq!(
                last_message_result(&message),
                expected.as_deref(),
                "{message:?}"
            );
        }
    }
}
