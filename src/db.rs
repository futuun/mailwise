//! Schema, vector search, and the `email_sources` upserts that drive
//! the per-client scan-and-diff sync.

use crate::clients::Source;
use crate::parser::Email;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, ffi::sqlite3_auto_extension};
use sqlite_vec::sqlite3_vec_init;
use std::collections::HashMap;
use std::path::PathBuf;
use zerocopy::IntoBytes;

pub fn default_db_path() -> Result<PathBuf> {
    Ok(crate::settings::mailwise_dir()?.join("mailwise.db"))
}

/// Wire sqlite-vec into every future Connection via the auto-extension
/// hook. Must be called once before any `Connection::open`.
#[allow(clippy::missing_transmute_annotations)]
pub fn register_sqlite_vec() {
    unsafe {
        sqlite3_auto_extension(Some(std::mem::transmute(sqlite3_vec_init as *const ())));
    }
}

/// Open the DB, set pragmas, and ensure the schema. WAL journaling so
/// the indexer's writes don't block concurrent search reads (and
/// vice-versa) -- mandatory once the indexer is running as a background
/// launchd agent that the user might query against any moment.
pub fn initialize(db_path: &std::path::Path) -> Result<Connection> {
    let conn = Connection::open(db_path)?;

    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA busy_timeout = 5000;

         CREATE TABLE IF NOT EXISTS emails (
            id INTEGER PRIMARY KEY,
            message_id TEXT NOT NULL UNIQUE,
            date INTEGER NOT NULL DEFAULT 0,
            sender TEXT,
            recipient TEXT,
            subject TEXT,
            body_text TEXT,
            body_length INTEGER NOT NULL DEFAULT 0,
            embedded BOOLEAN NOT NULL DEFAULT FALSE,
            mailing_list BOOLEAN NOT NULL DEFAULT FALSE
        );

        CREATE TABLE IF NOT EXISTS email_sources (
            source TEXT NOT NULL CHECK(source IN ('apple-mail', 'thunderbird', 'plain-mbox')),
            message_id TEXT NOT NULL,
            locator TEXT NOT NULL,
            PRIMARY KEY (message_id, source)
        );

        CREATE UNIQUE INDEX IF NOT EXISTS email_sources_locator
            ON email_sources (source, locator);

        CREATE VIRTUAL TABLE IF NOT EXISTS email_vectors USING vec0(
            id INTEGER PRIMARY KEY,
            embedding FLOAT[768] distance_metric=cosine
        );",
    )?;

    Ok(conn)
}

/// Insert a freshly-parsed message into `emails` + `email_sources`.
/// Both inserts are OR IGNORE: the `emails` write no-ops on a relocate
/// or cross-client dedup hit (preserving the existing body and
/// embedding); the `email_sources` write tolerates a stale row that
/// the gated remove path didn't drop.
pub fn insert_parsed_email(
    conn: &Connection,
    source: Source,
    locator: &str,
    email: &Email,
) -> Result<()> {
    let body_length = email.subject.len() + email.body_text.len();

    conn.execute(
        "INSERT OR IGNORE INTO emails
            (message_id, date, sender, recipient, subject, body_text, body_length, embedded, mailing_list)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, FALSE, ?8)",
        rusqlite::params![
            email.message_id,
            email.date,
            email.from,
            email.to,
            email.subject,
            email.body_text,
            body_length as i64,
            email.is_mailing_list,
        ],
    )?;

    insert_email_source(conn, source, &email.message_id, locator)
}

/// Used for relocates and cross-client dedup hits where the body is
/// already in `emails`. Skipping the parse here is what preserves
/// `emails.embedded` and the existing vector for the message.
pub fn insert_email_source(
    conn: &Connection,
    source: Source,
    message_id: &str,
    locator: &str,
) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO email_sources (source, message_id, locator)
         VALUES (?1, ?2, ?3)",
        rusqlite::params![source.as_str(), message_id, locator],
    )?;
    Ok(())
}

/// Every `message_id` currently in `emails`, regardless of source.
/// `clients::sync` checks against this set to skip body parsing for
/// relocates and cross-client dedup hits.
pub fn all_message_ids(conn: &Connection) -> Result<std::collections::HashSet<String>> {
    let mut stmt = conn.prepare("SELECT message_id FROM emails")?;
    Ok(stmt
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?)
}

