//! Generic mbox(rd) plumbing shared by every client that reads mbox
//! files. Anything depending on a specific mbox flavor (Thunderbird's
//! `X-Mozilla-Status` flag headers, the profile/`ImapMail/` filesystem
//! layout, the `.msf` sidecar marker) lives in the per-client module;
//! this file owns the format-agnostic primitives: filesystem walking,
//! SIMD envelope scanning, mboxrd un-escape, and the two operations
//! `MailClient` expects -- [`list_locators`] and [`fetch_email`].

use anyhow::{Context, Result};
use rayon::prelude::*;
use std::borrow::Cow;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use super::LocatorScan;
use crate::parser::{self, Email, peek_message_id};

// ---------------------------------------------------------------------------
// Read-only locator listing (the scan half of `sync`)
// ---------------------------------------------------------------------------

/// Walk every mbox the client considers indexable and emit one
/// `(Message-ID, "path#offset=N")` pair per non-skipped envelope.
/// Read-only; safe to call concurrently from multiple workers.
///
/// `scan_complete` flips to false on any per-mbox I/O error, which
/// `super::sync` honors by refusing to delete this source's rows
/// from `email_sources` -- a permission glitch or transient I/O
/// shouldn't read as mass deletion.
pub fn list_locators<E, S>(enumerate: E, should_skip: S) -> Result<LocatorScan>
where
    E: FnOnce() -> Result<Vec<PathBuf>>,
    S: Fn(&[u8]) -> bool + Send + Sync + Copy,
{
    let mboxes = enumerate()?;
    let scan_complete = AtomicBool::new(true);
    let pairs = mboxes
        .par_iter()
        .flat_map_iter(|path| match scan_locators_in_mbox(path, should_skip) {
            Ok(pairs) => pairs.into_iter(),
            Err(e) => {
                tracing::warn!("scan {}: {e:#}", path.display());
                scan_complete.store(false, Ordering::Relaxed);
                Vec::new().into_iter()
            }
        })
        .collect();
    Ok(LocatorScan {
        pairs,
        scan_complete: scan_complete.load(Ordering::Relaxed),
    })
}

fn scan_locators_in_mbox(
    path: &Path,
    should_skip: impl Fn(&[u8]) -> bool + Send + Sync,
) -> Result<Vec<(String, String)>> {
    let path_str = path.to_string_lossy().into_owned();
    let file = File::open(path)?;
    let len = file.metadata()?.len() as usize;
    if len == 0 {
        return Ok(Vec::new());
    }
    // SAFETY: live mboxes are only ever appended (Thunderbird) or read-only
    // (plain mbox archives). A concurrent truncation would SIGBUS, but
    // neither client model produces one under normal use.
    let mmap = unsafe { memmap2::Mmap::map(&file) }?;
    let bytes: &[u8] = &mmap;

    // Chunk-parallel envelope scan + per-message peek. Splitting the
    // mmap lets rayon distribute one giant mbox (Thunderbird's 750 MB
    // INBOX, e.g.) across every core instead of pinning the SIMD scan
    // to a single worker. Each chunk owns the envelopes whose start
    // offset falls in `[chunk_start, chunk_end)`; the SIMD overlap
    // (`-1` left, `+5` right) makes sure every `\nFrom ` is counted by
    // exactly one chunk.
    //
    // 4 MB: small enough to spread one big mbox over 10+ perf cores,
    // large enough to amortize per-chunk Finder construction.
    const CHUNK: usize = 4 * 1024 * 1024;
    let n_chunks = len.div_ceil(CHUNK).max(1);

    let pairs: Vec<(String, String)> = (0..n_chunks)
        .into_par_iter()
        .flat_map(|i| {
            let chunk_start = i * CHUNK;
            let chunk_end = ((i + 1) * CHUNK).min(len);
            // Catch a `\nFrom ` whose `\n` lives in the prior chunk
            // (left-overlap) or whose `From ` straddles into the next
            // (right-overrun by 5, the length of `From `).
            let scan_lo = chunk_start.saturating_sub(1);
            let scan_hi = (chunk_end + 5).min(len);

            let mut envelopes: Vec<usize> = Vec::new();
            // First message in the file has no leading `\n` to anchor on.
            if i == 0 && bytes.starts_with(b"From ") {
                envelopes.push(0);
            }
            let finder = memchr::memmem::Finder::new(b"\nFrom ");
            for p in finder.find_iter(&bytes[scan_lo..scan_hi]) {
                let envelope_start = scan_lo + p + 1;
                if envelope_start >= chunk_start && envelope_start < chunk_end {
                    envelopes.push(envelope_start);
                }
            }

            envelopes
                .into_iter()
                .filter_map(|env_start| {
                    let body_start =
                        memchr::memchr(b'\n', &bytes[env_start..]).map(|nl| env_start + nl + 1)?;
                    if body_start >= len {
                        return None;
                    }
                    // Cap the slice handed to `peek_message_id`. It
                    // stops at the first blank line, but a corrupt
                    // message with no blank line would otherwise scan
                    // to EOF. 64 KB is comfortable headroom over the
                    // longest realistic ARC-chain header block.
                    let msg_end = (body_start + 64 * 1024).min(len);
                    let msg = &bytes[body_start..msg_end];
                    if should_skip(msg) {
                        return None;
                    }
                    let mid = peek_message_id(msg)?;
                    Some((mid, format!("{path_str}#offset={env_start}")))
                })
                .collect::<Vec<_>>()
        })
        .collect();

    Ok(pairs)
}

