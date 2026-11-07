//! Reading a run's log from a byte offset, whole lines only (P8 part 6,
//! OWL-471): what the ticket page follows while a run writes.
//!
//! A call returns the lines that start at or after `from`, each once its
//! newline is written, as `owlshift`'s own `events::print` does: a half
//! written last line is withheld, and read whole by the call after the
//! newline lands, `next` being where that call goes on. Once the run has
//! ended (`ended`), a last line with no newline will never get one, so it is
//! served as a final line and `next` equals the size.
//!
//! The cap bounds an answer, not a line:
//!
//! - lines are added while the answer stays within `max` bytes, but the first
//!   line of an answer is always returned whole, however long, so a line
//!   between `max` and [`MAX_LINE`] (a `Write` tool's result repeats its
//!   content several times, a Codex `command_execution` carries its whole
//!   output) is served alone and `next` moves past it;
//! - a line past [`MAX_LINE`] is the one that is cut: its first
//!   [`CUT_PREFIX`] bytes are returned, marked with its byte length, and
//!   `next` moves past its newline, found by reading on in chunks without
//!   holding the line. An unterminated partial line past [`MAX_LINE`] on a
//!   run that goes on waits for its newline, like any other.
//!
//! A log that shrank (`from` past its end) is read again from 0 and says so
//! (`reset`); an absent file is an empty one.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// The most a line may weigh to be served whole: 8 MiB.
pub const MAX_LINE: usize = 8 * 1024 * 1024;

/// The bytes of a line past [`MAX_LINE`] that are served: 64 KiB.
pub const CUT_PREFIX: usize = 64 * 1024;

/// The bytes of an answer, past its first line: 256 KiB.
pub const MAX_ANSWER: usize = 256 * 1024;

/// One line of a log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// The offset of its first byte in the log.
    pub offset: u64,
    /// Its bytes, without the newline; its first [`CUT_PREFIX`] when `cut`.
    pub bytes: Vec<u8>,
    /// The byte length of the line, without its newline, when it is past
    /// [`MAX_LINE`] and `bytes` is only its first [`CUT_PREFIX`].
    pub cut: Option<u64>,
}

/// What a call returns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tail {
    pub lines: Vec<Line>,
    /// Where the next call goes on.
    pub next: u64,
    /// The size of the log when it was read.
    pub size: u64,
    /// The log is shorter than `from`: the lines are those from 0.
    pub reset: bool,
}

/// One line as scanned.
struct Scanned {
    /// The kept bytes, without the newline.
    bytes: Vec<u8>,
    /// The bytes it takes in the log, its newline included.
    taken: u64,
    terminated: bool,
    /// The content's length, when it is past [`MAX_LINE`].
    cut: Option<u64>,
}

/// What a scan found.
enum Scan {
    Line(Scanned),
    /// The line would take more than the room an answer has left.
    NoRoom,
    /// The end of what there is to read.
    End,
}

/// Scans the line `reader` is at. With `room`, a line that takes more than
/// that many bytes is left unread past that point: [`Scan::NoRoom`].
fn scan_line(reader: &mut impl BufRead, room: Option<u64>) -> io::Result<Scan> {
    let mut bytes = Vec::new();
    let mut taken = 0u64;
    let mut over = false;
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            return Ok(if taken == 0 {
                Scan::End
            } else {
                Scan::Line(finish(bytes, taken, false, over))
            });
        }
        let newline = chunk.iter().position(|&b| b == b'\n');
        let part = &chunk[..newline.unwrap_or(chunk.len())];
        let consumed = newline.map_or(chunk.len(), |at| at + 1);
        if room.is_some_and(|room| taken + consumed as u64 > room) {
            return Ok(Scan::NoRoom);
        }
        if !over {
            if bytes.len() + part.len() > MAX_LINE {
                over = true;
                bytes.extend_from_slice(&part[..part.len().min(CUT_PREFIX)]);
                bytes.truncate(CUT_PREFIX);
            } else {
                bytes.extend_from_slice(part);
            }
        }
        taken += consumed as u64;
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(Scan::Line(finish(bytes, taken, true, over)));
        }
    }
}

fn finish(mut bytes: Vec<u8>, taken: u64, terminated: bool, over: bool) -> Scanned {
    let content = taken - u64::from(terminated);
    if over {
        bytes.truncate(CUT_PREFIX);
    }
    Scanned {
        bytes,
        taken,
        terminated,
        cut: over.then_some(content),
    }
}

