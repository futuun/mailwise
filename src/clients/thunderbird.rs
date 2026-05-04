//! Thunderbird (mbox) source.
//!
//! Walks every IMAP-account mbox under
//! `~/Library/Thunderbird/Profiles/<profile>/ImapMail/<host>/`, hands
//! each file to the shared [`mbox`] driver, and supplies the two
//! flavor-specific bits the generic code can't know:
//!
//! * a `.msf` sidecar predicate so [`mbox::collect_mboxes`] only picks
//!   up real mail folders (stub IMAP folders that haven't been synced
//!   locally have a `.msf` without a matching mbox),
//! * a [`should_skip`] that honors `X-Mozilla-Status` /
//!   `X-Mozilla-Status2` deletion bits so expunged or IMAP-deleted
//!   messages stay out of the index.
//!
//! Subfolders live in sibling `<parent>.sbd/` directories;
//! [`mbox::collect_mboxes`] recurses unconditionally so they appear
//! without extra logic here.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

use super::mbox;
use super::{LocatorScan, MailClient, Source};
use crate::parser::Email;

const SOURCE: Source = Source::Thunderbird;

/// Profiles directory, relative to `$HOME`.
const PROFILES_ROOT: &str = "Library/Thunderbird/Profiles";

/// `X-Mozilla-Status` bit: message is expunged.
const STATUS_EXPUNGED: u16 = 0x0008;

/// `X-Mozilla-Status2` bit: user deleted the message in IMAP.
const STATUS2_IMAP_DELETED: u32 = 0x0001_0000;

pub struct Thunderbird;

impl Thunderbird {
    pub fn new() -> Self {
        Self
    }
}

impl MailClient for Thunderbird {
    fn source(&self) -> Source {
        SOURCE
    }

    fn is_available(&self) -> bool {
        let Ok(home) = std::env::var("HOME") else {
            return false;
        };
        PathBuf::from(home).join(PROFILES_ROOT).exists()
    }

    fn open(&self, _conn: &Connection, message_id: &str) -> Result<()> {
        let mid = format!("mid:{message_id}");
        std::process::Command::new("open")
            .args(["-a", "Thunderbird"])
            .arg(&mid)
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to launch Thunderbird: {e}"))?;
        Ok(())
    }

    fn list_locators(&self) -> Result<LocatorScan> {
        mbox::list_locators(enumerate_mboxes, should_skip)
    }

    fn fetch_email(&self, locator: &str) -> Result<Email> {
        mbox::fetch_email(locator)
    }
}

// ---------------------------------------------------------------------------
// Filesystem layout
// ---------------------------------------------------------------------------

/// Walk every Thunderbird profile and collect mbox files under each
/// IMAP account. The `.msf` sidecar (Mork index) is the marker for a
/// real mail folder -- Thunderbird writes it for synced folders, but
/// not-yet-synced stub folders have a `.msf` without a paired mbox,
/// and those we silently skip via [`has_msf_sidecar`].
fn enumerate_mboxes() -> Result<Vec<PathBuf>> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let profiles = PathBuf::from(home).join(PROFILES_ROOT);

    let mut mboxes = Vec::new();
    let profile_iter = match std::fs::read_dir(&profiles) {
        Ok(it) => it,
        Err(_) => {
            anyhow::bail!(
                "Thunderbird profiles directory not found at {}",
                profiles.display()
            );
        }
    };
    for entry in profile_iter {
        let profile_dir = entry?.path();
        let imap_root = profile_dir.join("ImapMail");
        if !imap_root.is_dir() {
            continue;
        }
        for account in std::fs::read_dir(&imap_root)? {
            let account = account?.path();
            if account.is_dir() {
                mbox::collect_mboxes(&account, has_msf_sidecar, &mut mboxes);
            }
        }
    }
    Ok(mboxes)
}

/// Predicate for [`mbox::collect_mboxes`]: Thunderbird tags every real
/// mail folder with a `<name>.msf` Mork index sidecar.
fn has_msf_sidecar(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if path.extension().is_some_and(|e| e == "msf") {
        return false; // the sidecar itself, not the mbox
    }
    path.with_file_name(format!("{name}.msf")).exists()
}

// ---------------------------------------------------------------------------
// Per-message filter
// ---------------------------------------------------------------------------

/// Honor Thunderbird's two synthetic flag headers (always written as
/// the first two of every stored message). Hot path -- runs once per
/// envelope on rayon, so it caps the scan at the first 256 bytes
/// instead of hauling multi-MB attachments through `.lines()`.
fn should_skip(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(256)];
    let head = std::str::from_utf8(head).unwrap_or("");

    let status: u16 = read_hex_header(head, "X-Mozilla-Status:")
        .and_then(|v| u16::try_from(v).ok())
        .unwrap_or(0);
    if status & STATUS_EXPUNGED != 0 {
        return true;
    }

    let status2: u32 = read_hex_header(head, "X-Mozilla-Status2:").unwrap_or(0);
    if status2 & STATUS2_IMAP_DELETED != 0 {
        return true;
    }

    false
}

fn read_hex_header(text: &str, name: &str) -> Option<u32> {
    text.lines()
        .find(|l| l.starts_with(name))
        .and_then(|l| l[name.len()..].split_whitespace().next())
        .and_then(|h| u32::from_str_radix(h, 16).ok())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_expunged_messages() {
        let head = b"X-Mozilla-Status: 0008\nX-Mozilla-Status2: 00000000\n";
        assert!(should_skip(head));
    }

    #[test]
    fn skip_imap_deleted_messages() {
        let head = b"X-Mozilla-Status: 0001\nX-Mozilla-Status2: 00010000\n";
        assert!(should_skip(head));
    }

    #[test]
    fn keep_normal_messages() {
        let head = b"X-Mozilla-Status: 0001\nX-Mozilla-Status2: 00000000\n";
        assert!(!should_skip(head));
    }

    #[test]
    fn keep_messages_without_flag_headers() {
        let head = b"From: alice@example.com\nSubject: hi\n";
        assert!(!should_skip(head));
    }
}
