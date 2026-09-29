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
#[allow(dead_code, unused_imports, reason = "helpers of earlier rounds are kept")]
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
        ACL, ACL_REVISION, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
        GetSecurityDescriptorSacl, InitializeAcl, LABEL_SECURITY_INFORMATION, NO_INHERITANCE,
        PSECURITY_DESCRIPTOR, PSID, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES,
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


    // ------------------------------------------------- restricted tokens

    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo,
    };
    use windows_sys::Win32::Security::{
        AddAccessAllowedAce, CreateRestrictedToken, DISABLE_MAX_PRIVILEGE,
        GROUP_SECURITY_INFORMATION, GetLengthSid, GetTokenInformation, OWNER_SECURITY_INFORMATION,
        SetTokenInformation, TOKEN_ALL_ACCESS, TOKEN_DEFAULT_DACL, TOKEN_GROUPS,
        TOKEN_MANDATORY_LABEL, TOKEN_USER, TokenDefaultDacl, TokenIntegrityLevel, TokenLogonSid,
        TokenUser,
    };
    use windows_sys::Win32::System::Threading::{
        CreateProcessAsUserW, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_VM_READ, STARTUPINFOW,
    };

    const GENERIC_ALL: u32 = 0x1000_0000;

    /// A restricted version of this process's token: every privilege but
    /// the bypass of traverse checks dropped, and a second access check
    /// against Everyone, Users and `run` only. Its default DACL names the
    /// user, `run` and SYSTEM, so what the process creates is its own.
    fn restricted_token(run: PSID, logon: bool, low: bool) -> Result<HANDLE, String> {
        // SAFETY: handles and buffers are live locals; the token handle is
        // leaked on purpose, the probe exits soon after.
        unsafe {
            let mut own: HANDLE = ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut own) == 0 {
                return Err(format!("OpenProcessToken: {}", GetLastError()));
            }
            let mut restricting = vec![string_sid("S-1-1-0"), string_sid("S-1-5-32-545"), run];
            if logon {
                let groups = Box::leak(vec![0u64; 16].into_boxed_slice());
                let mut length = 0u32;
                if GetTokenInformation(
                    own,
                    TokenLogonSid,
                    groups.as_mut_ptr().cast(),
                    (groups.len() * 8) as u32,
                    &mut length,
                ) == 0
                {
                    return Err(format!("TokenLogonSid: {}", GetLastError()));
                }
                restricting.push((*groups.as_ptr().cast::<TOKEN_GROUPS>()).Groups[0].Sid);
            }
            let sids: Vec<SID_AND_ATTRIBUTES> = restricting
                .iter()
                .map(|&sid| SID_AND_ATTRIBUTES {
                    Sid: sid,
                    Attributes: 0,
                })
                .collect();
            let mut token: HANDLE = ptr::null_mut();
            if CreateRestrictedToken(
                own,
                DISABLE_MAX_PRIVILEGE,
                0,
                ptr::null(),
                0,
                ptr::null(),
                sids.len() as u32,
                sids.as_ptr(),
                &mut token,
            ) == 0
            {
                return Err(format!("CreateRestrictedToken: {}", GetLastError()));
            }
            let mut user = vec![0u64; 16];
            let mut length = 0u32;
            if GetTokenInformation(
                own,
                TokenUser,
                user.as_mut_ptr().cast(),
                (user.len() * 8) as u32,
                &mut length,
            ) == 0
            {
                return Err(format!("GetTokenInformation: {}", GetLastError()));
            }
            let user_sid = (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid;
            let acl_buffer = Box::leak(vec![0u64; 64].into_boxed_slice());
            let acl = acl_buffer.as_mut_ptr().cast::<ACL>();
            InitializeAcl(acl, (acl_buffer.len() * 8) as u32, ACL_REVISION);
            for sid in [user_sid, run, string_sid("S-1-5-18")] {
                AddAccessAllowedAce(acl, ACL_REVISION, GENERIC_ALL, sid);
            }
            let default = TOKEN_DEFAULT_DACL { DefaultDacl: acl };
            if SetTokenInformation(
                token,
                TokenDefaultDacl,
                (&raw const default).cast(),
                size_of::<TOKEN_DEFAULT_DACL>() as u32,
            ) == 0
            {
                return Err(format!("SetTokenInformation: {}", GetLastError()));
            }
            if low {
                let low_sid = string_sid("S-1-16-4096");
                let label = TOKEN_MANDATORY_LABEL {
                    Label: SID_AND_ATTRIBUTES {
                        Sid: low_sid,
                        Attributes: 0x20, // SE_GROUP_INTEGRITY
                    },
                };
                if SetTokenInformation(
                    token,
                    TokenIntegrityLevel,
                    (&raw const label).cast(),
                    size_of::<TOKEN_MANDATORY_LABEL>() as u32 + GetLengthSid(low_sid),
                ) == 0
                {
                    return Err(format!("TokenIntegrityLevel: {}", GetLastError()));
                }
            }
            Ok(token)
        }
    }

    /// [`spawn_in`] for a restricted token rather than a container.
    fn spawn_as(token: HANDLE, application: &str, line: &str, cwd: &Path) -> Result<Child, u32> {
        let (reader, writer) = std::io::pipe().expect("pipe");
        let (stdin_reader, stdin_writer) = std::io::pipe().expect("pipe");
        drop(stdin_writer);
        // SAFETY: handles this process owns.
        unsafe {
            SetHandleInformation(writer.as_raw_handle(), HANDLE_FLAG_INHERIT, 1);
            SetHandleInformation(stdin_reader.as_raw_handle(), HANDLE_FLAG_INHERIT, 1);
        }
        let startup = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            dwFlags: STARTF_USESTDHANDLES,
            hStdInput: stdin_reader.as_raw_handle(),
            hStdOutput: writer.as_raw_handle(),
            hStdError: writer.as_raw_handle(),
            ..STARTUPINFOW::default()
        };
        let application_w = wide(application);
        let mut command_line = wide(format!("\"{application}\" {line}"));
        let cwd_w = wide(cwd);
        let mut info = PROCESS_INFORMATION::default();
        // SAFETY: every pointer is to a live, NUL-terminated local.
        let (created, error) = unsafe {
            let created = CreateProcessAsUserW(
                token,
                application_w.as_ptr(),
                command_line.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                1,
                0,
                ptr::null(),
                cwd_w.as_ptr(),
                &startup,
                &mut info,
            );
            (created, GetLastError())
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

    fn restricted(token: HANDLE, application: &str, line: &str, cwd: &Path) -> String {
        match spawn_as(token, application, line, cwd) {
            Ok(child) => child.wait(Duration::from_secs(40)),
            Err(error) => format!("not created: error {error}"),
        }
    }

    /// The NUL device's security descriptor, as SDDL.
    fn nul_sddl() -> String {
        use std::os::windows::fs::OpenOptionsExt;
        let file = match fs::OpenOptions::new()
            .access_mode(0x0002_0000)
            .open(r"\\.\NUL")
        {
            Ok(file) => file,
            Err(error) => return format!("not opened: {error}"),
        };
        let what = OWNER_SECURITY_INFORMATION
            | GROUP_SECURITY_INFORMATION
            | DACL_SECURITY_INFORMATION
            | LABEL_SECURITY_INFORMATION;
        // SAFETY: the handle is open; buffers are freed with LocalFree.
        unsafe {
            let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
            let error = GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                what,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut descriptor,
            );
            if error != 0 {
                return format!("GetSecurityInfo: {error}");
            }
            let mut text: PWSTR = ptr::null_mut();
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor,
                SDDL_REVISION_1,
                what,
                &mut text,
                ptr::null_mut(),
            );
            let mut len = 0;
            while !text.is_null() && *text.add(len) != 0 {
                len += 1;
            }
            let value = if text.is_null() {
                "?".to_owned()
            } else {
                String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
            };
            LocalFree(text.cast());
            LocalFree(descriptor);
            value
        }
    }

    // -------------------------------------------------------------- helper

    /// The probe run again, inside the sandbox under test.
    fn helper(args: &[String]) {
        match args.first().map(String::as_str) {
            Some("noop") => println!("helper ran"),
            Some("cred-read") => println!("{}", cred_read()),
            Some("open-nul") => {
                let open = |read: bool, write: bool| {
                    match fs::OpenOptions::new().read(read).write(write).open("NUL") {
                        Ok(_) => "ok".to_owned(),
                        Err(error) => format!("{:?}", error.raw_os_error()),
                    }
                };
                println!("NUL: read+write {}, read {}", open(true, true), open(true, false));
            }
            Some("open-process") => {
                let pid: u32 = args[1].parse().unwrap();
                for (name, access) in [
                    ("query", PROCESS_QUERY_LIMITED_INFORMATION),
                    ("read-memory", PROCESS_VM_READ),
                ] {
                    // SAFETY: a plain call; a handle returned is closed.
                    let result = unsafe {
                        let handle = OpenProcess(access, 0, pid);
                        if handle.is_null() {
                            format!("error {}", GetLastError())
                        } else {
                            CloseHandle(handle);
                            "OPENED".to_owned()
                        }
                    };
                    println!("{name}: {result}");
                }
            }
            other => println!("unknown helper {other:?}"),
        }
    }

    // ---------------------------------------------------------------- main

    /// Round 4: two restricted tokens, the logon SID among the restricting
    /// SIDs, at medium and at low integrity, against the same questions.
    pub fn main() {
        let args: Vec<String> = std::env::args().collect();
        if args.get(1).map(String::as_str) == Some("helper") {
            helper(&args[2..]);
            return;
        }
        let root = system_root();
        let exe = std::env::current_exe().unwrap();
        let exe_text = exe.display().to_string();
        let pid = std::process::id();
        println!("== OWL-72 probe, round 4 ==");
        let quoted = |path: &Path| format!("\"{}\"", path.display());
        let tree = SUB_CONTAINERS_AND_OBJECTS_INHERIT;
        let base = std::env::temp_dir().join(format!("owl72d-{pid}"));
        let home = base.join("home");
        fs::create_dir_all(home.join(r".config\gh")).unwrap();
        fs::create_dir_all(home.join("other-project")).unwrap();
        let token_file = home.join(r".config\gh\hosts.yml");
        let sibling_env = home.join(r"other-project\.env");
        fs::write(&token_file, format!("github.com:\n    oauth_token: gho_{MARKER}\n")).unwrap();
        fs::write(&sibling_env, format!("API_KEY={MARKER}\n")).unwrap();
        let identity = "-c user.name=owl72 -c user.email=owl72@example.invalid";

        for (index, (name, logon, low)) in [("logon", true, false), ("logon-low", true, true)]
            .into_iter()
            .enumerate()
        {
            let run = string_sid(&format!("S-1-9-1-{pid}-{index}"));
            let granted = base.join(format!("granted-{name}"));
            let repo = base.join(format!("repo-{name}"));
            fs::create_dir_all(&granted).unwrap();
            fs::create_dir_all(&repo).unwrap();
            fs::write(repo.join("README.md"), "hello\n").unwrap();
            bare(GIT, "init -q", &repo);
            bare(GIT, &format!("{identity} add README.md"), &repo);
            bare(GIT, &format!("{identity} commit -q -m seed"), &repo);
            for dir in [&granted, &repo] {
                let (error, _) = set_acl(dir, run, MODIFY, GRANT_ACCESS, tree);
                assert_eq!(error, 0, "grant on {}", dir.display());
                if low {
                    let (error, _) = set_label(dir, Some("S:(ML;OICI;NW;;;LW)"));
                    assert_eq!(error, 0, "label on {}", dir.display());
                }
            }
            let token = match restricted_token(run, logon, low) {
                Ok(token) => token,
                Err(error) => {
                    check(&format!("{name}.token"), error);
                    continue;
                }
            };
            let c = |what: &str, result: String| check(&format!("{name}.{what}"), result);
            c("helper", restricted(token, &exe_text, "helper noop", &root));
            for (what, path) in [("gh-token", &token_file), ("sibling-env", &sibling_env)] {
                c(
                    what,
                    restricted(token, &cmd(), &format!("/d /c type {}", quoted(path)), &root),
                );
            }
            let profile_dir = PathBuf::from(std::env::var_os("USERPROFILE").unwrap());
            c(
                "list-profile",
                restricted(token, &cmd(), &format!("/d /c dir /b {}", quoted(&profile_dir)), &root),
            );
            for (what, dir) in [
                ("programdata", PathBuf::from(r"C:\ProgramData")),
                ("workspace", std::env::current_dir().unwrap()),
                ("granted", granted.clone()),
            ] {
                let target = dir.join(format!("owl72d-{pid}-{name}.txt"));
                restricted(token, &cmd(), &format!("/d /c echo x> {}", quoted(&target)), &root);
                c(&format!("write-{what}-landed"), target.exists().to_string());
                let _ = fs::remove_file(&target);
            }
            c("nul", restricted(token, &exe_text, "helper open-nul", &root));
            c("git-version", restricted(token, GIT, "--version", &root));
            c("git-status", restricted(token, GIT, "status --porcelain", &repo));
            fs::write(repo.join("b.txt"), "b\n").unwrap();
            c("git-add", restricted(token, GIT, "add b.txt", &repo));
            c(
                "git-commit",
                restricted(token, GIT, &format!("{identity} commit -q -m confined"), &repo),
            );
            c("git-log-bare", bare(GIT, "log --oneline", &repo));
            c("net", restricted(token, &curl(), &curl_line("https://example.com/"), &root));
            let (port, answered) = host_listener();
            let result = restricted(
                token,
                &curl(),
                &curl_line(&format!("http://127.0.0.1:{port}/")),
                &root,
            );
            let _ = std::net::TcpStream::connect(("127.0.0.1", port));
            c(
                "loopback-to-host",
                format!("{result}; listener answered: {}", answered.join().unwrap()),
            );
            let written = cred_write(CRED_PERSIST_LOCAL_MACHINE);
            c(
                "cred-read",
                format!(
                    "write error {written}; {}",
                    restricted(token, &exe_text, "helper cred-read", &root)
                ),
            );
            cred_delete();
            c(
                "open-probe-process",
                restricted(token, &exe_text, &format!("helper open-process {pid}"), &root),
            );
            for dir in [&granted, &repo] {
                set_acl(dir, run, 0, REVOKE_ACCESS, NO_INHERITANCE);
            }
        }
        check("cleanup.base-removed", fs::remove_dir_all(&base).is_ok());
        println!("== done ==");
    }
}
