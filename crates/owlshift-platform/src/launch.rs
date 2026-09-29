//! `owlshift-launch`, the program that starts an agent command on native
//! Windows (OWL-71). It is test-only: native confinement is set aside (build
//! plan, 2026-09-29) and the launcher is slated for removal (OWL-89). The runner starts it as the root of the run's process
//! tree, inside its Job Object, and it creates the program itself with
//! `CreateProcessW`. A process in a job starts its children in that job
//! (`tree.rs` allows no breakaway), so stopping the tree stops the program
//! and everything it started.
//!
//! It is a program of its own because std's `Command` on stable Rust cannot
//! pass a proc-thread attribute (checked on rustc 1.98.1 on 2026-09-29, build
//! plan, "OWL-71"), and an AppContainer is given through one
//! (`STARTUPINFOEXW` with `SECURITY_CAPABILITIES`). No AppContainer is
//! planned; it applies nothing.
//!
//! Its arguments are exactly `-- PROGRAM LINE`, which `sandbox::wrap` and
//! `sandbox::wrap_line` build. `LINE` is the rest of the program's command
//! line, after its name, verbatim. The program's name is resolved as std's
//! `Command` resolves one, but for the runner's own folder and `PATH`:
//! `.exe` is added when it has no extension; a name with no folder is looked
//! for in the absolute folders of the `PATH` it was given, the agent's, then
//! in the system folders, never in the working directory, which the agent
//! writes. A batch file is refused, as `wrap` refuses it: `cmd.exe` would
//! read its arguments again.
//!
//! It passes on its three standard handles, which it inherited from the
//! runner, then closes its own copies, so the pipes end as they would for a
//! program the runner started itself; it waits for the program and exits
//! with its exit code. When it cannot start the program it says why on
//! standard error, `owlshift-launch: <reason>`, and exits with
//! [`LAUNCH_FAILED`].

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetExitCodeProcess, INFINITE, PROCESS_INFORMATION, STARTF_USESTDHANDLES,
    STARTUPINFOW, WaitForSingleObject,
};

use crate::sandbox::is_batch_file;

/// The status the launcher exits with when it could not start the program,
/// or lost track of it; its message on standard error tells it from a
/// program that exited with the same status.
pub const LAUNCH_FAILED: i32 = 127;

/// Runs the launcher with the process's own arguments and returns the
/// status to exit with: the program's, or [`LAUNCH_FAILED`].
pub fn main() -> i32 {
    let (program, line) = match arguments(std::env::args_os().skip(1)) {
        Ok(arguments) => arguments,
        Err(reason) => return failed(&reason),
    };
    let search = Search {
        path: std::env::var_os("PATH"),
        system_root: std::env::var_os("SYSTEMROOT"),
    };
    let application = match resolve(&program, &search) {
        Ok(application) => application,
        Err(reason) => return failed(&reason),
    };
    if is_batch_file(application.as_os_str()) {
        return failed(&format!(
            "{} is a batch file, or names a file stream, which the launcher does not start",
            application.display()
        ));
    }
    let Some(mut command_line) = child_command_line(&program, &line) else {
        return failed("a program's name cannot hold a quote");
    };
    match start(&application, &mut command_line) {
        Ok(process) => wait(&process),
        Err(error) => failed(&format!(
            "could not start {}: {error}",
            application.display()
        )),
    }
}

fn failed(reason: &str) -> i32 {
    eprintln!("owlshift-launch: {reason}");
    LAUNCH_FAILED
}

/// `-- PROGRAM LINE`, the arguments after the launcher's own name.
fn arguments(mut args: impl Iterator<Item = OsString>) -> Result<(OsString, OsString), String> {
    match (args.next(), args.next(), args.next(), args.next()) {
        (Some(dash), Some(program), Some(line), None) if dash == "--" => Ok((program, line)),
        _ => Err("usage: owlshift-launch -- PROGRAM LINE".to_owned()),
    }
}

/// Where a program's name with no folder is looked for.
struct Search {
    /// The `PATH` the launcher was given, the agent's.
    path: Option<OsString>,
    /// `SYSTEMROOT`, the Windows folder.
    system_root: Option<OsString>,
}

/// The file `program` names: `.exe` added when it has no extension, and a
/// name with no folder looked for in the absolute folders of the `PATH`,
/// then in `SYSTEMROOT\System32` and `SYSTEMROOT`, as std searches the
/// system folders too.
fn resolve(program: &OsStr, search: &Search) -> Result<PathBuf, String> {
    let mut name = PathBuf::from(program);
    if name.extension().is_none() {
        name.set_extension("exe");
    }
    let bare = name
        .parent()
        .is_some_and(|parent| parent.as_os_str().is_empty());
    if !bare {
        return Ok(name);
    }
    search
        .folders()
        .into_iter()
        .map(|folder| folder.join(&name))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| format!("{} was not found on the PATH", name.display()))
}

impl Search {
    /// The folders looked in, in order: the absolute ones of the `PATH`,
    /// empty and relative entries skipped, then `SYSTEMROOT\System32` and
    /// `SYSTEMROOT`.
    fn folders(&self) -> Vec<PathBuf> {
        let path_folders = self
            .path
            .iter()
            .flat_map(std::env::split_paths)
            .filter(|folder| folder.is_absolute());
        let system_folders = self
            .system_root
            .iter()
            .map(PathBuf::from)
            .filter(|root| root.is_absolute())
            .flat_map(|root| [root.join("System32"), root]);
        path_folders.chain(system_folders).collect()
    }
}

