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
//! - Native Windows: no confinement yet, so [`available`] refuses and agent
//!   runs there are refused; WSL2 runs the Linux sandbox (decision D9). The
//!   launch path is ready (OWL-71): given a launcher, [`wrap`] and
//!   [`wrap_line`] return a command that runs `owlshift-launch`, which the
//!   runner starts inside the run's Job Object and which creates the program
//!   itself with `CreateProcessW`, so the program is in that job too. It
//!   applies nothing of the policy. A shipped build has no launcher until
//!   OWL-72, which finds it next to the running program and adds an
//!   AppContainer; until then only a test sets one
//!   (`use_built_launcher`), and [`wrap`] refuses like [`available`].
//! - Anything else: [`SandboxError::Unsupported`].
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
    /// More folders neither read nor written but for what follows, like the
    /// home: the temporary folders every process of the user shares. The
    /// shared `/tmp` and `/var/tmp` are always closed.
    pub closed: Vec<PathBuf>,
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
    /// Where temporary files go, read and written: the run's own folder, made
    /// by the runner, never a folder the user shares. On Linux `/tmp` is
    /// private to the sandbox, and a folder named here inside it is created
    /// empty there.
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
    /// A program the Windows launcher does not start: a batch file, or a
    /// name that holds a file stream.
    BatchFile(PathBuf),
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
            Self::BatchFile(program) => write!(
                f,
                "{} is a batch file, or names a file stream: the Windows launcher starts \
                 programs itself and does not start these, since cmd.exe would read their \
                 arguments again",
                program.display()
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

/// [`wrap`] for a program that reads its command line its own way, such as
/// `cmd.exe`: `line` follows the program's name verbatim, where [`wrap`]
/// would quote each argument as std's `Command` does. Native Windows only.
#[cfg(windows)]
pub fn wrap_line(policy: &Policy, program: &OsStr, line: &OsStr) -> Result<Command, SandboxError> {
    imp::wrap_line(policy, program, line)
}

/// The launcher's file name, next to the `owlshift` program.
#[cfg(all(windows, any(test, feature = "testkit")))]
const LAUNCHER: &str = "owlshift-launch.exe";

/// The launcher [`wrap`] starts on Windows. A shipped build has none until
/// OWL-72, which looks next to the running program as it lifts the refusal
/// of [`available`].
#[cfg(all(windows, not(any(test, feature = "testkit"))))]
fn launcher() -> Option<PathBuf> {
    None
}

/// The launcher a test set on this thread, if any.
#[cfg(all(windows, any(test, feature = "testkit")))]
fn launcher() -> Option<PathBuf> {
    BUILT_LAUNCHER.with(|launcher| launcher.borrow().clone())
}

#[cfg(all(windows, any(test, feature = "testkit")))]
thread_local! {
    static BUILT_LAUNCHER: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Test only: makes [`wrap`] and [`wrap_line`] start the launcher Cargo
/// built in the workspace's target folder, on this thread, until the guard
/// is dropped. Commands are built on the test's own thread, so a test that
/// sets it never changes what a test running beside it sees. It exists only
/// in test builds and with the `testkit` feature, which no shipped crate
/// enables, and it confines nothing: `available` still refuses.
///
/// # Panics
///
/// When the launcher is not built: `cargo test --workspace` builds it with
/// `owlshift-cli`'s binaries, a test run of one package alone does not.
#[cfg(all(windows, any(test, feature = "testkit")))]
pub fn use_built_launcher() -> LauncherGuard {
    let exe = std::env::current_exe().expect("the test binary has a path");
    let mut dir = exe
        .parent()
        .expect("the test binary is in a folder")
        .to_owned();
    // Cargo puts test binaries in `deps`, and the workspace's binaries one
    // level up.
    if dir.file_name().is_some_and(|name| name == "deps") {
        dir.pop();
    }
    let path = dir.join(LAUNCHER);
    assert!(
        path.is_file(),
        "{} is not built: run `cargo build -p owlshift-cli --bin owlshift-launch` first, \
         or `cargo test --workspace`, which builds it",
        path.display()
    );
    let previous = BUILT_LAUNCHER.with(|launcher| launcher.replace(Some(path)));
    LauncherGuard { previous }
}

/// Restores the launcher that was set before [`use_built_launcher`] when
/// dropped.
#[cfg(all(windows, any(test, feature = "testkit")))]
#[must_use = "the launcher is unset when the guard is dropped"]
pub struct LauncherGuard {
    previous: Option<PathBuf>,
}

#[cfg(all(windows, any(test, feature = "testkit")))]
impl Drop for LauncherGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        BUILT_LAUNCHER.with(|launcher| *launcher.borrow_mut() = previous);
    }
}

