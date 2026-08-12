//! Direct access to Codex's local thread catalog.
//!
//! Codex keeps queryable thread metadata in `state_5.sqlite`. Conversation
//! bodies remain in rollout JSONL files and are parsed elsewhere. Renaming
//! mirrors Codex's local thread-store behavior: update the history-mode-specific
//! SQLite column, then append a compatibility record to `session_index.jsonl`.

use std::collections::HashMap;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const STATE_DB_FILENAME: &str = "state_5.sqlite";
const SESSION_INDEX_FILENAME: &str = "session_index.jsonl";
const WRITER_LOCK_DIR: &str = "thread-writer-locks";
const COORDINATION_LOCK_FILE: &str = ".coordination.lock";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadMeta {
    pub id: String,
    pub name: Option<String>,
    pub preview: String,
    pub cwd: String,
    pub updated_at: i64,
}

#[derive(Debug, Deserialize, Serialize)]
struct SessionIndexEntry {
    id: String,
    thread_name: String,
    updated_at: String,
}

struct WriterLock {
    path: PathBuf,
    file: Option<File>,
    coordination_path: PathBuf,
}

impl WriterLock {
    fn acquire(home: &Path, thread_id: &str) -> Result<Self> {
        if !valid_thread_id(thread_id) {
            bail!("invalid Codex thread id {thread_id:?}");
        }
        let directory = home.join(WRITER_LOCK_DIR);
        std::fs::create_dir_all(&directory)
            .with_context(|| format!("creating {}", directory.display()))?;
        let coordination_path = directory.join(COORDINATION_LOCK_FILE);
        let coordination = open_lock_file(&coordination_path)?;
        coordination
            .lock()
            .with_context(|| format!("locking {}", coordination_path.display()))?;

        let path = directory.join(format!("{thread_id}.lock"));
        let file = open_lock_file(&path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                bail!("Codex thread {thread_id} already has an active writer");
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(error).with_context(|| format!("locking {}", path.display()));
            }
        }
        drop(coordination);
        Ok(Self {
            path,
            file: Some(file),
            coordination_path,
        })
    }
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        let Ok(coordination) = open_lock_file(&self.coordination_path) else {
            return;
        };
        if coordination.lock().is_err() {
            return;
        }
        drop(self.file.take());
        // The rename itself is already complete, so lock cleanup is best effort.
        let _ = std::fs::remove_file(&self.path);
    }
}

fn open_lock_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))
}

fn valid_thread_id(thread_id: &str) -> bool {
    thread_id.len() == 36
        && thread_id.char_indices().all(|(index, character)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                character == '-'
            } else {
                character.is_ascii_hexdigit()
            }
        })
}

fn codex_home() -> Result<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .context("resolving CODEX_HOME")
}

fn sqlite_home(codex_home: &Path) -> PathBuf {
    std::env::var_os("CODEX_SQLITE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| codex_home.to_path_buf())
}

fn open_state(home: &Path, writable: bool) -> Result<Connection> {
    let flags = if writable {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let path = home.join(STATE_DB_FILENAME);
    let connection = Connection::open_with_flags(&path, flags)
        .with_context(|| format!("opening Codex state catalog {}", path.display()))?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .context("configuring Codex state catalog busy timeout")?;
    Ok(connection)
}

/// Return every active interactive Codex thread, newest first.
pub fn list_threads() -> Result<Vec<ThreadMeta>> {
    let codex_home = codex_home()?;
    list_threads_from(&codex_home, &sqlite_home(&codex_home))
}

#[cfg(test)]
fn list_threads_at(home: &Path) -> Result<Vec<ThreadMeta>> {
    list_threads_from(home, home)
}

fn list_threads_from(codex_home: &Path, sqlite_home: &Path) -> Result<Vec<ThreadMeta>> {
    let connection = open_state(sqlite_home, false)?;
    let index_names = read_session_index(codex_home)?;
    let mut statement = connection.prepare(
        r#"
SELECT
    id,
    name,
    title,
    first_user_message,
    preview,
    cwd,
    CASE
        WHEN COALESCE(updated_at_ms, 0) > 0 THEN updated_at_ms / 1000
        ELSE updated_at
    END AS updated_at,
    history_mode
FROM threads
WHERE archived = 0
  AND preview <> ''
  AND source IN ('cli', 'vscode')
ORDER BY COALESCE(NULLIF(updated_at_ms, 0), updated_at * 1000) DESC, id DESC
"#,
    )?;
    let mut rows = statement.query([])?;
    let mut threads = Vec::new();
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let sqlite_name: Option<String> = row.get(1)?;
        let title: String = row.get(2)?;
        let first_user_message: String = row.get(3)?;
        let history_mode: String = row.get(7)?;
        let name = match history_mode.as_str() {
            "paginated" => nonempty(sqlite_name.as_deref()).map(str::to_string),
            "legacy" => distinct_legacy_title(&title, &first_user_message)
                .map(str::to_string)
                .or_else(|| index_names.get(&id).cloned()),
            _ => nonempty(sqlite_name.as_deref())
                .or_else(|| distinct_legacy_title(&title, &first_user_message))
                .map(str::to_string),
        };
        threads.push(ThreadMeta {
            id,
            name,
            preview: row.get(4)?,
            cwd: row.get(5)?,
            updated_at: row.get(6)?,
        });
    }
    Ok(threads)
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn distinct_legacy_title<'a>(title: &'a str, first_user_message: &str) -> Option<&'a str> {
    let title = title.trim();
    if title.is_empty() || first_user_message.trim() == title {
        None
    } else {
        Some(title)
    }
}

