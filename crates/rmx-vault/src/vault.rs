//! The open vault: a local working copy of the database.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use log::debug;
use rusqlite::{
    Connection, ErrorCode, OpenFlags, OptionalExtension, TransactionBehavior, params,
    params_from_iter,
};
use zeroize::Zeroizing;

use crate::error::{Error, Result};
use crate::hlc::{Hlc, HlcClock};
use crate::key::{self, Credentials, KdfParams, VaultKey};
use crate::kind::VaultKind;
use crate::local_state::LocalState;
use crate::merge::{self, trim_history};
use crate::records::{Id, Record, TableDef};
use crate::schema;

/// The outcome of merging a snapshot into the working copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MergeReport {
    /// Whether the working copy gained anything.
    pub changed: bool,
}

/// A past (or the current) version of a row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revision<R> {
    /// When the version was written.
    pub hlc: Hlc,
    /// The row as it was.
    pub record: R,
}

pub(crate) struct Session {
    pub conn: Connection,
    /// `None` for a [plain](VaultKind::Plain) vault.
    pub key: Option<VaultKey>,
    pub clock: HlcClock,
}

/// A connection database, opened through a local working copy.
///
/// Dropping or [locking](Self::lock) it wipes the key. All methods block on disk, and the ones
/// that derive keys or sync also on Argon2id and the network: call them off the UI thread.
///
/// Never point this at a file inside a sync directory. Use [`sync`](Self::sync) instead.
#[derive(Debug)]
pub struct Vault {
    pub(crate) path: PathBuf,
    pub(crate) local: LocalState,
    pub(crate) session: Option<Session>,
    kind: VaultKind,
    idle_timeout: Option<Duration>,
    last_activity: Instant,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Session")
    }
}

impl Vault {
    /// Creates a new vault at `path`, which must not exist or must be empty.
    pub fn create(path: &Path, credentials: &Credentials, params: KdfParams) -> Result<Self> {
        claim_new_file(path)?;
        let key = credentials.derive(key::generate_salt(params)?, params)?;
        Self::initialize(path, Some(key))
    }

    /// Creates a new vault at `path` with no master password: a plain SQLite file.
    ///
    /// Everything in it, saved passwords and keys included, can be read by anyone who can read
    /// the file, here and on every sync target. Only for a user who has been told that and said
    /// so; empty [`Credentials`] are refused by [`create`](Self::create), so this is never reached
    /// by accident.
    pub fn create_plain(path: &Path) -> Result<Self> {
        claim_new_file(path)?;
        Self::initialize(path, None)
    }

    fn initialize(path: &Path, key: Option<VaultKey>) -> Result<Self> {
        let result = (|| {
            let conn = open_connection(path, key.as_ref())?;
            schema::create(&conn)?;
            conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
            Ok(conn)
        })();
        match result {
            Ok(conn) => Self::from_parts(path, conn, key),
            Err(e) => {
                remove_db_files(path);
                Err(e)
            },
        }
    }

    /// Opens the working copy at `path`.
    ///
    /// Fails with [`Error::WrongKey`] for a wrong password or key file.
    pub fn open(path: &Path, credentials: &Credentials) -> Result<Self> {
        let (salt, params) = key::parse_salt(&key::read_salt(path)?)?;
        let key = credentials.derive(salt, params)?;
        Self::open_with_key(path, key)
    }

    /// Opens the plain working copy at `path`, which needs no credentials.
    ///
    /// Fails with [`Error::NotAVault`] if the file is not a plain rmx vault.
    pub fn open_plain(path: &Path) -> Result<Self> {
        if VaultKind::of_file(path)? != Some(VaultKind::Plain) {
            return Err(Error::NotAVault);
        }
        let conn = open_connection(path, None)?;
        schema::check_version(&conn, "main").map_err(|e| match e {
            // Some other program's SQLite file.
            Error::Sqlite(_) => Error::NotAVault,
            other => other,
        })?;
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        Self::from_parts(path, conn, None)
    }

    pub(crate) fn open_with_key(path: &Path, key: VaultKey) -> Result<Self> {
        let conn = open_connection(path, Some(&key))?;
        schema::check_version(&conn, "main")?;
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        Self::from_parts(path, conn, Some(key))
    }

