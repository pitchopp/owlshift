//! A harness's home folder of one run, holding only a copy of the user's
//! login file, and the copy's way back (architecture section 5, "The Codex
//! harness", decided with the maintainer on 2026-10-15, OWL-298).
//!
//! The executor makes the folder in the run's own files and copies the login
//! in, keeping what it copied ([`SeededLogin`]) outside the sandbox. After the
//! run, once its processes are stopped, [`write_back`] brings a changed copy
//! back to the user's file only under the harness's own check and only when
//! the user's file has not changed since: a refreshed login is kept, and
//! nothing else the run wrote there outlives it.
//!
//! The same way, a session's transcript (OWL-394): kept out of a run's
//! configuration folder once its processes are stopped ([`keep_session`]),
//! and put back in a later run's empty one ([`seed_session`]), through no
//! link either way and nothing else of the folder.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use owlshift_contracts::ids::SessionId;
use owlshift_platform::confined::{ConfinedError, Refusal, read_confined};

use super::{Harness, MAX_SESSION_BYTES, ResumeFrom};

/// The most bytes a login file may hold, read back or copied in.
pub const MAX_LOGIN_BYTES: u64 = 64 * 1024;

/// What the executor copied into a run's home: kept by the runner, out of
/// the run's reach.
#[derive(Debug)]
pub(super) struct SeededLogin {
    /// The user's login file.
    source: PathBuf,
    /// The run's home folder.
    home: PathBuf,
    /// The login file's name, the same in both.
    name: OsString,
    /// The bytes copied.
    original: Vec<u8>,
}

impl SeededLogin {
    /// The bytes copied in.
    pub(super) fn original(&self) -> &[u8] {
        &self.original
    }
}

/// Makes `home`, private to the user, and copies the user's login file
/// `source` into it under the same name, readable and writable by the user
/// alone.
pub(super) fn seed(home: &Path, source: &Path) -> io::Result<SeededLogin> {
    let name = source
        .file_name()
        .ok_or_else(|| io::Error::other("the login file has no name"))?
        .to_owned();
    let original = read_capped(source)?;
    create_private_dir(home)?;
    let mut copy = create_private_file(&home.join(&name))?;
    copy.write_all(&original)?;
    copy.sync_all()?;
    Ok(SeededLogin {
        source: source.to_owned(),
        home: home.to_owned(),
        name,
        original,
    })
}

/// Brings the run's copy of the login back to the user's file when it
/// changed and `kept` accepts it, and the user's file is still the one
/// copied: written beside it under a temporary name, with its permissions,
/// then renamed over it. Returns a warning when a changed copy is dropped or
/// the write fails; `None` when there was nothing to bring back or it was.
///
/// The copy is read with links refused. It runs whatever the run came to: a
/// login the harness refreshed is otherwise lost, and a copy cut off mid
/// write fails `kept`.
pub(super) fn write_back(
    seeded: &SeededLogin,
    kept: impl FnOnce(&[u8], &[u8]) -> Result<(), String>,
) -> Option<String> {
    let source = seeded.source.display();
    let dropped = |why: String| {
        Some(format!(
            "the run's copy of {source} was not written back: {why}. If the harness now \
             reports being logged out, log it in again"
        ))
    };
    let name = seeded.name.to_string_lossy();
    let updated = match read_confined(&seeded.home, &name, MAX_LOGIN_BYTES) {
        Ok(bytes) => bytes,
        Err(ConfinedError::Refused {
            reason: Refusal::NotFound,
            ..
        }) => return dropped("the run removed it".to_owned()),
        Err(error) => return dropped(error.to_string()),
    };
    if updated == seeded.original {
        return None;
    }
    if let Err(reason) = kept(&seeded.original, &updated) {
        return dropped(reason);
    }
    match fs::symlink_metadata(&seeded.source) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return dropped("it is a symbolic link, which Owlshift does not replace".to_owned());
        }
        Ok(_) => {}
        Err(error) => return dropped(error.to_string()),
    }
    match read_capped(&seeded.source) {
        Ok(current) if current == seeded.original => {}
        Ok(_) => return dropped("it changed during the run".to_owned()),
        Err(error) => return dropped(error.to_string()),
    }
    replace(&seeded.source, &updated)
        .err()
        .and_then(|error| dropped(error.to_string()))
}