/// Reads the lines of the log at `path` from byte offset `from`, at most
/// `max` bytes of them past the first, as the module says. `ended` is
/// whether the run that writes the log has ended.
pub fn read_lines(path: &Path, from: u64, max: usize, ended: bool) -> Result<Tail, String> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Tail {
                reset: from > 0,
                ..Tail::default()
            });
        }
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let fail = |error: io::Error| format!("{}: {error}", path.display());
    let size = file.metadata().map_err(fail)?.len();
    let (start, reset) = if from > size {
        (0, true)
    } else {
        (from, false)
    };
    file.seek(SeekFrom::Start(start)).map_err(fail)?;
    let mut reader = BufReader::with_capacity(CUT_PREFIX, file.take(size - start));
    let mut lines: Vec<Line> = Vec::new();
    let mut total = 0usize;
    let mut next = start;
    loop {
        let room = (!lines.is_empty()).then(|| (max.saturating_sub(total)) as u64);
        let scanned = match scan_line(&mut reader, room).map_err(fail)? {
            Scan::Line(scanned) => scanned,
            Scan::NoRoom | Scan::End => break,
        };
        if !scanned.terminated && !ended {
            break;
        }
        lines.push(Line {
            offset: next,
            bytes: scanned.bytes,
            cut: scanned.cut,
        });
        next += scanned.taken;
        total = total.saturating_add(scanned.taken as usize);
        if !scanned.terminated {
            break;
        }
    }
    Ok(Tail {
        lines,
        next,
        size,
        reset,
    })
}

/// How many lines the log at `path` holds before byte `offset`, which must
/// be where a line starts: what numbers the records of a read that goes on
/// from there.
pub fn count_lines(path: &Path, offset: u64) -> Result<u64, String> {
    let fail = |error: io::Error| format!("{}: {error}", path.display());
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(fail(error)),
    };
    let mut reader = BufReader::with_capacity(CUT_PREFIX, file.take(offset));
    let mut count = 0u64;
    loop {
        let chunk = reader.fill_buf().map_err(fail)?;
        if chunk.is_empty() {
            return Ok(count);
        }
        count += chunk.iter().filter(|&&b| b == b'\n').count() as u64;
        let length = chunk.len();
        reader.consume(length);
    }
}

/// The file's identity, where the system gives one: a replaced file is not
/// the file that was being read, even when it is as long.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity(Option<(u64, u64)>);

impl Identity {
    fn of(meta: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self(Some((meta.dev(), meta.ino())))
        }
        #[cfg(not(unix))]
        {
            let _ = meta;
            Self(None)
        }
    }
}

/// One whole line a [`LineTail`] read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TailLine {
    /// The offset of its first byte in the file.
    pub offset: u64,
    /// Its text without the newline, lossy where it is not UTF-8, and cut
    /// with a `…` when it was longer than the call's `cut`.
    pub text: String,
}

/// What one call of [`LineTail::read`] found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TailRead {
    pub lines: Vec<TailLine>,
    /// The tail started again from byte 0: the file is shorter than the
    /// offset it had read to, is another file, or went away. What a caller
    /// kept from the file before is stale.
    pub restarted: bool,
}

/// A file read from a byte offset, whole lines only (OWL-472): the event log
/// the Orchestrator's index follows, and the daemon's log the screen shows.
///
/// - A line is returned once its newline is written, as `owlshift logs` does
///   (`events::print`); a half-written last line waits for the call after
///   its newline lands.
/// - It starts again from byte 0 when the file is shorter than the offset or
///   is another file (on unix the device and inode, elsewhere the length
///   alone), and says so ([`TailRead::restarted`]).
/// - A file that is not there reads nothing, and is read from its start when
///   it appears; one that was being read and went away has restarted.
#[derive(Debug)]
pub struct LineTail {
    path: PathBuf,
    /// The offset of the first byte not read: a line start.
    offset: u64,
    identity: Option<Identity>,
}