/// Whether Windows would run `program` through `cmd.exe`: its file name,
/// once the trailing dots and spaces Windows ignores are dropped, ends in
/// `.bat` or `.cmd` in any letter case, or holds a `:`, which names a file
/// stream (`x.bat::$DATA`). `cmd.exe` reads the arguments of a batch file
/// again, by rules of its own; std's `Command` guards that case with its own
/// escaping, the launcher does not, so it starts neither.
#[cfg_attr(
    not(windows),
    allow(dead_code, reason = "the launcher runs on Windows; tested everywhere")
)]
pub(crate) fn is_batch_file(program: &OsStr) -> bool {
    let program = program.to_string_lossy();
    let mut name = program.rsplit(['/', '\\']).next().unwrap_or_default();
    // A drive-relative name, such as `C:x.bat`, keeps its drive.
    if name.len() == program.len() {
        let mut chars = name.chars();
        if chars.next().is_some_and(|c| c.is_ascii_alphabetic()) && chars.next() == Some(':') {
            name = &name[2..];
        }
    }
    let name = name.trim_end_matches(['.', ' ']).to_ascii_lowercase();
    name.contains(':') || name.ends_with(".bat") || name.ends_with(".cmd")
}

/// The rest of a Windows command line after the program's name: `args`
/// quoted as std's `Command` quotes a regular argument, one space apart,
/// for a program that splits its command line by the Microsoft C runtime's
/// rules. An argument is quoted when it is empty or holds a space or a tab;
/// a quote inside it is escaped with a backslash, and the backslashes
/// before a quote, or before the closing quote, are doubled. The units are
/// UTF-16, as `CreateProcessW` takes them.
#[cfg_attr(
    not(windows),
    allow(dead_code, reason = "the launcher runs on Windows; tested everywhere")
)]
fn command_line<A: AsRef<[u16]>>(args: impl IntoIterator<Item = A>) -> Vec<u16> {
    const QUOTE: u16 = b'"' as u16;
    const BACKSLASH: u16 = b'\\' as u16;
    let mut line = Vec::new();
    for (index, arg) in args.into_iter().enumerate() {
        let arg = arg.as_ref();
        if index > 0 {
            line.push(u16::from(b' '));
        }
        let quote = arg.is_empty()
            || arg
                .iter()
                .any(|&unit| unit == u16::from(b' ') || unit == u16::from(b'\t'));
        if quote {
            line.push(QUOTE);
        }
        let mut backslashes = 0;
        for &unit in arg {
            if unit == BACKSLASH {
                backslashes += 1;
            } else {
                if unit == QUOTE {
                    line.extend(std::iter::repeat_n(BACKSLASH, backslashes + 1));
                }
                backslashes = 0;
            }
            line.push(unit);
        }
        if quote {
            line.extend(std::iter::repeat_n(BACKSLASH, backslashes));
            line.push(QUOTE);
        }
    }
    line
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
        closed: all(&policy.closed),
        readable: all(&policy.readable),
        writable: all(&policy.writable),
        protected: all(&policy.protected),
        hidden: all(&policy.hidden),
        temp: all(&policy.temp),
        workdir: real(&policy.workdir),
    }
}

/// The read operations every read rule of the Seatbelt profile names: a file's
/// content and extended attributes. Its metadata (`stat`) is left readable,
/// but for hidden paths.
const READ: &str = "file-read-data file-read-xattr";

