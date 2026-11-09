//! The row-level merge.
//!
//! For each row id a replica holds a set of content versions and at most one tombstone. Merging
//! is set union with per-element maximum, which makes it commutative, associative and idempotent:
//!
//! - the newest content version `(hlc, payload)` is the live row unless a tombstone is at least
//!   as new, in which case the row is gone and that version stays in `history`;
//! - a tombstone older than the live row is dropped: it can never win again;
//! - `history` keeps the other versions, trimmed to the newest [`HISTORY_LIMIT`]. "Newest k" of a
//!   union equals "newest k" of the unions of "newest k", so trimming preserves the laws.

use ring::digest::{self, SHA256};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};

use crate::error::Result;
use crate::hlc::Hlc;
use crate::records::{TABLES, TableDef};

/// Most superseded versions kept per live row. Part of the merge function: all devices must agree.
pub const HISTORY_LIMIT: u32 = 16;

/// Merges the attached schema `remote` into `main` in one transaction.
pub(crate) fn merge_attached(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "INSERT OR IGNORE INTO main.history SELECT * FROM remote.history;
         INSERT INTO main.tombstones SELECT * FROM remote.tombstones WHERE true
         ON CONFLICT (table_name, row_id) DO UPDATE SET hlc = excluded.hlc
         WHERE excluded.hlc > hlc;",
    )?;
    for table in TABLES {
        merge_table(&tx, table)?;
    }
    tx.commit()?;
    Ok(())
}

fn merge_table(tx: &Transaction<'_>, table: &TableDef) -> Result<()> {
    let (name, cols) = (table.name, table.column_list(""));
    let (live_payload, r_cols) = (table.payload_expr("l"), table.column_list("r"));
    let r_payload = table.payload_expr("r");

    // Every version of either side enters history; the winner is taken back out below.
    for schema in ["main", "remote"] {
        tx.execute(
            &format!(
                "INSERT OR IGNORE INTO main.history (table_name, row_id, hlc, payload)
                 SELECT '{name}', id, hlc, {} FROM {schema}.{name}",
                table.payload_expr(""),
            ),
            [],
        )?;
    }
    tx.execute_batch(&format!(
        "INSERT OR REPLACE INTO main.{name} (id, hlc, {cols})
           SELECT r.id, r.hlc, {r_cols} FROM remote.{name} r
           LEFT JOIN main.{name} l ON l.id = r.id
           WHERE l.id IS NULL OR (r.hlc, {r_payload}) > (l.hlc, {live_payload});
         DELETE FROM main.{name} AS x WHERE EXISTS (
           SELECT 1 FROM main.tombstones d
           WHERE d.table_name = '{name}' AND d.row_id = x.id AND d.hlc >= x.hlc);
         DELETE FROM main.tombstones AS d WHERE d.table_name = '{name}' AND EXISTS (
           SELECT 1 FROM main.{name} l WHERE l.id = d.row_id AND l.hlc > d.hlc);
         DELETE FROM main.history AS h WHERE h.table_name = '{name}' AND EXISTS (
           SELECT 1 FROM main.{name} l
           WHERE l.id = h.row_id AND l.hlc = h.hlc AND {live_payload} = h.payload);"
    ))?;
    trim_history(tx, table, None)
}

/// Enforces [`HISTORY_LIMIT`] for one row, or for the whole table.
pub(crate) fn trim_history(tx: &Connection, table: &TableDef, row: Option<&str>) -> Result<()> {
    let name = table.name;
    let filter = if row.is_some() { "AND row_id = ?1" } else { "" };
    // A deleted row has no live version, so it may keep one more in history; the total retained
    // per row is the same either way, which is what the union argument needs.
    let sql = format!(
        "DELETE FROM main.history WHERE rowid IN (
           SELECT rid FROM (
             SELECT rowid AS rid, row_id,
                    row_number() OVER (PARTITION BY row_id ORDER BY hlc DESC, payload DESC) AS n
             FROM main.history WHERE table_name = '{name}' {filter})
           WHERE n > {HISTORY_LIMIT}
             + (SELECT CASE WHEN EXISTS (SELECT 1 FROM main.{name} l WHERE l.id = row_id)
                THEN 0 ELSE 1 END))"
    );
    match row {
        Some(id) => tx.execute(&sql, [id])?,
        None => tx.execute(&sql, [])?,
    };
    Ok(())
}

/// Hash of the logical content of `schema`: equal digests mean converged replicas.
pub(crate) fn digest(conn: &Connection, schema: &str) -> Result<String> {
    let mut ctx = digest::Context::new(&SHA256);
    let mut feed = |sql: &str, columns: usize| -> Result<()> {
        let mut stmt = conn.prepare(sql)?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            for column in 0..columns {
                let value: String = row.get(column)?;
                ctx.update(&(value.len() as u64).to_le_bytes());
                ctx.update(value.as_bytes());
            }
        }
        ctx.update(&[0xff]);
        Ok(())
    };
    for table in TABLES {
        feed(
            &format!(
                "SELECT id, hlc, {} FROM {schema}.{} ORDER BY id",
                table.payload_expr(""),
                table.name
            ),
            3,
        )?;
    }
    feed(&format!("SELECT table_name, row_id, hlc FROM {schema}.tombstones ORDER BY 1, 2"), 3)?;
    feed(
        &format!(
            "SELECT table_name, row_id, hlc, payload FROM {schema}.history ORDER BY 1, 2, 3, 4"
        ),
        4,
    )?;
    let out = ctx.finish();
    Ok(out.as_ref().iter().map(|b| format!("{b:02x}")).collect())
}

/// The largest HLC stored in `schema`, for seeding the clock.
pub(crate) fn max_hlc(conn: &Connection, schema: &str) -> Result<Option<Hlc>> {
    let mut parts: Vec<String> =
        TABLES.iter().map(|t| format!("SELECT max(hlc) h FROM {schema}.{}", t.name)).collect();
    parts.push(format!("SELECT max(hlc) h FROM {schema}.tombstones"));
    parts.push(format!("SELECT max(hlc) h FROM {schema}.history"));
    let sql = format!("SELECT max(h) FROM ({})", parts.join(" UNION ALL "));
    let max: Option<String> = conn.query_row(&sql, [], |r| r.get(0)).optional()?.flatten();
    max.map(|s| s.parse()).transpose()
}