impl LineTail {
    pub fn new(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
            offset: 0,
            identity: None,
        }
    }

    /// The offset of the first byte not read.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Starts again from byte 0 on the next [`read`](Self::read), without
    /// saying so.
    pub fn rewind(&mut self) {
        self.offset = 0;
        self.identity = None;
    }

    /// Positions the tail at `offset`, which the caller read to before: used
    /// to go on from a cursor a page sent back.
    pub fn seek(&mut self, offset: u64) {
        self.offset = offset;
    }

    /// Reads the whole lines appended since the last call, at most `max`
    /// bytes of them; a line longer than `cut` bytes is cut (see
    /// [`TailLine::text`]) and a line with no newline in `max` bytes is
    /// taken as it stands, so a log with a line longer than `max` cannot
    /// stall the tail.
    pub fn read(&mut self, max: usize, cut: usize) -> io::Result<TailRead> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let had = self.offset > 0 || self.identity.is_some();
                self.rewind();
                return Ok(TailRead {
                    lines: Vec::new(),
                    restarted: had,
                });
            }
            Err(error) => return Err(error),
        };
        let meta = file.metadata()?;
        let identity = Identity::of(&meta);
        let size = meta.len();
        let restarted = size < self.offset || self.identity.is_some_and(|known| known != identity);
        if restarted {
            self.rewind();
        }
        self.identity = Some(identity);
        let remaining = size - self.offset;
        if remaining == 0 {
            return Ok(TailRead {
                lines: Vec::new(),
                restarted,
            });
        }
        let window = usize::try_from(remaining).unwrap_or(usize::MAX).min(max);
        file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::with_capacity(window.min(1 << 20));
        file.take(window as u64).read_to_end(&mut bytes)?;
        let whole = match bytes.iter().rposition(|&b| b == b'\n') {
            Some(at) => at + 1,
            // No newline in a full window: the line is longer than `max`.
            None if bytes.len() == max && remaining > max as u64 => bytes.len(),
            None => 0,
        };
        let mut lines = Vec::new();
        let mut at = self.offset;
        let mut rest = &bytes[..whole];
        while !rest.is_empty() {
            let end = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
            let taken = (end + 1).min(rest.len());
            let content = &rest[..end];
            let content = content.strip_suffix(b"\r").unwrap_or(content);
            if !content.iter().all(u8::is_ascii_whitespace) {
                let text = if content.len() > cut {
                    // Cut on a char boundary of the lossy text.
                    let lossy = String::from_utf8_lossy(&content[..cut]);
                    format!("{}…", lossy.trim_end_matches('\u{fffd}'))
                } else {
                    String::from_utf8_lossy(content).into_owned()
                };
                lines.push(TailLine { offset: at, text });
            }
            at += taken as u64;
            rest = &rest[taken..];
        }
        self.offset += whole as u64;
        Ok(TailRead { lines, restarted })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
    }

    fn append(path: &Path, bytes: &[u8]) {
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(bytes).unwrap();
    }

    fn texts(tail: &Tail) -> Vec<String> {
        tail.lines
            .iter()
            .map(|l| String::from_utf8_lossy(&l.bytes).into_owned())
            .collect()
    }

    #[test]
    fn a_half_written_line_waits_for_its_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout.log");
        write(&path, b"one\ntwo\nthr");
        let first = read_lines(&path, 0, MAX_ANSWER, false).unwrap();
        assert_eq!(texts(&first), ["one", "two"]);
        assert_eq!(first.next, 8);
        assert_eq!(first.size, 11);
        assert_eq!(first.lines[1].offset, 4);
        let again = read_lines(&path, first.next, MAX_ANSWER, false).unwrap();
        assert!(again.lines.is_empty());
        assert_eq!(again.next, 8);
        append(&path, b"ee\nfou");
        let second = read_lines(&path, first.next, MAX_ANSWER, false).unwrap();
        assert_eq!(texts(&second), ["three"]);
        assert_eq!(second.next, 14);
        append(&path, b"r\n");
        let third = read_lines(&path, second.next, MAX_ANSWER, false).unwrap();
        assert_eq!(texts(&third), ["four"]);
        assert_eq!(third.next, third.size);
    }

    #[test]
    fn an_ended_run_serves_its_unterminated_last_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout.log");
        write(&path, b"one\nlast");
        let tail = read_lines(&path, 0, MAX_ANSWER, true).unwrap();
        assert_eq!(texts(&tail), ["one", "last"]);
        assert_eq!(tail.next, 8);
        assert_eq!(tail.next, tail.size);
        let after = read_lines(&path, tail.next, MAX_ANSWER, true).unwrap();
        assert!(after.lines.is_empty());
        assert_eq!(after.next, 8);
    }

    #[test]
    fn a_shrunk_log_is_read_again_from_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout.log");
        write(&path, b"a long first line\n");
        let tail = read_lines(&path, 0, MAX_ANSWER, false).unwrap();
        write(&path, b"new\n");
        let again = read_lines(&path, tail.next, MAX_ANSWER, false).unwrap();
        assert!(again.reset);
        assert_eq!(texts(&again), ["new"]);
        assert_eq!(again.next, 4);
    }

    #[test]
    fn an_absent_log_answers_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let tail = read_lines(&dir.path().join("none.log"), 0, MAX_ANSWER, false).unwrap();
        assert_eq!(tail, Tail::default());
    }

    #[test]
    fn a_long_log_comes_back_in_bounded_calls_that_make_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout.log");
        let mut file = Vec::new();
        for n in 0..8000 {
            file.extend_from_slice(format!("line {n} {}\n", "x".repeat(40)).as_bytes());
        }
        assert!(file.len() > 300 * 1024);
        write(&path, &file);
        let (mut from, mut joined, mut calls) = (0, Vec::new(), 0);
        while from < file.len() as u64 {
            let tail = read_lines(&path, from, MAX_ANSWER, false).unwrap();
            let bytes: usize = tail.lines.iter().map(|l| l.bytes.len() + 1).sum();
            assert!(bytes <= MAX_ANSWER, "{bytes}");
            assert!(!tail.lines.is_empty());
            for line in &tail.lines {
                joined.extend_from_slice(&line.bytes);
                joined.push(b'\n');
            }
            from = tail.next;
            calls += 1;
        }
        assert!(calls >= 2);
        assert_eq!(joined, file);
    }

    #[test]
    fn a_line_past_the_cap_is_served_alone_and_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout.log");
        let mut file = "y".repeat(1024 * 1024).into_bytes();
        file.extend_from_slice(b"\nshort\n");
        write(&path, &file);
        let first = read_lines(&path, 0, MAX_ANSWER, false).unwrap();
        assert_eq!(first.lines.len(), 1);
        assert_eq!(first.lines[0].bytes.len(), 1024 * 1024);
        assert_eq!(first.lines[0].cut, None);
        assert_eq!(first.next, 1024 * 1024 + 1);
        let second = read_lines(&path, first.next, MAX_ANSWER, false).unwrap();
        assert_eq!(texts(&second), ["short"]);
        assert_eq!(second.next, second.size);
    }

    #[test]
    fn a_line_after_a_short_one_that_does_not_fit_waits_for_the_next_call() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout.log");
        let mut file = b"short\n".to_vec();
        file.extend_from_slice("y".repeat(1024).as_bytes());
        file.push(b'\n');
        write(&path, &file);
        let first = read_lines(&path, 0, 100, false).unwrap();
        assert_eq!(texts(&first), ["short"]);
        assert_eq!(first.next, 6);
        let second = read_lines(&path, first.next, 100, false).unwrap();
        assert_eq!(second.lines[0].bytes.len(), 1024);
    }

    #[test]
    fn a_line_past_eight_mebibytes_is_cut_and_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout.log");
        let length = MAX_LINE + 100;
        let mut file = "z".repeat(length).into_bytes();
        file.extend_from_slice(b"\nafter\n");
        write(&path, &file);
        let first = read_lines(&path, 0, MAX_ANSWER, false).unwrap();
        assert_eq!(first.lines.len(), 1);
        assert_eq!(first.lines[0].bytes.len(), CUT_PREFIX);
        assert_eq!(first.lines[0].cut, Some(length as u64));
        assert_eq!(first.next, length as u64 + 1);
        let second = read_lines(&path, first.next, MAX_ANSWER, false).unwrap();
        assert_eq!(texts(&second), ["after"]);
        // A line of exactly the limit is whole.
        let mut exact = "q".repeat(MAX_LINE).into_bytes();
        exact.push(b'\n');
        write(&path, &exact);
        let whole = read_lines(&path, 0, MAX_ANSWER, false).unwrap();
        assert_eq!(whole.lines[0].cut, None);
        assert_eq!(whole.lines[0].bytes.len(), MAX_LINE);
    }

    #[test]
    fn a_partial_line_past_eight_mebibytes_waits_on_a_live_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout.log");
        write(&path, "w".repeat(MAX_LINE + 10).as_bytes());
        let live = read_lines(&path, 0, MAX_ANSWER, false).unwrap();
        assert!(live.lines.is_empty());
        assert_eq!(live.next, 0);
        let ended = read_lines(&path, 0, MAX_ANSWER, true).unwrap();
        assert_eq!(ended.lines.len(), 1);
        assert_eq!(ended.lines[0].cut, Some(MAX_LINE as u64 + 10));
        assert_eq!(ended.next, ended.size);
    }

    #[test]
    fn a_byte_that_is_not_utf8_does_not_fail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout.log");
        write(&path, b"ok \xff\xfe bytes\n");
        let tail = read_lines(&path, 0, MAX_ANSWER, false).unwrap();
        assert_eq!(texts(&tail), ["ok \u{fffd}\u{fffd} bytes"]);
    }

    #[test]
    fn lines_before_an_offset_are_counted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout.log");
        write(&path, b"a\n\nccc\ndd\n");
        assert_eq!(count_lines(&path, 0).unwrap(), 0);
        assert_eq!(count_lines(&path, 2).unwrap(), 1);
        assert_eq!(count_lines(&path, 7).unwrap(), 3);
        assert_eq!(count_lines(&dir.path().join("none"), 5).unwrap(), 0);
    }

    fn line_texts(read: &TailRead) -> Vec<&str> {
        read.lines.iter().map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn a_line_tail_returns_appended_lines_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        write(&path, b"one\ntwo\n");
        let mut tail = LineTail::new(&path);
        let first = tail.read(usize::MAX, usize::MAX).unwrap();
        assert_eq!(line_texts(&first), ["one", "two"]);
        assert!(!first.restarted);
        assert_eq!(first.lines[1].offset, 4);
        assert_eq!(tail.offset(), 8);
        assert!(tail.read(usize::MAX, usize::MAX).unwrap().lines.is_empty());
        append(&path, b"three\n");
        let more = tail.read(usize::MAX, usize::MAX).unwrap();
        assert_eq!(line_texts(&more), ["three"]);
        assert!(!more.restarted);
        assert_eq!(more.lines[0].offset, 8);
    }

    #[test]
    fn a_half_written_line_is_returned_after_its_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        write(&path, b"whole\nhal");
        let mut tail = LineTail::new(&path);
        assert_eq!(
            line_texts(&tail.read(usize::MAX, usize::MAX).unwrap()),
            ["whole"]
        );
        assert_eq!(tail.offset(), 6);
        append(&path, b"f\n");
        assert_eq!(
            line_texts(&tail.read(usize::MAX, usize::MAX).unwrap()),
            ["half"]
        );
    }

    #[test]
    fn a_truncated_or_replaced_file_is_read_again_from_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        write(&path, b"a long first line\nsecond\n");
        let mut tail = LineTail::new(&path);
        tail.read(usize::MAX, usize::MAX).unwrap();
        write(&path, b"new\n");
        let again = tail.read(usize::MAX, usize::MAX).unwrap();
        assert!(again.restarted);
        assert_eq!(line_texts(&again), ["new"]);
        // Replaced by a file of the same length: another file on unix.
        #[cfg(unix)]
        {
            let other = dir.path().join("other");
            write(&other, b"abc\n");
            std::fs::rename(&other, &path).unwrap();
            let replaced = tail.read(usize::MAX, usize::MAX).unwrap();
            assert!(replaced.restarted);
            assert_eq!(line_texts(&replaced), ["abc"]);
        }
    }

    #[test]
    fn a_missing_file_reads_nothing_and_is_picked_up_when_it_appears() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        let mut tail = LineTail::new(&path);
        let none = tail.read(usize::MAX, usize::MAX).unwrap();
        assert!(none.lines.is_empty() && !none.restarted);
        write(&path, b"here\n");
        let found = tail.read(usize::MAX, usize::MAX).unwrap();
        assert_eq!(line_texts(&found), ["here"]);
        assert!(!found.restarted);
        std::fs::remove_file(&path).unwrap();
        let gone = tail.read(usize::MAX, usize::MAX).unwrap();
        assert!(gone.lines.is_empty() && gone.restarted);
    }

    #[test]
    fn long_lines_are_cut_and_a_small_window_cannot_stall_the_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        write(&path, format!("{}\nshort\n", "x".repeat(100)).as_bytes());
        let mut tail = LineTail::new(&path);
        let read = tail.read(usize::MAX, 10).unwrap();
        assert_eq!(line_texts(&read), ["xxxxxxxxxx…", "short"]);
        // A window smaller than the first line takes the window as a line.
        let mut small = LineTail::new(&path);
        let first = small.read(40, usize::MAX).unwrap();
        assert_eq!(first.lines.len(), 1);
        assert_eq!(first.lines[0].text.len(), 40);
        assert_eq!(small.offset(), 40);
    }
}
