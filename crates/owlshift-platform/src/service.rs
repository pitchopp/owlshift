//! The background service in the operating system's service manager
//! (OWL-437, runtime and operations, "Install, lifecycle & uninstall"): where
//! a user LaunchAgent or a systemd user unit and its launcher live
//! ([`Layout`]), and the manager's commands that enable, start and unload it
//! ([`Manager`], [`HostManager`] on this machine).
//!
//! What the manager does was checked live: `launchctl kickstart` starts a
//! loaded job that stopped, `bootout` sends SIGTERM to the job's main process
//! and returns at once, and a property list removed does not unload its job
//! (2026-10-24, build plan, "P7 · Background service", L1); on a label that is
//! not loaded `launchctl kickstart` exits 113 and `bootout` exits 3, "No such
//! process" (2026-10-25, part 6). `systemctl --user stop` returns once the
//! unit has stopped (2026-10-24, L2).

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// The LaunchAgent's label.
pub const LABEL: &str = "dev.owlshift.daemon";

/// The systemd user unit's name.
pub const SYSTEMD_UNIT: &str = "owlshift.service";

/// The launcher's file name, beside the unit.
pub const LAUNCHER: &str = "owlshift-launcher.sh";

/// `launchctl`, by an absolute path, so no program earlier on the `PATH`
/// stands in.
const LAUNCHCTL: &str = "/bin/launchctl";

/// `launchctl bootout`'s exit status for a job that is not loaded.
const BOOTOUT_NOT_LOADED: i32 = 3;

/// How long one command of the manager may take: `systemctl --user disable
/// --now` waits for the unit to stop, which its `TimeoutStopSec` of 90
/// seconds bounds.
const MANAGER_TIMEOUT: Duration = Duration::from_secs(120);

/// A service manager.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// launchd, on macOS: a user LaunchAgent.
    Launchd,
    /// systemd, on Linux and WSL2: a user unit.
    Systemd,
}

/// Where the service's unit and launcher live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub kind: Kind,
    /// The folder of the unit: `~/Library/LaunchAgents`, or the systemd
    /// user units' folder.
    pub dir: PathBuf,
}

impl Layout {
    /// This machine's: `~/Library/LaunchAgents` on macOS; elsewhere
    /// `systemd/user` in the configuration directory `config_home`
    /// (`$XDG_CONFIG_HOME`, or `~/.config` without one).
    pub fn for_this_machine(home: &Path, config_home: Option<&Path>) -> Self {
        if cfg!(target_os = "macos") {
            Self {
                kind: Kind::Launchd,
                dir: home.join("Library").join("LaunchAgents"),
            }
        } else {
            let config = config_home.map_or_else(|| home.join(".config"), Path::to_owned);
            Self {
                kind: Kind::Systemd,
                dir: config.join("systemd").join("user"),
            }
        }
    }

    /// The unit's file.
    pub fn unit(&self) -> PathBuf {
        self.dir.join(match self.kind {
            Kind::Launchd => format!("{LABEL}.plist"),
            Kind::Systemd => SYSTEMD_UNIT.to_owned(),
        })
    }

    /// The launcher's file.
    pub fn launcher(&self) -> PathBuf {
        self.dir.join(LAUNCHER)
    }

    /// The link `systemctl enable` makes, which starts a systemd unit at
    /// login; none for launchd.
    pub fn link(&self) -> Option<PathBuf> {
        match self.kind {
            Kind::Launchd => None,
            Kind::Systemd => Some(self.dir.join("default.target.wants").join(SYSTEMD_UNIT)),
        }
    }

    /// The commands a person runs to remove a service of Owlshift's label
    /// at these paths that no manifest lists, one per line.
    pub fn removal_commands(&self) -> Vec<String> {
        let files = format!(
            "rm -f {} {}",
            sh_quote(&self.unit().to_string_lossy()),
            sh_quote(&self.launcher().to_string_lossy())
        );
        match self.kind {
            Kind::Launchd => vec![format!("launchctl bootout gui/$(id -u)/{LABEL}"), files],
            Kind::Systemd => vec![
                format!("systemctl --user disable --now {SYSTEMD_UNIT}"),
                files,
                "systemctl --user daemon-reload".to_owned(),
            ],
        }
    }
}

