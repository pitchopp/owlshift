//! Reading a file inside a directory without following any symbolic link.
//!
//! An agent is untrusted. It names its artifacts by paths relative to its
//! worktree, and it could leave a symbolic link there (`plan.md` pointing to
//! `~/.ssh/id_rsa`, or a linked parent directory) for the runner to read and
//! publish. [`read_confined`] refuses every link from the root down, even one
//! that points back inside: there is no resolution logic to get wrong.
//!
//! A hard link is the same trap without a link to see: `plan.md` made a second
//! name of `~/.ssh/id_rsa` is a regular file in the worktree. Where the other
//! names are cannot be known without searching the whole volume, so a file
//! with more than one name is refused, wherever they are. A checkout, and the
//! tools that write artifacts, create files with a single name.
//!
//! Resolution and read are one walk. Each level is opened relative to the
//! handle of the level above, never through a path looked up again, and is
//! checked on that handle. A level swapped for a link after it was opened
//! therefore cannot redirect the read.
//!
//! - Unix: `openat` with `O_NOFOLLOW` for each level, then `fstat` on the
//!   opened file, which must be a regular file with a link count of 1.
//!   `O_NONBLOCK` keeps the open of a FIFO from waiting for a writer.
//! - Windows: `NtCreateFile` relative to the parent handle with
//!   `FILE_OPEN_REPARSE_POINT` for each level; a name-surrogate reparse point
//!   (symbolic link, junction) is refused. No handle allows delete sharing, so
//!   a level cannot be renamed while the walk holds it. The file's link count,
//!   from `GetFileInformationByHandle`, must be 1.
//!
//! The link count is read on the handle the bytes are then read from, so the
//! file checked is the file read, and it must be exactly 1: a file deleted
//! after it was opened has none. The count can still change between the open
//! and the check. A process that removes the inside name of a hard link to an
//! outside file in that window leaves a count of 1, and the outside content is
//! read. The reader cannot close that window; the runner does, by stopping the
//! run's whole process tree before it reads the artifacts (OWL-15), so no
//! process of the run is left to act. Known limits: a process that escaped
//! that tree, and a network or FUSE mount that supports hard links but
//! reports a count of 1.
//!
//! The root's ancestors may be links (macOS reaches temporary directories
//! through `/var`): the system resolves them once, when the root is opened.
//! The root itself must be a real directory.
//!
//! The read stops one byte past a limit the caller gives: a file that holds,
//! or grows to hold, more than the limit before the reader sees its end is
//! refused without being read to the end, however large, sparse or endless it
//! is.

use std::error::Error;
use std::fmt;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

/// Why an entry, or the root, was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A symbolic link, or on Windows a junction or other name surrogate.
    SymbolicLink,
    /// A level of the path that is not a directory.
    NotADirectory,
    /// Nothing by that name.
    NotFound,
    /// The final entry is not a regular file; the kind is named
    /// (`"directory"`, `"FIFO"`, `"character device"`, ...).
    NotARegularFile(&'static str),
    /// The file holds more than the limit, in bytes, the read was given.
    TooLarge(u64),
    /// The file has this many names (hard links), more than one.
    HardLink(u64),
    /// The file has no name left: it was deleted after it was opened.
    Unlinked,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SymbolicLink => f.write_str("is a symbolic link"),
            Self::NotADirectory => f.write_str("is not a directory"),
            Self::NotFound => f.write_str("does not exist"),
            Self::NotARegularFile(kind) => write!(f, "is a {kind}, not a regular file"),
            Self::TooLarge(limit) => write!(f, "is larger than {limit} bytes"),
            Self::HardLink(links) => write!(
                f,
                "has {links} names (hard links); an artifact must be a file of its own"
            ),
            Self::Unlinked => f.write_str("was deleted while it was being read"),
        }
    }
}

/// A file [`read_confined`] would not, or could not, read.
#[derive(Debug)]
pub enum ConfinedError {
    /// The relative path is not plain: it is empty, has an empty or `..`
    /// segment, ends in `.`, or holds a backslash, a colon or a NUL. The rule
    /// is the one `RelativePath` enforces in `owlshift-contracts`, checked
    /// again here because this reader takes any string.
    InvalidPath { relative: String },
    /// The root path does not end in a directory name (`/`, `wt/..`).
    InvalidRoot { root: PathBuf },
    /// The root is not a real directory.
    Root { root: PathBuf, reason: Refusal },
    /// An entry on the way was refused; `at` is the part of `relative` that
    /// names it (`docs` for `docs/plan.md`).
    Refused {
        relative: String,
        at: String,
        reason: Refusal,
    },
    /// The file system failed for another reason.
    Io { relative: String, source: io::Error },
}

