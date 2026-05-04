//! Plain mboxrd source -- a directory of mbox files (Gmail Takeout
//! dumps, Apple Mail "Export Mailbox" output, mailing-list archives,
//! anything mboxrd).
//!
//! No deletion flags, no sidecar marker, no per-client locator quirks
//! -- just feed every file through the shared [`mbox`] driver with a
//! no-op `should_skip`. Extensions vary in the wild (`.mbox`, `.txt`,
//! none for Apple Mail exports, `<list>-YYYY-MM-DD.mbox` for Mailman),
//! so we sniff for an envelope rather than gating on extension.
//!
//! Root comes from `config.plain_mbox_path`; without it the client
//! reports unavailable.

use anyhow::{Context, Result};
use mailparse::MailHeaderMap;
use rusqlite::Connection;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use super::mbox;
use super::{LocatorScan, MailClient, Source};
use crate::parser::Email;

const SOURCE: Source = Source::PlainMbox;

pub struct PlainMbox {
    root: Option<PathBuf>,
}

impl PlainMbox {
    pub fn new() -> Self {
        Self {
            root: crate::settings::active().plain_mbox_root(),
        }
    }
}

impl MailClient for PlainMbox {
    fn source(&self) -> Source {
        SOURCE
    }

    fn is_available(&self) -> bool {
        self.root.as_deref().is_some_and(Path::is_dir)
    }

    fn list_locators(&self) -> Result<LocatorScan> {
        let root = self.root.clone();
        // No flag headers in plain mboxes; every message stays.
        mbox::list_locators(move || enumerate_mboxes(root.as_deref()), |_| false)
    }

    fn fetch_email(&self, locator: &str) -> Result<Email> {
        mbox::fetch_email(locator)
    }

    fn open(&self, conn: &Connection, message_id: &str) -> Result<()> {
        // No UI to launch into; write a preview to $TMPDIR and hand it
        // to the OS. `.html` for HTML messages (browser), `.txt` with
        // raw RFC 2822 bytes for everything else (TextEdit) -- the raw
        // text keeps Subject / From / Date and mailing-list trailers
        // visible without rendering them away.
        let locator: String = conn
            .query_row(
                "SELECT locator FROM email_sources WHERE message_id = ?1 AND source = ?2",
                rusqlite::params![message_id, SOURCE.as_str()],
                |row| row.get(0),
            )
            .with_context(|| format!("No plain-mbox locator for message {message_id}"))?;

        let bytes = read_message_bytes(&locator)?;

        if crate::settings::active().open_google_takeout_in_gmail
            && let Some(url) = gmail_url_from_headers(&bytes)
        {
            std::process::Command::new("open")
                .arg(&url)
                .spawn()
                .with_context(|| format!("opening {url}"))?;
            return Ok(());
        }

        let (preview_bytes, ext) = match extract_html_body(&bytes) {
            Some(html) => (html.into_bytes(), "html"),
            None => (bytes, "txt"),
        };

        let path = preview_path(message_id, ext);
        std::fs::write(&path, &preview_bytes)
            .with_context(|| format!("writing preview to {}", path.display()))?;
        std::process::Command::new("open")
            .arg(&path)
            .spawn()
            .with_context(|| format!("opening {}", path.display()))?;
        println!("preview: {}", path.display());
        Ok(())
    }
}

/// Build a Gmail web URL from `X-GM-THRID` (and `X-GM-MSGID` when
/// present, for message-level deep links). `None` when the headers are
/// absent or unparseable -- e.g. a non-Takeout mbox file mixed into
/// the plain-mbox folder. Caller falls back to the local preview.
///
/// Encoding: base64-encode `f:<thrid>` (no padding), then re-interpret
/// that base64 string as a base-64 number and emit it in a 40-char
/// reduced alphabet.
fn gmail_url_from_headers(bytes: &[u8]) -> Option<String> {
    let (headers, _) = mailparse::parse_headers(bytes).ok()?;
    let thrid = headers
        .get_first_value("X-GM-THRID")
        .and_then(|v| v.trim().parse::<u64>().ok())?;
    let thread_token = encode_gmail_token(thrid);
    let url = match headers
        .get_first_value("X-GM-MSGID")
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        Some(msgid) => format!(
            "https://mail.google.com/mail/u/0/#inbox/{thread_token}/{}",
            encode_gmail_token(msgid),
        ),
        None => format!("https://mail.google.com/mail/u/0/#inbox/{thread_token}"),
    };
    Some(url)
}

