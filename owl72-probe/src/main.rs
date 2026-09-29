//! Throwaway probe for OWL-72: how an AppContainer behaves on windows-latest,
//! before Owlshift confines its own agent runs in one (OWL-72b, OWL-72c).
//!
//! It starts only its own child processes in a container, against fake
//! secrets it plants in its own temporary folder and removes afterwards, and
//! prints one `CHECK name: result` line per question. Removed before the
//! pull request; the results are recorded in the build plan.

#[cfg(not(windows))]
fn main() {
    eprintln!("the OWL-72 probe runs on Windows only");
}

#[cfg(windows)]
fn main() {
    win::main();
}

#[cfg(windows)]
#[allow(dead_code, unused_imports, reason = "round 1 checks are kept for reference")]
mod win {
    use std::ffi::{OsStr, c_void};
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::ptr;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{
        CloseHandle, FILETIME, GetLastError, HANDLE, HANDLE_FLAG_INHERIT, LocalFree,
        SetHandleInformation, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Security::Authorization::{
        ACCESS_MODE, ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        ConvertStringSidToSidW, DENY_ACCESS, EXPLICIT_ACCESS_W, GRANT_ACCESS,
        GetNamedSecurityInfoW, NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS, SDDL_REVISION_1,
        SE_FILE_OBJECT, SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID,
        TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::Credentials::{
        CRED_PERSIST, CRED_PERSIST_LOCAL_MACHINE, CRED_PERSIST_SESSION, CRED_TYPE_GENERIC,
        CREDENTIALW, CredDeleteW, CredFree, CredReadW, CredWriteW,
    };
    use windows_sys::Win32::Security::Isolation::{
        CreateAppContainerProfile, DeleteAppContainerProfile,
        DeriveAppContainerSidFromAppContainerName,
    };
    use windows_sys::Win32::Security::{
        ACL, ACL_REVISION, AddAce, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
        GetSecurityDescriptorSacl, InitializeAcl, LABEL_SECURITY_INFORMATION, NO_INHERITANCE,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_CAPABILITIES,
        SID_AND_ATTRIBUTES,
        SUB_CONTAINERS_AND_OBJECTS_INHERIT,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
    };
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
        GetCurrentProcess, GetExitCodeProcess, InitializeProcThreadAttributeList,
        LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
        PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess,
        UpdateProcThreadAttribute, WaitForSingleObject,
    };
    use windows_sys::core::PWSTR;

    const PROFILE: &str = "owlshift.owl72.probe";
    const MARKER: &str = "FAKE_owl72";
    const CRED_TARGET: &str = "owlshift-owl72-probe";
    const CRED_BLOB: &[u8] = b"FAKE_owl72_credential";

    /// Read and execute: `FILE_GENERIC_READ | FILE_GENERIC_EXECUTE`.
    const READ_EXECUTE: u32 = 0x0012_00A9;
    /// "Modify": read, write, execute and `DELETE`, not `FILE_DELETE_CHILD`,
    /// `WRITE_DAC` or `WRITE_OWNER`.
    const MODIFY: u32 = 0x0013_01BF;
    /// Everything on a file or folder: `FILE_ALL_ACCESS`.
    const ALL: u32 = 0x001F_01FF;
    /// Every way to change a file or folder: write data, append, extended
    /// attributes, attributes, `FILE_DELETE_CHILD`, `DELETE`, `WRITE_DAC`,
    /// `WRITE_OWNER`.
    const ANY_WRITE: u32 = 0x0001_0116 | 0x40 | 0x0004_0000 | 0x0008_0000;

    const INTERNET_CLIENT: &str = "S-1-15-3-1";
    const INTERNET_CLIENT_SERVER: &str = "S-1-15-3-2";
    const PRIVATE_NETWORK: &str = "S-1-15-3-3";

    fn wide(text: impl AsRef<OsStr>) -> Vec<u16> {
        text.as_ref().encode_wide().chain([0]).collect()
    }

    fn check(name: &str, result: impl std::fmt::Display) {
        println!("CHECK {name}: {result}");
    }

    fn system_root() -> PathBuf {
        PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()))
    }

    fn cmd() -> String {
        system_root()
            .join(r"System32\cmd.exe")
            .display()
            .to_string()
    }

    fn curl() -> String {
        system_root()
            .join(r"System32\curl.exe")
            .display()
            .to_string()
    }

    const GIT: &str = r"C:\Program Files\Git\cmd\git.exe";

    // ---------------------------------------------------------------- SIDs

    /// The container's SID, its profile created, or found when it exists.
    fn profile_sid(name: &str) -> (PSID, String) {
        let name_w = wide(name);
        let mut sid: PSID = ptr::null_mut();
        // SAFETY: NUL-terminated strings live for the call; `sid` is written.
        let hr = unsafe {
            CreateAppContainerProfile(
                name_w.as_ptr(),
                name_w.as_ptr(),
                name_w.as_ptr(),
                ptr::null(),
                0,
                &mut sid,
            )
        };
        if hr >= 0 {
            return (sid, "created".into());
        }
        if hr as u32 == 0x8007_00B7 {
            return (derived_sid(name), "existed".into());
        }
        panic!("CreateAppContainerProfile failed: {hr:#x}");
    }

    /// The container's SID, derived from its name, no profile involved.
    fn derived_sid(name: &str) -> PSID {
        let name_w = wide(name);
        let mut sid: PSID = ptr::null_mut();
        // SAFETY: as above.
        let hr = unsafe { DeriveAppContainerSidFromAppContainerName(name_w.as_ptr(), &mut sid) };
        assert!(hr >= 0, "DeriveAppContainerSidFromAppContainerName: {hr:#x}");
        sid
    }

    fn string_sid(text: &str) -> PSID {
        let text_w = wide(text);
        let mut sid: PSID = ptr::null_mut();
        // SAFETY: as above.
        let ok = unsafe { ConvertStringSidToSidW(text_w.as_ptr(), &mut sid) };
        assert!(ok != 0, "ConvertStringSidToSidW({text})");
        sid
    }

    fn sid_text(sid: PSID) -> String {
        let mut text: PWSTR = ptr::null_mut();
        // SAFETY: `sid` is valid; the string is freed with LocalFree.
        unsafe {
            if ConvertSidToStringSidW(sid, &mut text) == 0 {
                return "?".into();
            }
            let mut len = 0;
            while *text.add(len) != 0 {
                len += 1;
            }
            let value = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
            LocalFree(text.cast());
            value
        }
    }

    // ----------------------------------------------------------- processes

    struct Child {
        process: HANDLE,
        output: mpsc::Receiver<String>,
    }

    /// Starts `application` with `line` in the container `sid`, with these
    /// capabilities, in `cwd`, its output and errors on one pipe. `Err` is
    /// the error `CreateProcessW` gave.
    fn spawn_in(
        sid: PSID,
        caps: &[PSID],
        application: &str,
        line: &str,
        cwd: &Path,
    ) -> Result<Child, u32> {
        let mut attributes: Vec<SID_AND_ATTRIBUTES> = caps
            .iter()
            .map(|&cap| SID_AND_ATTRIBUTES {
                Sid: cap,
                Attributes: 4, // SE_GROUP_ENABLED
            })
            .collect();
        let capabilities = SECURITY_CAPABILITIES {
            AppContainerSid: sid,
            Capabilities: if attributes.is_empty() {
                ptr::null_mut()
            } else {
                attributes.as_mut_ptr()
            },
            CapabilityCount: attributes.len() as u32,
            Reserved: 0,
        };
        let mut size = 0usize;
        // SAFETY: the first call only reports the size.
        unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut size) };
        let mut buffer = vec![0u64; size.div_ceil(8)];
        let list = buffer.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        // SAFETY: `buffer` holds `size` bytes and outlives the list's use;
        // `capabilities` and `attributes` live until CreateProcessW returns.
        unsafe {
            if InitializeProcThreadAttributeList(list, 1, 0, &mut size) == 0 {
                return Err(GetLastError());
            }
            if UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
                (&raw const capabilities).cast::<c_void>(),
                size_of::<SECURITY_CAPABILITIES>(),
                ptr::null_mut(),
                ptr::null(),
            ) == 0
            {
                let error = GetLastError();
                DeleteProcThreadAttributeList(list);
                return Err(error);
            }
        }
        let (reader, writer) = std::io::pipe().expect("pipe");
        // Standard input: a pipe whose writer is already closed, so the
        // program reads an end of file, as a runner's command would.
        let (stdin_reader, stdin_writer) = std::io::pipe().expect("pipe");
        drop(stdin_writer);
        // SAFETY: handles this process owns.
        unsafe {
            SetHandleInformation(writer.as_raw_handle(), HANDLE_FLAG_INHERIT, 1);
            SetHandleInformation(stdin_reader.as_raw_handle(), HANDLE_FLAG_INHERIT, 1);
        }
        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = stdin_reader.as_raw_handle();
        startup.StartupInfo.hStdOutput = writer.as_raw_handle();
        startup.StartupInfo.hStdError = writer.as_raw_handle();
        startup.lpAttributeList = list;
        let application_w = wide(application);
        let mut command_line = wide(format!("\"{application}\" {line}"));
        let cwd_w = wide(cwd);
        let mut info = PROCESS_INFORMATION::default();
        // SAFETY: every pointer is to a live, NUL-terminated local.
        let (created, error) = unsafe {
            let created = CreateProcessW(
                application_w.as_ptr(),
                command_line.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                1,
                EXTENDED_STARTUPINFO_PRESENT,
                ptr::null(),
                cwd_w.as_ptr(),
                &startup.StartupInfo,
                &mut info,
            );
            let error = GetLastError();
            DeleteProcThreadAttributeList(list);
            (created, error)
        };
        drop(writer);
        drop(stdin_reader);
        if created == 0 {
            return Err(error);
        }
        // SAFETY: a handle the call returned.
        unsafe { CloseHandle(info.hThread) };
        let (send, output) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = reader;
            let mut bytes = Vec::new();
            let _ = reader.read_to_end(&mut bytes);
            let _ = send.send(String::from_utf8_lossy(&bytes).into_owned());
        });
        Ok(Child {
            process: info.hProcess,
            output,
        })
    }

    impl Child {
        fn wait(self, timeout: Duration) -> String {
            // SAFETY: `process` is open until closed here.
            let status = unsafe {
                if WaitForSingleObject(self.process, timeout.as_millis() as u32) == WAIT_OBJECT_0 {
                    let mut code = 0u32;
                    GetExitCodeProcess(self.process, &mut code);
                    format!("exit {code}")
                } else {
                    TerminateProcess(self.process, 1);
                    "timed out".to_owned()
                }
            };
            // SAFETY: as above.
            unsafe { CloseHandle(self.process) };
            let output = self
                .output
                .recv_timeout(Duration::from_secs(5))
                .unwrap_or_else(|_| "(output still open)".into());
            format!("{status}{}", shown(&output))
        }
    }

    fn shown(output: &str) -> String {
        let flat: String = output
            .trim()
            .replace("\r\n", " / ")
            .replace('\n', " / ")
            .chars()
            .take(240)
            .collect();
        let marker = if output.contains(MARKER) {
            " [MARKER SEEN]"
        } else {
            ""
        };
        if flat.is_empty() {
            marker.to_owned()
        } else {
            format!("{marker} | {flat}")
        }
    }

    /// Runs `application` with `line` in the container and describes the
    /// outcome.
    fn confined(sid: PSID, caps: &[PSID], application: &str, line: &str, cwd: &Path) -> String {
        match spawn_in(sid, caps, application, line, cwd) {
            Ok(child) => child.wait(Duration::from_secs(40)),
            Err(error) => format!("not created: error {error}"),
        }
    }

    /// The same, outside any container: the control.
    fn bare(application: &str, line: &str, cwd: &Path) -> String {
        match Command::new(application)
            .raw_arg(line)
            .current_dir(cwd)
            .output()
        {
            Ok(out) => {
                let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&out.stderr));
                format!("exit {}{}", out.status.code().unwrap_or(-1), shown(&text))
            }
            Err(error) => format!("not started: {error}"),
        }
    }

    // ---------------------------------------------------------------- ACLs

    /// Adds, denies or revokes `mask` for `sid` on `path`, the change
    /// propagated to what is inside; returns the error code and the time.
    fn set_acl(
        path: &Path,
        sid: PSID,
        mask: u32,
        mode: ACCESS_MODE,
        inherit: u32,
    ) -> (u32, Duration) {
        let path_w = wide(path);
        let started = Instant::now();
        let mut old: *mut ACL = ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: out-pointers are live locals; buffers freed with LocalFree.
        unsafe {
            let error = GetNamedSecurityInfoW(
                path_w.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut old,
                ptr::null_mut(),
                &mut descriptor,
            );
            if error != 0 {
                return (error, started.elapsed());
            }
            let entry = EXPLICIT_ACCESS_W {
                grfAccessPermissions: mask,
                grfAccessMode: mode,
                grfInheritance: inherit,
                Trustee: TRUSTEE_W {
                    pMultipleTrustee: ptr::null_mut(),
                    MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                    TrusteeForm: TRUSTEE_IS_SID,
                    TrusteeType: TRUSTEE_IS_UNKNOWN,
                    ptstrName: sid.cast(),
                },
            };
            let mut new: *mut ACL = ptr::null_mut();
            let error = SetEntriesInAclW(1, &entry, old, &mut new);
            if error != 0 {
                LocalFree(descriptor);
                return (error, started.elapsed());
            }
            let error = SetNamedSecurityInfoW(
                path_w.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                new,
                ptr::null(),
            );
            LocalFree(new.cast());
            LocalFree(descriptor);
            (error, started.elapsed())
        }
    }

    /// Sets the integrity label an SDDL string gives, such as
    /// `S:(ML;OICI;NW;;;LW)`, or removes the explicit label (`None`).
    fn set_label(path: &Path, label: Option<&str>) -> (u32, Duration) {
        let path_w = wide(path);
        let started = Instant::now();
        // SAFETY: as in `set_acl`; the empty ACL lives in a local buffer.
        unsafe {
            let mut empty = [0u64; 8];
            let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
            let sacl: *mut ACL = if let Some(label) = label {
                let sddl = wide(label);
                if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    ptr::null_mut(),
                ) == 0
                {
                    return (GetLastError(), started.elapsed());
                }
                let (mut present, mut defaulted) = (0, 0);
                let mut sacl: *mut ACL = ptr::null_mut();
                GetSecurityDescriptorSacl(descriptor, &mut present, &mut sacl, &mut defaulted);
                sacl
            } else {
                let acl = empty.as_mut_ptr().cast::<ACL>();
                InitializeAcl(acl, size_of_val(&empty) as u32, ACL_REVISION);
                acl
            };
            let error = SetNamedSecurityInfoW(
                path_w.as_ptr(),
                SE_FILE_OBJECT,
                LABEL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
                sacl,
            );
            if !descriptor.is_null() {
                LocalFree(descriptor);
            }
            (error, started.elapsed())
        }
    }

    /// The explicit allowed and denied ACEs naming `sid` on `path`.
    fn aces_for(path: &Path, sid: PSID) -> String {
        let path_w = wide(path);
        let mut dacl: *mut ACL = ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: as in `set_acl`; each ACE is read within the ACL.
        unsafe {
            let error = GetNamedSecurityInfoW(
                path_w.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut dacl,
                ptr::null_mut(),
                &mut descriptor,
            );
            if error != 0 {
                return format!("error {error}");
            }
            let (mut allowed, mut denied, mut inherited) = (0, 0, 0);
            for index in 0..(*dacl).AceCount {
                let mut ace: *mut c_void = ptr::null_mut();
                if GetAce(dacl, u32::from(index), &mut ace) == 0 {
                    continue;
                }
                let bytes = ace.cast::<u8>();
                let (kind, flags) = (*bytes, *bytes.add(1));
                let ace_sid: PSID = bytes.add(8).cast();
                if (kind == 0 || kind == 1) && EqualSid(ace_sid, sid) != 0 {
                    if flags & 0x10 != 0 {
                        inherited += 1;
                    } else if kind == 0 {
                        allowed += 1;
                    } else {
                        denied += 1;
                    }
                }
            }
            LocalFree(descriptor);
            format!("{allowed} allowed, {denied} denied, {inherited} inherited")
        }
    }

    // --------------------------------------------------------- credentials

    fn cred_write(persist: CRED_PERSIST) -> u32 {
        let mut target = wide(CRED_TARGET);
        let mut user = wide("owl72");
        let mut blob = CRED_BLOB.to_vec();
        let credential = CREDENTIALW {
            Flags: 0,
            Type: CRED_TYPE_GENERIC,
            TargetName: target.as_mut_ptr(),
            Comment: ptr::null_mut(),
            LastWritten: FILETIME::default(),
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: persist,
            AttributeCount: 0,
            Attributes: ptr::null_mut(),
            TargetAlias: ptr::null_mut(),
            UserName: user.as_mut_ptr(),
        };
        // SAFETY: the credential's buffers live for the call.
        unsafe {
            if CredWriteW(&credential, 0) == 0 {
                GetLastError()
            } else {
                0
            }
        }
    }

    fn cred_read() -> String {
        let target = wide(CRED_TARGET);
        let mut credential: *mut CREDENTIALW = ptr::null_mut();
        // SAFETY: the returned credential is freed with CredFree.
        unsafe {
            if CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) == 0 {
                return format!("not read: error {}", GetLastError());
            }
            let blob = std::slice::from_raw_parts(
                (*credential).CredentialBlob,
                (*credential).CredentialBlobSize as usize,
            );
            let matches = blob == CRED_BLOB;
            CredFree(credential.cast());
            format!("READ, blob matches: {matches}")
        }
    }

    fn cred_delete() -> u32 {
        let target = wide(CRED_TARGET);
        // SAFETY: a NUL-terminated local.
        unsafe {
            if CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) == 0 {
                GetLastError()
            } else {
                0
            }
        }
    }

    // ------------------------------------------------------------- network

    /// A listener on the loopback that answers one request; its port, and
    /// whether a connection came within 30 s.
    fn host_listener() -> (u16, thread::JoinHandle<bool>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let handle = thread::spawn(move || answer_one(&listener, Duration::from_secs(30)));
        (port, handle)
    }

    fn answer_one(listener: &TcpListener, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).ok();
                    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                    let mut request = [0u8; 1024];
                    let _ = stream.read(&mut request);
                    let _ = stream
                        .write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok");
                    return true;
                }
                Err(_) => thread::sleep(Duration::from_millis(50)),
            }
        }
        false
    }

    fn curl_line(url: &str) -> String {
        format!("-sS -m 15 -o NUL -w \"%{{http_code}}\" {url}")
    }


    /// Stops `path` from inheriting, keeps every ACE but those naming `sid`
    /// (the inherited ones made explicit), then grants `grant` to `sid` when
    /// given; the change propagated to what is inside.
    fn protect_without(path: &Path, sid: PSID, grant: Option<(u32, u32)>) -> u32 {
        let path_w = wide(path);
        // SAFETY: as in `set_acl`; each ACE is copied within its size into
        // an ACL buffer as large as the old one.
        unsafe {
            let mut dacl: *mut ACL = ptr::null_mut();
            let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
            let error = GetNamedSecurityInfoW(
                path_w.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut dacl,
                ptr::null_mut(),
                &mut descriptor,
            );
            if error != 0 {
                return error;
            }
            let mut buffer = vec![0u64; usize::from((*dacl).AclSize).div_ceil(8) + 8];
            let new = buffer.as_mut_ptr().cast::<ACL>();
            InitializeAcl(new, (buffer.len() * 8) as u32, ACL_REVISION);
            for index in 0..(*dacl).AceCount {
                let mut ace: *mut c_void = ptr::null_mut();
                if GetAce(dacl, u32::from(index), &mut ace) == 0 {
                    continue;
                }
                let bytes = ace.cast::<u8>();
                let size = u16::from_le_bytes([*bytes.add(2), *bytes.add(3)]);
                if (*bytes == 0 || *bytes == 1) && EqualSid(bytes.add(8).cast(), sid) != 0 {
                    continue;
                }
                let mut copy = std::slice::from_raw_parts(bytes, usize::from(size)).to_vec();
                copy[1] &= !0x10;
                AddAce(new, ACL_REVISION, u32::MAX, copy.as_ptr().cast(), u32::from(size));
            }
            let mut granted: *mut ACL = ptr::null_mut();
            let mut chosen = new;
            if let Some((mask, inherit)) = grant {
                let entry = EXPLICIT_ACCESS_W {
                    grfAccessPermissions: mask,
                    grfAccessMode: GRANT_ACCESS,
                    grfInheritance: inherit,
                    Trustee: TRUSTEE_W {
                        pMultipleTrustee: ptr::null_mut(),
                        MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                        TrusteeForm: TRUSTEE_IS_SID,
                        TrusteeType: TRUSTEE_IS_UNKNOWN,
                        ptstrName: sid.cast(),
                    },
                };
                let error = SetEntriesInAclW(1, &entry, new, &mut granted);
                if error != 0 {
                    LocalFree(descriptor);
                    return error;
                }
                chosen = granted;
            }
            let error = SetNamedSecurityInfoW(
                path_w.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                chosen,
                ptr::null(),
            );
            if !granted.is_null() {
                LocalFree(granted.cast());
            }
            LocalFree(descriptor);
            error
        }
    }

    // -------------------------------------------------------------- helper

    /// The probe run again, as a program inside the container.
    fn helper(args: &[String]) {
        match args.first().map(String::as_str) {
            Some("noop") => println!("helper ran"),
            Some("open-nul") => {
                for name in ["NUL", r"\\.\NUL"] {
                    let open = |read: bool, write: bool| {
                        match fs::OpenOptions::new().read(read).write(write).open(name) {
                            Ok(_) => "ok".to_owned(),
                            Err(error) => format!("{:?}", error.raw_os_error()),
                        }
                    };
                    println!(
                        "{name}: read+write {}, read {}, write {}",
                        open(true, true),
                        open(true, false),
                        open(false, true)
                    );
                }
            }
            other => println!("unknown helper {other:?}"),
        }
    }

    // ---------------------------------------------------------------- main

    /// Round 2: what round 1 (run 36603469888) left open or turned up.
    pub fn main() {
        let args: Vec<String> = std::env::args().collect();
        if args.get(1).map(String::as_str) == Some("helper") {
            helper(&args[2..]);
            return;
        }
        let root = system_root();
        let exe = std::env::current_exe().unwrap();
        let exe_text = exe.display().to_string();
        let exe_dir = exe.parent().unwrap().to_owned();
        println!("== OWL-72 AppContainer probe, round 2 ==");
        check("os", bare(&cmd(), "/d /c ver", &root));
        let (sid, how) = profile_sid(PROFILE);
        check("profile", format!("{how}, {}", sid_text(sid)));
        let quoted = |path: &Path| format!("\"{}\"", path.display());
        let tree = SUB_CONTAINERS_AND_OBJECTS_INHERIT;

        let base = std::env::temp_dir().join(format!("owl72b-{}", std::process::id()));
        let granted = base.join("granted");
        for dir in ["hooks", "hooks2", "ml-dir"] {
            fs::create_dir_all(granted.join(dir)).unwrap();
            fs::write(granted.join(dir).join("sample"), "sample\n").unwrap();
        }
        for file in ["secret.env", "secret2.env", "secret3.env", "ml-file.env"] {
            fs::write(granted.join(file), format!("API_KEY={MARKER}\n")).unwrap();
        }
        let (error, _) = set_acl(&granted, sid, MODIFY, GRANT_ACCESS, tree);
        check("setup.grant", format!("error {error}"));

        // 1. The program the launcher starts, and one its child starts.
        check(
            "image.launched-directly",
            confined(sid, &[], &exe_text, "helper noop", &root),
        );
        let grandchild = format!("/d /s /c \"\"{exe_text}\" helper noop\"");
        check(
            "image.started-by-child-not-granted",
            confined(sid, &[], &cmd(), &grandchild, &root),
        );
        let (error, _) = set_acl(&exe_dir, sid, READ_EXECUTE, GRANT_ACCESS, tree);
        check(
            "image.started-by-child-granted",
            format!(
                "grant error {error}; {}",
                confined(sid, &[], &cmd(), &grandchild, &root)
            ),
        );

        // 2. The NUL device.
        check(
            "nul.cmd-redirect",
            confined(
                sid,
                &[],
                &cmd(),
                "/d /c echo x> NUL && echo redirected",
                &root,
            ),
        );
        check("nul.open-bare", bare(&exe_text, "helper open-nul", &root));
        check(
            "nul.open-confined",
            confined(sid, &[], &exe_text, "helper open-nul", &root),
        );

        // 3. Git, with a standard input this time, in a granted repository.
        let repo = base.join("repo");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("README.md"), "hello\n").unwrap();
        let identity = "-c user.name=owl72 -c user.email=owl72@example.invalid";
        bare(GIT, "init -q", &repo);
        bare(GIT, &format!("{identity} add README.md"), &repo);
        check(
            "git.bare-commit",
            bare(GIT, &format!("{identity} commit -q -m seed"), &repo),
        );
        let (error, _) = set_acl(&repo, sid, MODIFY, GRANT_ACCESS, tree);
        check("git.grant", format!("error {error}"));
        check("git.version", confined(sid, &[], GIT, "--version", &root));
        check(
            "git.status",
            confined(sid, &[], GIT, "status --porcelain", &repo),
        );
        fs::write(repo.join("b.txt"), "b\n").unwrap();
        check("git.add", confined(sid, &[], GIT, "add b.txt", &repo));
        check(
            "git.commit",
            confined(
                sid,
                &[],
                GIT,
                &format!("{identity} commit -q -m confined"),
                &repo,
            ),
        );
        check("git.log-bare", bare(GIT, "log --oneline", &repo));

        // 4. Deny entries naming the container, dumped this time.
        let secret = granted.join("secret.env");
        let hooks = granted.join("hooks");
        let (error, _) = set_acl(&secret, sid, ALL, DENY_ACCESS, NO_INHERITANCE);
        check(
            "deny.secret",
            format!("error {error}; {}", aces_for(&secret, sid)),
        );
        check(
            "deny.secret-read",
            confined(sid, &[], &cmd(), "/d /c type secret.env", &granted),
        );
        let (error, _) = set_acl(&hooks, sid, ANY_WRITE, DENY_ACCESS, tree);
        check(
            "deny.hooks",
            format!("error {error}; {}", aces_for(&hooks, sid)),
        );
        confined(
            sid,
            &[],
            &cmd(),
            "/d /c echo x> hooks\\pre-commit",
            &granted,
        );
        check("deny.hooks-write-landed", hooks.join("pre-commit").exists());
        let (error, _) = set_acl(&secret, sid, 0, REVOKE_ACCESS, NO_INHERITANCE);
        check(
            "deny.secret-after-revoke",
            format!("error {error}; {}", aces_for(&secret, sid)),
        );
        // A deny entry for every container: ALL APPLICATION PACKAGES.
        let secret3 = granted.join("secret3.env");
        let all_packages = string_sid("S-1-15-2-1");
        let (error, _) = set_acl(&secret3, all_packages, ALL, DENY_ACCESS, NO_INHERITANCE);
        check(
            "deny.all-packages-read",
            format!(
                "error {error}; {}",
                confined(sid, &[], &cmd(), "/d /c type secret3.env", &granted)
            ),
        );

        // 5. Inheritance broken, the container left out: a hidden file, and
        // a folder it may only read.
        let secret2 = granted.join("secret2.env");
        let error = protect_without(&secret2, sid, None);
        check(
            "protect.secret",
            format!("error {error}; {}", aces_for(&secret2, sid)),
        );
        check(
            "protect.secret-read",
            confined(sid, &[], &cmd(), "/d /c type secret2.env", &granted),
        );
        confined(sid, &[], &cmd(), "/d /c del /q secret2.env", &granted);
        check("protect.secret-still-there-after-del", secret2.exists());
        confined(
            sid,
            &[],
            &cmd(),
            "/d /c ren secret2.env moved.env",
            &granted,
        );
        check("protect.secret-still-there-after-ren", secret2.exists());
        let hooks2 = granted.join("hooks2");
        let error = protect_without(&hooks2, sid, Some((READ_EXECUTE, tree)));
        check(
            "protect.hooks",
            format!("error {error}; {}", aces_for(&hooks2, sid)),
        );
        check(
            "protect.hooks-read",
            confined(sid, &[], &cmd(), "/d /c type hooks2\\sample", &granted),
        );
        confined(
            sid,
            &[],
            &cmd(),
            "/d /c echo x> hooks2\\pre-commit",
            &granted,
        );
        check(
            "protect.hooks-write-landed",
            hooks2.join("pre-commit").exists(),
        );
        confined(sid, &[], &cmd(), "/d /c echo x>> hooks2\\sample", &granted);
        check(
            "protect.hooks-append-landed",
            fs::read_to_string(hooks2.join("sample"))
                .unwrap()
                .contains('x'),
        );
        confined(sid, &[], &cmd(), "/d /c del /q hooks2\\sample", &granted);
        check(
            "protect.hooks-still-there-after-del",
            hooks2.join("sample").exists(),
        );
        confined(sid, &[], &cmd(), "/d /c rd /s /q hooks2", &granted);
        check(
            "protect.hooks-still-there-after-rd",
            hooks2.join("sample").exists(),
        );
        confined(sid, &[], &cmd(), "/d /c ren hooks2 hooks2-moved", &granted);
        check(
            "protect.hooks-still-there-after-ren",
            hooks2.join("sample").exists(),
        );

        // 6. Explicit medium integrity labels inside the granted folder.
        let ml_file = granted.join("ml-file.env");
        let (error, _) = set_label(&ml_file, Some("S:(ML;;NWNR;;;ME)"));
        check(
            "label.medium-no-read-up-file",
            format!(
                "error {error}; {}",
                confined(sid, &[], &cmd(), "/d /c type ml-file.env", &granted)
            ),
        );
        let ml_dir = granted.join("ml-dir");
        let (error, _) = set_label(&ml_dir, Some("S:(ML;OICI;NW;;;ME)"));
        confined(sid, &[], &cmd(), "/d /c echo x> ml-dir\\new.txt", &granted);
        check(
            "label.medium-no-write-up-folder",
            format!(
                "error {error}; write landed {}",
                ml_dir.join("new.txt").exists()
            ),
        );
        check(
            "label.container-level",
            confined(
                sid,
                &[],
                &cmd(),
                "/d /c whoami /groups | findstr /i Label",
                &root,
            ),
        );
        check(
            "label.granted-folder",
            bare(&cmd(), &format!("/d /c icacls {}", quoted(&granted)), &root),
        );

        // Cleanup.
        set_acl(&exe_dir, sid, 0, REVOKE_ACCESS, NO_INHERITANCE);
        // SAFETY: a NUL-terminated local.
        let deleted = unsafe { DeleteAppContainerProfile(wide(PROFILE).as_ptr()) };
        check("cleanup.profile-deleted", format!("hr {deleted:#x}"));
        check("cleanup.base-removed", fs::remove_dir_all(&base).is_ok());
        println!("== done ==");
    }
}
