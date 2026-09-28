//! Confining a program at the operating-system level (OWL-41): what an agent
//! run and the project's gate commands may read and write, enforced by the
//! system rather than asked of a model.
//!
//! [`wrap`] turns a program and its arguments into a [`Command`] that runs
//! them inside a sandbox built from a [`Policy`]:
//!
//! - macOS: `/usr/bin/sandbox-exec` with a Seatbelt profile given inline
//!   (`-p`), every path passed as a `-D` parameter, so no path is quoted into
//!   the profile and no profile file exists for anything to rewrite.
//! - Linux: `bwrap` (bubblewrap), found in the system folders only, never on
//!   the agent's `PATH`, with a private `/tmp`, an empty `/run/user`, its own
//!   PID namespace and `/proc`, and the policy's folders bound back in.
//! - Anything else, native Windows included: [`SandboxError::Unsupported`].
//!   Agent runs there are refused; WSL2 runs the Linux sandbox (decision D9).
//!
//! The policy is the same on both systems. The home is neither read nor
//! written, but for the folders the policy names; nothing is written outside
//! the writable folders, the temporary folders and the devices; the system
//! credential store is closed (the Keychain on macOS, the Secret Service's
//! socket under `/run/user` on Linux). Each rule rests on a live check
//! recorded in the build plan (results, "OWL-41").
//!
//! The network is left open: the harness needs it. Other limits are listed
//! in the build plan and in architecture section 8.

use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::time::Duration;

/// What a confined program may reach. Paths may be given as they are: [`wrap`]
/// resolves each to its real path, links included, as the systems compare
/// them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Policy {
    /// The home, neither read nor written but for what follows; `None` when
    /// there is none to protect.
    pub home: Option<PathBuf>,
    /// Read, never written: tool chains, the harness's install folder, git's
    /// own configuration.
    pub readable: Vec<PathBuf>,
    /// Read and written: the worktree, the repository's git folder, the
    /// harness's login folder.
    pub writable: Vec<PathBuf>,
    /// Never written, even inside a writable folder: the git folder's hooks
    /// and configuration.
    pub protected: Vec<PathBuf>,
    /// Neither read nor written, wherever they are: secret files and the
    /// runner's own run folder.
    pub hidden: Vec<PathBuf>,
    /// Where temporary files go, written. On Linux `/tmp` is private to the
    /// run, and a folder named here inside it is created empty.
    pub temp: Vec<PathBuf>,
    /// The working directory of the confined program.
    pub workdir: PathBuf,
}

/// Why a program cannot be confined.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SandboxError {
    /// This system has no sandbox Owlshift can use.
    Unsupported,
    /// The sandbox program is not installed.
    Missing { program: &'static str },
    /// The sandbox program is installed but cannot confine anything here,
    /// such as bwrap without user namespaces.
    Blocked {
        program: &'static str,
        reason: String,
    },
    /// A path that cannot be given to the sandbox as is.
    Path(PathBuf),
}

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => f.write_str(
                "agent runs are confined with sandbox-exec on macOS and bwrap on Linux; \
                 native Windows has no confinement yet, so Owlshift refuses to run an agent \
                 there: run Owlshift under WSL2",
            ),
            Self::Missing { program } => write!(
                f,
                "{program} is not installed, and agent runs are not started without it: \
                 install bubblewrap (for instance `sudo apt install bubblewrap`)"
            ),
            Self::Blocked { program, reason } => write!(
                f,
                "{program} cannot confine an agent run here ({reason}). On Ubuntu 23.10 \
                 and later, AppArmor keeps unprivileged programs from creating user \
                 namespaces: allow them for bwrap alone by saving this profile, as root, \
                 to /etc/apparmor.d/bwrap and loading it with \
                 `sudo apparmor_parser -r /etc/apparmor.d/bwrap`:\n{BWRAP_APPARMOR_PROFILE}"
            ),
            Self::Path(path) => write!(
                f,
                "{} cannot be given to the sandbox: it is not valid UTF-8",
                path.display()
            ),
        }
    }
}

impl Error for SandboxError {}

/// The AppArmor profile that lets bwrap, and bwrap alone, create user
/// namespaces on a system that restricts them (Ubuntu 23.10 and later).
/// Checked live on 2026-09-28 on `ubuntu-latest` (build plan, "OWL-41").
pub const BWRAP_APPARMOR_PROFILE: &str = "abi <abi/4.0>,\n\
include <tunables/global>\n\
profile bwrap /usr/bin/bwrap flags=(unconfined) {\n  userns,\n  include if exists <local/bwrap>\n}\n";