const GMAIL_TOKEN_REDUCED: &[u8; 40] = b"BCDFGHJKLMNPQRSTVWXZbcdfghjklmnpqrstvwxz";

fn encode_gmail_token(n: u64) -> String {
    let payload = format!("f:{n}");
    let b64_digits = b64_digits_no_padding(payload.as_bytes());
    let reduced = convert_base(&b64_digits, 64, 40);
    reduced
        .into_iter()
        .map(|d| GMAIL_TOKEN_REDUCED[d as usize] as char)
        .collect()
}

/// Standard base64 over `+/`, padding stripped. Returns digit values
/// (0..64) directly so the caller can feed them to [`convert_base`]
/// without a second decode pass.
fn b64_digits_no_padding(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= input.len() {
        let (a, b, c) = (input[i], input[i + 1], input[i + 2]);
        out.push(a >> 2);
        out.push(((a & 0b11) << 4) | (b >> 4));
        out.push(((b & 0b1111) << 2) | (c >> 6));
        out.push(c & 0b111111);
        i += 3;
    }
    match input.len() - i {
        1 => {
            let a = input[i];
            out.push(a >> 2);
            out.push((a & 0b11) << 4);
        }
        2 => {
            let (a, b) = (input[i], input[i + 1]);
            out.push(a >> 2);
            out.push(((a & 0b11) << 4) | (b >> 4));
            out.push((b & 0b1111) << 2);
        }
        _ => {}
    }
    out
}

/// Schoolbook long-division: read `digits` as a `from_base` numeral
/// (most-significant first) and re-emit it in `to_base`, also
/// most-significant first. Bounded enough for our 64→40 conversion
/// (input ~30 digits, output ~34) that a `Vec<u8>` is plenty.
fn convert_base(digits: &[u8], from_base: u32, to_base: u32) -> Vec<u8> {
    let mut input = digits.to_vec();
    let mut output = Vec::new();
    while !input.is_empty() {
        let mut next = Vec::with_capacity(input.len());
        let mut rem: u32 = 0;
        for &d in &input {
            let acc = rem * from_base + d as u32;
            let q = acc / to_base;
            rem = acc % to_base;
            if !next.is_empty() || q != 0 {
                next.push(q as u8);
            }
        }
        output.push(rem as u8);
        input = next;
    }
    output.reverse();
    output
}

/// Returns the message's `text/html` part if present. `None` for
/// plain-text messages and anything we can't parse; caller falls back
/// to raw RFC 2822 bytes.
fn extract_html_body(bytes: &[u8]) -> Option<String> {
    let parsed = mailparse::parse_mail(bytes).ok()?;
    find_part(&parsed, "text/html")
}

/// Walk the MIME tree, returning the body of the first part whose mimetype
/// matches. `mailparse` already normalizes `ctype.mimetype` to lowercase
/// without parameters, so a direct `eq_ignore_ascii_case` match is enough.
fn find_part(parsed: &mailparse::ParsedMail, mime_type: &str) -> Option<String> {
    if parsed.subparts.is_empty() {
        return parsed
            .ctype
            .mimetype
            .eq_ignore_ascii_case(mime_type)
            .then(|| parsed.get_body().ok())
            .flatten();
    }
    parsed
        .subparts
        .iter()
        .find_map(|part| find_part(part, mime_type))
}

