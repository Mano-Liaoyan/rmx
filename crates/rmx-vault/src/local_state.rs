//! Per-device state that never syncs: window layout, sidebar width, sync cursors, node identity.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::Result;

/// A small plaintext database beside the working copy.
///
/// Holds nothing secret and can be opened before the vault is unlocked, so the UI can restore its
/// layout first.
#[derive(Debug)]
pub struct LocalState {
    conn: Connection,
}

/// What the last successful sync with a backend established.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncCursor {
    /// The remote version; `None` when the server did not report one.
    pub version: Option<String>,
    /// Logical content digest of the working copy at that point.
    pub digest: String,
}

impl LocalState {
    /// The state file belonging to the working copy at `vault_path`.
    pub fn path_for(vault_path: &Path) -> PathBuf {
        let mut path = vault_path.as_os_str().to_owned();
        path.push(".local");
        PathBuf::from(path)
    }

    /// Opens or creates the state file at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        if !path.exists() {
            crate::key::create_private(path)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS sync_cursor (
                 backend TEXT PRIMARY KEY NOT NULL, version TEXT, digest TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS node (id INTEGER NOT NULL);",
        )?;
        Ok(Self { conn })
    }

    /// This device's HLC node id, chosen randomly on first use.
    pub fn node_id(&self) -> Result<u64> {
        let existing: Option<i64> =
            self.conn.query_row("SELECT id FROM node", [], |r| r.get(0)).optional()?;
        if let Some(id) = existing {
            return Ok(id as u64);
        }
        let id = getrandom::u64().map_err(|_| crate::error::Error::Kdf)?;
        self.conn.execute("INSERT INTO node (id) VALUES (?1)", [id as i64])?;
        Ok(id)
    }

    /// Reads an application value such as a serialized window layout.
    pub fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    /// Stores an application value.
    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        self.conn
            .execute("INSERT OR REPLACE INTO kv (key, value) VALUES (?1, ?2)", [key, value])?;
        Ok(())
    }

    /// Removes an application value.
    pub fn remove(&self, key: &str) -> Result<()> {
        self.conn.execute("DELETE FROM kv WHERE key = ?1", [key])?;
        Ok(())
    }

    /// The cursor of the last successful sync with `backend`.
    pub fn sync_cursor(&self, backend: &str) -> Result<Option<SyncCursor>> {
        Ok(self
            .conn
            .query_row(
                "SELECT version, digest FROM sync_cursor WHERE backend = ?1",
                [backend],
                |r| Ok(SyncCursor { version: r.get(0)?, digest: r.get(1)? }),
            )
            .optional()?)
    }

    /// Records the outcome of a successful sync with `backend`.
    pub fn set_sync_cursor(&self, backend: &str, cursor: &SyncCursor) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO sync_cursor (backend, version, digest) VALUES (?1, ?2, ?3)",
            params![backend, cursor.version, cursor.digest],
        )?;
        Ok(())
    }

    /// Forgets all cursors, so the next sync of every backend starts with a full download.
    pub fn clear_sync_cursors(&self) -> Result<()> {
        self.conn.execute("DELETE FROM sync_cursor", [])?;
        Ok(())
    }
}
