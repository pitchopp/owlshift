//! The web UI's record, `<data>/service/ui.json` (OWL-467, `UiRecord`,
//! format 1): where the UI the daemon started listens. The UI writes it
//! once it holds the socket the daemon handed it; the daemon removes it at
//! its start, before each start of the UI and when the port cannot be
//! bound. Whether a record is live is settled by the UI's proof
//! (`ui::probe`), never by its pid.

use std::io;
use std::path::{Path, PathBuf};

use owlshift_contracts::service::UiRecord;

use crate::daemon::service_dir;

/// `<data>/service/ui.json`.
pub fn file(data: &Path) -> PathBuf {
    service_dir(data).join("ui.json")
}

/// Writes `record` whole, atomically.
pub fn write(data: &Path, record: &UiRecord) -> io::Result<()> {
    std::fs::create_dir_all(service_dir(data))?;
    crate::step::write_atomically(&file(data), &record.render())
}

/// The record, `None` when there is none; one that does not read is an
/// error saying why.
pub fn read(data: &Path) -> Result<Option<UiRecord>, String> {
    match std::fs::read_to_string(file(data)) {
        Ok(text) => UiRecord::parse(&text)
            .map(Some)
            .map_err(|error| error.to_string()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{} cannot be read: {error}", file(data).display())),
    }
}

/// Removes the record; none there is no error.
pub fn remove(data: &Path) -> io::Result<()> {
    match std::fs::remove_file(file(data)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use owlshift_contracts::format::Format;

    use super::*;

    #[test]
    fn a_record_round_trips_and_is_removed() {
        let data = tempfile::tempdir().unwrap();
        assert_eq!(read(data.path()).unwrap(), None);
        let record = UiRecord {
            format: Format,
            pid: 42,
            port: 18466,
            since: "2026-11-05T09:00:00Z".parse().unwrap(),
            build: None,
        };
        write(data.path(), &record).unwrap();
        assert_eq!(read(data.path()).unwrap(), Some(record));
        remove(data.path()).unwrap();
        remove(data.path()).unwrap();
        assert_eq!(read(data.path()).unwrap(), None);
        std::fs::write(file(data.path()), "{").unwrap();
        assert!(read(data.path()).is_err());
    }
}