/// Read one message's bytes, mboxrd-un-escaped so the result is ready
/// for `mailparse`. Open-path only; indexing uses [`mbox::fetch_email`]
/// which already returns a parsed [`Email`].
fn read_message_bytes(locator: &str) -> Result<Vec<u8>> {
    let (path, envelope_start) = mbox::parse_locator(locator)?;
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let len = file
        .metadata()
        .with_context(|| format!("statting {}", path.display()))?
        .len();
    if (envelope_start as u64) >= len {
        anyhow::bail!(
            "offset {envelope_start} past end of {} ({len} bytes)",
            path.display()
        );
    }
    // SAFETY: plain-mbox archives are read-only after they land on disk.
    let mmap =
        unsafe { memmap2::Mmap::map(&file) }.with_context(|| format!("mmap {}", path.display()))?;
    let bytes: &[u8] = &mmap;
    let r = mbox::scan_envelopes(bytes, envelope_start)
        .into_iter()
        .next()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no envelope at offset {envelope_start} in {} -- mbox file may have been rewritten since indexing",
                path.display()
            )
        })?;
    Ok(mbox::maybe_unescape_mboxrd(&bytes[r.body_start..r.end]).into_owned())
}

/// Stable per-message temp file: re-previewing the same message overwrites
/// in place rather than littering `$TMPDIR`. Hash key is the message-id so
/// the path doesn't leak any other identifying info into a shared dir.
fn preview_path(message_id: &str, ext: &str) -> PathBuf {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    message_id.hash(&mut h);
    std::env::temp_dir().join(format!("mailwise-preview-{:016x}.{ext}", h.finish()))
}

/// Walk every file under the configured root. We don't gate on extensions,
/// because .mbox, .txt, and no-extension are all common for mboxrd dumps;
/// instead we sniff the first ~512 bytes for the `From ` envelope which is
/// cheap and avoids mmap-scanning large unrelated files.
fn enumerate_mboxes(root: Option<&Path>) -> Result<Vec<PathBuf>> {
    let Some(root) = root else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    mbox::collect_mboxes(root, looks_like_mbox, &mut out);
    Ok(out)
}

fn looks_like_mbox(path: &Path) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; 512];
    let n = file.read(&mut head).unwrap_or(0);
    if n == 0 {
        return false;
    }
    let head = &head[..n];
    head.starts_with(b"From ") || memchr::memmem::find(head, b"\nFrom ").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_gmail_token_matches_reference() {
        // Reference value from the Python prototype in plan.md.
        assert_eq!(
            encode_gmail_token(1863898510737174703),
            "FMfcgzQgLXwbMppwTSWCXbRcbqNrxBjt"
        );
    }

    #[test]
    fn gmail_url_uses_thread_only_when_msgid_absent() {
        let msg = b"X-GM-THRID: 1863898510737174703\r\n\
                    Subject: hi\r\n\
                    \r\n\
                    body\r\n";
        assert_eq!(
            gmail_url_from_headers(msg).as_deref(),
            Some("https://mail.google.com/mail/u/0/#inbox/FMfcgzQgLXwbMppwTSWCXbRcbqNrxBjt"),
        );
    }

    #[test]
    fn gmail_url_includes_message_token_when_present() {
        let msg = b"X-GM-THRID: 1863898510737174703\r\n\
                    X-GM-MSGID: 1863898510737174703\r\n\
                    Subject: hi\r\n\
                    \r\n\
                    body\r\n";
        assert_eq!(
            gmail_url_from_headers(msg).as_deref(),
            Some(
                "https://mail.google.com/mail/u/0/#inbox/FMfcgzQgLXwbMppwTSWCXbRcbqNrxBjt/FMfcgzQgLXwbMppwTSWCXbRcbqNrxBjt"
            ),
        );
    }

    #[test]
    fn gmail_url_none_without_thrid() {
        let msg = b"Subject: hi\r\n\r\nbody\r\n";
        assert_eq!(gmail_url_from_headers(msg), None);
    }
}
