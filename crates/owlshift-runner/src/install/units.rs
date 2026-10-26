//! The files of the service installed at login, written from templates in
//! the binary (build plan, "P7 · Background service", "Contracts and
//! formats"): the unit, a LaunchAgent's property list or a systemd user unit,
//! which runs `/bin/sh <launcher>`; and the launcher, which starts `owlshift
//! daemon` while its binary and the install manifest exist and removes the
//! service once either is gone.

use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

use owlshift_contracts::service::MANIFEST_SERVICE_LINE;
use owlshift_platform::service::{Kind, LABEL, Layout, sh_quote};

/// The grace launchd gives the daemon after SIGTERM (`ExitTimeOut`): 60
/// seconds, which a bootout, a logout and a shutdown all cap it at whatever
/// larger value is named (build plan, "P7 · Background service", L1).
pub const LAUNCHD_GRACE: Duration = Duration::from_secs(60);

/// The grace systemd gives it (`TimeoutStopSec`, L2).
pub const SYSTEMD_GRACE: Duration = Duration::from_secs(90);

/// How long either manager waits before it starts a daemon that ended
/// unsuccessfully again (`ThrottleInterval`, `RestartSec`; architecture
/// section 5, "Restarts").
pub const RESTART_DELAY: Duration = Duration::from_secs(30);

/// What the launcher names.
pub(crate) struct Launcher<'a> {
    /// The binary it starts.
    pub binary: &'a Path,
    /// The data directory, `OWLSHIFT_DATA_DIR`.
    pub data: &'a Path,
    /// The personal file's folder, `OWLSHIFT_CONFIG_DIR`, when there is one.
    pub config_dir: Option<&'a Path>,
    /// The `PATH` it gives the daemon, when there is one.
    pub path: Option<&'a str>,
    /// Where the unit and the launcher are.
    pub layout: &'a Layout,
}

/// `path` as text the service's files can hold: absolute, UTF-8 and free of
/// control characters, which neither a property list nor a unit file nor a
/// line of the launcher carries safely.
pub(crate) fn checked<'a>(path: &'a Path, what: &str) -> Result<&'a str, String> {
    path.to_str()
        .filter(|text| path.is_absolute() && !text.chars().any(char::is_control))
        .ok_or_else(|| {
            format!(
                "{what}, {}, is not an absolute path in UTF-8 free of control characters, which \
                 the service at login needs",
                path.display()
            )
        })
}

/// The launcher's text.
pub(crate) fn launcher_text(launcher: &Launcher<'_>) -> Result<String, String> {
    let layout = launcher.layout;
    let binary = checked(launcher.binary, "the binary")?;
    let data = checked(launcher.data, "the data directory")?;
    let unit = layout.unit();
    let mut removed = vec![checked(&unit, "the unit")?.to_owned()];
    if let Some(link) = layout.link() {
        removed.push(checked(&link, "the unit's link")?.to_owned());
    }
    let own = layout.launcher();
    removed.push(checked(&own, "the launcher")?.to_owned());
    let mut text = format!(
        "#!/bin/sh\n\
         # Owlshift's background service ({LABEL}), written by `owlshift start --at-login`;\n\
         # `owlshift stop --at-login` and `owlshift uninstall` remove it. It starts\n\
         # `owlshift daemon` while its binary and the install manifest exist, and removes\n\
         # the service once either is gone, so that nothing is left failing in a loop.\n\
         binary={}\n\
         data={}\n\
         manifest=\"$data/service/manifest.json\"\n\
         if [ ! -e \"$binary\" ]; then\n\
         \t# A package manager's upgrade relinks the binary: look once more.\n\
         \tsleep 2\n\
         fi\n\
         if [ ! -e \"$binary\" ] || [ ! -f \"$manifest\" ]; then\n",
        sh_quote(binary),
        sh_quote(data),
    );
    for file in &removed {
        text.push_str(&format!("\trm -f {}\n", sh_quote(file)));
    }
    text.push_str(&format!(
        "\tif [ -f \"$manifest\" ]; then\n\
         \t\tgrep -v {} \"$manifest\" >\"$manifest.$$\" && mv -f \"$manifest.$$\" \"$manifest\"\n\
         \t\trm -f \"$manifest.$$\"\n\
         \tfi\n\
         \texit 0\n\
         fi\n",
        sh_quote(&format!("^{MANIFEST_SERVICE_LINE}"))
    ));
    if let Some(path) = launcher.path {
        text.push_str(&format!("PATH={}\nexport PATH\n", sh_quote(path)));
    }
    text.push_str("OWLSHIFT_DATA_DIR=$data\nexport OWLSHIFT_DATA_DIR\n");
    if let Some(config) = launcher.config_dir {
        let config = checked(config, "the personal file's folder")?;
        text.push_str(&format!(
            "OWLSHIFT_CONFIG_DIR={}\nexport OWLSHIFT_CONFIG_DIR\n",
            sh_quote(config)
        ));
    }
    text.push_str(
        "umask 077\n\
         cd \"$data/service\" || exit 1\n\
         exec \"$binary\" daemon >>daemon.log 2>&1\n",
    );
    Ok(text)
}

