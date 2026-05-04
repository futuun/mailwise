//! `mailwise index` -- initial sync, then a steady-state poll loop.

use anyhow::Result;
use rayon::prelude::*;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use tracing::info;

use crate::clients::{self, MailClient};
use crate::embeddings;
use crate::process_lock;
use crate::{db, settings};

/// Maximum emails per embedding-model context. Anything larger blows
/// past the GPU's KV-cache budget at the longest-sequence sizing we use.
const CHUNK_SIZE: usize = 256;

pub fn run(include_mailing_lists: bool) -> Result<()> {
    use std::time::Instant;
    let start = Instant::now();
    let cfg = settings::active();
    if cfg.clients.is_empty() {
        anyhow::bail!("No mail clients configured. Run `mailwise config` to set up.");
    }
    // Held for this process's lifetime so `config` (and any future
    // destructive command) can detect a running indexer before
    // clobbering the DB.
    let Some(_lock) = process_lock::try_acquire()? else {
        anyhow::bail!(
            "Another mailwise indexer is already running (foreground or launchd agent). \
             Stop it before running `mailwise index` again."
        );
    };
    let poll_interval = cfg.poll_interval();

    let db_path = db::default_db_path()?;
    info!("Database: {}", db_path.display());
    let conn = db::initialize(&db_path)?;

    let active: Vec<_> = clients::all_clients()
        .into_iter()
        .filter(|c| c.is_available())
        .collect();
    if active.is_empty() {
        let configured = cfg
            .clients
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!(
            "None of the configured clients ({configured}) are available on this machine. \
             Re-run `mailwise config` or check your config."
        );
    }
    let names: Vec<&str> = active.iter().map(|c| c.source().as_str()).collect();
    info!("Active sources: {}", names.join(", "));

    // Walk + diff before touching the embedder: model load is ~10s and
    // pointless if we have nothing new to embed.
    sync_all_clients(&active, &conn, true)?;
    info!("Loading embedding model...");
    let mut embedder = embeddings::Embedder::new()?;
    let embedded = embed_pending(&conn, &mut embedder, true, include_mailing_lists)?;

    if embedded > 0 {
        let elapsed = start.elapsed();
        let db_size = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
        info!("Initial sync complete:");
        info!("  Embedded: {embedded}");
        info!("  Time: {:.1}s", elapsed.as_secs_f64());
        info!("  DB size: {:.1} MB", db_size as f64 / 1_048_576.0);
    } else {
        info!("All emails up to date.");
    }

    // Sleep AFTER each cycle so a sync slower than `poll_interval` can't
    // queue overlapping polls.
    loop {
        thread::sleep(poll_interval);
        sync_all_clients(&active, &conn, false)?;
        embed_pending(&conn, &mut embedder, false, include_mailing_lists)?;
    }
}

/// One sync cycle across every active client, followed by a single
/// cross-source GC. GC runs after every client has settled so a message
/// present in two clients survives until the last one drops it.
fn sync_all_clients(
    active: &[Arc<dyn MailClient>],
    conn: &rusqlite::Connection,
    verbose: bool,
) -> Result<()> {
    // Worker-per-client gets us SQLite-level parallelism via WAL
    // (N readers + 1 writer). Steady-state polls do no writes so
    // all clients run fully concurrent.
    let db_path = PathBuf::from(
        conn.path()
            .ok_or_else(|| anyhow::anyhow!("connection has no on-disk path"))?,
    );

    active.par_iter().try_for_each(|client| -> Result<()> {
        let worker_conn = db::initialize(&db_path)?;
        clients::sync(client.as_ref(), &worker_conn, verbose)
    })?;

    let removed = db::gc_orphan_emails(conn)?;
    if verbose && removed > 0 {
        info!("GC: dropped {removed} orphan email(s) and their embeddings");
    }
    Ok(())
}

/// Drain `emails.embedded = FALSE` rows in length-sorted batches so each
/// embedding context is sized to the longest sequence in *its* batch
/// rather than the longest in the whole DB.
fn embed_pending(
    conn: &rusqlite::Connection,
    embedder: &mut embeddings::Embedder,
    show_progress: bool,
    include_mailing_lists: bool,
) -> Result<usize> {
    use std::io::IsTerminal as _;

    let total = db::count_unembedded(conn, include_mailing_lists)?;
    if total == 0 {
        return Ok(0);
    }

    let mut embedded = 0;

    loop {
        let batch = db::fetch_unembedded(conn, CHUNK_SIZE, include_mailing_lists)?;
        if batch.is_empty() {
            break;
        }

        let texts: Vec<String> = batch
            .iter()
            .map(|(_, e)| format!("{} {}", e.body_text, e.subject))
            .collect();

        let vectors = embedder.embed_documents(&texts)?;

        let tx = conn.unchecked_transaction()?;
        for ((id, _), vector) in batch.iter().zip(vectors.iter()) {
            db::mark_embedded(conn, *id, vector)?;
        }
        tx.commit()?;

        embedded += batch.len();

        if show_progress {
            let pct = (embedded as f64 / total as f64 * 100.0).floor() as u32;
            tick(format_args!("Embedding: {embedded}/{total} ({pct}%)"));
        } else {
            info!("Embedded {} new emails", batch.len());
        }
    }

    if show_progress && embedded > 0 && std::io::stdout().is_terminal() {
        println!();
    }

    Ok(embedded)
}

/// Carriage-return overwriting for TTYs; per-tick `info!` for pipes so
/// launchd's `indexer.log` still shows progress.
fn tick(msg: std::fmt::Arguments) {
    use std::io::IsTerminal as _;
    use std::io::Write;
    if std::io::stdout().is_terminal() {
        print!("\r{msg}");
        std::io::stdout().flush().ok();
    } else {
        info!("{}", msg);
    }
}
