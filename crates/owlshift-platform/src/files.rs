//! What a folder's entry is, beyond what the standard library tells.

use std::ffi::OsStr;
use std::path::Path;

/// Whether the entry `name` of the folder `dir` is being deleted (OWL-425,
/// OWL-426). On Windows, a file being deleted stays listed for a moment,
/// until the deletion ends, or as long as another handle keeps it open, and
/// every open of it meanwhile is refused as "access denied"
/// (`STATUS_DELETE_PENDING`), which says nothing of a file merely
/// unreadable. Other systems drop the name at once: never.
pub fn delete_pending(dir: &Path, name: &OsStr) -> bool {
    sys::delete_pending(dir, name)
}

#[cfg(not(windows))]
mod sys {
    use std::ffi::OsStr;
    use std::path::Path;

    pub(super) fn delete_pending(_dir: &Path, _name: &OsStr) -> bool {
        false
    }
}

#[cfg(windows)]
mod sys {
    use std::ffi::OsStr;
    use std::fs::OpenOptions;
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
        FILE_SHARE_WRITE, SYNCHRONIZE,
    };
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

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