    fn from_parts(path: &Path, conn: Connection, key: Option<VaultKey>) -> Result<Self> {
        let local = LocalState::open(&LocalState::path_for(path))?;
        let mut vault = Self {
            path: path.to_owned(),
            local,
            session: None,
            kind: if key.is_some() { VaultKind::Encrypted } else { VaultKind::Plain },
            idle_timeout: None,
            last_activity: Instant::now(),
        };
        vault.install_session(conn, key)?;
        Ok(vault)
    }

    pub(crate) fn install_session(&mut self, conn: Connection, key: Option<VaultKey>) -> Result<()> {
        let mut clock = HlcClock::new(self.local.node_id()?);
        if let Some(seen) = merge::max_hlc(&conn, "main")? {
            clock.observe(seen);
        }
        self.session = Some(Session { conn, key, clock });
        self.last_activity = Instant::now();
        Ok(())
    }

    /// Path of the working copy.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the working copy is encrypted.
    pub fn kind(&self) -> VaultKind {
        self.kind
    }

    /// Device-local state stored beside the working copy.
    pub fn local_state(&self) -> &LocalState {
        &self.local
    }

    /// Whether the vault is locked. Applies the idle timeout first.
    pub fn is_locked(&mut self) -> bool {
        self.apply_idle_timeout();
        self.session.is_none()
    }

    /// Closes the database and wipes the key.
    ///
    /// A plain vault has no key to wipe and nothing to unlock it with, so it stays open.
    pub fn lock(&mut self) {
        if self.kind == VaultKind::Encrypted {
            self.session = None;
        }
    }

    /// Unlocks again after [`lock`](Self::lock) or the idle timeout.
    pub fn unlock(&mut self, credentials: &Credentials) -> Result<()> {
        if self.kind == VaultKind::Plain {
            return Ok(());
        }
        let (salt, params) = key::parse_salt(&key::read_salt(&self.path)?)?;
        let key = credentials.derive(salt, params)?;
        let conn = open_connection(&self.path, Some(&key))?;
        schema::check_version(&conn, "main")?;
        self.install_session(conn, Some(key))
    }

    /// Locks automatically once no operation has run for `timeout`; `None` disables it.
    ///
    /// Checked on every operation, so an expired vault refuses access with [`Error::Locked`]
    /// even if nothing polled [`is_locked`](Self::is_locked).
    pub fn set_idle_timeout(&mut self, timeout: Option<Duration>) {
        self.idle_timeout = timeout;
    }

    fn apply_idle_timeout(&mut self) {
        if self.idle_timeout.is_some_and(|limit| self.last_activity.elapsed() >= limit) {
            self.lock();
        }
    }

    pub(crate) fn session(&mut self) -> Result<&mut Session> {
        self.apply_idle_timeout();
        self.last_activity = Instant::now();
        self.session.as_mut().ok_or(Error::Locked)
    }

    /// Replaces the wall-clock source of the HLC, for tests that simulate skewed devices.
    pub fn set_time_source(&mut self, time: impl Fn() -> u64 + Send + 'static) -> Result<()> {
        self.session()?.clock.set_time_source(time);
        Ok(())
    }

    /// The vault's identity, shared by all its replicas.
    pub fn vault_id(&mut self) -> Result<String> {
        let session = self.session()?;
        schema::meta(&session.conn, "main", "vault_id")?.ok_or(Error::NotAVault)
    }

    /// Fetches one row.
    pub fn get<R: Record>(&mut self, id: Id) -> Result<Option<R>> {
        let table = R::TABLE;
        let session = self.session()?;
        Ok(session
            .conn
            .query_row(
                &format!("SELECT {} FROM {} WHERE id = ?1", table.column_list(""), table.name),
                [id],
                |row| R::from_row(id, row, 0),
            )
            .optional()?)
    }

