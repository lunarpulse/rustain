//! The TUI's own state file, `<config_dir>/state.toml`: the one place the terminal UI writes
//! outside the session store. It holds exactly two keys, `session_count` (how many sessions
//! started, which fades the contextual hints) and `tour_done` (the onboarding tour was
//! finished or skipped). `config.toml` is never written.
//!
//! Every write is one transaction under an exclusive advisory lock on a stable sibling file
//! (`state.toml.lock`, never replaced, so the lock identity survives the atomic replace of the
//! state file): it re-reads the file, keeps every other key, computes its change from what it
//! just read and replaces the file atomically through a randomly named, owner-only temporary
//! file in the same directory. No predictable temporary path exists to plant a link at, a reader
//! never sees a half-written file, and two processes starting or finishing at once can neither
//! lose a `session_count` increment nor erase `tour_done` (no writer ever clears it).

use std::io::Write;
use std::path::{Path, PathBuf};

const FILE_NAME: &str = "state.toml";
const SESSION_COUNT: &str = "session_count";
const TOUR_DONE: &str = "tour_done";

/// What the file says. A missing or unreadable file, and a value of the wrong type, read as the
/// defaults (`0`, `false`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PersistedTuiState {
    pub session_count: u32,
    pub tour_done: bool,
}

/// `<config_dir>/state.toml` (the directory follows `RUSTAIN_CONFIG_DIR`).
pub fn state_path() -> anyhow::Result<PathBuf> {
    Ok(super::paths::config_dir()?.join(FILE_NAME))
}

/// Read the file at `path`.
pub fn load(path: &Path) -> PersistedTuiState {
    parse(&read_table(path))
}

fn parse(table: &toml::Table) -> PersistedTuiState {
    PersistedTuiState {
        session_count: table
            .get(SESSION_COUNT)
            .and_then(toml::Value::as_integer)
            .and_then(|count| u32::try_from(count).ok())
            .unwrap_or(0),
        tour_done: table
            .get(TOUR_DONE)
            .and_then(toml::Value::as_bool)
            .unwrap_or(false),
    }
}

/// Count this session in `<config_dir>/state.toml`: returns the file's path (where the tour's
/// "done" mark is written) or why the config directory could not be resolved (the tour reports
/// it when it cannot save), and what the file says after the count.
pub fn start_session() -> (Result<PathBuf, String>, PersistedTuiState) {
    match state_path() {
        Ok(path) => {
            let persisted = start_session_at(&path);
            (Ok(path), persisted)
        }
        Err(error) => (
            Err(format!("{error:#}")),
            PersistedTuiState {
                session_count: 1,
                tour_done: false,
            },
        ),
    }
}

/// [`start_session`] for the state file at `path`: the new `session_count` is computed from the
/// file inside the locked transaction and persisted (a failed write is not an error: the hints
/// just fade a session later); returned with the rest of the file.
pub fn start_session_at(path: &Path) -> PersistedTuiState {
    let counted = update(path, |table| {
        let mut persisted = parse(table);
        persisted.session_count = persisted.session_count.saturating_add(1);
        table.insert(
            SESSION_COUNT.to_owned(),
            toml::Value::Integer(i64::from(persisted.session_count)),
        );
        persisted
    });
    counted.unwrap_or_else(|_| {
        let mut persisted = load(path);
        persisted.session_count = persisted.session_count.saturating_add(1);
        persisted
    })
}

/// Record that the tour was finished or skipped.
pub fn mark_tour_done(path: &Path) -> std::io::Result<()> {
    update(path, |table| {
        table.insert(TOUR_DONE.to_owned(), toml::Value::Boolean(true));
    })
}

fn read_table(path: &Path) -> toml::Table {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .unwrap_or_default()
}

