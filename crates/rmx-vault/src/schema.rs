//! Table definitions, and upgrading files written by older versions.
//!
//! Versions only ever add tables, so a newer build reads an older file as it is: a table the file
//! lacks is read as empty, by the merge as by the digest. A working copy is upgraded in place when
//! it is opened; a remote copy is left alone and gains the tables when this device next uploads.
//!
//! - 1: the first format.
//! - 2: `folder_logins`, the logins folders hand down ([`crate::FolderLogin`]).
//!
//! A build that predates a version refuses files of that version with
//! [`Error::UnsupportedFormat`] rather than dropping what it cannot read.

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Error, Result};
use crate::records::{Id, TABLES, TableDef};

/// Schema version of the vault format; part of the file, compared before any merge.
pub const SCHEMA_VERSION: i64 = 2;

/// The oldest version this build reads and upgrades.
const OLDEST_SUPPORTED: i64 = 1;

pub(crate) fn create(conn: &Connection) -> Result<Id> {
    let mut sql = String::from(
        "CREATE TABLE meta (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
         CREATE TABLE tombstones (
             table_name TEXT NOT NULL, row_id TEXT NOT NULL, hlc TEXT NOT NULL,
             PRIMARY KEY (table_name, row_id));
         CREATE TABLE history (
             table_name TEXT NOT NULL, row_id TEXT NOT NULL, hlc TEXT NOT NULL,
             payload TEXT NOT NULL,
             PRIMARY KEY (table_name, row_id, hlc, payload));",
    );
    for table in TABLES {
        sql += &create_table(table, "");
    }
    conn.execute_batch(&sql)?;
    let id = Id::new();
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('schema_version', ?1), ('vault_id', ?2)",
        params![SCHEMA_VERSION.to_string(), id],
    )?;
    Ok(id)
}

fn create_table(table: &TableDef, if_not_exists: &str) -> String {
    let columns: String = table.columns.iter().map(|(c, d)| format!(", {c} {d}")).collect();
    format!(
        "CREATE TABLE {if_not_exists} {} (id TEXT PRIMARY KEY NOT NULL, hlc TEXT NOT NULL{columns});",
        table.name
    )
}

/// The schema version of `schema`, which this build must be able to read.
pub(crate) fn check_version(conn: &Connection, schema: &str) -> Result<i64> {
    let found = meta(conn, schema, "schema_version")?
        .and_then(|v| v.parse::<i64>().ok())
        .ok_or(Error::NotAVault)?;
    if !(OLDEST_SUPPORTED..=SCHEMA_VERSION).contains(&found) {
        return Err(Error::UnsupportedFormat { found, supported: SCHEMA_VERSION });
    }
    Ok(found)
}

/// Checks the working copy's version and brings an older one up to [`SCHEMA_VERSION`].
pub(crate) fn open_and_upgrade(conn: &Connection) -> Result<()> {
    if check_version(conn, "main")? == SCHEMA_VERSION {
        return Ok(());
    }
    let mut sql = String::from("BEGIN IMMEDIATE;");
    for table in TABLES {
        sql += &create_table(table, "IF NOT EXISTS");
    }
    sql += &format!(
        "UPDATE meta SET value = '{SCHEMA_VERSION}' WHERE key = 'schema_version'; COMMIT;"
    );
    if let Err(e) = conn.execute_batch(&sql) {
        let _ = conn.execute_batch("ROLLBACK");
        return Err(e.into());
    }
    log::debug!("vault upgraded to schema version {SCHEMA_VERSION}");
    Ok(())
}

/// Whether `schema` has the table, which a file of an older version may not.
pub(crate) fn has_table(conn: &Connection, schema: &str, table: &TableDef) -> Result<bool> {
    Ok(conn
        .query_row(
            &format!("SELECT 1 FROM {schema}.sqlite_master WHERE type = 'table' AND name = ?1"),
            [table.name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

pub(crate) fn meta(conn: &Connection, schema: &str, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(&format!("SELECT value FROM {schema}.meta WHERE key = ?1"), [key], |r| r.get(0))
        .optional()?)
}