/// The program's command line, NUL-terminated: its name in quotes, as std
/// writes it, then `line` after a space when there is one. `None` when the
/// name holds a quote, which it cannot be written with.
fn child_command_line(program: &OsStr, line: &OsStr) -> Option<Vec<u16>> {
    let quote = u16::from(b'"');
    let name: Vec<u16> = program.encode_wide().collect();
    if name.contains(&quote) {
        return None;
    }
    let mut command_line = vec![quote];
    command_line.extend(name);
    command_line.push(quote);
    if !line.is_empty() {
        command_line.push(u16::from(b' '));
        command_line.extend(line.encode_wide());
    }
    command_line.push(0);
    Some(command_line)
}

/// Creates the program with the launcher's standard handles, then closes
/// the launcher's own copies of them.
fn start(application: &Path, command_line: &mut [u16]) -> io::Result<OwnedHandle> {
    let application: Vec<u16> = application.as_os_str().encode_wide().chain([0]).collect();
    let handles: [RawHandle; 3] = [
        io::stdin().as_raw_handle(),
        io::stdout().as_raw_handle(),
        io::stderr().as_raw_handle(),
    ];
    let startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        dwFlags: STARTF_USESTDHANDLES,
        hStdInput: handles[0],
        hStdOutput: handles[1],
        hStdError: handles[2],
        ..STARTUPINFOW::default()
    };
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: `application` and `command_line` are NUL-terminated and live
    // for the call, and `command_line` is writable, as the call requires;
    // `startup` and `info` are live locals; the null pointers are the
    // optional security attributes, environment and directory, inherited.
    let created = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            1,
            0,
            ptr::null(),
            ptr::null(),
            &startup,
            &mut info,
        )
    };
    if created == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call succeeded, so both handles are new and owned here.
    let (process, thread) = unsafe {
        (
            OwnedHandle::from_raw_handle(info.hProcess),
            OwnedHandle::from_raw_handle(info.hThread),
        )
    };
    drop(thread);
    close_standard_handles(&handles);
    Ok(process)
}

/// Closes the launcher's copies of its standard handles, each once: the
/// program holds its own. Nothing is written to them afterwards.
fn close_standard_handles(handles: &[RawHandle; 3]) {
    for (index, &handle) in handles.iter().enumerate() {
        if handle.is_null() || handles[..index].contains(&handle) {
            continue;
        }
        // SAFETY: an open handle of this process, closed once; std's
        // standard streams are not used after this. Best effort: a failure
        // leaves the handle open until the launcher exits.
        unsafe { CloseHandle(handle) };
    }
}

/// Waits for the program and returns its exit code. The standard handles
/// are closed by then, so a failure here is only the status.
fn wait(process: &OwnedHandle) -> i32 {
    // SAFETY: `process` is open for the duration of both calls, and `code`
    // is a live local.
    unsafe {
        if WaitForSingleObject(process.as_raw_handle(), INFINITE) != WAIT_OBJECT_0 {
            return LAUNCH_FAILED;
        }
        let mut code = 0u32;
        if GetExitCodeProcess(process.as_raw_handle(), &mut code) == 0 {
            return LAUNCH_FAILED;
        }
        // Windows exit codes are 32 bits; `process::exit` gives the same
        // bits back.
        code as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_arguments_are_exactly_a_program_and_its_line() {
        let args = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            arguments(args(&["--", "git", "log"]).into_iter()),
            Ok(("git".into(), "log".into()))
        );
        for wrong in [
            &["git", "log"][..],
            &["--", "git"],
            &["--", "git", "a", "b"],
            &["-x", "git", "a"],
        ] {
            assert!(arguments(args(wrong).into_iter()).is_err(), "{wrong:?}");
        }
    }

    /// A name with no folder is looked for in the absolute folders of the
    /// `PATH`, then the system folders: never in a relative entry, which
    /// would be the agent's working directory or a folder in it.
    #[test]
    fn a_program_is_found_as_std_finds_it_but_never_from_the_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        let root = dir.path().join("Windows");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(root.join("System32")).unwrap();
        std::fs::write(bin.join("tool.exe"), "").unwrap();
        std::fs::write(root.join("System32").join("cmd.exe"), "").unwrap();
        let path = std::env::join_paths([PathBuf::from("."), PathBuf::new(), bin.clone()]).unwrap();
        let search = Search {
            path: Some(path),
            system_root: Some(root.clone().into_os_string()),
        };
        assert_eq!(
            search.folders(),
            [bin.clone(), root.join("System32"), root.clone()]
        );
        assert_eq!(
            resolve(OsStr::new("tool"), &search),
            Ok(bin.join("tool.exe"))
        );
        assert_eq!(
            resolve(OsStr::new("cmd.exe"), &search),
            Ok(root.join("System32").join("cmd.exe"))
        );
        assert!(resolve(OsStr::new("missing"), &search).is_err());
        // A name with a folder is taken as it is, `.exe` added.
        assert_eq!(
            resolve(OsStr::new(r"C:\x\run"), &search),
            Ok(PathBuf::from(r"C:\x\run.exe"))
        );
    }

    #[test]
    fn the_program_name_is_quoted_and_the_line_follows_verbatim() {
        let text = |units: Vec<u16>| String::from_utf16(&units[..units.len() - 1]).unwrap();
        let line = child_command_line(OsStr::new(r"C:\a b\cmd.exe"), OsStr::new(r#"/d /s /c "x""#));
        assert_eq!(text(line.unwrap()), r#""C:\a b\cmd.exe" /d /s /c "x""#);
        let bare = child_command_line(OsStr::new("git"), OsStr::new(""));
        assert_eq!(text(bare.unwrap()), r#""git""#);
        assert_eq!(child_command_line(OsStr::new("a\"b"), OsStr::new("")), None);
    }
}