/// Where the sandbox programs are looked for: system folders only, so a
/// program an agent could plant earlier on its `PATH` is never taken.
#[cfg(target_os = "linux")]
const SYSTEM_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin";

/// How long [`available`] waits for its trial run.
#[cfg(any(target_os = "macos", target_os = "linux"))]
const TRIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// Returns a command that runs `program` with `args` inside the sandbox
/// `policy` describes, in its working directory. The caller then sets the
/// command's environment and standard streams: none are set here.
///
/// The arguments reach the program as they are, after a `--`, whatever they
/// hold. This only builds the command; [`available`] tells whether it can
/// run here.
pub fn wrap<I, S>(policy: &Policy, program: &OsStr, args: I) -> Result<Command, SandboxError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    imp::wrap(policy, program, args)
}

/// Whether programs can be confined here: the sandbox program is found and a
/// trial run under it succeeds. The error says what to fix.
pub fn available() -> Result<(), SandboxError> {
    imp::available()
}

/// `path` as the system resolves it: its longest existing part made real,
/// links included, and the rest appended. Seatbelt matches real paths
/// (`/tmp` is `/private/tmp` on macOS), and bwrap binds real ones.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux")),
    allow(dead_code, reason = "no sandbox to build on this system")
)]
fn real(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut rest: Vec<OsString> = Vec::new();
    loop {
        if let Ok(resolved) = fs::canonicalize(&existing) {
            return rest.iter().rev().fold(resolved, |acc, part| acc.join(part));
        }
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name.to_owned());
                existing = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// The policy with every path made real, the order of each list kept.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux")),
    allow(dead_code, reason = "no sandbox to build on this system")
)]
fn resolved(policy: &Policy) -> Policy {
    let all = |paths: &[PathBuf]| paths.iter().map(|path| real(path)).collect();
    Policy {
        home: policy.home.as_deref().map(real),
        readable: all(&policy.readable),
        writable: all(&policy.writable),
        protected: all(&policy.protected),
        hidden: all(&policy.hidden),
        temp: all(&policy.temp),
        workdir: real(&policy.workdir),
    }
}

/// The Seatbelt profile of a resolved policy, and the parameters its rules
/// name, as `(name, path)`: the profile quotes no path.
///
/// The last matching rule wins, so the rules go from the widest to the
/// narrowest: everything allowed, then no write anywhere but the devices and
/// the temporary folders, then the home closed, then the readable and
/// writable folders opened again, then the protected and hidden paths and
/// the Keychain closed whatever came before.
pub fn seatbelt_profile(policy: &Policy) -> Result<(String, Vec<(String, String)>), SandboxError> {
    let mut params = Vec::new();
    let mut param = |prefix: &str, path: &Path| -> Result<String, SandboxError> {
        let value = path
            .to_str()
            .ok_or_else(|| SandboxError::Path(path.to_owned()))?;
        let name = format!("{prefix}{}", params.len());
        params.push((name.clone(), value.to_owned()));
        Ok(format!("(subpath (param \"{name}\"))"))
    };
    let mut profile = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
    profile.push_str("(allow file-write* (subpath \"/dev\"))\n");
    for path in &policy.temp {
        profile.push_str(&format!("(allow file-write* {})\n", param("T", path)?));
    }
    if let Some(home) = &policy.home {
        profile.push_str(&format!(
            "(deny file-read-data file-read-xattr file-write* {})\n",
            param("HOME", home)?
        ));
    }
    for path in &policy.readable {
        profile.push_str(&format!(
            "(allow file-read-data file-read-xattr {})\n",
            param("R", path)?
        ));
    }
    for path in &policy.writable {
        profile.push_str(&format!(
            "(allow file-read* file-write* {})\n",
            param("W", path)?
        ));
    }
    for path in &policy.protected {
        profile.push_str(&format!("(deny file-write* {})\n", param("P", path)?));
    }
    for path in &policy.hidden {
        profile.push_str(&format!(
            "(deny file-read* file-write* {})\n",
            param("H", path)?
        ));
    }
    profile.push_str(
        "(deny mach-lookup (global-name \"com.apple.SecurityServer\") \
         (global-name \"com.apple.securityd.xpc\"))\n",
    );
    Ok((profile, params))
}

