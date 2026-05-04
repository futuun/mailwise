//! Client-agnostic RFC 2822 / MIME parsing.
//!
//! Each mail-client module hands this layer raw message bytes; everything
//! from header extraction through HTML cleanup lives here.

use anyhow::{Context, Result};
use mailparse::MailHeaderMap;

mod html;
mod plaintext;

/// Header-only Message-ID extraction; cheap because mailparse stops at
/// the blank line. Drives the scan side of `sync`: every locator's
/// Message-ID lands in the diff, and only genuinely new ids trigger
/// full body parsing.
pub fn peek_message_id(bytes: &[u8]) -> Option<String> {
    let (headers, _body_offset) = mailparse::parse_headers(bytes).ok()?;
    let raw = headers.get_first_value("Message-ID")?;
    let trimmed = raw.trim_matches(|c: char| c == '<' || c == '>' || c == ' ');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// A fully parsed email, ready for storage and embedding.
#[derive(Debug, Clone)]
pub struct Email {
    pub message_id: String,
    pub from: String,
    pub to: String,
    pub subject: String,
    pub date: i64,
    pub body_text: String,
    /// True if `List-Id` or `List-Unsubscribe` was present in the headers.
    pub is_mailing_list: bool,
}

/// `parse_from_bytes` return type; promoted to [`Email`] by
/// [`build_email`].
#[derive(Debug, Clone)]
struct ParsedFields {
    /// Angle brackets stripped. `None` when the header was missing.
    message_id: Option<String>,
    from: String,
    to: String,
    subject: String,
    date: i64,
    body_text: String,
    is_mailing_list: bool,
}

/// Bails on a missing `Message-ID:` header. `list_locators` already
/// filtered such messages out via [`peek_message_id`], so this only
/// fires when the file changed between peek and fetch -- the next
/// sync's scan picks up the new state.
pub fn build_email(msg_bytes: &[u8]) -> Result<Email> {
    let fields = parse_from_bytes(msg_bytes)?;
    let message_id = fields
        .message_id
        .ok_or_else(|| anyhow::anyhow!("message has no Message-ID header"))?;
    Ok(Email {
        message_id,
        from: fields.from,
        to: fields.to,
        subject: fields.subject,
        date: fields.date,
        body_text: fields.body_text,
        is_mailing_list: fields.is_mailing_list,
    })
}

fn parse_from_bytes(msg_bytes: &[u8]) -> Result<ParsedFields> {
    let parsed = mailparse::parse_mail(msg_bytes).context("Failed to parse RFC2822 message")?;
    let headers = &parsed.headers;

    let is_mailing_list = headers.get_first_value("List-Id").is_some()
        || headers.get_first_value("List-Unsubscribe").is_some();

    let message_id = headers
        .get_first_value("Message-ID")
        .map(|v| {
            v.trim_matches(|c| c == '<' || c == '>' || c == ' ')
                .to_string()
        })
        .filter(|s| !s.is_empty());

    let date = headers
        .get_first_value("Date")
        .and_then(|d| mailparse::dateparse(&d).ok())
        .or_else(|| {
            // Fallback: the date portion of the first Received header (after the ';')
            headers.get_first_value("Received").and_then(|v| {
                v.rsplit_once(';')
                    .and_then(|(_, d)| mailparse::dateparse(d.trim()).ok())
            })
        })
        .unwrap_or(0);

    let (body_part, kind) = extract_body_part(&parsed);
    // HTML through html5ever; plaintext through the mboxrd-style
    // cleanup (unflow, sigdash + Mailman footer strip, paragraph
    // unwrap heuristic) before sharing `clean_body_text` with the HTML
    // path. Non-text singletons drop to empty -- see [`BodyKind`].
    let body_text = match kind {
        BodyKind::Html => html::extract_body_text(&body_part),
        BodyKind::Plain { is_flowed, delsp } => {
            let normalized = plaintext::normalize(&body_part, is_flowed, delsp);
            html::clean_body_text(&normalized)
        }
        BodyKind::Empty => String::new(),
    };

    Ok(ParsedFields {
        message_id,
        from: headers.get_first_value("From").unwrap_or_default(),
        to: headers.get_first_value("To").unwrap_or_default(),
        subject: headers.get_first_value("Subject").unwrap_or_default(),
        date,
        body_text,
        is_mailing_list,
    })
}

/// What `extract_body_part` picked. `Plain` carries the format=flowed
/// parameters here because they live on the chosen leaf's Content-Type
/// (not the message root), and [`plaintext::normalize`] needs them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyKind {
    Html,
    Plain {
        is_flowed: bool,
        delsp: bool,
    },
    /// Attachment-only single-part (PDF, octet-stream, ...) or
    /// multipart with no text leaf. Treated as empty -- without this
    /// guard binary bytes would land in the DB as gibberish.
    Empty,
}

/// Pick the body part from a parsed message and report its kind. HTML
/// wins over plain because link text is inline and there are no hard
/// wrap breaks -- cleaner input for the embedding model.
fn extract_body_part(parsed: &mailparse::ParsedMail) -> (String, BodyKind) {
    let Some(leaf) = pick_body_leaf(parsed) else {
        return (String::new(), BodyKind::Empty);
    };
    let body = leaf.get_body().unwrap_or_default();
    let mime = &leaf.ctype.mimetype;
    if mime.eq_ignore_ascii_case("text/html") {
        return (body, BodyKind::Html);
    }
    let (is_flowed, delsp) = plaintext::flow_params(&leaf.ctype.params);
    (body, BodyKind::Plain { is_flowed, delsp })
}

/// HTML wins over plain text; both win over anything else. `None` for
/// attachment-only messages.
fn pick_body_leaf<'a>(
    parsed: &'a mailparse::ParsedMail<'a>,
) -> Option<&'a mailparse::ParsedMail<'a>> {
    if parsed.subparts.is_empty() {
        let mime = &parsed.ctype.mimetype;
        if mime.eq_ignore_ascii_case("text/html") || mime.eq_ignore_ascii_case("text/plain") {
            return Some(parsed);
        }
        return None;
    }
    if let Some(p) = find_leaf(parsed, "text/html") {
        return Some(p);
    }
    find_leaf(parsed, "text/plain")
}

fn find_leaf<'a>(
    parsed: &'a mailparse::ParsedMail<'a>,
    mime_type: &str,
) -> Option<&'a mailparse::ParsedMail<'a>> {
    if parsed.subparts.is_empty() {
        return parsed
            .ctype
            .mimetype
            .eq_ignore_ascii_case(mime_type)
            .then_some(parsed);
    }
    parsed
        .subparts
        .iter()
        .find_map(|part| find_leaf(part, mime_type))
}