// ---------------------------------------------------------------------------
// Single-message fetch (the parse half of `sync`)
// ---------------------------------------------------------------------------

/// Parse the message at `locator` (`<path>#offset=N`) into an [`Email`].
/// Fails if the recorded offset no longer points at an envelope (file
/// was rewritten between scan and fetch); the next poll's scan picks up
/// the new layout, so this stays idempotent.
pub fn fetch_email(locator: &str) -> Result<Email> {
    let (path, envelope_start) = parse_locator(locator)?;
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
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
    // SAFETY: see `scan_locators_in_mbox`.
    let mmap =
        unsafe { memmap2::Mmap::map(&file) }.with_context(|| format!("mmap {}", path.display()))?;
    let bytes: &[u8] = &mmap;
    let r = scan_envelopes(bytes, envelope_start)
        .into_iter()
        .next()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no envelope at offset {envelope_start} in {} -- mbox file may have been rewritten since indexing",
                path.display()
            )
        })?;
    let parseable = maybe_unescape_mboxrd(&bytes[r.body_start..r.end]);
    parser::build_email(&parseable)
}

/// Inverse of the locator format written in [`scan_locators_in_mbox`]:
/// `<path>#offset=<N>`. `rsplit_once` so a `#` inside the path doesn't
/// break parsing.
pub fn parse_locator(locator: &str) -> Result<(&Path, usize)> {
    let (path, suffix) = locator
        .rsplit_once('#')
        .ok_or_else(|| anyhow::anyhow!("malformed mbox locator (no '#'): {locator}"))?;
    let offset: usize = suffix
        .strip_prefix("offset=")
        .ok_or_else(|| anyhow::anyhow!("malformed mbox locator (no 'offset='): {locator}"))?
        .parse()
        .with_context(|| format!("malformed mbox locator (bad offset): {locator}"))?;
    Ok((Path::new(path), offset))
}

// ---------------------------------------------------------------------------
// Filesystem walk
// ---------------------------------------------------------------------------

/// Recursive walk under `dir`. Per-client format markers (Thunderbird's
/// `.msf` sidecar, plain-mbox's "first 512 bytes look like an envelope")
/// live in `is_mbox`; this function only handles dotfile-skip and
/// recursion.
pub fn collect_mboxes<P>(dir: &Path, is_mbox: P, out: &mut Vec<PathBuf>)
where
    P: Fn(&Path) -> bool + Copy,
{
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_mboxes(&path, is_mbox, out);
        } else if file_type.is_file() && is_mbox(&path) {
            out.push(path);
        }
    }
}

// ---------------------------------------------------------------------------
// Envelope scan
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub struct MessageRange {
    /// Byte offset of the envelope `From -` line; used as the locator.
    pub envelope_start: usize,
    /// Byte offset of the first header line (just past the envelope's `\n`).
    pub body_start: usize,
    /// Byte offset just past the last byte of this message: the next
    /// envelope's start, or `bytes.len()` for the final message.
    pub end: usize,
}

/// Walk `bytes[scan_from..]` and produce a [`MessageRange`] per message.
/// mboxrd's `>From `, `>>From ` body escapes can't collide with the
/// envelope because they always have a `>` between the `\n` and the `F`.
///
/// Offsets are absolute regardless of `scan_from`. Pass `0` to scan the
/// whole file; pass a known envelope offset to fetch just that message.
pub fn scan_envelopes(bytes: &[u8], scan_from: usize) -> Vec<MessageRange> {
    if scan_from >= bytes.len() {
        return Vec::new();
    }
    let scan_slice = &bytes[scan_from..];

    let mut starts: Vec<usize> = Vec::new();
    // First envelope in the scan window has no leading `\n` to anchor on.
    if scan_slice.starts_with(b"From ") {
        starts.push(scan_from);
    }
    let finder = memchr::memmem::Finder::new(b"\nFrom ");
    starts.extend(finder.find_iter(scan_slice).map(|p| scan_from + p + 1));

    let mut ranges = Vec::with_capacity(starts.len());
    for (i, &envelope_start) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(bytes.len());
        let body_start = memchr::memchr(b'\n', &bytes[envelope_start..end])
            .map(|nl| envelope_start + nl + 1)
            .unwrap_or(end);
        ranges.push(MessageRange {
            envelope_start,
            body_start,
            end,
        });
    }
    ranges
}

/// Mboxrd escape: a body line matching `^>+From ` had one `>` prepended
/// on write and needs exactly one stripped on read.
fn is_escaped_from(line: &[u8]) -> bool {
    if !line.starts_with(b">") {
        return false;
    }
    let n_gt = line.iter().take_while(|&&b| b == b'>').count();
    line[n_gt..].starts_with(b"From ")
}