/// The unit's text: it runs `/bin/sh <launcher>` with the restart and grace
/// values of architecture section 5, "Restarts" and "Checked stop".
pub(crate) fn unit_text(layout: &Layout) -> Result<String, String> {
    let launcher = layout.launcher();
    let launcher = checked(&launcher, "the launcher")?;
    Ok(match layout.kind {
        Kind::Launchd => format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n\
             <dict>\n\
             \t<key>Label</key>\n\
             \t<string>{LABEL}</string>\n\
             \t<key>ProgramArguments</key>\n\
             \t<array>\n\
             \t\t<string>/bin/sh</string>\n\
             \t\t<string>{}</string>\n\
             \t</array>\n\
             \t<key>RunAtLoad</key>\n\
             \t<true/>\n\
             \t<key>KeepAlive</key>\n\
             \t<dict>\n\
             \t\t<key>SuccessfulExit</key>\n\
             \t\t<false/>\n\
             \t</dict>\n\
             \t<key>ThrottleInterval</key>\n\
             \t<integer>{}</integer>\n\
             \t<key>ExitTimeOut</key>\n\
             \t<integer>{}</integer>\n\
             </dict>\n\
             </plist>\n",
            xml_escaped(launcher),
            RESTART_DELAY.as_secs(),
            LAUNCHD_GRACE.as_secs(),
        ),
        Kind::Systemd => format!(
            "# Owlshift's background service, written by `owlshift start --at-login`;\n\
             # `owlshift stop --at-login` and `owlshift uninstall` remove it.\n\
             [Unit]\n\
             Description=Owlshift background service\n\
             StartLimitIntervalSec=0\n\
             \n\
             [Service]\n\
             Type=simple\n\
             ExecStart=/bin/sh \"{}\"\n\
             Restart=on-failure\n\
             RestartSec={}\n\
             TimeoutStopSec={}\n\
             KillMode=mixed\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n",
            systemd_escaped(launcher),
            RESTART_DELAY.as_secs(),
            SYSTEMD_GRACE.as_secs(),
        ),
    })
}

/// The `PATH` the service is given: the absolute entries of `path`, the one
/// `owlshift start --at-login` runs with, each once and in order; `None` when
/// none is left. A login's own `PATH` under launchd is `/usr/bin:/bin:
/// /usr/sbin:/sbin`, which does not reach the harness CLIs.
pub(crate) fn service_path(path: Option<&OsStr>) -> Option<String> {
    let mut kept: Vec<String> = Vec::new();
    for entry in std::env::split_paths(path?) {
        let Some(text) = entry.to_str() else { continue };
        if entry.is_absolute()
            && !text.contains(':')
            && !text.chars().any(char::is_control)
            && !kept.iter().any(|seen| seen == text)
        {
            kept.push(text.to_owned());
        }
    }
    (!kept.is_empty()).then(|| kept.join(":"))
}

/// `text` inside a property list's `<string>`.
fn xml_escaped(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// `text` inside double quotes on a unit's `ExecStart=` line, where systemd
/// expands `%` specifiers and `$` variables and reads C escapes.
fn systemd_escaped(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "$$")
        .replace('%', "%%")
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn layout(kind: Kind) -> Layout {
        Layout {
            kind,
            dir: PathBuf::from("/home/a d'a/%h $x & <y>/units"),
        }
    }

    /// The values the design sets, and every path quoted for where it goes.
    #[test]
    fn the_units_carry_the_designs_values_and_quote_their_paths() {
        let plist = unit_text(&layout(Kind::Launchd)).unwrap();
        for line in [
            "<string>dev.owlshift.daemon</string>",
            "<string>/bin/sh</string>",
            "<string>/home/a d&apos;a/%h $x &amp; &lt;y&gt;/units/owlshift-launcher.sh</string>",
            "<key>RunAtLoad</key>\n\t<true/>",
            "<key>SuccessfulExit</key>\n\t\t<false/>",
            "<key>ThrottleInterval</key>\n\t<integer>30</integer>",
            "<key>ExitTimeOut</key>\n\t<integer>60</integer>",
        ] {
            assert!(plist.contains(line), "{line}\n{plist}");
        }
        let unit = unit_text(&layout(Kind::Systemd)).unwrap();
        for line in [
            "ExecStart=/bin/sh \"/home/a d'a/%%h $$x & <y>/units/owlshift-launcher.sh\"\n",
            "Restart=on-failure\n",
            "RestartSec=30\n",
            "StartLimitIntervalSec=0\n",
            "TimeoutStopSec=90\n",
            "KillMode=mixed\n",
            "WantedBy=default.target\n",
        ] {
            assert!(unit.contains(line), "{line}\n{unit}");
        }
        let control = Layout {
            kind: Kind::Systemd,
            dir: PathBuf::from("/home/ada\n/units"),
        };
        assert!(
            unit_text(&control)
                .unwrap_err()
                .contains("free of control characters")
        );
    }

    #[test]
    fn the_service_path_keeps_absolute_entries_once() {
        let path =
            std::env::join_paths(["/opt/homebrew/bin", "relative/bin", "", "/usr/bin"]).unwrap();
        let mut with_again = path.clone();
        with_again.push(":/opt/homebrew/bin");
        assert_eq!(
            service_path(Some(&with_again)).as_deref(),
            Some("/opt/homebrew/bin:/usr/bin")
        );
        assert_eq!(service_path(Some(OsStr::new("relative"))), None);
        assert_eq!(service_path(None), None);
    }
}