impl fmt::Display for ConfinedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath { relative } => {
                write!(f, "{relative:?} is not a plain relative path")
            }
            Self::InvalidRoot { root } => {
                write!(
                    f,
                    "worktree {} does not end in a directory name",
                    root.display()
                )
            }
            Self::Root { root, reason } => write!(f, "worktree {} {reason}", root.display()),
            Self::Refused {
                relative,
                at,
                reason,
            } if relative == at => write!(f, "`{at}` {reason}"),
            Self::Refused {
                relative,
                at,
                reason,
            } => write!(f, "`{relative}`: `{at}` {reason}"),
            Self::Io { relative, source } => write!(f, "`{relative}`: {source}"),
        }
    }
}

impl Error for ConfinedError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Reads the regular file at `relative` inside the directory `root`, refusing
/// any symbolic link on the way. `relative` is `/`-separated; `.` segments are
/// skipped.
///
/// A file holding more than `limit` bytes is refused with
/// [`Refusal::TooLarge`] after reading at most `limit + 1` of them. The limit
/// is a memory budget the caller chooses; `u64::MAX` sets no limit.
pub fn read_confined(root: &Path, relative: &str, limit: u64) -> Result<Vec<u8>, ConfinedError> {
    walk(root, relative, limit, &mut |_| {})
}

/// The walk behind [`read_confined`]. `after_open` runs once each directory
/// level, and then the file, is open, with the part of the path it names, so
/// tests can change the tree mid-walk.
fn walk(
    root: &Path,
    relative: &str,
    limit: u64,
    after_open: &mut dyn FnMut(&str),
) -> Result<Vec<u8>, ConfinedError> {
    let segments = segments(relative)?;
    let root = normalize_root(root)?;
    let walk = Walk {
        root: &root,
        relative,
        segments: &segments,
        limit,
    };
    sys::walk(&walk, after_open)
}

/// Why a regular file with `links` names is refused: every count but 1.
fn link_refusal(links: u64) -> Option<Refusal> {
    match links {
        0 => Some(Refusal::Unlinked),
        1 => None,
        links => Some(Refusal::HardLink(links)),
    }
}

/// Reads `reader` to its end, or stops once it has read more than `limit`
/// bytes: `None` then. Never reads more than `limit + 1` bytes, and sizes its
/// buffer from what it read, never from what the file claims to hold.
fn read_capped(reader: impl Read, limit: u64) -> io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    Ok((bytes.len() as u64 <= limit).then_some(bytes))
}

/// The segments of a plain relative path, `.` segments left out.
fn segments(relative: &str) -> Result<Vec<&str>, ConfinedError> {
    let invalid = || ConfinedError::InvalidPath {
        relative: relative.to_owned(),
    };
    if relative.split('/').next_back() == Some(".") {
        return Err(invalid());
    }
    let mut segments = Vec::new();
    for segment in relative.split('/') {
        if segment.is_empty() || segment == ".." || segment.contains(['\\', ':', '\0']) {
            return Err(invalid());
        }
        if segment != "." {
            segments.push(segment);
        }
    }
    Ok(segments)
}

/// The root without trailing separators or `.` components, which would
/// otherwise let a root that is a link be opened through it (`link/`,
/// `link/.`).
fn normalize_root(root: &Path) -> Result<PathBuf, ConfinedError> {
    let normalized: PathBuf = root.components().collect();
    match normalized.components().next_back() {
        Some(Component::Normal(_)) => Ok(normalized),
        _ => Err(ConfinedError::InvalidRoot {
            root: root.to_owned(),
        }),
    }
}

/// One walk's inputs, and the errors it reports.
struct Walk<'a> {
    root: &'a Path,
    relative: &'a str,
    /// Never empty: the last one names the file.
    segments: &'a [&'a str],
    /// The most bytes the file may hold.
    limit: u64,
}

impl Walk<'_> {
    /// The part of the path up to and including segment `index`.
    fn at(&self, index: usize) -> String {
        self.segments[..=index].join("/")
    }

    fn refused(&self, index: usize, reason: Refusal) -> ConfinedError {
        ConfinedError::Refused {
            relative: self.relative.to_owned(),
            at: self.at(index),
            reason,
        }
    }

    fn root_refused(&self, reason: Refusal) -> ConfinedError {
        ConfinedError::Root {
            root: self.root.to_owned(),
            reason,
        }
    }

    fn io(&self, source: io::Error) -> ConfinedError {
        ConfinedError::Io {
            relative: self.relative.to_owned(),
            source,
        }
    }

    /// Reads the opened final file within the limit.
    fn read(&self, file: impl Read) -> Result<Vec<u8>, ConfinedError> {
        read_capped(file, self.limit)
            .map_err(|error| self.io(error))?
            .ok_or_else(|| self.refused(self.segments.len() - 1, Refusal::TooLarge(self.limit)))
    }
}