    /// Lists every row of a table.
    pub fn list<R: Record>(&mut self) -> Result<Vec<R>> {
        let table = R::TABLE;
        let session = self.session()?;
        let mut stmt = session.conn.prepare(&format!(
            "SELECT id, {} FROM {} ORDER BY id",
            table.column_list(""),
            table.name
        ))?;
        let rows = stmt.query_map([], |row| R::from_row(row.get(0)?, row, 1))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Inserts or updates a row. The previous version moves to history.
    ///
    /// A write identical to the live row is a no-op. Writing a deleted id brings it back.
    pub fn put<R: Record>(&mut self, record: &R) -> Result<()> {
        let table = R::TABLE;
        let id = record.id();
        let id_text = id.to_string();
        let values = record.values();
        let session = self.session()?;
        let hlc = session.clock.now();
        let tx = session.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let placeholders = vec!["?"; values.len()].join(", ");
        let payload: String = tx.query_row(
            &format!("SELECT json_array({placeholders})"),
            params_from_iter(values.iter()),
            |r| r.get(0),
        )?;
        let current = current_version(&tx, table, &id_text)?;
        if current.as_ref().is_some_and(|(_, p)| *p == payload) {
            return Ok(());
        }
        if let Some((old_hlc, old_payload)) = &current {
            add_history(&tx, table, &id_text, old_hlc, old_payload)?;
        }
        tx.execute(
            &format!(
                "INSERT OR REPLACE INTO {} (id, hlc, {}) VALUES (?, ?, {placeholders})",
                table.name,
                table.column_list("")
            ),
            params_from_iter(
                [&id_text as &dyn rusqlite::ToSql, &hlc.to_string() as &dyn rusqlite::ToSql]
                    .into_iter()
                    .chain(values.iter().copied()),
            ),
        )?;
        tx.execute(
            "DELETE FROM tombstones WHERE table_name = ?1 AND row_id = ?2",
            [table.name, &id_text],
        )?;
        trim_history(&tx, table, Some(&id_text))?;
        tx.commit()?;
        Ok(())
    }

    /// Deletes a row, leaving a tombstone that propagates the delete. The last version stays in
    /// history. Returns whether the row existed.
    pub fn delete<R: Record>(&mut self, id: Id) -> Result<bool> {
        let table = R::TABLE;
        let id_text = id.to_string();
        let session = self.session()?;
        let hlc = session.clock.now();
        let tx = session.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some((old_hlc, old_payload)) = current_version(&tx, table, &id_text)? else {
            return Ok(false);
        };
        add_history(&tx, table, &id_text, &old_hlc, &old_payload)?;
        tx.execute(&format!("DELETE FROM {} WHERE id = ?1", table.name), [&id_text])?;
        tx.execute(
            "INSERT OR REPLACE INTO tombstones (table_name, row_id, hlc) VALUES (?1, ?2, ?3)",
            params![table.name, id_text, hlc.to_string()],
        )?;
        trim_history(&tx, table, Some(&id_text))?;
        tx.commit()?;
        Ok(true)
    }

    /// The current version of a row with its write time.
    pub fn revision<R: Record>(&mut self, id: Id) -> Result<Option<Revision<R>>> {
        let table = R::TABLE;
        let session = self.session()?;
        session
            .conn
            .query_row(
                &format!("SELECT hlc, {} FROM {} WHERE id = ?1", table.column_list(""), table.name),
                [id],
                |row| Ok((row.get::<_, String>(0)?, R::from_row(id, row, 1)?)),
            )
            .optional()?
            .map(|(hlc, record)| Ok(Revision { hlc: hlc.parse()?, record }))
            .transpose()
    }

    /// Superseded versions of a row, newest first.
    pub fn history<R: Record>(&mut self, id: Id) -> Result<Vec<Revision<R>>> {
        let table = R::TABLE;
        let extract: Vec<String> =
            (0..table.columns.len()).map(|i| format!("json_extract(payload, '$[{i}]')")).collect();
        let session = self.session()?;
        let mut stmt = session.conn.prepare(&format!(
            "SELECT hlc, {} FROM history WHERE table_name = ?1 AND row_id = ?2
             ORDER BY hlc DESC, payload DESC",
            extract.join(", ")
        ))?;
        let rows = stmt
            .query_map(params![table.name, id], |row| {
                Ok((row.get::<_, String>(0)?, R::from_row(id, row, 1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(|(hlc, record)| Ok(Revision { hlc: hlc.parse()?, record })).collect()
    }

    /// Makes the version written at `hlc` the live row again, as a new write.
    pub fn restore<R: Record>(&mut self, id: Id, hlc: Hlc) -> Result<()> {
        let revision =
            self.history::<R>(id)?.into_iter().find(|r| r.hlc == hlc).ok_or(Error::NotFound)?;
        self.put(&revision.record)
    }

    /// Writes a consistent single-file copy to `dest`, which must not exist.
    ///
    /// The copy has no WAL, is of the same [kind](VaultKind) as the working copy, and opens with
    /// the same credentials.
    pub fn snapshot(&mut self, dest: &Path) -> Result<()> {
        let path = utf8(dest)?.to_owned();
        key::create_private(dest)?;
        let session = self.session()?;
        if let Err(e) = session.conn.execute("VACUUM INTO ?1", [path]) {
            remove_db_files(dest);
            return Err(e.into());
        }
        Ok(())
    }

    /// Merges the snapshot at `path` into the working copy, in one transaction.
    pub fn merge_snapshot(&mut self, path: &Path) -> Result<MergeReport> {
        let key = self.session()?.key.clone();
        let (report, _) = self.merge_file(path, key.as_ref())?;
        Ok(report)
    }

    /// Hash of the logical content. Replicas that converged have equal digests.
    pub fn digest(&mut self) -> Result<String> {
        merge::digest(&self.session()?.conn, "main")
    }

    /// Merges the vault file at `path`, decrypting it with `key` unless it is a plain one, which
    /// is what `key` being `None` expects. Returns the file's own digest.
    pub(crate) fn merge_file(
        &mut self,
        path: &Path,
        key: Option<&VaultKey>,
    ) -> Result<(MergeReport, String)> {
        let remote = VaultKind::of_file(path)?.ok_or(Error::RemoteCorrupt)?;
        if remote != self.kind {
            return Err(Error::KindMismatch { local: self.kind, remote });
        }
        if let Some(key) = key {
            let salt = key::read_salt(path).map_err(|e| match e {
                Error::NotAVault => Error::RemoteCorrupt,
                other => other,
            })?;
            if salt != key.salt() {
                return Err(Error::RemoteKeyChanged);
            }
        }
        let path = utf8(path)?.to_owned();
        // SQLCipher reads an empty key as "no encryption".
        let keyspec = key.map_or_else(|| Zeroizing::new(String::new()), VaultKey::keyspec);
        let session = self.session()?;
        session
            .conn
            .execute("ATTACH DATABASE ?1 AS remote KEY ?2", params![path, keyspec.as_str()])
            .map_err(|e| if is_not_a_database(&e) { Error::RemoteCorrupt } else { e.into() })?;
        drop(keyspec);
        let result = merge_attached(session);
        let detached = session.conn.execute_batch("DETACH DATABASE remote");
        let merged = result?;
        detached?;
        Ok(merged)
    }

    /// Changes the master password or key file, re-encrypting the working copy.
    ///
    /// Remote copies keep the old credentials until
    /// [`push_new_credentials`](Self::push_new_credentials) replaces them. A plain vault has none
    /// to change: [`Error::Unencrypted`].
    pub fn change_credentials(
        &mut self,
        credentials: &Credentials,
        params: KdfParams,
    ) -> Result<()> {
        self.require_encrypted()?;
        let key = credentials.derive(key::generate_salt(params)?, params)?;
        self.reencrypt(key)?;
        self.local.clear_sync_cursors()
    }

    /// Checks that `credentials` are the ones the working copy is encrypted with, as before
    /// [`change_credentials`](Self::change_credentials): [`Error::WrongKey`] if not.
    pub fn check_credentials(&mut self, credentials: &Credentials) -> Result<()> {
        self.require_encrypted()?;
        let (_, params) = key::parse_salt(&key::read_salt(&self.path)?)?;
        let session = self.session()?;
        let current = session.key.as_ref().ok_or(Error::Locked)?;
        let derived = credentials.derive(current.salt(), params)?;
        if derived.same_as(current) { Ok(()) } else { Err(Error::WrongKey) }
    }

    pub(crate) fn require_encrypted(&self) -> Result<()> {
        match self.kind {
            VaultKind::Encrypted => Ok(()),
            VaultKind::Plain => Err(Error::Unencrypted),
        }
    }

    /// Replaces the working copy's encryption with `key`, atomically.
    pub(crate) fn reencrypt(&mut self, key: VaultKey) -> Result<()> {
        let tmp = TempDb::new(&self.path, "rekey");
        key::create_private(tmp.path())?;
        let keyspec = key.keyspec();
        let tmp_utf8 = utf8(tmp.path())?.to_owned();
        let session = self.session()?;
        session
            .conn
            .execute("ATTACH DATABASE ?1 AS rekey KEY ?2", params![tmp_utf8, keyspec.as_str()])?;
        drop(keyspec);
        let exported = session.conn.query_row("SELECT sqlcipher_export('rekey')", [], |_| Ok(()));
        session.conn.execute_batch("DETACH DATABASE rekey")?;
        exported?;
        let clock_seed = merge::max_hlc(&session.conn, "main")?;
        self.session = None;
        remove_wal_files(&self.path);
        fs::rename(tmp.path(), &self.path)?;
        let conn = open_connection(&self.path, Some(&key))?;
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        debug!("working copy re-encrypted");
        self.install_session(conn, Some(key))?;
        if let (Some(seen), Some(session)) = (clock_seed, self.session.as_mut()) {
            session.clock.observe(seen);
        }
        Ok(())
    }
}

fn merge_attached(session: &mut Session) -> Result<(MergeReport, String)> {
    let check = schema::check_version(&session.conn, "remote");
    match check {
        Err(Error::Sqlite(e)) if is_not_a_database(&e) => return Err(Error::RemoteCorrupt),
        other => other?,
    }
    let ours = schema::meta(&session.conn, "main", "vault_id")?;
    if ours != schema::meta(&session.conn, "remote", "vault_id")? {
        return Err(Error::VaultMismatch);
    }
    let remote_digest = merge::digest(&session.conn, "remote")?;
    let before = merge::digest(&session.conn, "main")?;
    merge::merge_attached(&mut session.conn)?;
    if let Some(seen) = merge::max_hlc(&session.conn, "remote")? {
        session.clock.observe(seen);
    }
    let changed = before != merge::digest(&session.conn, "main")?;
    Ok((MergeReport { changed }, remote_digest))
}

fn current_version(
    conn: &Connection,
    table: &TableDef,
    id: &str,
) -> Result<Option<(String, String)>> {
    Ok(conn
        .query_row(
            &format!("SELECT hlc, {} FROM {} WHERE id = ?1", table.payload_expr(""), table.name),
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

fn add_history(
    conn: &Connection,
    table: &TableDef,
    id: &str,
    hlc: &str,
    payload: &str,
) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO history (table_name, row_id, hlc, payload)
         VALUES (?1, ?2, ?3, ?4)",
        [table.name, id, hlc, payload],
    )?;
    Ok(())
}

fn is_not_a_database(e: &rusqlite::Error) -> bool {
    e.sqlite_error_code() == Some(ErrorCode::NotADatabase)
}

/// Makes sure nothing but an empty file is at `path`, creating it private to its owner.
fn claim_new_file(path: &Path) -> Result<()> {
    match fs::metadata(path) {
        Ok(m) if m.len() > 0 => Err(Error::AlreadyExists(path.to_owned())),
        Ok(_) => Ok(()),
        Err(_) => {
            key::create_private(path)?;
            Ok(())
        },
    }
}

fn open_connection(path: &Path, key: Option<&VaultKey>) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    // SQLCipher otherwise prints every failed unlock to stderr.
    conn.execute_batch("PRAGMA cipher_log_level = NONE;")?;
    if let Some(key) = key {
        let statement = Zeroizing::new(format!("PRAGMA key = \"{}\";", key.keyspec().as_str()));
        conn.execute_batch(&statement)?;
    }
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))
        .map_err(|e| match (is_not_a_database(&e), key) {
            (true, Some(_)) => Error::WrongKey,
            (true, None) => Error::NotAVault,
            (false, _) => e.into(),
        })?;
    conn.execute_batch("PRAGMA secure_delete = ON;")?;
    Ok(conn)
}

fn utf8(path: &Path) -> Result<&str> {
    path.to_str().ok_or_else(|| Error::InvalidPath(path.to_owned()))
}

fn remove_wal_files(path: &Path) {
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        let _ = fs::remove_file(name);
    }
}

pub(crate) fn remove_db_files(path: &Path) {
    let _ = fs::remove_file(path);
    remove_wal_files(path);
}

/// A database file beside the working copy, removed with its sidecars on drop.
pub(crate) struct TempDb(PathBuf);

impl TempDb {
    pub(crate) fn new(working_copy: &Path, purpose: &str) -> Self {
        let mut name = working_copy.as_os_str().to_owned();
        name.push(format!(".{purpose}-{}", Id::new()));
        Self(PathBuf::from(name))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        remove_db_files(&self.0);
    }
}
