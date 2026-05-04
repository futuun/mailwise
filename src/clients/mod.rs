//! Mail-client abstractions.
//!
//! Every backend implements [`MailClient`] by exposing two read-only
//! primitives: list `(Message-ID, locator)` pairs for every message on
//! disk ([`MailClient::list_locators`]), and parse one message given a
//! locator ([`MailClient::fetch_email`]). Reconciliation against
//! `email_sources` -- the diff, the gated removes, the parallel persist
//! -- lives in [`sync`] and runs identically for every backend.

use anyhow::Result;
use rayon::prelude::*;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, SyncSender};

use crate::db;
use crate::parser::Email;

/// Closed set of mail sources we know about. The string form is
/// stored in TOML config and the DB `source` column, so changing
/// [`Source::as_str`] requires a migration.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    AppleMail,
    Thunderbird,
    PlainMbox,
}

impl Source {
    /// Display order. Single source of truth for `mailwise config`'s
    /// pickers and the schema `CHECK` constraint.
    pub const ALL: &'static [Source] = &[Source::AppleMail, Source::Thunderbird, Source::PlainMbox];

    /// Stable kebab-case identifier. Persisted in TOML and the DB; do
    /// not change without a migration.
    pub const fn as_str(self) -> &'static str {
        match self {
            Source::AppleMail => "apple-mail",
            Source::Thunderbird => "thunderbird",
            Source::PlainMbox => "plain-mbox",
        }
    }

    /// Free to change without breaking stored data.
    pub const fn pretty(self) -> &'static str {
        match self {
            Source::AppleMail => "Apple Mail",
            Source::Thunderbird => "Thunderbird",
            Source::PlainMbox => "Plain mbox",
        }
    }
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Source {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "apple-mail" => Ok(Source::AppleMail),
            "thunderbird" => Ok(Source::Thunderbird),
            "plain-mbox" => Ok(Source::PlainMbox),
            other => anyhow::bail!("unknown mail source `{other}`"),
        }
    }
}

// Serialize via `as_str` so manually-edited TOML uses the user-facing
// kebab-case names (apple-mail, thunderbird, plain-mbox).
impl serde::Serialize for Source {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for Source {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Persist-batch size: one transaction per batch keeps per-row write
/// overhead amortized while bounding peak memory.
const SYNC_BATCH_SIZE: usize = 128;

/// Refuse deletes when a single sync would drop more than this fraction
/// of a source's stored rows -- almost always a transient scan glitch
/// (full-disk-access lost, profile transiently empty during a rebuild)
/// rather than the user actually deleting thousands of messages at once.
const REMOVAL_RATIO_LIMIT: f64 = 0.10;

#[derive(Debug)]
pub struct LocatorScan {
    /// Every `(Message-ID, locator)` pair the client found, in scan
    /// order. Duplicates kept so the caller can spot them.
    pub pairs: Vec<(String, String)>,
    /// True iff the walk visited every file successfully. False here
    /// makes [`sync`] refuse the destructive remove pass for this
    /// source -- a partial scan that looks like mass deletion is
    /// almost always a glitch we'd rather wait out than act on.
    pub scan_complete: bool,
}

pub mod apple_mail;
pub mod mbox;
pub mod plain_mbox;
pub mod thunderbird;

/// One mail source. A single mailwise install may have several active.
pub trait MailClient: Send + Sync {
    /// Which [`Source`] this client implements.
    fn source(&self) -> Source;

    /// True if this client is usable on the current machine
    /// (data directory present, etc.). Cheap, no I/O beyond a path stat.
    fn is_available(&self) -> bool;

    /// Open the message in whatever native UI this client has. Apple
    /// Mail and Thunderbird hand the Message-ID to their URL schemes;
    /// plain-mbox writes a preview file to `$TMPDIR` and hands it to
    /// the OS. `conn` is borrowed so plain-mbox can resolve locator
    /// without opening a second connection.
    fn open(&self, conn: &Connection, message_id: &str) -> Result<()>;

    /// Read-only walk of the client's data root. Returns every
    /// `(Message-ID, locator)` pair plus a `scan_complete` flag.
    fn list_locators(&self) -> Result<LocatorScan>;