/// `text` as one word of `/bin/sh`: in single quotes, a single quote inside
/// written `'\''`.
pub fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// What the service manager does with the installed service.
pub trait Manager {
    /// Registers the unit for the next logins: systemd's `daemon-reload` then
    /// `enable`; nothing for launchd, which loads the property lists of
    /// `~/Library/LaunchAgents` at login.
    fn enable(&self, layout: &Layout) -> Result<(), String>;
    /// Starts the installed service now: launchd's `kickstart`, then
    /// `bootstrap` when that fails, as for a job not loaded; systemd's
    /// `start`.
    fn start(&self, layout: &Layout) -> Result<(), String>;
    /// Unloads it, which sends SIGTERM to a daemon it runs: launchd's
    /// `bootout`, a job not loaded being fine; systemd's `disable --now`.
    fn unload(&self, layout: &Layout) -> Result<(), String>;
    /// Once its files are gone: systemd's `daemon-reload`; nothing for
    /// launchd.
    fn reload(&self, layout: &Layout) -> Result<(), String>;
}

/// How a command of the manager ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exit {
    /// Its exit status; `None` when a signal ended it.
    pub code: Option<i32>,
    /// What it said on standard error, trimmed.
    pub said: String,
}

/// Runs one command of the manager: its program and arguments.
type Runner = Box<dyn Fn(&Path, &[String]) -> Result<Exit, String>>;

/// The manager of this machine, through `launchctl` or `systemctl --user`.
pub struct HostManager {
    run: Runner,
    /// `systemctl`, found on the `PATH`.
    systemctl: Option<PathBuf>,
    /// This user's id, which names launchd's `gui/<uid>` domain.
    uid: u32,
}

impl HostManager {
    /// The manager of this machine, for this user.
    pub fn for_this_machine() -> Self {
        Self {
            run: Box::new(|program, args| {
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                crate::process::run(program, &args, None, MANAGER_TIMEOUT)
                    .map(|captured| Exit {
                        code: captured.code,
                        said: String::from_utf8_lossy(&captured.stderr).trim().to_owned(),
                    })
                    .map_err(|error| format!("{}: {error}", program.display()))
            }),
            systemctl: crate::process::find_executable("systemctl"),
            uid: user_id(),
        }
    }

    fn launchctl(&self, args: &[String]) -> Result<Exit, String> {
        (self.run)(Path::new(LAUNCHCTL), args)
    }

    fn systemctl(&self, args: &[&str]) -> Result<(), String> {
        let program = self.systemctl.as_deref().ok_or_else(|| {
            "systemctl is not on the PATH: the service at login needs systemd's user manager"
                .to_owned()
        })?;
        let mut all = vec!["--user".to_owned()];
        all.extend(args.iter().map(|&arg| arg.to_owned()));
        let exit = (self.run)(program, &all)?;
        succeeded(&format!("systemctl {}", all.join(" ")), &exit)
    }

    fn domain(&self) -> String {
        format!("gui/{}", self.uid)
    }

    fn target(&self) -> String {
        format!("gui/{}/{LABEL}", self.uid)
    }
}

/// `Ok` when `exit` is a success, else why `command` failed.
fn succeeded(command: &str, exit: &Exit) -> Result<(), String> {
    if exit.code == Some(0) {
        return Ok(());
    }
    let status = exit
        .code
        .map_or_else(|| "a signal".to_owned(), |code| format!("exit {code}"));
    Err(if exit.said.is_empty() {
        format!("`{command}` failed ({status})")
    } else {
        format!("`{command}` failed ({status}): {}", exit.said)
    })
}

impl Manager for HostManager {
    fn enable(&self, layout: &Layout) -> Result<(), String> {
        match layout.kind {
            Kind::Launchd => Ok(()),
            Kind::Systemd => {
                self.systemctl(&["daemon-reload"])?;
                self.systemctl(&["enable", SYSTEMD_UNIT])
            }
        }
    }

    fn start(&self, layout: &Layout) -> Result<(), String> {
        match layout.kind {
            Kind::Launchd => {
                let kick = vec!["kickstart".to_owned(), self.target()];
                let kicked = self.launchctl(&kick)?;
                let Err(first) = succeeded(&format!("launchctl {}", kick.join(" ")), &kicked)
                else {
                    return Ok(());
                };
                let boot = vec![
                    "bootstrap".to_owned(),
                    self.domain(),
                    layout.unit().to_string_lossy().into_owned(),
                ];
                let booted = self.launchctl(&boot)?;
                succeeded(&format!("launchctl {}", boot.join(" ")), &booted)
                    .map_err(|second| format!("{first}; then {second}"))
            }
            Kind::Systemd => self.systemctl(&["start", SYSTEMD_UNIT]),
        }
    }

    fn unload(&self, layout: &Layout) -> Result<(), String> {
        match layout.kind {
            Kind::Launchd => {
                let args = vec!["bootout".to_owned(), self.target()];
                let exit = self.launchctl(&args)?;
                if exit.code == Some(BOOTOUT_NOT_LOADED) {
                    return Ok(());
                }
                succeeded(&format!("launchctl {}", args.join(" ")), &exit)
            }
            Kind::Systemd => self.systemctl(&["disable", "--now", SYSTEMD_UNIT]),
        }
    }

