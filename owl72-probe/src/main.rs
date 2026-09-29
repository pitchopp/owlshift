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
        // SAFETY: a handle this process owns.
        unsafe { SetHandleInformation(writer.as_raw_handle(), HANDLE_FLAG_INHERIT, 1) };
        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
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

    /// Sets a low integrity label, inherited by what is inside
    /// (`low = true`), or removes the explicit label (`low = false`).
    fn set_label(path: &Path, low: bool) -> (u32, Duration) {
        let path_w = wide(path);
        let started = Instant::now();
        // SAFETY: as in `set_acl`; the empty ACL lives in a local buffer.
        unsafe {
            let mut empty = [0u64; 8];
            let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
            let sacl: *mut ACL = if low {
                let sddl = wide("S:(ML;OICI;NW;;;LW)");
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

    // -------------------------------------------------------------- helper

    /// The probe run again, as a program inside the container.
    fn helper(args: &[String]) {
        match args.first().map(String::as_str) {
            Some("noop") => println!("helper ran"),
            Some("cred-read") => println!("{}", cred_read()),
            Some("listen") => {
                let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
                let port = listener.local_addr().unwrap().port();
                fs::write(Path::new(&args[1]).join("port.txt"), port.to_string())
                    .expect("port file");
                listener.set_nonblocking(true).unwrap();
                println!("accepted: {}", answer_one(&listener, Duration::from_secs(25)));
            }
            Some("spawn") => {
                // args: container name, capability SIDs (may be none)
                let sid = derived_sid(&args[1]);
                let caps: Vec<PSID> = args[2..].iter().map(|text| string_sid(text)).collect();
                println!(
                    "{}",
                    confined(sid, &caps, &cmd(), "/d /c exit 7", &system_root())
                );
            }
            other => println!("unknown helper {other:?}"),
        }
    }

    // ---------------------------------------------------------------- main

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
        println!("== OWL-72 AppContainer probe ==");
        check("os", bare(&cmd(), "/d /c ver", &root));
        check("probe", &exe_text);

        // The probe in a job of its own, as the runner puts the launcher.
        // SAFETY: a new job; our own process handle.
        let job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        let assigned = unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) };
        check("job.probe-assigned", assigned != 0);

        let (sid, how) = profile_sid(PROFILE);
        check("profile", format!("{how}, {}", sid_text(sid)));
        let internet = string_sid(INTERNET_CLIENT);
        let internet_server = string_sid(INTERNET_CLIENT_SERVER);
        let private = string_sid(PRIVATE_NETWORK);
        let net = [internet, private];

        // Fake secrets and folders, in the user's temporary folder.
        let base = std::env::temp_dir().join(format!("owl72-{}", std::process::id()));
        let home = base.join("home");
        fs::create_dir_all(home.join(r".config\gh")).unwrap();
        fs::create_dir_all(home.join("other-project")).unwrap();
        let token = home.join(r".config\gh\hosts.yml");
        let sibling_env = home.join(r"other-project\.env");
        fs::write(&token, format!("github.com:\n    oauth_token: gho_{MARKER}\n")).unwrap();
        fs::write(&sibling_env, format!("API_KEY={MARKER}\n")).unwrap();
        let outside = std::env::current_dir().unwrap().join("owl72-outside.txt");
        fs::write(&outside, format!("{MARKER}\n")).unwrap();
        check("paths", format!("base {}, outside {}", base.display(), outside.display()));

        // --- 1. What a container reaches by default.
        let sys_file = root.join(r"System32\drivers\etc\hosts");
        check(
            "default.system-file",
            confined(sid, &[], &cmd(), &format!("/d /c type \"{}\"", sys_file.display()), &root),
        );
        let quoted = |path: &Path| format!("\"{}\"", path.display());
        for (name, path) in [("gh-token", &token), ("sibling-env", &sibling_env)] {
            let line = format!("/d /c type {}", quoted(path));
            check(&format!("default.{name}.bare"), bare(&cmd(), &line, &root));
            check(&format!("default.{name}.confined"), confined(sid, &[], &cmd(), &line, &root));
        }
        check(
            "default.gh-token.grandchild",
            confined(
                sid,
                &[],
                &cmd(),
                &format!("/d /c \"{}\" /d /c type {}", cmd(), quoted(&token)),
                &root,
            ),
        );
        let profile_dir = PathBuf::from(std::env::var_os("USERPROFILE").unwrap());
        check(
            "default.list-profile",
            confined(sid, &[], &cmd(), &format!("/d /c dir /b {}", quoted(&profile_dir)), &root),
        );
        if let Some(appdata) = std::env::var_os("APPDATA") {
            check(
                "default.list-appdata",
                confined(
                    sid,
                    &[],
                    &cmd(),
                    &format!("/d /c dir /b {}", quoted(Path::new(&appdata))),
                    &root,
                ),
            );
        }
        check(
            "default.outside-profile-file",
            confined(sid, &[], &cmd(), &format!("/d /c type {}", quoted(&outside)), &root),
        );
        check("default.git-version", confined(sid, &[], GIT, "--version", &root));
        check(
            "default.program-in-workspace",
            confined(sid, &[], &exe_text, "helper noop", &root),
        );

        // A job's process starts its children in the job, a container's too.
        match spawn_in(sid, &[], &cmd(), "/d /c ping -n 3 127.0.0.1 > NUL", &root) {
            Ok(child) => {
                let mut in_job = 0;
                // SAFETY: both handles are open.
                unsafe { IsProcessInJob(child.process, job, &mut in_job) };
                check("job.container-child-in-job", in_job != 0);
                child.wait(Duration::from_secs(20));
            }
            Err(error) => check("job.container-child-in-job", format!("not created: {error}")),
        }

        // A container with no profile.
        let bare_sid = derived_sid(&format!("owlshift.owl72.noprofile.{}", std::process::id()));
        check(
            "profile.not-created",
            confined(
                bare_sid,
                &[],
                &cmd(),
                &format!("/d /c type \"{}\" > NUL && echo ran", sys_file.display()),
                &root,
            ),
        );

        // --- 2. Grants: reading a program's folder, writing with and
        // without a low integrity label.
        let (error, took) = set_acl(
            &exe_dir,
            sid,
            READ_EXECUTE,
            GRANT_ACCESS,
            SUB_CONTAINERS_AND_OBJECTS_INHERIT,
        );
        check("grant.program-folder", format!("error {error}, {took:?}"));
        check(
            "grant.program-in-workspace",
            confined(sid, &[], &exe_text, "helper noop", &root),
        );

        let no_label = base.join("w-nolabel");
        let low = base.join("w-low");
        for dir in [&no_label, &low] {
            fs::create_dir_all(dir.join("hooks")).unwrap();
            fs::write(dir.join("existing.txt"), "existing\n").unwrap();
            fs::write(dir.join(r"hooks\sample"), "sample\n").unwrap();
            fs::write(dir.join("secret.env"), format!("API_KEY={MARKER}\n")).unwrap();
            let (error, _) =
                set_acl(dir, sid, MODIFY, GRANT_ACCESS, SUB_CONTAINERS_AND_OBJECTS_INHERIT);
            assert_eq!(error, 0, "grant on {}", dir.display());
        }
        let (error, _) = set_label(&low, true);
        check("label.set-low", format!("error {error}"));
        for (name, dir) in [("nolabel", &no_label), ("low", &low)] {
            check(
                &format!("write.{name}.read-existing"),
                confined(sid, &[], &cmd(), "/d /c type existing.txt", dir),
            );
            confined(sid, &[], &cmd(), "/d /c echo new> new.txt", dir);
            check(&format!("write.{name}.create-file"), dir.join("new.txt").exists());
            confined(sid, &[], &cmd(), "/d /c echo more>> existing.txt", dir);
            let appended = fs::read_to_string(dir.join("existing.txt")).unwrap();
            check(&format!("write.{name}.append"), appended.contains("more"));
            confined(sid, &[], &cmd(), "/d /c md sub && echo x> sub\\f.txt", dir);
            check(&format!("write.{name}.mkdir-and-write"), dir.join(r"sub\f.txt").exists());
        }

        // --- 3. Denied paths inside a granted, labelled folder: a secret
        // file, and a protected folder (git's hooks).
        let secret = low.join("secret.env");
        let hooks = low.join("hooks");
        let (error, _) = set_acl(&secret, sid, ALL, DENY_ACCESS, NO_INHERITANCE);
        check("deny.secret-set", format!("error {error}"));
        let (error, _) =
            set_acl(&hooks, sid, ANY_WRITE, DENY_ACCESS, SUB_CONTAINERS_AND_OBJECTS_INHERIT);
        check("deny.hooks-set", format!("error {error}"));
        check(
            "deny.secret-read",
            confined(sid, &[], &cmd(), "/d /c type secret.env", &low),
        );
        confined(sid, &[], &cmd(), "/d /c del /q secret.env", &low);
        check("deny.secret-still-there-after-del", secret.exists());
        confined(sid, &[], &cmd(), "/d /c ren secret.env moved.env", &low);
        check("deny.secret-still-there-after-ren", secret.exists());
        check(
            "deny.hooks-read",
            confined(sid, &[], &cmd(), "/d /c type hooks\\sample", &low),
        );
        confined(sid, &[], &cmd(), "/d /c echo x> hooks\\pre-commit", &low);
        check("deny.hooks-write-landed", hooks.join("pre-commit").exists());
        confined(sid, &[], &cmd(), "/d /c rd /s /q hooks", &low);
        check("deny.hooks-still-there-after-rd", hooks.join("sample").exists());
        confined(sid, &[], &cmd(), "/d /c ren hooks hooks-moved", &low);
        check("deny.hooks-still-there-after-ren", hooks.join("sample").exists());

        // --- 4. Git in a granted, labelled repository, with the profile
        // (git's HOME) closed.
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
        let (error, _) =
            set_acl(&repo, sid, MODIFY, GRANT_ACCESS, SUB_CONTAINERS_AND_OBJECTS_INHERIT);
        let (label_error, _) = set_label(&repo, true);
        check("git.grant", format!("acl error {error}, label error {label_error}"));
        check("git.status", confined(sid, &[], GIT, "status --porcelain", &repo));
        fs::write(repo.join("b.txt"), "b\n").unwrap();
        check("git.add", confined(sid, &[], GIT, "add b.txt", &repo));
        check(
            "git.commit",
            confined(sid, &[], GIT, &format!("{identity} commit -q -m confined"), &repo),
        );
        check("git.log", bare(GIT, "log --oneline", &repo));

        // --- 5. The Credential Manager.
        let mut written = cred_write(CRED_PERSIST_LOCAL_MACHINE);
        let mut persist = "local machine";
        if written != 0 {
            check("cred.write-local-machine", format!("error {written}"));
            written = cred_write(CRED_PERSIST_SESSION);
            persist = "session";
        }
        check("cred.write", format!("error {written} ({persist})"));
        check("cred.read-in-probe", cred_read());
        check("cred.read-bare-helper", bare(&exe_text, "helper cred-read", &root));
        check(
            "cred.read-confined",
            confined(sid, &[], &exe_text, "helper cred-read", &root),
        );
        check(
            "cred.read-confined-with-network-caps",
            confined(sid, &net, &exe_text, "helper cred-read", &root),
        );
        let cmdkey = root.join(r"System32\cmdkey.exe").display().to_string();
        let list = format!("/list:{CRED_TARGET}");
        check("cred.cmdkey-bare", bare(&cmdkey, &list, &root));
        check("cred.cmdkey-confined", confined(sid, &[], &cmdkey, &list, &root));
        check("cred.delete", format!("error {}", cred_delete()));

        // --- 6. The network.
        let url = "https://example.com/";
        check("net.bare", bare(&curl(), &curl_line(url), &root));
        check("net.no-capability", confined(sid, &[], &curl(), &curl_line(url), &root));
        check(
            "net.internetClient",
            confined(sid, &[internet], &curl(), &curl_line(url), &root),
        );
        check(
            "net.internetClient+privateNetwork",
            confined(sid, &net, &curl(), &curl_line(url), &root),
        );
        for (name, caps) in [
            ("loopback-to-host.no-capability", &[][..]),
            ("loopback-to-host.internet+private", &net[..]),
            (
                "loopback-to-host.internet+server+private",
                &[internet, internet_server, private][..],
            ),
        ] {
            let (port, answered) = host_listener();
            let result = confined(
                sid,
                caps,
                &curl(),
                &curl_line(&format!("http://127.0.0.1:{port}/")),
                &root,
            );
            // Unblock a listener nobody reached.
            let _ = std::net::TcpStream::connect(("127.0.0.1", port));
            let reached = answered.join().unwrap();
            check(&format!("net.{name}"), format!("{result}; listener answered: {reached}"));
        }
        let all_net = [internet, internet_server, private];
        let _ = fs::remove_file(low.join("port.txt"));
        match spawn_in(sid, &all_net, &exe_text, &format!("helper listen {}", quoted(&low)), &low)
        {
            Ok(server) => {
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut port = None;
                while Instant::now() < deadline && port.is_none() {
                    port = fs::read_to_string(low.join("port.txt")).ok();
                    thread::sleep(Duration::from_millis(100));
                }
                match port {
                    Some(port) => {
                        let client = confined(
                            sid,
                            &all_net,
                            &curl(),
                            &curl_line(&format!("http://127.0.0.1:{port}/")),
                            &root,
                        );
                        let from_host = bare(
                            &curl(),
                            &curl_line(&format!("http://127.0.0.1:{port}/")),
                            &root,
                        );
                        check(
                            "net.loopback-same-container",
                            format!("client {client}; host client {from_host}; server {}",
                                server.wait(Duration::from_secs(30))),
                        );
                    }
                    None => check(
                        "net.loopback-same-container",
                        format!("no port; server {}", server.wait(Duration::from_secs(5))),
                    ),
                }
            }
            Err(error) => check("net.loopback-same-container", format!("not created: {error}")),
        }

        // --- 7. A container's process starting one in another container,
        // or with a capability it lacks.
        check(
            "nest.other-container",
            confined(
                sid,
                &[],
                &exe_text,
                &format!("helper spawn owlshift.owl72.other.{}", std::process::id()),
                &root,
            ),
        );
        check(
            "nest.same-container-plus-capability",
            confined(
                sid,
                &[],
                &exe_text,
                &format!("helper spawn {PROFILE} {INTERNET_CLIENT}"),
                &root,
            ),
        );
        check(
            "nest.same-container-same-capabilities",
            confined(sid, &[], &exe_text, &format!("helper spawn {PROFILE}"), &root),
        );

        // --- 8. Undoing: what REVOKE_ACCESS removes, and the profile.
        check("undo.before", format!(
            "w-low {}; secret {}; hooks {}",
            aces_for(&low, sid),
            aces_for(&secret, sid),
            aces_for(&hooks, sid)
        ));
        // SAFETY: a NUL-terminated local.
        let deleted = unsafe { DeleteAppContainerProfile(wide(PROFILE).as_ptr()) };
        check("undo.profile-deleted", format!("hr {deleted:#x}"));
        check("undo.after-profile-delete", aces_for(&low, sid));
        let (error, took) = set_acl(&low, sid, 0, REVOKE_ACCESS, NO_INHERITANCE);
        check("undo.revoke-folder", format!("error {error}, {took:?}"));
        let (error, _) = set_acl(&secret, sid, 0, REVOKE_ACCESS, NO_INHERITANCE);
        check("undo.revoke-secret", format!("error {error}"));
        check("undo.after-revoke", format!(
            "w-low {}; secret {}; hooks {}; existing.txt {}",
            aces_for(&low, sid),
            aces_for(&secret, sid),
            aces_for(&hooks, sid),
            aces_for(&low.join("existing.txt"), sid)
        ));

        // --- 9. How fast a change spreads through a large folder.
        let big = base.join("big");
        let started = Instant::now();
        let (dirs, files) = (100, 200);
        for d in 0..dirs {
            let dir = big.join(format!("d{d:03}"));
            fs::create_dir_all(&dir).unwrap();
            for f in 0..files {
                fs::write(dir.join(format!("f{f:03}.txt")), b"x").unwrap();
            }
        }
        let count = dirs * files;
        check("speed.create", format!("{count} files in {:?}", started.elapsed()));
        let rate = |took: Duration| format!("{took:?} ({:.0} files/s)", count as f64 / took.as_secs_f64());
        let (error, took) =
            set_acl(&big, sid, MODIFY, GRANT_ACCESS, SUB_CONTAINERS_AND_OBJECTS_INHERIT);
        check("speed.grant", format!("error {error}, {}", rate(took)));
        let (error, took) = set_label(&big, true);
        check("speed.label-low", format!("error {error}, {}", rate(took)));
        let (error, took) = set_acl(&big, sid, 0, REVOKE_ACCESS, NO_INHERITANCE);
        check("speed.revoke", format!("error {error}, {}", rate(took)));
        let (error, took) = set_label(&big, false);
        check("speed.label-remove", format!("error {error}, {}", rate(took)));
        check(
            "speed.after",
            format!("big {}; a file {}", aces_for(&big, sid), aces_for(&big.join(r"d000\f000.txt"), sid)),
        );

        // Cleanup: the program folder's grant, the planted files.
        set_acl(&exe_dir, sid, 0, REVOKE_ACCESS, NO_INHERITANCE);
        let _ = fs::remove_file(&outside);
        check("cleanup.base-removed", fs::remove_dir_all(&base).is_ok());
        println!("== done ==");
    }
}