/// Lock, read, change, atomically replace. The lock is released when the guard file drops.
fn update<R>(path: &Path, change: impl FnOnce(&mut toml::Table) -> R) -> std::io::Result<R> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("the state file has no parent directory"))?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let guard = options.open(path.with_extension("toml.lock"))?;
    guard.lock()?;
    let mut table = read_table(path);
    let result = change(&mut table);
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(table.to_string().as_bytes())?;
    temp.persist(path).map_err(|error| error.error)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_file(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join(FILE_NAME)
    }

    #[test]
    fn story21_14_load_reads_the_two_keys_and_defaults_everything_else() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_file(&dir);
        assert_eq!(load(&path), PersistedTuiState::default());
        std::fs::write(&path, "session_count = 3\ntour_done = true\n").unwrap();
        assert_eq!(
            load(&path),
            PersistedTuiState {
                session_count: 3,
                tour_done: true
            }
        );
        std::fs::write(&path, "session_count = \"x\"\ntour_done = \"yes\"\n").unwrap();
        assert_eq!(load(&path), PersistedTuiState::default());
        std::fs::write(&path, "not toml @@@").unwrap();
        assert_eq!(load(&path), PersistedTuiState::default());
        std::fs::write(&path, "session_count = -4\n").unwrap();
        assert_eq!(load(&path).session_count, 0);
    }

    #[test]
    fn story21_14_mark_tour_done_keeps_every_other_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_file(&dir);
        std::fs::write(&path, "session_count = 7\nunknown_key = \"kept\"\n").unwrap();
        mark_tour_done(&path).unwrap();
        let table = read_table(&path);
        assert_eq!(table.get(SESSION_COUNT).unwrap().as_integer(), Some(7));
        assert_eq!(table.get("unknown_key").unwrap().as_str(), Some("kept"));
        assert_eq!(table.get(TOUR_DONE).unwrap().as_bool(), Some(true));
        let mut files: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        files.sort();
        assert_eq!(
            files,
            ["state.toml", "state.toml.lock"],
            "no temporary file is left behind"
        );
    }

    #[test]
    fn story21_14_a_garbage_file_is_replaced_by_a_valid_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_file(&dir);
        std::fs::write(&path, "not toml @@@").unwrap();
        mark_tour_done(&path).unwrap();
        assert!(load(&path).tour_done);
    }

    #[cfg(unix)]
    #[test]
    fn story21_14_the_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = state_file(&dir);
        std::fs::write(&path, "session_count = 1\n").unwrap();
        mark_tour_done(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn story21_14_an_unwritable_directory_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join(FILE_NAME);
        assert!(mark_tour_done(&path).is_err());
    }

    /// 14-A-01: a writer that read the file before another finished must not replace it with its
    /// older table. The test holds the lock itself, lets a tour writer queue behind it, changes the
    /// file as another process's startup would, then releases.
    #[test]
    fn story21_14_a_queued_writer_reads_after_the_lock_holder_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_file(&dir);
        std::fs::write(&path, "unknown_key = \"kept\"\n").unwrap();
        let holder = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path.with_extension("toml.lock"))
            .unwrap();
        holder.lock().unwrap();
        let (entered, wait) = std::sync::mpsc::channel();
        let writer = {
            let path = path.clone();
            std::thread::spawn(move || {
                entered.send(()).unwrap();
                mark_tour_done(&path).unwrap();
            })
        };
        wait.recv().unwrap();
        std::fs::write(&path, "session_count = 9\nunknown_key = \"kept\"\n").unwrap();
        drop(holder);
        writer.join().unwrap();
        let table = read_table(&path);
        assert_eq!(table[SESSION_COUNT].as_integer(), Some(9), "{table:?}");
        assert_eq!(table["unknown_key"].as_str(), Some("kept"));
        assert_eq!(table[TOUR_DONE].as_bool(), Some(true));
    }

    /// 14-A-01: a startup count and a tour finish racing never lose either one.
    #[test]
    fn story21_14_concurrent_startup_and_finish_never_erase_the_done_mark() {
        for round in 0..100 {
            let dir = tempfile::tempdir().unwrap();
            let path = state_file(&dir);
            std::fs::write(&path, "unknown_key = \"kept\"\n").unwrap();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let finish = {
                let (path, barrier) = (path.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    mark_tour_done(&path).unwrap();
                })
            };
            let start = {
                let (path, barrier) = (path.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    start_session_at(&path)
                })
            };
            finish.join().unwrap();
            assert_eq!(start.join().unwrap().session_count, 1, "round {round}");
            let table = read_table(&path);
            assert_eq!(
                table[TOUR_DONE].as_bool(),
                Some(true),
                "round {round}: {table:?}"
            );
            assert_eq!(table[SESSION_COUNT].as_integer(), Some(1), "round {round}");
            assert_eq!(table["unknown_key"].as_str(), Some("kept"), "round {round}");
        }
    }
}