/// Writes `bytes` beside `target` under a temporary name, with `target`'s
/// permissions, and renames it over `target`.
fn replace(target: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::other("the login file has no folder"))?;
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    let temporary = parent.join(format!(".{name}.owlshift-{}", std::process::id()));
    let permissions = fs::metadata(target)?.permissions();
    let written = (|| {
        let mut file = create_private_file(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::set_permissions(&temporary, permissions)?;
        fs::rename(&temporary, target)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written
}

/// Replaces whatever a harness's run left at `file` with `message`, the
/// result it gave as its last message, or with nothing: a file the run
/// wrote there itself never stands.
pub(super) fn result_from_message(file: &Path, message: Option<&str>) -> io::Result<()> {
    match fs::remove_file(file) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    if let Some(message) = message {
        let mut out = create_private_file(file)?;
        out.write_all(message.as_bytes())?;
    }
    Ok(())
}

/// Puts the transcript of the session `from` names in the run's empty
/// configuration folder `config`, at the place it has in the kept folder
/// (OWL-394). Says why not when the kept folder holds none or it cannot be
/// copied ([`copy_confined`]).
pub(super) fn seed_session(
    harness: &dyn Harness,
    from: &ResumeFrom,
    config: &Path,
) -> Result<(), String> {
    let relative = harness
        .session_transcript(&from.folder, &from.id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("no transcript of it is kept in {}", from.folder.display()))?;
    copy_confined(&from.folder, &relative, config)
}

/// Keeps the transcript of session `id` the run left in its configuration
/// folder `config` in `folder`, at the same place, once the run's processes
/// are stopped (OWL-394); returns `folder`.
pub(super) fn keep_session(
    harness: &dyn Harness,
    config: &Path,
    id: &SessionId,
    folder: &Path,
) -> Result<PathBuf, String> {
    let relative = harness
        .session_transcript(config, id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "the run left no transcript of it".to_owned())?;
    copy_confined(config, &relative, folder)?;
    Ok(folder.to_owned())
}

/// Copies the file `relative` of `from` to the same place in `to`, at most
/// [`MAX_SESSION_BYTES`]: read through no link ([`read_confined`]), written
/// as a new file private to the user, in folders made private to the user.
/// A file cut off by a failed write is removed.
fn copy_confined(from: &Path, relative: &str, to: &Path) -> Result<(), String> {
    let bytes =
        read_confined(from, relative, MAX_SESSION_BYTES).map_err(|error| error.to_string())?;
    let target = to.join(relative);
    let written = (|| {
        let mut dir = to.to_owned();
        let parent = Path::new(relative).parent().unwrap_or(Path::new(""));
        for segment in [None].into_iter().chain(parent.components().map(Some)) {
            if let Some(segment) = segment {
                dir.push(segment);
            }
            match create_private_dir(&dir) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        let mut file = create_private_file(&target)?;
        file.write_all(&bytes)?;
        file.sync_all()
    })();
    written.map_err(|error| {
        let _ = fs::remove_file(&target);
        format!("{}: {error}", target.display())
    })
}

fn read_capped(path: &Path) -> io::Result<Vec<u8>> {
    let bytes = fs::read(path)?;
    if bytes.len() as u64 > MAX_LOGIN_BYTES {
        return Err(io::Error::other(format!(
            "{} holds more than {MAX_LOGIN_BYTES} bytes",
            path.display()
        )));
    }
    Ok(bytes)
}

fn create_private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    let builder = fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = builder;
        builder.mode(0o700);
        builder
    };
    builder.create(path)
}

#[cfg(test)]
mod tests {
    use super::super::harness::ClaudeHarness;
    use super::*;

    /// OWL-394: a session's transcript is kept out of the run's
    /// configuration folder at the place it had there, private to the user,
    /// and put back in a fresh one at the same place; nothing else of the
    /// folder goes either way, and a transcript the folder holds through a
    /// link is never followed.
    #[test]
    fn a_sessions_transcript_alone_is_kept_and_put_back() {
        let claude = ClaudeHarness {
            program: PathBuf::from("claude"),
            prompt: String::new(),
            model: None,
            effort: None,
            max_budget_usd: None,
            login: None,
            bash: false,
        };
        let id = SessionId::new("75744e41-7eac-4fdf-aeea-df03acb72bff").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        let place = format!("projects/-work-DEMO-1/{id}.jsonl");
        fs::create_dir_all(config.join("projects/-work-DEMO-1")).unwrap();
        fs::write(config.join(&place), "{\"type\":\"user\"}\n").unwrap();
        fs::write(config.join(".claude.json"), "{}").unwrap();

        let kept = dir.path().join("run/session");
        fs::create_dir(dir.path().join("run")).unwrap();
        assert_eq!(keep_session(&claude, &config, &id, &kept).unwrap(), kept);
        assert_eq!(
            fs::read_to_string(kept.join(&place)).unwrap(),
            "{\"type\":\"user\"}\n"
        );
        assert!(!kept.join(".claude.json").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&kept.join(&place)), 0o600);
            assert_eq!(mode(&kept), 0o700);
        }

        let fresh = dir.path().join("fresh");
        fs::create_dir(&fresh).unwrap();
        let from = ResumeFrom {
            id: id.clone(),
            folder: kept.clone(),
            cost_micros: 0,
        };
        seed_session(&claude, &from, &fresh).unwrap();
        assert_eq!(
            fs::read_to_string(fresh.join(&place)).unwrap(),
            "{\"type\":\"user\"}\n"
        );
        let entries: Vec<_> = fs::read_dir(&fresh).unwrap().collect();
        assert_eq!(entries.len(), 1, "the projects folder alone");

        // A kept folder without the transcript gives nothing to resume.
        let other = ResumeFrom {
            folder: dir.path().join("nothing"),
            ..from
        };
        assert!(seed_session(&claude, &other, &fresh).is_err());

        #[cfg(unix)]
        {
            let planted = dir.path().join("planted");
            let folder = planted.join("projects/-work");
            fs::create_dir_all(&folder).unwrap();
            fs::write(dir.path().join("secret"), "secret\n").unwrap();
            std::os::unix::fs::symlink(
                dir.path().join("secret"),
                folder.join(format!("{id}.jsonl")),
            )
            .unwrap();
            let refused = keep_session(&claude, &planted, &id, &dir.path().join("kept2"));
            assert!(refused.is_err());
            assert!(!dir.path().join("kept2").exists());
        }
    }

    struct Setup {
        _dir: tempfile::TempDir,
        source: PathBuf,
        seeded: SeededLogin,
    }

    fn setup(original: &str) -> Setup {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("user")).unwrap();
        let source = dir.path().join("user/auth.json");
        fs::write(&source, original).unwrap();
        let seeded = seed(&dir.path().join("home"), &source).unwrap();
        Setup {
            _dir: dir,
            source,
            seeded,
        }
    }

    fn accept(_: &[u8], _: &[u8]) -> Result<(), String> {
        Ok(())
    }

    #[test]
    fn the_copy_is_the_users_file_alone_and_private() {
        let s = setup("v1");
        let copy = s.seeded.home.join("auth.json");
        assert_eq!(fs::read_to_string(&copy).unwrap(), "v1");
        let entries: Vec<_> = fs::read_dir(&s.seeded.home).unwrap().collect();
        assert_eq!(entries.len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&copy), 0o600);
            assert_eq!(mode(&s.seeded.home), 0o700);
        }
    }

    #[test]
    fn a_changed_copy_the_harness_keeps_replaces_the_users_file() {
        let s = setup("v1");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&s.source, fs::Permissions::from_mode(0o640)).unwrap();
        }
        // Unchanged: nothing to do, whatever the check says.
        assert_eq!(write_back(&s.seeded, |_, _| Err("no".into())), None);
        fs::write(s.seeded.home.join("auth.json"), "v2").unwrap();
        assert_eq!(write_back(&s.seeded, accept), None);
        assert_eq!(fs::read_to_string(&s.source).unwrap(), "v2");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&s.source).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o640, "the user's permissions are kept");
        }
        let left: Vec<_> = fs::read_dir(s.source.parent().unwrap()).unwrap().collect();
        assert_eq!(left.len(), 1, "no temporary file is left");
    }

    #[test]
    fn a_changed_copy_is_dropped_with_a_warning_when_it_cannot_be_trusted() {
        // The harness's check refuses it.
        let s = setup("v1");
        fs::write(s.seeded.home.join("auth.json"), "v2").unwrap();
        let warning = write_back(&s.seeded, |_, _| Err("another account".into())).unwrap();
        assert!(warning.contains("another account"), "{warning}");
        assert!(warning.contains("log it in again"), "{warning}");
        assert_eq!(fs::read_to_string(&s.source).unwrap(), "v1");

        // The user's file changed during the run.
        let s = setup("v1");
        fs::write(s.seeded.home.join("auth.json"), "v2").unwrap();
        fs::write(&s.source, "v1-by-the-user").unwrap();
        let warning = write_back(&s.seeded, accept).unwrap();
        assert!(warning.contains("changed during the run"), "{warning}");
        assert_eq!(fs::read_to_string(&s.source).unwrap(), "v1-by-the-user");

        // The run removed its copy.
        let s = setup("v1");
        fs::remove_file(s.seeded.home.join("auth.json")).unwrap();
        assert!(write_back(&s.seeded, accept).unwrap().contains("removed"));
        assert_eq!(fs::read_to_string(&s.source).unwrap(), "v1");
    }

    #[cfg(unix)]
    #[test]
    fn links_are_never_followed_either_way() {
        // The run replaced its copy with a link to another file.
        let s = setup("v1");
        let elsewhere = s.seeded.home.parent().unwrap().join("elsewhere");
        fs::write(&elsewhere, "v2").unwrap();
        let copy = s.seeded.home.join("auth.json");
        fs::remove_file(&copy).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &copy).unwrap();
        assert!(write_back(&s.seeded, accept).is_some());
        assert_eq!(fs::read_to_string(&s.source).unwrap(), "v1");

        // The user's file is a link: it is read through, never replaced.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dotfiles-auth.json");
        fs::write(&target, "v1").unwrap();
        let link = dir.path().join("auth.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let seeded = seed(&dir.path().join("home"), &link).unwrap();
        fs::write(dir.path().join("home/auth.json"), "v2").unwrap();
        let warning = write_back(&seeded, accept).unwrap();
        assert!(warning.contains("symbolic link"), "{warning}");
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "v1");
    }

    #[test]
    fn a_result_left_by_the_run_never_stands() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("result.json");
        fs::write(&file, "planted").unwrap();
        result_from_message(&file, None).unwrap();
        assert!(!file.exists());
        fs::write(&file, "planted").unwrap();
        result_from_message(&file, Some("{\"format\":9}")).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "{\"format\":9}");
        result_from_message(&file, None).unwrap();
        assert!(!file.exists());
    }
}