    fn reload(&self, layout: &Layout) -> Result<(), String> {
        match layout.kind {
            Kind::Launchd => Ok(()),
            Kind::Systemd => self.systemctl(&["daemon-reload"]),
        }
    }
}

/// This user's id.
fn user_id() -> u32 {
    #[cfg(unix)]
    {
        rustix::process::getuid().as_raw()
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// Whether this process runs as root, whose service would be root's own,
/// its files owned by root in the user's folders.
pub fn runs_as_root() -> bool {
    #[cfg(unix)]
    {
        rustix::process::geteuid().is_root()
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Sends SIGTERM to the process `pid`, a daemon's checked stop: refused for
/// pid 0 and 1, this process, and a pid beyond what the system names, any of
/// which would signal another process than the one meant.
pub fn terminate(pid: u32) -> io::Result<()> {
    if pid <= 1 || pid == std::process::id() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("pid {pid} is not a daemon's"),
        ));
    }
    #[cfg(unix)]
    {
        use rustix::process::{Pid, Signal, kill_process};
        let pid = i32::try_from(pid)
            .ok()
            .and_then(Pid::from_raw)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("pid {pid} is out of range"),
                )
            })?;
        kill_process(pid, Signal::TERM).map_err(io::Error::from)
    }
    #[cfg(not(unix))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "native Windows runs no daemon (D9)",
        ))
    }
}

