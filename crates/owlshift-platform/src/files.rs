//! A folder's entries, beyond what the standard library does.

use std::ffi::OsStr;
use std::io;
use std::path::Path;

/// Gives the file at `from` the name `to`, failing when `to` is taken
/// (OWL-425): a file written whole under `from` appears whole under `to`.
/// A hard link then the removal of `from` on Unix; on Windows a move that
/// replaces nothing, since removing one name of a linked file there may
/// make every open of the file fail until it is done.
pub fn publish(from: &Path, to: &Path) -> io::Result<()> {
    sys::publish(from, to)
}

/// Whether the entry `name` of the folder `dir` is being deleted (OWL-425).
/// On Windows, a file removed while another process still holds it open
/// without sharing its deletion, such as a virus scanner reading a new file,
/// stays listed until that handle closes, and every open of it meanwhile is
/// refused as "access denied" (`STATUS_DELETE_PENDING`), which says nothing
/// of a file merely unreadable. Other systems drop the name at once: never.
pub fn delete_pending(dir: &Path, name: &OsStr) -> bool {
    sys::delete_pending(dir, name)
}

#[cfg(not(windows))]
mod sys {
    use std::ffi::OsStr;
    use std::path::Path;
    use std::{fs, io};

    pub(super) fn publish(from: &Path, to: &Path) -> io::Result<()> {
        fs::hard_link(from, to)?;
        // Published already: a name left behind is the caller's partial.
        let _ = fs::remove_file(from);
        Ok(())
    }

    pub(super) fn delete_pending(_dir: &Path, _name: &OsStr) -> bool {
        false
    }
}

#[cfg(windows)]
mod sys {
    use std::ffi::OsStr;
    use std::fs::OpenOptions;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::Path;
    use std::ptr;

    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_OPEN, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
    };
    use windows_sys::Win32::Foundation::{
        HANDLE, OBJ_CASE_INSENSITIVE, STATUS_DELETE_PENDING, UNICODE_STRING,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, MOVEFILE_WRITE_THROUGH, MoveFileExW, SYNCHRONIZE,
    };
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

    pub(super) fn publish(from: &Path, to: &Path) -> io::Result<()> {
        let wide =
            |path: &Path| -> Vec<u16> { path.as_os_str().encode_wide().chain(Some(0)).collect() };
        let (from, to) = (wide(from), wide(to));
        // SAFETY: both are live, NUL-terminated wide strings for the whole
        // call; without `MOVEFILE_REPLACE_EXISTING` a taken `to` fails.
        if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn delete_pending(dir: &Path, name: &OsStr) -> bool {
        let Ok(parent) = OpenOptions::new()
            .read(true)
            .share_mode(SHARE_ALL)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(dir)
        else {
            return false;
        };
        let wide: Vec<u16> = name.encode_wide().collect();
        let Ok(length) = u16::try_from(wide.len() * 2) else {
            return false;
        };
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
                FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                &attributes,
                &mut status_block,
                ptr::null(),
                0,
                SHARE_ALL,
                FILE_OPEN,
                FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
                ptr::null(),
                0,
            )
        };
        if status >= 0 {
            // SAFETY: the call succeeded, so `handle` is a new handle that
            // nothing else owns; dropping it closes it.
            drop(unsafe { OwnedHandle::from_raw_handle(handle) });
        }
        status == STATUS_DELETE_PENDING
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::delete_pending;

    /// A published file has its new name only, and a taken name is refused,
    /// the file left where it was.
    #[test]
    fn publish_takes_a_free_name_only() {
        let dir = tempfile::tempdir().unwrap();
        let (from, to) = (dir.path().join("from"), dir.path().join("to"));
        fs::write(&from, "one").unwrap();
        super::publish(&from, &to).unwrap();
        assert!(!from.exists());
        assert_eq!(fs::read_to_string(&to).unwrap(), "one");
        fs::write(&from, "two").unwrap();
        let taken = super::publish(&from, &to).unwrap_err();
        assert_eq!(taken.kind(), std::io::ErrorKind::AlreadyExists, "{taken}");
        assert_eq!(fs::read_to_string(&to).unwrap(), "one");
        assert_eq!(fs::read_to_string(&from).unwrap(), "two");
    }

    /// A file that is there, and one that is gone, are not being deleted.
    #[test]
    fn a_plain_or_missing_file_is_not_being_deleted() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("here"), "x").unwrap();
        assert!(!delete_pending(dir.path(), "here".as_ref()));
        assert!(!delete_pending(dir.path(), "gone".as_ref()));
    }

    /// On Windows, a file removed while another handle that shares its
    /// deletion is open stays listed, being deleted, when the removal did
    /// not take its name at once, until that handle closes.
    #[cfg(windows)]
    #[test]
    fn a_file_removed_while_held_open_is_being_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("held");
        fs::write(&path, "x").unwrap();
        let held = fs::File::open(&path).unwrap();
        fs::remove_file(&path).unwrap();
        let listed = || {
            fs::read_dir(dir.path())
                .unwrap()
                .any(|entry| entry.unwrap().file_name() == "held")
        };
        if listed() {
            assert!(fs::File::open(&path).is_err());
            assert!(delete_pending(dir.path(), "held".as_ref()));
        }
        drop(held);
        assert!(!listed());
        assert!(!delete_pending(dir.path(), "held".as_ref()));
    }
}
