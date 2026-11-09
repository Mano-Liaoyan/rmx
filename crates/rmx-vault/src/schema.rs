//! Table definitions.

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Error, Result};
use crate::records::{Id, TABLES};

/// Schema version of the vault format; part of the file, compared before any merge.
pub const SCHEMA_VERSION: i64 = 1;

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
        let columns: String = table.columns.iter().map(|(c, d)| format!(", {c} {d}")).collect();
        sql += &format!(
            "CREATE TABLE {} (id TEXT PRIMARY KEY NOT NULL, hlc TEXT NOT NULL{columns});",
            table.name
        );
    }
    conn.execute_batch(&sql)?;
    let id = Id::new();
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('schema_version', ?1), ('vault_id', ?2)",
        params![SCHEMA_VERSION.to_string(), id],
    )?;
    Ok(id)
}

pub(crate) fn check_version(conn: &Connection, schema: &str) -> Result<()> {
    let found = meta(conn, schema, "schema_version")?
        .and_then(|v| v.parse::<i64>().ok())
        .ok_or(Error::NotAVault)?;
    if found != SCHEMA_VERSION {
        return Err(Error::UnsupportedFormat { found, supported: SCHEMA_VERSION });
    }
    Ok(())
}

pub(crate) fn meta(conn: &Connection, schema: &str, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(&format!("SELECT value FROM {schema}.meta WHERE key = ?1"), [key], |r| r.get(0))
        .optional()?)
}
