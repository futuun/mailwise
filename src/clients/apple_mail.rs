//! Apple Mail (.emlx) source.
//!
//! Walks `~/Library/Mail/V<N>/` (highest available) and parses each
//! `.emlx` (byte-count line + RFC 2822 payload + trailing plist we
//! ignore). No retry on per-file read errors -- the next poll's fresh
//! scan picks up anything we missed, so a torn read just becomes a
//! re-parse one cycle later.
//!
//! Reconciliation (orphan GC, locator updates) lives in
//! [`super::sync`].

use anyhow::{Context, Result};
use rayon::prelude::*;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

use super::{LocatorScan, MailClient, Source};
use crate::parser::{self, Email, peek_message_id};

const SOURCE: Source = Source::AppleMail;

/// Parent of Apple Mail's per-version stores. Each macOS generation
/// may write a sibling `V<N>/` (V8 = Big Sur, V9 = Monterey, V10 =
/// Ventura through Tahoe / macOS 26); we pick the highest-numbered at
/// runtime. The V bump tracks Mail.app's SQLite envelope-index schema,
/// not the .emlx format itself, which has been effectively stable
/// since 2005 -- this parser should survive future bumps as-is.
const MAIL_PARENT: &str = "Library/Mail";

pub struct AppleMail;

impl AppleMail {
    pub fn new() -> Self {
        Self
    }
}

impl MailClient for AppleMail {
    fn source(&self) -> Source {
        SOURCE
    }

    fn is_available(&self) -> bool {
        mail_root().is_ok()
    }

    fn open(&self, _conn: &Connection, message_id: &str) -> Result<()> {
        let url = format!("message://%3C{message_id}%3E");
        std::process::Command::new("open")
            .arg(&url)
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to open Apple Mail: {e}"))?;
        Ok(())
    }

    fn list_locators(&self) -> Result<LocatorScan> {
        let root = mail_root()?;
        let (locators, scan_complete) = walk_emlx(&root);
        let pairs = locators
            .par_iter()
            .filter_map(|loc| peek_emlx_message_id(loc))
            .collect();
        Ok(LocatorScan {
            pairs,
            scan_complete,
        })
    }

    fn fetch_email(&self, locator: &str) -> Result<Email> {
        let raw = std::fs::read(locator).with_context(|| format!("reading {locator}"))?;
        let msg_bytes = extract_emlx_message(&raw)
            .with_context(|| format!("parsing .emlx framing for {locator}"))?;
        parser::build_email(msg_bytes)
    }
}

/// Slurp + extract emlx framing + peek the Message-ID header. Read
/// failures drop silently; next poll re-walks.
fn peek_emlx_message_id(locator: &str) -> Option<(String, String)> {
    let raw = std::fs::read(locator).ok()?;
    let msg = extract_emlx_message(&raw).ok()?;
    let mid = peek_message_id(msg)?;
    Some((mid, locator.to_string()))
}

/// .emlx framing: ASCII byte-count line, exactly that many bytes of
/// RFC 2822 message, then an Apple plist trailer we don't need.
fn extract_emlx_message(bytes: &[u8]) -> Result<&[u8]> {
    let nl = memchr::memchr(b'\n', bytes)
        .ok_or_else(|| anyhow::anyhow!("no newline after byte count"))?;
    let line = std::str::from_utf8(&bytes[..nl]).context("byte count line not valid UTF-8")?;
    let count: usize = line
        .trim()
        .parse()
        .with_context(|| format!("invalid byte count: {:?}", line.trim()))?;
    let start = nl + 1;
    let end = start
        .checked_add(count)
        .ok_or_else(|| anyhow::anyhow!("byte count overflow"))?;
    if end > bytes.len() {
        anyhow::bail!(
            "byte count ({count}) exceeds remaining file length ({})",
            bytes.len() - start
        );
    }
    Ok(&bytes[start..end])
}

/// Walk for `.emlx` files. Any walkdir error flips `walk_complete` to
/// false, which `super::sync` uses to gate the destructive remove
/// pass -- a transient I/O glitch shouldn't read as mass deletion.
fn walk_emlx(root: &Path) -> (Vec<String>, bool) {
    let mut entries = Vec::new();
    let mut walk_complete = true;

    for entry in walkdir::WalkDir::new(root) {
        let dent = match entry {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!("walk error: {err}");
                walk_complete = false;
                continue;
            }
        };
        if !dent.file_type().is_file() {
            continue;
        }
        if dent.path().extension().and_then(|x| x.to_str()) != Some("emlx") {
            continue;
        }
        entries.push(dent.into_path().to_string_lossy().into_owned());
    }

    (entries, walk_complete)
}

/// Highest-numbered `V<N>/` under `~/Library/Mail/`. See [`MAIL_PARENT`]
/// for why we pick by version number.
fn mail_root() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME environment variable not set")?;
    let parent = PathBuf::from(home).join(MAIL_PARENT);
    if !parent.is_dir() {
        anyhow::bail!(
            "Apple Mail parent directory not found at {}. Is Apple Mail configured?",
            parent.display()
        );
    }
    let entries =
        std::fs::read_dir(&parent).with_context(|| format!("reading {}", parent.display()))?;
    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name();
            let stripped = name.to_str()?.strip_prefix('V')?;
            let version: u32 = stripped.parse().ok()?;
            let path = e.path();
            if path.is_dir() {
                Some((version, path))
            } else {
                None
            }
        })
        .max_by_key(|(v, _)| *v)
        .map(|(_, p)| p)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No Apple Mail V<N>/ directory found under {}. Is Apple Mail configured?",
                parent.display()
            )
        })
}