#[cfg(unix)]
mod sys {
    use std::fs::File;
    use std::os::fd::OwnedFd;

    use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags, fstat, open, openat, statat};
    use rustix::io::Errno;

    use super::{ConfinedError, Refusal, Walk, link_refusal};

    const DIRECTORY: OFlags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);
    const FILE: OFlags = OFlags::RDONLY
        .union(OFlags::NOFOLLOW)
        .union(OFlags::NONBLOCK)
        .union(OFlags::NOCTTY)
        .union(OFlags::CLOEXEC);

    pub(super) fn walk(
        walk: &Walk<'_>,
        after_open: &mut dyn FnMut(&str),
    ) -> Result<Vec<u8>, ConfinedError> {
        let mut dir =
            open(walk.root, DIRECTORY, Mode::empty()).map_err(|error| root_error(walk, error))?;

        let last = walk.segments.len() - 1;
        for (index, name) in walk.segments[..last].iter().enumerate() {
            dir = openat(&dir, *name, DIRECTORY, Mode::empty())
                .map_err(|error| entry_error(walk, &dir, index, error, true))?;
            after_open(&walk.at(index));
        }

        let file = openat(&dir, walk.segments[last], FILE, Mode::empty())
            .map_err(|error| entry_error(walk, &dir, last, error, false))?;
        after_open(&walk.at(last));
        let stat = fstat(&file).map_err(|error| walk.io(error.into()))?;
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::RegularFile => {}
            other => return Err(walk.refused(last, Refusal::NotARegularFile(kind(other)))),
        }
        #[allow(
            clippy::useless_conversion,
            reason = "`st_nlink` is u16 on macOS and u32 on Linux aarch64, u64 elsewhere"
        )]
        let links = u64::from(stat.st_nlink);
        if let Some(reason) = link_refusal(links) {
            return Err(walk.refused(last, reason));
        }
        walk.read(File::from(file))
    }

    /// Names what refused the root; the open's own error when nothing is
    /// wrong with its type.
    fn root_error(walk: &Walk<'_>, error: Errno) -> ConfinedError {
        if error == Errno::NOENT {
            return walk.root_refused(Refusal::NotFound);
        }
        match statat(CWD, walk.root, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => match FileType::from_raw_mode(stat.st_mode) {
                FileType::Symlink => walk.root_refused(Refusal::SymbolicLink),
                FileType::Directory => walk.io(error.into()),
                _ => walk.root_refused(Refusal::NotADirectory),
            },
            Err(_) => walk.io(error.into()),
        }
    }

    /// Names what refused segment `index`. The open already refused it; the
    /// `statat` only chooses the message.
    fn entry_error(
        walk: &Walk<'_>,
        dir: &OwnedFd,
        index: usize,
        error: Errno,
        want_directory: bool,
    ) -> ConfinedError {
        if error == Errno::NOENT {
            return walk.refused(index, Refusal::NotFound);
        }
        let Ok(stat) = statat(dir, walk.segments[index], AtFlags::SYMLINK_NOFOLLOW) else {
            return walk.io(error.into());
        };
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::Symlink => walk.refused(index, Refusal::SymbolicLink),
            FileType::Directory if want_directory => walk.io(error.into()),
            _ if want_directory => walk.refused(index, Refusal::NotADirectory),
            FileType::RegularFile => walk.io(error.into()),
            other => walk.refused(index, Refusal::NotARegularFile(kind(other))),
        }
    }

    fn kind(file_type: FileType) -> &'static str {
        match file_type {
            FileType::RegularFile => "regular file",
            FileType::Directory => "directory",
            FileType::Symlink => "symbolic link",
            FileType::Fifo => "FIFO",
            FileType::Socket => "socket",
            FileType::CharacterDevice => "character device",
            FileType::BlockDevice => "block device",
            FileType::Unknown => "special file",
        }
    }
}