/// Snapshot of `(message_id -> locator)` for one source. Drives the
/// per-client diff in `clients::sync`.
pub fn email_sources_for(conn: &Connection, source: Source) -> Result<HashMap<String, String>> {
    let mut stmt =
        conn.prepare("SELECT message_id, locator FROM email_sources WHERE source = ?1")?;
    Ok(stmt
        .query_map([source.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<HashMap<_, _>, _>>()?)
}

/// Bulk delete `email_sources` rows for one source. Chunked because
/// SQLite caps bind parameters at `SQLITE_MAX_VARIABLE_NUMBER` (32766
/// in 3.32+); 16384 stays well under that and the 10% removal-ratio
/// gate caps any realistic batch well under the chunk size anyway.
pub fn delete_email_sources(
    conn: &Connection,
    source: Source,
    message_ids: &[String],
) -> Result<usize> {
    if message_ids.is_empty() {
        return Ok(0);
    }
    const CHUNK: usize = 16384;
    let mut deleted = 0;
    for chunk in message_ids.chunks(CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "DELETE FROM email_sources WHERE source = ? AND message_id IN ({placeholders})"
        );
        // params_from_iter wants one type, so pack `source` and the
        // chunk as owned Strings together.
        let mut params: Vec<String> = Vec::with_capacity(chunk.len() + 1);
        params.push(source.as_str().to_string());
        params.extend_from_slice(chunk);
        deleted += conn.execute(&sql, rusqlite::params_from_iter(params))?;
    }
    Ok(deleted)
}

/// Sweep `emails` + `email_vectors` for rows with no `email_sources`
/// row from any client. Run AFTER every active client's `sync` has
/// settled in the same poll cycle, otherwise a message present in two
/// clients gets dropped after the first one removes it.
pub fn gc_orphan_emails(conn: &Connection) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    // sqlite-vec's `email_vectors` is an external-content virtual table
    // (no FK cascades), so wipe vectors first while the join through
    // `emails.id` is still available.
    tx.execute(
        "DELETE FROM email_vectors WHERE id IN (
            SELECT e.id FROM emails e
            LEFT JOIN email_sources s ON s.message_id = e.message_id
            WHERE s.message_id IS NULL
        )",
        [],
    )?;
    let removed = tx.execute(
        "DELETE FROM emails WHERE message_id IN (
            SELECT e.message_id FROM emails e
            LEFT JOIN email_sources s ON s.message_id = e.message_id
            WHERE s.message_id IS NULL
        )",
        [],
    )?;
    tx.commit()?;
    Ok(removed)
}

pub fn count_unembedded(conn: &Connection, include_mailing_lists: bool) -> Result<usize> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM emails
         WHERE embedded = FALSE AND (?1 OR mailing_list = FALSE)",
        [include_mailing_lists],
        |row| row.get::<_, i64>(0).map(|n| n as usize),
    )?)
}

/// Body-length-sorted batch (shortest first) so the embedding context
/// allocated per batch is sized to that batch's longest sequence rather
/// than the corpus-wide max.
pub fn fetch_unembedded(
    conn: &Connection,
    limit: usize,
    include_mailing_lists: bool,
) -> Result<Vec<(i64, Email)>> {
    let mut stmt = conn.prepare(
        "SELECT id, message_id, sender, recipient, subject, date, body_text, mailing_list
         FROM emails
         WHERE embedded = FALSE AND (?1 OR mailing_list = FALSE)
         ORDER BY body_length ASC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![include_mailing_lists, limit as i64],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                Email {
                    message_id: row.get(1)?,
                    from: row.get(2)?,
                    to: row.get(3)?,
                    subject: row.get(4)?,
                    date: row.get(5)?,
                    body_text: row.get(6)?,
                    is_mailing_list: row.get(7)?,
                },
            ))
        },
    )?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn mark_embedded(conn: &Connection, id: i64, vector: &[f32]) -> Result<()> {
    conn.execute(
        "INSERT INTO email_vectors (id, embedding) VALUES (?1, ?2)",
        rusqlite::params![id, vector.as_bytes()],
    )?;
    conn.execute("UPDATE emails SET embedded = TRUE WHERE id = ?1", [id])?;
    Ok(())
}

/// KNN against `email_vectors`, returning `(id, distance)` pairs in
/// ascending-distance order.
pub fn search_vectors(
    conn: &Connection,
    query_vector: &[f32],
    limit: usize,
) -> Result<Vec<(i64, f64)>> {
    Ok(conn
        .prepare(
            "SELECT id, distance
             FROM email_vectors
             WHERE embedding MATCH ?1
             ORDER BY distance
             LIMIT ?2",
        )?
        .query_map(
            rusqlite::params![query_vector.as_bytes(), limit as i64],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?)),
        )?
        .collect::<Result<Vec<_>, _>>()?)
}

pub fn get_email_by_id(conn: &Connection, id: i64) -> Result<Option<Email>> {
    Ok(conn
        .prepare(
            "SELECT message_id, sender, recipient, subject, date, body_text, mailing_list
             FROM emails WHERE id = ?1",
        )?
        .query_row([id], |row| {
            Ok(Email {
                message_id: row.get(0)?,
                from: row.get(1)?,
                to: row.get(2)?,
                subject: row.get(3)?,
                date: row.get(4)?,
                body_text: row.get(5)?,
                is_mailing_list: row.get(6)?,
            })
        })
        .optional()?)
}

/// First entry in `preferences` that has a locator for `message_id`,
/// falling back to any source that does. `None` only when nobody has
/// indexed this message -- impossible for ids that came out of
/// `search_vectors`.
pub fn pick_open_source(
    conn: &Connection,
    message_id: &str,
    preferences: &[crate::clients::Source],
) -> Result<Option<crate::clients::Source>> {
    let available: Vec<crate::clients::Source> = conn
        .prepare("SELECT source FROM email_sources WHERE message_id = ?1")?
        .query_map([message_id], |row| row.get::<_, String>(0))?
        .filter_map(|r| r.ok().and_then(|s| s.parse().ok()))
        .collect();
    if available.is_empty() {
        return Ok(None);
    }
    for pref in preferences {
        if available.contains(pref) {
            return Ok(Some(*pref));
        }
    }
    Ok(available.into_iter().next())
}