    /// Parse the message at `locator` into an [`Email`]. Called once
    /// per new locator that the [`sync`] diff says needs a body.
    fn fetch_email(&self, locator: &str) -> Result<Email>;
}

fn instantiate(source: Source) -> Arc<dyn MailClient> {
    match source {
        Source::AppleMail => Arc::new(apple_mail::AppleMail::new()),
        Source::Thunderbird => Arc::new(thunderbird::Thunderbird::new()),
        Source::PlainMbox => Arc::new(plain_mbox::PlainMbox::new()),
    }
}

/// Resolve `message_id` to a client honoring user preference order,
/// then open. Bails when nothing has indexed the message.
pub fn open_message(conn: &Connection, message_id: &str) -> Result<()> {
    let prefs = preference_order();
    match crate::db::pick_open_source(conn, message_id, &prefs)? {
        Some(source) => instantiate(source).open(conn, message_id),
        None => anyhow::bail!(
            "No client has a locator for message {message_id}. \
             Has it been indexed?"
        ),
    }
}

/// Configured clients in preference order. When a message exists in
/// multiple clients, the first available one in this list wins the
/// open. Caller filters by [`MailClient::is_available`].
pub fn all_clients() -> Vec<Arc<dyn MailClient>> {
    crate::settings::active()
        .clients
        .iter()
        .copied()
        .map(instantiate)
        .collect()
}

/// Blocking-first-greedy-rest: park on `recv` so the consumer sleeps
/// when idle, then drain everything else queued without blocking.
/// `None` after all senders drop.
fn drain<T>(rx: &Receiver<T>, max: usize) -> Option<Vec<T>> {
    let mut batch = Vec::with_capacity(max);
    match rx.recv() {
        Ok(e) => batch.push(e),
        Err(_) => return None,
    }
    while batch.len() < max {
        match rx.try_recv() {
            Ok(e) => batch.push(e),
            Err(_) => break,
        }
    }
    Some(batch)
}

fn preference_order() -> Vec<Source> {
    crate::settings::active().clients.clone()
}

/// One full sync cycle for one client.
///
/// 1. `list_locators` scans the client's data root.
/// 2. Diff vs. this source's `email_sources` rows: anything new or
///    relocated goes into `to_persist`; anything stored but no longer
///    seen goes into `to_remove`.
/// 3. Pre-cleanup transaction: bulk-delete `to_remove` (gated by
///    `scan_complete` + 10% ratio) and every `to_persist` mid's
///    existing row. Clearing existing rows up front makes mbox
///    compactions safe -- two messages swapping locators would
///    otherwise trip the `(source, locator)` UNIQUE index mid-batch.
/// 4. Persist: classify each `to_persist` mid against
///    `emails.message_id`. Already-known mids (relocates and
///    cross-client dedup hits) just get a new `email_sources` row,
///    preserving the existing body and embedding; truly new mids
///    parse + insert into `emails`.
///
/// Cross-client cleanup of `emails` / `email_vectors` is deliberately
/// NOT done here -- a message present in multiple clients must survive
/// until the last one drops it. Caller runs
/// [`crate::db::gc_orphan_emails`] after every active client's
/// `sync` has returned.
pub fn sync(client: &dyn MailClient, conn: &Connection, verbose: bool) -> Result<()> {
    let source = client.source();
    let name = source.as_str();

    let scan = client.list_locators()?;
    let stored = db::email_sources_for(conn, source)?;

    // Last pair wins on collisions; the schema allows only one locator
    // per (source, message_id) anyway.
    let scanned: HashMap<String, String> = scan.pairs.into_iter().collect();

    let to_persist: Vec<(String, String)> = scanned
        .iter()
        .filter(|(mid, loc)| stored.get(*mid) != Some(*loc))
        .map(|(m, l)| (m.clone(), l.clone()))
        .collect();
    let to_remove: Vec<String> = stored
        .keys()
        .filter(|mid| !scanned.contains_key(*mid))
        .cloned()
        .collect();

    if verbose {
        tracing::info!(
            "[{name}] scan: {} pairs (complete={}) | DB has {} | persist={} remove={}",
            scanned.len(),
            scan.scan_complete,
            stored.len(),
            to_persist.len(),
            to_remove.len(),
        );
    }

    let allow_removes = if to_remove.is_empty() {
        true
    } else if !scan.scan_complete {
        tracing::warn!(
            "[{name}] skipping {} removes: scan had errors",
            to_remove.len()
        );
        false
    } else {
        let ratio = to_remove.len() as f64 / stored.len().max(1) as f64;
        if ratio > REMOVAL_RATIO_LIMIT {
            tracing::warn!(
                "[{name}] refusing to remove {} ({:.1}% of {}) -- likely scan glitch. \
                 Re-run after the source settles.",
                to_remove.len(),
                ratio * 100.0,
                stored.len(),
            );
            false
        } else {
            true
        }
    };

    // Pre-cleanup: drop everything that's about to change in one
    // transaction so the persist phase can't trip on stale UNIQUE
    // constraints from rows it's about to replace.
    if (allow_removes && !to_remove.is_empty()) || !to_persist.is_empty() {
        let dbtx = conn.unchecked_transaction()?;
        if allow_removes {
            db::delete_email_sources(conn, source, &to_remove)?;
        }
        if !to_persist.is_empty() {
            let mids: Vec<String> = to_persist.iter().map(|(m, _)| m.clone()).collect();
            db::delete_email_sources(conn, source, &mids)?;
        }
        dbtx.commit()?;
    }

    if !to_persist.is_empty() {
        let known_emails = db::all_message_ids(conn)?;
        process_persist(client, source, conn, &to_persist, &known_emails)?;
    }

    Ok(())
}

/// One unit of persist work.
#[derive(Debug)]
enum PersistItem {
    /// `mid` already has a row in `emails` (this source's previous
    /// poll, or another client). Just record the locator; the existing
    /// body and embedding stay put.
    Existing { message_id: String, locator: String },
    /// `mid` is genuinely new to `emails`. Body parsed; embedding will
    /// follow on the next embed pass.
    Fresh { locator: String, email: Email },
}

/// Parse fresh messages in parallel and persist everything via batched
/// transactions. Relocates and cross-client dedup hits skip the parse;
/// `known_emails` is the snapshot the producer checks against to make
/// that call.
fn process_persist(
    client: &dyn MailClient,
    source: Source,
    conn: &Connection,
    to_persist: &[(String, String)],
    known_emails: &HashSet<String>,
) -> Result<()> {
    let (tx, rx) = mpsc::sync_channel::<PersistItem>(SYNC_BATCH_SIZE);

    std::thread::scope(|s| -> Result<()> {
        s.spawn(move || feed_persist_items(client, to_persist, known_emails, tx));

        while let Some(items) = drain(&rx, SYNC_BATCH_SIZE) {
            let dbtx = conn.unchecked_transaction()?;
            for item in items {
                match item {
                    PersistItem::Existing {
                        message_id,
                        locator,
                    } => {
                        db::insert_email_source(conn, source, &message_id, &locator)?;
                    }
                    PersistItem::Fresh { locator, email } => {
                        db::insert_parsed_email(conn, source, &locator, &email)?;
                    }
                }
            }
            dbtx.commit()?;
        }

        Ok(())
    })?;

    Ok(())
}

fn feed_persist_items(
    client: &dyn MailClient,
    to_persist: &[(String, String)],
    known_emails: &HashSet<String>,
    tx: SyncSender<PersistItem>,
) {
    let _: Result<(), ()> = to_persist
        .par_iter()
        .try_for_each_with(tx, |tx, (mid, loc)| {
            if known_emails.contains(mid) {
                // Body's already in `emails` (relocate or cross-client
                // dedup). Skip the parse so we don't reset
                // `embedded = TRUE` and waste a re-embed.
                return tx
                    .send(PersistItem::Existing {
                        message_id: mid.clone(),
                        locator: loc.clone(),
                    })
                    .map_err(drop);
            }
            match client.fetch_email(loc) {
                Ok(email) => tx
                    .send(PersistItem::Fresh {
                        locator: loc.clone(),
                        email,
                    })
                    .map_err(drop),
                Err(e) => {
                    tracing::warn!("Failed to fetch {loc}: {e:#}");
                    Ok(())
                }
            }
        });
}