fn read_session_index(home: &Path) -> Result<HashMap<String, String>> {
    let path = home.join(SESSION_INDEX_FILENAME);
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(error) => return Err(error).with_context(|| format!("opening {}", path.display())),
    };
    let mut names = HashMap::new();
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else {
            continue;
        };
        let Ok(entry) = serde_json::from_str::<SessionIndexEntry>(line.trim()) else {
            continue;
        };
        if let Some(name) = nonempty(Some(&entry.thread_name)) {
            names.insert(entry.id, name.to_string());
        }
    }
    Ok(names)
}

/// Persist a user-facing name using the same local writes as Codex's thread store.
pub fn set_thread_name(thread_id: &str, name: &str) -> Result<()> {
    let codex_home = codex_home()?;
    set_thread_name_in(&codex_home, &sqlite_home(&codex_home), thread_id, name)
}

#[cfg(test)]
fn set_thread_name_at(home: &Path, thread_id: &str, name: &str) -> Result<()> {
    set_thread_name_in(home, home, thread_id, name)
}

fn set_thread_name_in(
    codex_home: &Path,
    sqlite_home: &Path,
    thread_id: &str,
    name: &str,
) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        bail!("thread name must not be empty");
    }
    let _writer_lock = WriterLock::acquire(codex_home, thread_id)?;

    let mut connection = open_state(sqlite_home, true)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let history_mode: Option<String> = transaction
        .query_row(
            "SELECT history_mode FROM threads WHERE id = ?1 AND archived = 0",
            [thread_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(history_mode) = history_mode else {
        bail!("Codex thread {thread_id} was not found");
    };
    let changed = match history_mode.as_str() {
        "legacy" => transaction.execute(
            "UPDATE threads SET title = ?1 WHERE id = ?2 AND archived = 0",
            params![name, thread_id],
        )?,
        "paginated" => transaction.execute(
            "UPDATE threads SET name = ?1 WHERE id = ?2 AND archived = 0",
            params![name, thread_id],
        )?,
        other => bail!("unsupported Codex thread history mode {other:?}"),
    };
    if changed != 1 {
        bail!("Codex thread {thread_id} changed while it was being renamed");
    }
    transaction.commit()?;

    let index_result = append_session_index(codex_home, thread_id, name);
    if history_mode == "legacy" {
        index_result?;
    }
    Ok(())
}

fn append_session_index(home: &Path, thread_id: &str, name: &str) -> Result<()> {
    let path = home.join(SESSION_INDEX_FILENAME);
    let updated_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string());
    let entry = SessionIndexEntry {
        id: thread_id.to_string(),
        thread_name: name.to_string(),
        updated_at,
    };
    let mut line = serde_json::to_vec(&entry)?;
    line.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.write_all(&line)
        .with_context(|| format!("appending {}", path.display()))?;
    file.flush()
        .with_context(|| format!("flushing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn test_home() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("space-codex-state-{nonce}"));
        fs::create_dir_all(&path).expect("create test home");
        path
    }

    fn create_catalog(home: &Path) -> Connection {
        let connection = Connection::open(home.join(STATE_DB_FILENAME)).expect("create catalog");
        connection
            .execute_batch(
                r#"
CREATE TABLE threads (
    id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    cwd TEXT NOT NULL,
    title TEXT NOT NULL,
    name TEXT,
    first_user_message TEXT NOT NULL DEFAULT '',
    preview TEXT NOT NULL DEFAULT '',
    updated_at INTEGER NOT NULL,
    updated_at_ms INTEGER,
    archived INTEGER NOT NULL DEFAULT 0,
    history_mode TEXT NOT NULL DEFAULT 'legacy'
);
"#,
            )
            .expect("create threads table");
        connection
    }

    #[test]
    fn catalog_filters_sources_and_resolves_mode_specific_names() {
        let home = test_home();
        let connection = create_catalog(&home);
        let insert = |id: &str,
                      source: &str,
                      title: &str,
                      first: &str,
                      preview: &str,
                      updated: i64,
                      mode: &str,
                      name: Option<&str>| {
            connection
                .execute(
                    "INSERT INTO threads (id, source, cwd, title, name, first_user_message, preview, updated_at, updated_at_ms, history_mode) VALUES (?1, ?2, '/repo', ?3, ?4, ?5, ?6, ?7, ?7 * 1000, ?8)",
                    params![id, source, title, name, first, preview, updated, mode],
                )
                .expect("insert thread");
        };
        insert(
            "legacy-auto",
            "cli",
            "prompt",
            "prompt",
            "prompt",
            10,
            "legacy",
            None,
        );
        insert(
            "legacy-custom",
            "cli",
            "Renamed",
            "prompt",
            "prompt",
            20,
            "legacy",
            None,
        );
        insert(
            "paginated",
            "vscode",
            "prompt",
            "prompt",
            "prompt",
            30,
            "paginated",
            Some("Paged name"),
        );
        insert("exec", "exec", "exec", "exec", "exec", 40, "legacy", None);
        insert("empty", "cli", "empty", "empty", "", 50, "legacy", None);
        append_session_index(&home, "legacy-auto", "Indexed name").expect("append index");

        let threads = list_threads_at(&home).expect("list threads");
        assert_eq!(
            threads
                .iter()
                .map(|thread| (thread.id.as_str(), thread.name.as_deref()))
                .collect::<Vec<_>>(),
            vec![
                ("paginated", Some("Paged name")),
                ("legacy-custom", Some("Renamed")),
                ("legacy-auto", Some("Indexed name")),
            ]
        );
        fs::remove_dir_all(home).expect("remove test home");
    }

    #[test]
    fn rename_mirrors_codex_history_mode_writes() {
        let home = test_home();
        let connection = create_catalog(&home);
        let legacy_id = "00000000-0000-0000-0000-000000000001";
        let paged_id = "00000000-0000-0000-0000-000000000002";
        for (id, mode) in [(legacy_id, "legacy"), (paged_id, "paginated")] {
            connection
                .execute(
                    "INSERT INTO threads (id, source, cwd, title, first_user_message, preview, updated_at, history_mode) VALUES (?1, 'cli', '/repo', 'prompt', 'prompt', 'prompt', 1, ?2)",
                    params![id, mode],
                )
                .expect("insert thread");
        }
        drop(connection);

        set_thread_name_at(&home, legacy_id, " Legacy name ").expect("rename legacy");
        set_thread_name_at(&home, paged_id, "Paged name").expect("rename paginated");

        let connection = open_state(&home, false).expect("reopen catalog");
        let legacy: (String, Option<String>) = connection
            .query_row(
                "SELECT title, name FROM threads WHERE id = ?1",
                [legacy_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read legacy");
        let paged: (String, Option<String>) = connection
            .query_row(
                "SELECT title, name FROM threads WHERE id = ?1",
                [paged_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read paginated");
        assert_eq!(legacy, ("Legacy name".to_string(), None));
        assert_eq!(
            paged,
            ("prompt".to_string(), Some("Paged name".to_string()))
        );
        let index = read_session_index(&home).expect("read index");
        assert_eq!(
            index.get(legacy_id).map(String::as_str),
            Some("Legacy name")
        );
        assert_eq!(index.get(paged_id).map(String::as_str), Some("Paged name"));
        drop(connection);
        fs::remove_dir_all(home).expect("remove test home");
    }

    #[test]
    fn rename_refuses_a_thread_with_an_active_writer() {
        let home = test_home();
        let thread_id = "00000000-0000-0000-0000-000000000003";
        let connection = create_catalog(&home);
        connection
            .execute(
                "INSERT INTO threads (id, source, cwd, title, first_user_message, preview, updated_at, history_mode) VALUES (?1, 'cli', '/repo', 'prompt', 'prompt', 'prompt', 1, 'legacy')",
                [thread_id],
            )
            .expect("insert thread");
        drop(connection);
        let active_writer = WriterLock::acquire(&home, thread_id).expect("acquire writer lock");

        let error = set_thread_name_at(&home, thread_id, "Blocked name")
            .expect_err("rename should fail while another writer is active");
        assert!(error.to_string().contains("already has an active writer"));
        let connection = open_state(&home, false).expect("reopen catalog");
        let title: String = connection
            .query_row(
                "SELECT title FROM threads WHERE id = ?1",
                [thread_id],
                |row| row.get(0),
            )
            .expect("read title");
        assert_eq!(title, "prompt");
        drop(connection);
        drop(active_writer);
        fs::remove_dir_all(home).expect("remove test home");
    }
}