/// The Seatbelt profile of a resolved policy, and the parameters its rules
/// name, as `(name, path)`: the profile quotes no path.
///
/// Among rules that name the same operations, the last one that matches
/// wins, so the rules go from the widest to the narrowest: everything
/// allowed, then no write anywhere but the devices, then the home and the
/// closed folders shut, then the readable, writable and temporary folders
/// opened again, then the protected and hidden paths and the Keychain closed
/// whatever came before.
///
/// Every read rule names [`READ`] rather than the wildcard `file-read*`: an
/// operation named outranks the wildcard whatever the order, so a wildcard
/// allow would not reopen a closed folder, and a wildcard deny would not
/// close a hidden file inside an opened one (both checked live on
/// 2026-09-29). Every write rule names the wildcard `file-write*`.
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
    if let Some(home) = &policy.home {
        let home = param("HOME", home)?;
        profile.push_str(&format!("(deny {READ} file-write* {home})\n"));
    }
    for path in &policy.closed {
        let path = param("C", path)?;
        profile.push_str(&format!("(deny {READ} file-write* {path})\n"));
    }
    for path in &policy.readable {
        let path = param("R", path)?;
        profile.push_str(&format!("(allow {READ} {path})\n"));
    }
    for path in &policy.writable {
        let path = param("W", path)?;
        profile.push_str(&format!("(allow {READ} file-write* {path})\n"));
    }
    for path in &policy.temp {
        let path = param("T", path)?;
        profile.push_str(&format!("(allow {READ} file-write* {path})\n"));
    }
    // `getcwd` lists every folder above the working directory: the folders
    // above an opened one, where they are closed, can be listed themselves,
    // their files staying closed.
    for path in above_opened(policy) {
        let name = param("A", &path)?;
        let literal = name.replacen("(subpath ", "(literal ", 1);
        profile.push_str(&format!("(allow file-read-data {literal})\n"));
    }
    for path in &policy.protected {
        profile.push_str(&format!("(deny file-write* {})\n", param("P", path)?));
    }
    for path in &policy.hidden {
        let path = param("H", path)?;
        profile.push_str(&format!(
            "(deny {READ} file-read-metadata file-write* {path})\n"
        ));
    }
    profile.push_str(
        "(deny mach-lookup (global-name \"com.apple.SecurityServer\") \
         (global-name \"com.apple.securityd.xpc\"))\n",
    );
    Ok((profile, params))
}

