//! What the tests read of a process, without running a program: `ps` is
//! setuid root on macOS, and a process confined by Seatbelt may not exec a
//! setuid binary, whatever its profile allows (OWL-110). The CLI's tests
//! include this file with `#[path]`.

/// A process's state, as the letter `ps` shows first: `R` running, `S`
/// asleep, `T` stopped, `Z` zombie. Empty once the process is gone. Panics
/// on a state it does not know: for a test's own assertions, never for a
/// `Drop`, which uses [`try_state`].
pub fn state(pid: u32) -> String {
    imp::state(pid).unwrap_or_else(|error| panic!("{error}"))
}

/// [`state`], empty instead of a panic when the state cannot be read or is
/// not known: for a `Drop`, which also runs while a failed test unwinds, and
/// where a panic would abort the test binary and hide the first failure.
pub fn try_state(pid: u32) -> String {
    imp::state(pid).unwrap_or_default()
}

/// A process's arguments joined by spaces; empty once it is gone, or when
/// its arguments cannot be read. Never panics, so a `Drop` may call it.
pub fn command(pid: u32) -> String {
    imp::command(pid)
}

/// Whether a process is still running; a zombie is not.
pub fn is_alive(pid: u32) -> bool {
    let state = state(pid);
    !state.is_empty() && !state.starts_with('Z')
}

#[cfg(target_os = "linux")]
mod imp {
    use std::{fs, io};

    /// `ESRCH` on Linux, whatever the architecture: the process exited
    /// between the open and the read of `/proc/<pid>/stat`.
    const ESRCH: i32 = 3;

    pub fn state(pid: u32) -> Result<String, String> {
        match fs::read_to_string(format!("/proc/{pid}/stat")) {
            // `pid (comm) S ...`: the name may hold spaces and parentheses,
            // so the state follows the last closing one.
            Ok(stat) => Ok(stat
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.trim_start().chars().next())
                .map(String::from)
                .unwrap_or_default()),
            // The process is gone, or went while it was read.
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(ESRCH) =>
            {
                Ok(String::new())
            }
            Err(error) => Err(error.to_string()),
        }
    }

    pub fn command(pid: u32) -> String {
        fs::read(format!("/proc/{pid}/cmdline"))
            .map(|raw| {
                let args: Vec<_> = raw
                    .split(|&byte| byte == 0)
                    .filter(|arg| !arg.is_empty())
                    .map(String::from_utf8_lossy)
                    .collect();
                args.join(" ")
            })
            .unwrap_or_default()
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::c_void;
    use std::ptr;

    /// Where `p_stat` lies in `kinfo_proc`, which `libc` does not define:
    /// `kp_proc` (`extern_proc`) starts with `p_un` (16 bytes), `p_vmspace`
    /// and `p_sigacts` (8 each), then `p_flag` (4), then `p_stat`.
    const P_STAT: usize = 36;
    /// `sizeof(struct kinfo_proc)` on 64-bit macOS.
    const KINFO_PROC_SIZE: usize = 648;

    /// Reads one `sysctl`; `None` when it fails or returns nothing, as it
    /// does for a process that is gone.
    fn sysctl(mib: &mut [libc::c_int], size: usize) -> Option<Vec<u8>> {
        let mut buffer = vec![0u8; size];
        let mut length = buffer.len();
        // SAFETY: `buffer` is `length` bytes long and outlives the call; no
        // new value is set.
        let status = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as libc::c_uint,
                buffer.as_mut_ptr().cast::<c_void>(),
                &mut length,
                ptr::null_mut(),
                0,
            )
        };
        if status != 0 || length == 0 {
            return None;
        }
        buffer.truncate(length);
        Some(buffer)
    }

    pub fn state(pid: u32) -> Result<String, String> {
        let mut mib = [
            libc::CTL_KERN,
            libc::KERN_PROC,
            libc::KERN_PROC_PID,
            pid as libc::c_int,
        ];
        let Some(info) = sysctl(&mut mib, KINFO_PROC_SIZE) else {
            return Ok(String::new());
        };
        let Some(&stat) = info.get(P_STAT) else {
            return Err(format!("short kinfo_proc: {}", info.len()));
        };
        match u32::from(stat) {
            libc::SIDL => Ok("I"),
            libc::SRUN => Ok("R"),
            libc::SSLEEP => Ok("S"),
            libc::SSTOP => Ok("T"),
            libc::SZOMB => Ok("Z"),
            other => Err(format!("unknown process state {other}")),
        }
        .map(str::to_owned)
    }

    pub fn command(pid: u32) -> String {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
        let Some(raw) = sysctl(&mut mib, 1 << 20) else {
            return String::new();
        };
        // `argc` (an int), the executable's path, its NUL padding, then the
        // arguments, each ended by a NUL.
        let Some((argc, rest)) = raw.split_first_chunk::<4>() else {
            return String::new();
        };
        let argc = i32::from_ne_bytes(*argc).max(0) as usize;
        let mut parts = rest.split(|&byte| byte == 0);
        parts.next(); // the executable's path
        let args: Vec<_> = parts
            .skip_while(|part| part.is_empty())
            .take(argc)
            .map(String::from_utf8_lossy)
            .collect();
        args.join(" ")
    }
}