/// The path the service names for this binary, `current`: the path it was
/// run by, `invoked` (its first argument), when that is the same file:
/// looked up on the `PATH` by `lookup` when it is a bare name, taken against
/// `cwd` when it is relative. `current` otherwise. So the service names a
/// package manager's stable link, such as `/opt/homebrew/bin/owlshift`,
/// rather than the versioned folder it points to, which an upgrade removes:
/// on Linux `current_exe` is `/proc/self/exe`, links resolved, while on
/// macOS it is the path as run (checked on 2026-10-25, macOS 26.6.2).
pub fn service_binary(
    invoked: Option<&OsStr>,
    cwd: &Path,
    current: &Path,
    lookup: &dyn Fn(&OsStr) -> Option<PathBuf>,
) -> PathBuf {
    let candidate = invoked.and_then(|invoked| {
        let path = Path::new(invoked);
        if path.components().count() == 1 && !path.is_absolute() {
            lookup(invoked)
        } else {
            Some(cwd.join(path))
        }
    });
    let Some(candidate) = candidate else {
        return current.to_owned();
    };
    // `cwd.join("./x")` keeps the `.`: the components drop it.
    let candidate: PathBuf = candidate
        .components()
        .filter(|part| *part != Component::CurDir)
        .collect();
    match (fs::canonicalize(&candidate), fs::canonicalize(current)) {
        (Ok(a), Ok(b)) if a == b => candidate,
        _ => current.to_owned(),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;

    #[test]
    fn each_manager_has_its_files_and_removal_commands() {
        let launchd = Layout {
            kind: Kind::Launchd,
            dir: PathBuf::from("/Users/ada/Library/LaunchAgents"),
        };
        assert_eq!(
            launchd.unit(),
            Path::new("/Users/ada/Library/LaunchAgents/dev.owlshift.daemon.plist")
        );
        assert_eq!(
            launchd.launcher(),
            Path::new("/Users/ada/Library/LaunchAgents/owlshift-launcher.sh")
        );
        assert_eq!(launchd.link(), None);
        assert_eq!(
            launchd.removal_commands(),
            [
                "launchctl bootout gui/$(id -u)/dev.owlshift.daemon",
                "rm -f '/Users/ada/Library/LaunchAgents/dev.owlshift.daemon.plist' \
                 '/Users/ada/Library/LaunchAgents/owlshift-launcher.sh'"
            ]
        );
        let home = Path::new("/home/ada");
        let systemd = if cfg!(target_os = "macos") {
            Layout {
                kind: Kind::Systemd,
                dir: home.join(".config/systemd/user"),
            }
        } else {
            assert_eq!(
                Layout::for_this_machine(home, Some(Path::new("/x"))).dir,
                Path::new("/x/systemd/user")
            );
            Layout::for_this_machine(home, None)
        };
        assert_eq!(systemd.kind, Kind::Systemd);
        assert_eq!(
            systemd.unit(),
            Path::new("/home/ada/.config/systemd/user/owlshift.service")
        );
        assert_eq!(
            systemd.link(),
            Some(PathBuf::from(
                "/home/ada/.config/systemd/user/default.target.wants/owlshift.service"
            ))
        );
        assert_eq!(systemd.removal_commands().len(), 3);
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
    }

    /// The commands run, and what each answers, by its first argument.
    fn scripted(answers: &[(&str, i32)]) -> (HostManager, Rc<RefCell<Vec<String>>>) {
        let ran = Rc::new(RefCell::new(Vec::new()));
        let seen = Rc::clone(&ran);
        let answers: Vec<(String, i32)> = answers
            .iter()
            .map(|(command, code)| ((*command).to_owned(), *code))
            .collect();
        let manager = HostManager {
            run: Box::new(move |program, args| {
                let line = format!("{} {}", program.display(), args.join(" "));
                seen.borrow_mut().push(line.clone());
                let code = answers
                    .iter()
                    .find(|(command, _)| line.contains(command.as_str()))
                    .map_or(0, |(_, code)| *code);
                Ok(Exit {
                    code: Some(code),
                    said: if code == 0 {
                        String::new()
                    } else {
                        "no".to_owned()
                    },
                })
            }),
            systemctl: Some(PathBuf::from("/usr/bin/systemctl")),
            uid: 501,
        };
        (manager, ran)
    }

    /// A job not loaded is bootstrapped once kickstart fails, and booted out
    /// without an error; systemd's commands are `--user` ones.
    #[test]
    fn the_host_manager_runs_each_managers_commands() {
        let launchd = Layout {
            kind: Kind::Launchd,
            dir: PathBuf::from("/Users/ada/Library/LaunchAgents"),
        };
        let (manager, ran) = scripted(&[("kickstart", 113), ("bootout", 3)]);
        manager.enable(&launchd).unwrap();
        manager.start(&launchd).unwrap();
        manager.unload(&launchd).unwrap();
        manager.reload(&launchd).unwrap();
        assert_eq!(
            *ran.borrow(),
            [
                "/bin/launchctl kickstart gui/501/dev.owlshift.daemon",
                "/bin/launchctl bootstrap gui/501 \
                 /Users/ada/Library/LaunchAgents/dev.owlshift.daemon.plist",
                "/bin/launchctl bootout gui/501/dev.owlshift.daemon",
            ]
        );
        let (manager, _) = scripted(&[("kickstart", 113), ("bootstrap", 5), ("bootout", 1)]);
        let error = manager.start(&launchd).unwrap_err();
        assert!(
            error.contains("kickstart") && error.contains("bootstrap") && error.contains("exit 5"),
            "{error}"
        );
        assert!(manager.unload(&launchd).is_err());

        let systemd = Layout {
            kind: Kind::Systemd,
            dir: PathBuf::from("/home/ada/.config/systemd/user"),
        };
        let (manager, ran) = scripted(&[]);
        manager.enable(&systemd).unwrap();
        manager.start(&systemd).unwrap();
        manager.unload(&systemd).unwrap();
        manager.reload(&systemd).unwrap();
        assert_eq!(
            *ran.borrow(),
            [
                "/usr/bin/systemctl --user daemon-reload",
                "/usr/bin/systemctl --user enable owlshift.service",
                "/usr/bin/systemctl --user start owlshift.service",
                "/usr/bin/systemctl --user disable --now owlshift.service",
                "/usr/bin/systemctl --user daemon-reload",
            ]
        );
        let none = HostManager {
            systemctl: None,
            ..scripted(&[]).0
        };
        assert!(
            none.start(&systemd)
                .unwrap_err()
                .contains("systemctl is not on the PATH")
        );
    }

    #[test]
    fn terminate_refuses_what_is_no_daemons_pid() {
        for pid in [0, 1, std::process::id()] {
            assert_eq!(
                terminate(pid).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "{pid}"
            );
        }
        assert_eq!(
            terminate(u32::MAX).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    /// The path run by, when it is the same file: a link is kept, a bare
    /// name is looked up, another file gives the current one.
    #[test]
    fn the_service_names_the_path_it_was_run_by() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("Cellar/owlshift/0.4/bin/owlshift");
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::write(&real, "").unwrap();
        let link = dir.path().join("bin/owlshift");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let none = |_: &OsStr| None;
        let found = |_: &OsStr| Some(link.clone());
        let at = |invoked: &str, lookup: &dyn Fn(&OsStr) -> Option<PathBuf>| {
            service_binary(Some(OsStr::new(invoked)), dir.path(), &real, lookup)
        };
        assert_eq!(at(&link.to_string_lossy(), &none), link);
        assert_eq!(at("./bin/owlshift", &none), link);
        assert_eq!(at("owlshift", &found), link);
        assert_eq!(at("owlshift", &none), real);
        assert_eq!(at("./elsewhere", &none), real);
        assert_eq!(service_binary(None, dir.path(), &real, &none), real);
    }
}