/// The folders above the opened ones (readable, writable, temporary, and the
/// working directory) that lie in the home or a closed folder, each once,
/// in the order met.
fn above_opened(policy: &Policy) -> Vec<PathBuf> {
    let closed = |path: &Path| {
        policy
            .home
            .iter()
            .chain(&policy.closed)
            .any(|root| path.starts_with(root))
    };
    let mut above: Vec<PathBuf> = Vec::new();
    let opened = policy
        .readable
        .iter()
        .chain(&policy.writable)
        .chain(&policy.temp)
        .chain([&policy.workdir]);
    for path in opened {
        for ancestor in path.ancestors().skip(1) {
            if closed(ancestor) && !above.iter().any(|seen| seen == ancestor) {
                above.push(ancestor.to_owned());
            }
        }
    }
    above
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
    let var_tmp = Path::new("/var/tmp");
    add(tmp, 0, vec![os("--tmpfs"), tmp.into()]);
    for shared in [run_user, var_tmp] {
        if kind(shared) == Some(Kind::Directory) {
            add(shared, 0, vec![os("--tmpfs"), shared.into()]);
        }
    }
    if let Some(home) = &policy.home
        && kind(home) == Some(Kind::Directory)
    {
        add(home, 0, vec![os("--tmpfs"), home.into()]);
    }
    for path in &policy.closed {
        // A folder inside a private one is already out of sight.
        let private = [tmp, run_user, var_tmp]
            .into_iter()
            .chain(policy.home.as_deref())
            .any(|root| path.starts_with(root));
        if !private && path.parent().is_some() && kind(path) == Some(Kind::Directory) {
            add(path, 0, vec![os("--tmpfs"), path.into()]);
        }
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
        [tmp, run_user, var_tmp]
            .into_iter()
            .chain(policy.home.as_deref())
            .chain(policy.closed.iter().map(PathBuf::as_path))
            .any(|root| path.starts_with(root))
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
        // The temporary folders every process of the user shares.
        policy.closed.push("/private/tmp".into());
        policy.closed.push("/private/var/tmp".into());
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

#[cfg(windows)]
mod imp {
    use std::ffi::{OsStr, OsString};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::process::Command;

    use super::{Policy, SandboxError, command_line, is_batch_file, launcher};

    pub(super) fn wrap<I, S>(
        policy: &Policy,
        program: &OsStr,
        args: I,
    ) -> Result<Command, SandboxError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<Vec<u16>> = args
            .into_iter()
            .map(|arg| arg.as_ref().encode_wide().collect())
            .collect();
        wrap_line(policy, program, &OsString::from_wide(&command_line(&args)))
    }

    /// The launcher's command: `owlshift-launch -- PROGRAM LINE`, in the
    /// policy's working directory, and nothing else of the policy yet. The
    /// options before `--` are left for the confinement OWL-72 adds. The
    /// working directory is not made real: `canonicalize` gives a `\\?\`
    /// path on Windows, which `cmd.exe` cannot run in.
    pub(super) fn wrap_line(
        policy: &Policy,
        program: &OsStr,
        line: &OsStr,
    ) -> Result<Command, SandboxError> {
        let launcher = launcher().ok_or(SandboxError::Unsupported)?;
        if is_batch_file(program) {
            return Err(SandboxError::BatchFile(program.into()));
        }
        let mut command = Command::new(launcher);
        command.arg("--").arg(program).arg(line);
        command.current_dir(&policy.workdir);
        Ok(command)
    }

    pub(super) fn available() -> Result<(), SandboxError> {
        // OWL-72 lifts this once the launcher applies an AppContainer.
        Err(SandboxError::Unsupported)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
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
            closed: vec!["/srv/shared-tmp".into()],
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

    /// The temporary folder is opened after the closed ones, so a run's own
    /// folder inside a shared one is the only temporary folder it reaches.
    #[test]
    fn the_seatbelt_profile_closes_the_home_the_writes_and_the_keychain() {
        let (profile, params) = seatbelt_profile(&policy()).unwrap();
        assert_eq!(
            profile,
            "(version 1)\n(allow default)\n(deny file-write*)\n\
             (allow file-write* (subpath \"/dev\"))\n\
             (deny file-read-data file-read-xattr file-write* (subpath (param \"HOME0\")))\n\
             (deny file-read-data file-read-xattr file-write* (subpath (param \"C1\")))\n\
             (allow file-read-data file-read-xattr (subpath (param \"R2\")))\n\
             (allow file-read-data file-read-xattr file-write* (subpath (param \"W3\")))\n\
             (allow file-read-data file-read-xattr file-write* (subpath (param \"W4\")))\n\
             (allow file-read-data file-read-xattr file-write* (subpath (param \"T5\")))\n\
             (allow file-read-data (literal (param \"A6\")))\n\
             (deny file-write* (subpath (param \"P7\")))\n\
             (deny file-read-data file-read-xattr file-read-metadata file-write* (subpath (param \"H8\")))\n\
             (deny file-read-data file-read-xattr file-read-metadata file-write* (subpath (param \"H9\")))\n\
             (deny file-read-data file-read-xattr file-read-metadata file-write* (subpath (param \"H10\")))\n\
             (deny mach-lookup (global-name \"com.apple.SecurityServer\") \
             (global-name \"com.apple.securityd.xpc\"))\n"
        );
        let values: Vec<&str> = params.iter().map(|(_, value)| value.as_str()).collect();
        assert_eq!(
            values,
            [
                "/home/op",
                "/srv/shared-tmp",
                "/home/op/.rustup",
                "/home/op/wt",
                "/srv/repo/.git",
                "/tmp/agent",
                "/home/op",
                "/srv/repo/.git/hooks",
                "/home/op/.ssh",
                "/home/op/wt/.env",
                "/srv/run",
            ]
        );
    }

    /// `getcwd` on macOS lists each folder above the working directory, so
    /// the folders above an opened one, inside the home or a closed folder,
    /// can be listed, themselves only: their files stay closed. Checked live
    /// on 2026-09-29: without it `pwd -P`, Python's `getcwd` and git fail
    /// with "Operation not permitted" in a worktree under a closed folder.
    #[test]
    fn the_folders_above_an_opened_one_can_be_listed_and_no_more() {
        let (profile, params) = seatbelt_profile(&policy()).unwrap();
        let listed: Vec<&str> = params
            .iter()
            .filter(|(name, _)| name.starts_with('A'))
            .map(|(_, value)| value.as_str())
            .collect();
        // Above the home's opened folders: the home itself, and nothing
        // outside the home or a closed folder.
        assert_eq!(listed, ["/home/op"]);
        assert!(profile.contains("(allow file-read-data (literal (param \"A"));
        // Listing comes after the closing rules, and before the hidden ones.
        let list = profile.find("(literal").unwrap();
        assert!(profile.find("(param \"C1\")").unwrap() < list);
        assert!(list < profile.find("file-read-metadata").unwrap());
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
            Some(
                "/run/user" | "/var/tmp" | "/home/op" | "/home/op/.ssh" | "/srv/run"
                | "/srv/shared-tmp",
            ) => Some(Kind::Directory),
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
            "/var/tmp",
            "--tmpfs",
            "/home/op",
            "--tmpfs",
            "/srv/shared-tmp",
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

    /// The launcher's line quotes each argument as std's `Command` does, so
    /// the program reads back the arguments it was given (OWL-71).
    #[test]
    fn arguments_are_quoted_as_std_quotes_them() {
        let quoted = |args: &[&str]| {
            let args: Vec<Vec<u16>> = args
                .iter()
                .map(|arg| arg.encode_utf16().collect())
                .collect();
            String::from_utf16(&command_line(&args)).unwrap()
        };
        let cases: [(&[&str], &str); 9] = [
            (&["a"], "a"),
            (&["a b"], r#""a b""#),
            (&[""], r#""""#),
            (&["a\"b"], r#"a\"b"#),
            (&[r"a\b"], r"a\b"),
            (&[r#"a\"b"#], r#"a\\\"b"#),
            (&[r"a b\"], r#""a b\\""#),
            (&["a\tb"], "\"a\tb\""),
            (&["x", "", "y z"], r#"x "" "y z""#),
        ];
        for (args, line) in cases {
            assert_eq!(quoted(args), line, "{args:?}");
        }
    }

    #[test]
    fn batch_files_and_file_streams_are_recognised() {
        for program in [
            "x.cmd",
            r"C:\tools\X.BAT",
            "x.cmd.",
            "x.cmd ",
            r"C:\x\a.bat::$DATA",
            "C:x.bat",
            "dir/run.Cmd",
        ] {
            assert!(is_batch_file(OsStr::new(program)), "{program}");
        }
        for program in [
            "x.exe",
            "cmd",
            r"C:\Windows\system32\cmd.exe",
            "C:x.exe",
            "bat",
        ] {
            assert!(!is_batch_file(OsStr::new(program)), "{program}");
        }
    }

    /// With a launcher, the command runs it with the program and its line,
    /// in the working directory, and nothing else of the policy (OWL-71); a
    /// batch file is refused before anything runs.
    #[cfg(windows)]
    #[test]
    fn the_launcher_gets_the_program_and_its_line_and_nothing_else() {
        let _launcher = use_built_launcher();
        let command = wrap(&policy(), OsStr::new("git"), ["log", "a b"]).unwrap();
        assert_eq!(
            Path::new(command.get_program()).file_name(),
            Some(OsStr::new(LAUNCHER))
        );
        let args: Vec<&OsStr> = command.get_args().collect();
        assert_eq!(args, ["--", "git", "log \"a b\""]);
        assert_eq!(command.get_current_dir(), Some(Path::new("/home/op/wt")));

        let error = wrap(&policy(), OsStr::new("npm.cmd"), ["test"]).unwrap_err();
        assert_eq!(error, SandboxError::BatchFile("npm.cmd".into()));
    }
}