/// Strip exactly one `>` from each body line matching `^>+From `. The
/// `\n>` precheck returns a borrow in the common case (most messages
/// have no escapes at all); only allocates when there's actual work.
pub fn maybe_unescape_mboxrd(input: &[u8]) -> Cow<'_, [u8]> {
    // Callers always pass a header-aligned slice, so escape candidates
    // only ever appear after a `\n`. No `\n>` pair -> no escapes.
    if memchr::memmem::find(input, b"\n>").is_none() {
        return Cow::Borrowed(input);
    }
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let line_end = memchr::memchr(b'\n', &input[i..])
            .map(|n| i + n + 1)
            .unwrap_or(input.len());
        let line = &input[i..line_end];
        if is_escaped_from(line) {
            out.extend_from_slice(&line[1..]);
        } else {
            out.extend_from_slice(line);
        }
        i = line_end;
    }
    Cow::Owned(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescape_mboxrd_from_lines() {
        assert!(is_escaped_from(b">From bar\n"));
        assert!(is_escaped_from(b">>From bar\n"));
        assert!(!is_escaped_from(b">foo\n"));
        assert!(!is_escaped_from(b"From bar\n")); // envelope, not body
    }

    #[test]
    fn scan_envelopes_finds_each_message() {
        let mbox: &[u8] = b"From - Sat\nSubject: a\n\nbody\n\nFrom - Sun\nSubject: b\n\nbye\n";
        let ranges = scan_envelopes(mbox, 0);
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].envelope_start, 0);
        assert_eq!(
            &mbox[ranges[0].body_start..ranges[0].body_start + 10],
            b"Subject: a"
        );
        assert_eq!(
            &mbox[ranges[1].envelope_start..ranges[1].envelope_start + 5],
            b"From "
        );
        assert_eq!(
            &mbox[ranges[1].body_start..ranges[1].body_start + 10],
            b"Subject: b"
        );
        assert_eq!(ranges[1].end, mbox.len());
    }

    #[test]
    fn scan_envelopes_ignores_escaped_from_in_body() {
        let mbox: &[u8] = b"From - Sat\nSubject: a\n\n>From the desk of Bob\nbye\n";
        assert_eq!(scan_envelopes(mbox, 0).len(), 1);
    }

    #[test]
    fn scan_envelopes_handles_empty_input() {
        assert!(scan_envelopes(b"", 0).is_empty());
    }

    #[test]
    fn scan_envelopes_handles_no_trailing_newline() {
        let mbox: &[u8] = b"From - Sat\nSubject: a\n\nbody";
        let ranges = scan_envelopes(mbox, 0);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].end, mbox.len());
    }

    #[test]
    fn scan_envelopes_skips_to_tail_when_offset_given() {
        let mbox: &[u8] = b"From - Sat\nSubject: a\n\nbody\n\nFrom - Sun\nSubject: b\n\nbye\n";
        let second_start = b"From - Sat\nSubject: a\n\nbody\n\n".len();
        let ranges = scan_envelopes(mbox, second_start);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].envelope_start, second_start);
        assert_eq!(ranges[0].end, mbox.len());
        assert_eq!(
            &mbox[ranges[0].body_start..ranges[0].body_start + 10],
            b"Subject: b"
        );
    }

    #[test]
    fn scan_envelopes_resumes_after_terminating_newline() {
        let mbox: &[u8] = b"From - Sat\nSubject: a\n\nbody\nFrom - Sun\nSubject: b\n\nbye\n";
        let second_start = b"From - Sat\nSubject: a\n\nbody\n".len();
        let ranges = scan_envelopes(mbox, second_start);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].envelope_start, second_start);
    }

    #[test]
    fn scan_envelopes_handles_offset_past_end() {
        let mbox: &[u8] = b"From - Sat\nSubject: a\n\nbody\n";
        assert!(scan_envelopes(mbox, mbox.len()).is_empty());
        assert!(scan_envelopes(mbox, mbox.len() + 100).is_empty());
    }

    #[test]
    fn maybe_unescape_borrows_when_no_escapes() {
        let input: &[u8] = b"Subject: hi\n\nhello\nworld\n";
        let out = maybe_unescape_mboxrd(input);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out.as_ref(), input);
    }

    #[test]
    fn maybe_unescape_strips_one_gt_per_escape() {
        let input: &[u8] = b"Subject: hi\n\n>From the desk of Bob\n>>From the lab\nbye\n";
        let out = maybe_unescape_mboxrd(input);
        assert!(matches!(out, Cow::Owned(_)));
        let expected: &[u8] = b"Subject: hi\n\nFrom the desk of Bob\n>From the lab\nbye\n";
        assert_eq!(out.as_ref(), expected);
    }

    #[test]
    fn maybe_unescape_leaves_non_from_gt_lines_alone() {
        let input: &[u8] = b"Subject: hi\n\n> quoted reply text\n>>also quoted\n";
        let out = maybe_unescape_mboxrd(input);
        assert_eq!(out.as_ref(), input);
    }
}