/// What a path is on the host, as the bwrap arguments need to know.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Directory,
    File,
}

/// The bwrap options of a resolved policy, up to and including `--chdir`:
/// the caller appends `--`, the program and its arguments. `kind` tells what
/// a path is on the host, `None` when it does not exist.
///
/// Mounts are ordered from the shallowest path to the deepest, so a folder
/// bound inside another, or a path hidden inside a bound folder, lands on
/// top of it; at the same depth, the private folders come first, then the
/// read-only binds, the writable ones, the protected paths and the hidden
/// ones.
pub fn bwrap_args(policy: &Policy, kind: impl Fn(&Path) -> Option<Kind>) -> Vec<OsString> {
    // Rank orders mounts of the same depth.
    let mut mounts: Vec<(PathBuf, u8, Vec<OsString>)> = Vec::new();
    let mut add = |path: &Path, rank: u8, args: Vec<OsString>| {
        mounts.push((path.to_owned(), rank, args));
    };
    let os = |text: &str| OsString::from(text);
    let tmp = Path::new("/tmp");
    let run_user = Path::new("/run/user");
    add(tmp, 0, vec![os("--tmpfs"), tmp.into()]);
    if kind(run_user) == Some(Kind::Directory) {
        add(run_user, 0, vec![os("--tmpfs"), run_user.into()]);
    }
    if let Some(home) = &policy.home
        && kind(home) == Some(Kind::Directory)
    {
        add(home, 0, vec![os("--tmpfs"), home.into()]);
    }
    for path in &policy.temp {
        if path.starts_with(tmp) {
            add(path, 1, vec![os("--dir"), path.into()]);
        } else {
            add(path, 3, vec![os("--bind-try"), path.into(), path.into()]);
        }
    }
    for path in &policy.readable {
        add(path, 2, vec![os("--ro-bind-try"), path.into(), path.into()]);
    }
    for path in &policy.writable {
        add(path, 3, vec![os("--bind-try"), path.into(), path.into()]);
    }
    for path in &policy.protected {
        add(path, 4, vec![os("--ro-bind-try"), path.into(), path.into()]);
    }
    let bound = |path: &Path| {
        policy
            .readable
            .iter()
            .chain(&policy.writable)
            .any(|root| path.starts_with(root))
    };
    let private = |path: &Path| {
        path.starts_with(tmp)
            || path.starts_with(run_user)
            || policy
                .home
                .as_ref()
                .is_some_and(|home| path.starts_with(home))
    };
    for path in &policy.hidden {
        // A hidden path under a private folder is already out of sight,
        // unless a bound folder brought it back.
        if private(path) && !bound(path) {
            continue;
        }
        match kind(path) {
            Some(Kind::Directory) => add(path, 5, vec![os("--tmpfs"), path.into()]),
            Some(Kind::File) => add(path, 5, vec![os("--ro-bind"), os("/dev/null"), path.into()]),
            None => {}
        }
    }
    let depth = |path: &Path| {
        path.components()
            .filter(|c| matches!(c, Component::Normal(_)))
            .count()
    };
    mounts.sort_by_key(|(path, rank, _)| (depth(path), *rank));

    let mut args: Vec<OsString> = [
        "--die-with-parent",
        "--unshare-pid",
        "--ro-bind",
        "/",
        "/",
        "--dev",
        "/dev",
        "--proc",
        "/proc",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    args.extend(mounts.into_iter().flat_map(|(_, _, args)| args));
    args.push(os("--chdir"));
    args.push(policy.workdir.clone().into_os_string());
    args
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::OsStr;
    use std::path::Path;
    use std::process::Command;

    use super::{Policy, SandboxError, TRIAL_TIMEOUT, resolved, seatbelt_profile};

    const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

    pub(super) fn wrap<I, S>(
        policy: &Policy,
        program: &OsStr,
        args: I,
    ) -> Result<Command, SandboxError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut policy = resolved(policy);
        // The Keychain, whatever the policy says: its files, the user's and
        // the system's.
        if let Some(home) = &policy.home {
            policy.hidden.push(home.join("Library/Keychains"));
        }
        policy.hidden.push("/Library/Keychains".into());
        let (profile, params) = seatbelt_profile(&policy)?;
        let mut command = Command::new(SANDBOX_EXEC);
        for (name, value) in params {
            command.arg("-D").arg(format!("{name}={value}"));
        }
        command
            .arg("-p")
            .arg(profile)
            .arg("--")
            .arg(program)
            .args(args);
        command.current_dir(&policy.workdir);
        Ok(command)
    }

    pub(super) fn available() -> Result<(), SandboxError> {
        let program = Path::new(SANDBOX_EXEC);
        if !program.is_file() {
            return Err(SandboxError::Missing {
                program: "sandbox-exec",
            });
        }
        let trial = crate::process::run(
            program,
            &["-p", "(version 1)(allow default)", "--", "/usr/bin/true"],
            None,
            TRIAL_TIMEOUT,
        );
        match trial {
            Ok(out) if out.code == Some(0) => Ok(()),
            Ok(out) => Err(SandboxError::Blocked {
                program: "sandbox-exec",
                reason: first_line(&out.stderr, out.code),
            }),
            Err(error) => Err(SandboxError::Blocked {
                program: "sandbox-exec",
                reason: error.to_string(),
            }),
        }
    }

    fn first_line(stderr: &[u8], code: Option<i32>) -> String {
        let text = String::from_utf8_lossy(stderr);
        match text.lines().next() {
            Some(line) if !line.trim().is_empty() => line.trim().to_owned(),
            _ => format!("the trial run exited with status {code:?}"),
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::ffi::OsStr;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::{Kind, Policy, SYSTEM_PATH, SandboxError, TRIAL_TIMEOUT, bwrap_args, resolved};

    fn bwrap() -> Option<PathBuf> {
        crate::process::find_executable_in("bwrap", SYSTEM_PATH)
    }

    fn kind(path: &Path) -> Option<Kind> {
        match fs::metadata(path) {
            Ok(meta) if meta.is_dir() => Some(Kind::Directory),
            Ok(_) => Some(Kind::File),
            Err(_) => None,
        }
    }

    pub(super) fn wrap<I, S>(
        policy: &Policy,
        program: &OsStr,
        args: I,
    ) -> Result<Command, SandboxError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let program_path = bwrap().ok_or(SandboxError::Missing { program: "bwrap" })?;
        let policy = resolved(policy);
        let mut command = Command::new(program_path);
        command
            .args(bwrap_args(&policy, kind))
            .arg("--")
            .arg(program)
            .args(args);
        command.current_dir(&policy.workdir);
        Ok(command)
    }

    pub(super) fn available() -> Result<(), SandboxError> {
        let program = bwrap().ok_or(SandboxError::Missing { program: "bwrap" })?;
        let trial = crate::process::run(
            &program,
            &[
                "--die-with-parent",
                "--unshare-pid",
                "--ro-bind",
                "/",
                "/",
                "--dev",
                "/dev",
                "--proc",
                "/proc",
                "--",
                "true",
            ],
            None,
            TRIAL_TIMEOUT,
        );
        match trial {
            Ok(out) if out.code == Some(0) => Ok(()),
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stderr);
                let reason = text
                    .lines()
                    .next()
                    .filter(|line| !line.trim().is_empty())
                    .map_or_else(
                        || format!("the trial run exited with status {:?}", out.code),
                        |line| line.trim().to_owned(),
                    );
                Err(SandboxError::Blocked {
                    program: "bwrap",
                    reason,
                })
            }
            Err(error) => Err(SandboxError::Blocked {
                program: "bwrap",
                reason: error.to_string(),
            }),
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use std::ffi::OsStr;
    use std::process::Command;

    use super::{Policy, SandboxError};

    pub(super) fn wrap<I, S>(
        _policy: &Policy,
        _program: &OsStr,
        _args: I,
    ) -> Result<Command, SandboxError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Err(SandboxError::Unsupported)
    }

    pub(super) fn available() -> Result<(), SandboxError> {
        Err(SandboxError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        Policy {
            home: Some("/home/op".into()),
            readable: vec!["/home/op/.rustup".into()],
            writable: vec!["/home/op/wt".into(), "/srv/repo/.git".into()],
            protected: vec!["/srv/repo/.git/hooks".into()],
            hidden: vec![
                "/home/op/.ssh".into(),
                "/home/op/wt/.env".into(),
                "/srv/run".into(),
            ],
            temp: vec!["/tmp/agent".into()],
            workdir: "/home/op/wt".into(),
        }
    }

    #[test]
    fn the_seatbelt_profile_closes_the_home_the_writes_and_the_keychain() {
        let (profile, params) = seatbelt_profile(&policy()).unwrap();
        assert_eq!(
            profile,
            "(version 1)\n(allow default)\n(deny file-write*)\n\
             (allow file-write* (subpath \"/dev\"))\n\
             (allow file-write* (subpath (param \"T0\")))\n\
             (deny file-read-data file-read-xattr file-write* (subpath (param \"HOME1\")))\n\
             (allow file-read-data file-read-xattr (subpath (param \"R2\")))\n\
             (allow file-read* file-write* (subpath (param \"W3\")))\n\
             (allow file-read* file-write* (subpath (param \"W4\")))\n\
             (deny file-write* (subpath (param \"P5\")))\n\
             (deny file-read* file-write* (subpath (param \"H6\")))\n\
             (deny file-read* file-write* (subpath (param \"H7\")))\n\
             (deny file-read* file-write* (subpath (param \"H8\")))\n\
             (deny mach-lookup (global-name \"com.apple.SecurityServer\") \
             (global-name \"com.apple.securityd.xpc\"))\n"
        );
        let values: Vec<&str> = params.iter().map(|(_, value)| value.as_str()).collect();
        assert_eq!(
            values,
            [
                "/tmp/agent",
                "/home/op",
                "/home/op/.rustup",
                "/home/op/wt",
                "/srv/repo/.git",
                "/srv/repo/.git/hooks",
                "/home/op/.ssh",
                "/home/op/wt/.env",
                "/srv/run",
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_path_that_is_not_utf8_is_refused_rather_than_mangled() {
        use std::os::unix::ffi::OsStringExt;
        let mut policy = policy();
        let odd = PathBuf::from(OsString::from_vec(b"/home/op/\xff".to_vec()));
        policy.readable.push(odd.clone());
        assert_eq!(seatbelt_profile(&policy), Err(SandboxError::Path(odd)));
    }

    #[test]
    fn the_bwrap_arguments_bind_the_policy_in_order() {
        let kind = |path: &Path| match path.to_str() {
            Some("/run/user" | "/home/op" | "/home/op/.ssh" | "/srv/run") => Some(Kind::Directory),
            Some("/home/op/wt/.env") => Some(Kind::File),
            _ => None,
        };
        let args: Vec<String> = bwrap_args(&policy(), kind)
            .into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect();
        // `/home/op/.ssh` is not there: the private home already hides it.
        let expected = [
            "--die-with-parent",
            "--unshare-pid",
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--tmpfs",
            "/tmp",
            "--tmpfs",
            "/run/user",
            "--tmpfs",
            "/home/op",
            "--dir",
            "/tmp/agent",
            "--tmpfs",
            "/srv/run",
            "--ro-bind-try",
            "/home/op/.rustup",
            "/home/op/.rustup",
            "--bind-try",
            "/home/op/wt",
            "/home/op/wt",
            "--bind-try",
            "/srv/repo/.git",
            "/srv/repo/.git",
            "--ro-bind-try",
            "/srv/repo/.git/hooks",
            "/srv/repo/.git/hooks",
            "--ro-bind",
            "/dev/null",
            "/home/op/wt/.env",
            "--chdir",
            "/home/op/wt",
        ];
        assert_eq!(args, expected);
    }

    #[test]
    fn real_paths_keep_the_part_that_does_not_exist_yet() {
        let base = tempfile::tempdir().unwrap();
        let real_base = fs::canonicalize(base.path()).unwrap();
        assert_eq!(
            real(&base.path().join("missing/deeper")),
            real_base.join("missing/deeper")
        );
        assert_eq!(real(base.path()), real_base);
    }

    #[test]
    fn an_unsupported_system_names_wsl2() {
        assert!(SandboxError::Unsupported.to_string().contains("WSL2"));
        let blocked = SandboxError::Blocked {
            program: "bwrap",
            reason: "setting up uid map: Permission denied".into(),
        }
        .to_string();
        assert!(
            blocked.contains("apparmor_parser -r /etc/apparmor.d/bwrap"),
            "{blocked}"
        );
        assert!(blocked.contains("userns,"), "{blocked}");
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    #[test]
    fn native_windows_refuses_to_confine() {
        assert_eq!(available(), Err(SandboxError::Unsupported));
        let error = wrap(&policy(), OsStr::new("cmd"), ["/c", "echo"]).unwrap_err();
        assert!(error.to_string().contains("WSL2"), "{error}");
    }
}
