//! Reading a file inside a directory without following any symbolic link.
//!
//! An agent is untrusted. It names its artifacts by paths relative to its
//! worktree, and it could leave a symbolic link there (`plan.md` pointing to
//! `~/.ssh/id_rsa`, or a linked parent directory) for the runner to read and
//! publish. [`read_confined`] refuses every link from the root down, even one
//! that points back inside: there is no resolution logic to get wrong.
//!
//! Resolution and read are one walk. Each level is opened relative to the
//! handle of the level above, never through a path looked up again, and is
//! checked on that handle. A level swapped for a link after it was opened
//! therefore cannot redirect the read.
//!
//! - Unix: `openat` with `O_NOFOLLOW` for each level, then `fstat` on the
//!   opened file, which must be a regular file. `O_NONBLOCK` keeps the open of
//!   a FIFO from waiting for a writer.
//! - Windows: `NtCreateFile` relative to the parent handle with
//!   `FILE_OPEN_REPARSE_POINT` for each level; a name-surrogate reparse point
//!   (symbolic link, junction) is refused. No handle allows delete sharing, so
//!   a level cannot be renamed while the walk holds it.
//!
//! The root's ancestors may be links (macOS reaches temporary directories
//! through `/var`): the system resolves them once, when the root is opened.
//! The root itself must be a real directory.
//!
//! Hard links are not detected: a hard link is a file of the worktree in its
//! own right.

use std::error::Error;
use std::fmt;
use std::io;
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
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SymbolicLink => f.write_str("is a symbolic link"),
            Self::NotADirectory => f.write_str("is not a directory"),
            Self::NotFound => f.write_str("does not exist"),
            Self::NotARegularFile(kind) => write!(f, "is a {kind}, not a regular file"),
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
pub fn read_confined(root: &Path, relative: &str) -> Result<Vec<u8>, ConfinedError> {
    walk(root, relative, &mut |_| {})
}

/// The walk behind [`read_confined`]. `after_level` runs once each directory
/// level is open, with the part of the path it names, so tests can change the
/// tree mid-walk.
fn walk(
    root: &Path,
    relative: &str,
    after_level: &mut dyn FnMut(&str),
) -> Result<Vec<u8>, ConfinedError> {
    let segments = segments(relative)?;
    let root = normalize_root(root)?;
    let walk = Walk {
        root: &root,
        relative,
        segments: &segments,
    };
    sys::walk(&walk, after_level)
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
}

#[cfg(unix)]
mod sys {
    use std::fs::File;
    use std::io::Read;
    use std::os::fd::OwnedFd;

    use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags, fstat, open, openat, statat};
    use rustix::io::Errno;

    use super::{ConfinedError, Refusal, Walk};

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
        after_level: &mut dyn FnMut(&str),
    ) -> Result<Vec<u8>, ConfinedError> {
        let mut dir =
            open(walk.root, DIRECTORY, Mode::empty()).map_err(|error| root_error(walk, error))?;

        let last = walk.segments.len() - 1;
        for (index, name) in walk.segments[..last].iter().enumerate() {
            dir = openat(&dir, *name, DIRECTORY, Mode::empty())
                .map_err(|error| entry_error(walk, &dir, index, error, true))?;
            after_level(&walk.at(index));
        }

        let file = openat(&dir, walk.segments[last], FILE, Mode::empty())
            .map_err(|error| entry_error(walk, &dir, last, error, false))?;
        let stat = fstat(&file).map_err(|error| walk.io(error.into()))?;
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::RegularFile => {}
            other => return Err(walk.refused(last, Refusal::NotARegularFile(kind(other)))),
        }
        let mut bytes = Vec::new();
        File::from(file)
            .read_to_end(&mut bytes)
            .map_err(|error| walk.io(error))?;
        Ok(bytes)
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
    use std::io::{self, Read};
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
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ,
        FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FILE_TRAVERSE, SYNCHRONIZE,
    };
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    use super::{ConfinedError, Refusal, Walk};

    const DIRECTORY_ACCESS: u32 =
        FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
    const DIRECTORY_SHARE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE;

    pub(super) fn walk(
        walk: &Walk<'_>,
        after_level: &mut dyn FnMut(&str),
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
            after_level(&walk.at(index));
        }

        let parent = levels.last().expect("the root is always there");
        let mut file = open_relative(
            parent,
            walk.segments[last],
            FILE_GENERIC_READ,
            FILE_SHARE_READ,
        )
        .map_err(|error| entry_error(walk, last, error))?;
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
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| walk.io(error))?;
        Ok(bytes)
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

    use super::walk;

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
        let bytes = walk(&wt, "docs/plan.md", &mut |at| {
            assert_eq!(at, "docs");
            fs::rename(wt.join("docs"), wt.join("docs.orig")).unwrap();
            std::os::unix::fs::symlink(&outside, wt.join("docs")).unwrap();
            swapped = true;
        })
        .unwrap();
        assert!(swapped);
        assert_eq!(bytes, b"inside");
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
        let bytes = walk(&wt, "docs/plan.md", &mut |_| {
            renamed = Some(fs::rename(wt.join("docs"), wt.join("docs.moved")).is_ok());
        })
        .unwrap();
        assert_eq!(renamed, Some(false), "a held level was renamed");
        assert_eq!(bytes, b"inside");
    }
}