#[cfg(windows)]
mod sys {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::ptr;

    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_OPEN, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
    };
    use windows_sys::Win32::Foundation::{
        HANDLE, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_GENERIC_READ, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_TRAVERSE, GetFileInformationByHandle, SYNCHRONIZE,
    };
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    use super::{ConfinedError, Refusal, Walk, link_refusal};

    const DIRECTORY_ACCESS: u32 =
        FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
    const DIRECTORY_SHARE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE;

    pub(super) fn walk(
        walk: &Walk<'_>,
        after_open: &mut dyn FnMut(&str),
    ) -> Result<Vec<u8>, ConfinedError> {
        // The root is opened by path, which resolves its ancestors; the
        // reparse-point flag stops at the root itself if it is a link.
        let root = OpenOptions::new()
            .read(true)
            .share_mode(DIRECTORY_SHARE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(walk.root)
            .map_err(|error| match error.kind() {
                io::ErrorKind::NotFound => walk.root_refused(Refusal::NotFound),
                _ => walk.io(error),
            })?;
        let file_type = root.metadata().map_err(|error| walk.io(error))?.file_type();
        if file_type.is_symlink() {
            return Err(walk.root_refused(Refusal::SymbolicLink));
        }
        if !file_type.is_dir() {
            return Err(walk.root_refused(Refusal::NotADirectory));
        }

        // Every level stays open until the read is done, so none can be
        // renamed or deleted while the walk depends on it.
        let mut levels = vec![root];
        let last = walk.segments.len() - 1;
        for index in 0..last {
            let parent = levels.last().expect("the root is always there");
            let dir = open_relative(
                parent,
                walk.segments[index],
                DIRECTORY_ACCESS,
                DIRECTORY_SHARE,
            )
            .map_err(|error| entry_error(walk, index, error))?;
            let file_type = dir.metadata().map_err(|error| walk.io(error))?.file_type();
            if file_type.is_symlink() {
                return Err(walk.refused(index, Refusal::SymbolicLink));
            }
            if !file_type.is_dir() {
                return Err(walk.refused(index, Refusal::NotADirectory));
            }
            levels.push(dir);
            after_open(&walk.at(index));
        }

        let parent = levels.last().expect("the root is always there");
        let file = open_relative(
            parent,
            walk.segments[last],
            FILE_GENERIC_READ,
            FILE_SHARE_READ,
        )
        .map_err(|error| entry_error(walk, last, error))?;
        after_open(&walk.at(last));
        let file_type = file.metadata().map_err(|error| walk.io(error))?.file_type();
        if file_type.is_symlink() {
            return Err(walk.refused(last, Refusal::SymbolicLink));
        }
        if file_type.is_dir() {
            return Err(walk.refused(last, Refusal::NotARegularFile("directory")));
        }
        if !file_type.is_file() {
            return Err(walk.refused(last, Refusal::NotARegularFile("special file")));
        }
        let links = link_count(&file).map_err(|error| walk.io(error))?;
        if let Some(reason) = link_refusal(u64::from(links)) {
            return Err(walk.refused(last, reason));
        }
        walk.read(file)
    }

    /// The number of names (hard links) of the open `file`. The standard
    /// library reads it too, but exposes it only behind the unstable
    /// `windows_by_handle` feature.
    fn link_count(file: &File) -> io::Result<u32> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: `file` is an open handle for the whole call, and `info` is
        // a live local the call only writes.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(info.nNumberOfLinks)
    }

    fn entry_error(walk: &Walk<'_>, index: usize, error: io::Error) -> ConfinedError {
        match error.kind() {
            io::ErrorKind::NotFound => walk.refused(index, Refusal::NotFound),
            io::ErrorKind::NotADirectory => walk.refused(index, Refusal::NotADirectory),
            _ => walk.io(error),
        }
    }

    /// Opens the single component `name` inside the directory `parent`,
    /// without following it if it is a reparse point: the Windows
    /// counterpart of `openat(parent, name, O_NOFOLLOW)`. The name is looked
    /// up in `parent` itself, whatever has happened to the path that led to
    /// it since it was opened.
    fn open_relative(parent: &File, name: &str, access: u32, share: u32) -> io::Result<File> {
        let wide: Vec<u16> = name.encode_utf16().collect();
        let length = u16::try_from(wide.len() * 2)
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidFilename))?;
        let object_name = UNICODE_STRING {
            Length: length,
            MaximumLength: length,
            Buffer: wide.as_ptr().cast_mut(),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: parent.as_raw_handle(),
            ObjectName: &object_name,
            Attributes: OBJ_CASE_INSENSITIVE,
            SecurityDescriptor: ptr::null(),
            SecurityQualityOfService: ptr::null(),
        };
        let mut handle: HANDLE = ptr::null_mut();
        let mut status_block = IO_STATUS_BLOCK::default();
        // SAFETY: every pointer passed refers to a live local (`handle`,
        // `attributes` and through it `object_name` and `wide`, and
        // `status_block`) that outlives the call; the name buffer is only
        // read; `parent` is an open directory handle for the whole call; no
        // allocation size or extended attributes are passed.
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                access,
                &attributes,
                &mut status_block,
                ptr::null(),
                0,
                share,
                FILE_OPEN,
                FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
                ptr::null(),
                0,
            )
        };
        if status < 0 {
            // SAFETY: a pure conversion of a status code.
            let code = unsafe { RtlNtStatusToDosError(status) };
            return Err(io::Error::from_raw_os_error(code as i32));
        }
        // SAFETY: the call succeeded, so `handle` is a new handle that
        // nothing else owns.
        Ok(File::from(unsafe { OwnedHandle::from_raw_handle(handle) }))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{self, Read};

    use super::{read_capped, walk};

    /// A reader that counts the bytes taken from it.
    struct Counting<R> {
        inner: R,
        read: u64,
    }

    impl<R: Read> Read for Counting<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read += n as u64;
            Ok(n)
        }
    }

    /// An endless source, like a file that keeps growing, is refused after
    /// `limit + 1` bytes: the read never goes further.
    #[test]
    fn an_endless_source_is_refused_after_one_byte_past_the_limit() {
        let mut source = Counting {
            inner: io::repeat(b'x'),
            read: 0,
        };
        assert_eq!(read_capped(&mut source, 1024).unwrap(), None);
        assert_eq!(source.read, 1025);
    }

    #[test]
    fn a_source_at_the_limit_is_read_whole() {
        assert_eq!(
            read_capped(&b"sixteen bytes..."[..], 16)
                .unwrap()
                .as_deref(),
            Some(&b"sixteen bytes..."[..])
        );
        assert_eq!(read_capped(&b""[..], 0).unwrap(), Some(Vec::new()));
        assert_eq!(read_capped(&b"x"[..], 0).unwrap(), None);
        assert_eq!(
            read_capped(&b"no limit"[..], u64::MAX).unwrap().as_deref(),
            Some(&b"no limit"[..])
        );
    }

    /// A directory swapped for a link to an outside one after the walk opened
    /// it does not redirect the read: the next level is looked up in the
    /// directory the walk holds.
    #[cfg(unix)]
    #[test]
    fn a_level_swapped_after_it_was_opened_does_not_redirect_the_read() {
        let base = tempfile::tempdir().unwrap();
        let wt = base.path().join("wt");
        let outside = base.path().join("outside");
        fs::create_dir_all(wt.join("docs")).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(wt.join("docs/plan.md"), b"inside").unwrap();
        fs::write(outside.join("plan.md"), b"outside").unwrap();

        let mut swapped = false;
        let bytes = walk(&wt, "docs/plan.md", u64::MAX, &mut |at| {
            if at != "docs" {
                return;
            }
            fs::rename(wt.join("docs"), wt.join("docs.orig")).unwrap();
            std::os::unix::fs::symlink(&outside, wt.join("docs")).unwrap();
            swapped = true;
        })
        .unwrap();
        assert!(swapped);
        assert_eq!(bytes, b"inside");
    }

    /// A file deleted after the walk opened it has no name left, and is
    /// refused rather than read through the handle.
    #[cfg(unix)]
    #[test]
    fn a_file_deleted_after_it_was_opened_is_refused() {
        let base = tempfile::tempdir().unwrap();
        let wt = base.path().join("wt");
        fs::create_dir(&wt).unwrap();
        fs::write(wt.join("plan.md"), b"inside").unwrap();

        let error = walk(&wt, "plan.md", u64::MAX, &mut |at| {
            assert_eq!(at, "plan.md");
            fs::remove_file(wt.join("plan.md")).unwrap();
        })
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "`plan.md` was deleted while it was being read"
        );
        assert!(matches!(
            error,
            super::ConfinedError::Refused {
                reason: super::Refusal::Unlinked,
                ..
            }
        ));
    }

    /// A directory the walk holds cannot be renamed, so it cannot be swapped
    /// for a link.
    #[cfg(windows)]
    #[test]
    fn a_level_the_walk_holds_cannot_be_renamed() {
        let base = tempfile::tempdir().unwrap();
        let wt = base.path().join("wt");
        fs::create_dir_all(wt.join("docs")).unwrap();
        fs::write(wt.join("docs/plan.md"), b"inside").unwrap();

        let mut renamed = None;
        let bytes = walk(&wt, "docs/plan.md", u64::MAX, &mut |at| {
            if at != "docs" {
                return;
            }
            renamed = Some(fs::rename(wt.join("docs"), wt.join("docs.moved")).is_ok());
        })
        .unwrap();
        assert_eq!(renamed, Some(false), "a held level was renamed");
        assert_eq!(bytes, b"inside");
    }
}
