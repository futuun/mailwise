//! Plain-text body cleanup. Layered ahead of
//! [`super::html::clean_body_text`] for any `text/plain` body. Pipeline:
//!
//! 0. **CRLF -> LF.** Real-world mail mixes line endings (Windows MUAs,
//!    Mailman dumps, Unix MUAs); the line-pair heuristics below
//!    shouldn't have to think about a phantom `\r` on every comparison.
//! 1. **format=flowed unflow** (RFC 3676). Deterministic: joins lines
//!    the sender's MUA marked as soft-wrapped via a trailing space.
//!    DelSp=yes drops the joining space (CJK).
//! 2. **PGP armor strip.** Drops the `-----BEGIN PGP SIGNED MESSAGE-----`
//!    header (and its `Hash:` line) plus the entire
//!    `-----BEGIN PGP SIGNATURE-----` ... `-----END PGP SIGNATURE-----`
//!    block. Common in mailing-list traffic.
//! 3. **Signature strip** (RFC 3676 sec 4.3 / Usenet sigdash tradition).
//!    Drops everything from a `-- ` line onward.
//! 4. **Mailman footer strip.** Drops everything from a separator line
//!    of 30+ underscores onward -- the `_______________` pattern Mailman
//!    writes before unsubscribe boilerplate.
//! 5. **Paragraph unwrap** (non-flowed only). Conservative heuristic:
//!    join two consecutive non-blank lines when the first looks
//!    plausibly wrapped (long, no terminal punctuation) and the second
//!    isn't a structural marker (list bullet, table, indented code).
//!    Skipped for flowed text since unflow already did the work.
//!
//! No heuristic is perfect for non-flowed mail; we err on *not*
//! joining when uncertain. False breaks read fine, false joins mash
//! unrelated content together.

use std::borrow::Cow;

/// Read the `format` and `delsp` parameters from a Content-Type's `params`
/// map (mailparse lowercases param keys). Defaults: not flowed, no delsp.
pub fn flow_params(params: &std::collections::BTreeMap<String, String>) -> (bool, bool) {
    let is_flowed = params
        .get("format")
        .is_some_and(|v| v.eq_ignore_ascii_case("flowed"));
    let delsp = params
        .get("delsp")
        .is_some_and(|v| v.eq_ignore_ascii_case("yes"));
    (is_flowed, delsp)
}

/// Run the full plaintext cleanup pipeline. `is_flowed` / `delsp` come
/// from the Content-Type parameters of the chosen part.
pub fn normalize(body: &str, is_flowed: bool, delsp: bool) -> String {
    let body: Cow<str> = if body.contains("\r\n") {
        Cow::Owned(body.replace("\r\n", "\n"))
    } else {
        Cow::Borrowed(body)
    };

    let after_flow: Cow<str> = if is_flowed {
        Cow::Owned(unflow(&body, delsp))
    } else {
        body
    };
    let after_pgp = strip_pgp_armor(&after_flow);
    let after_outlook = strip_outlook_citation(&after_pgp);
    let after_sig = strip_signature(&after_outlook);
    let after_footer = strip_mailman_footer(after_sig);
    if is_flowed {
        // After unflow, every `\n` is intentional; don't apply the
        // ambiguous-wrap heuristic.
        after_footer.to_string()
    } else {
        unwrap_paragraphs(after_footer)
    }
}

/// RFC 3676 unflow. A line ending in a single space is a soft-wrapped
/// continuation; without, a hard break. `delsp=yes` drops the trailing
/// space instead of using it as the joining whitespace (CJK and other
/// languages without word-boundary spaces).
///
/// `-- ` (the sigdash) ends in a space but is NOT a soft wrap; joining
/// it with the next line would hide it from [`strip_signature`].
fn unflow(input: &str, delsp: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for segment in input.split_inclusive('\n') {
        let line = segment.strip_suffix('\n').unwrap_or(segment);
        let is_soft_wrap = line.ends_with(' ') && line != "-- ";
        if is_soft_wrap {
            out.push_str(&line[..line.len() - 1]);
            if !delsp {
                out.push(' ');
            }
        } else {
            out.push_str(segment);
        }
    }
    out
}

/// Strip Outlook-style `-----Original Message-----` citation header
/// blocks (marker + pseudo-headers like `From:`/`Sent:`/`To:`/`Subject:`,
/// terminated by the first blank line). The body that follows stays:
/// Outlook quotes without `>` markers, so we can't tell quoted prose
/// from new content the author may have written below (bottom-posted
/// replies are rare but real). Removing only the noisy header block is
/// the safe trade.
///
/// Loops to flatten forwarded chains. Falls through unchanged on a
/// malformed marker (no following blank line) -- better to keep
/// content than guess where it ends.
pub fn strip_outlook_citation(text: &str) -> Cow<'_, str> {
    let marker = "-----Original Message-----";
    if at_line_start(text, marker).is_none() {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut tail = text;
    while let Some(start) = at_line_start(tail, marker) {
        out.push_str(&tail[..start]);
        let after_marker = start + marker.len();
        match tail[after_marker..].find("\n\n") {
            Some(off) => {
                tail = &tail[after_marker + off + 2..];
            }
            None => {
                // Malformed -- re-attach this marker + the rest verbatim.
                tail = &tail[start..];
                break;
            }
        }
    }
    out.push_str(tail);
    Cow::Owned(out)
}

/// Strip inline PGP armor blocks. Two forms in the wild:
///
/// * **Clearsigned**: `-----BEGIN PGP SIGNED MESSAGE-----`, then
///   `Hash:` line(s), blank line, signed content, then
///   `-----BEGIN PGP SIGNATURE-----` ... `-----END PGP SIGNATURE-----`.
///   Drop both the header block (header + Hash + blank) and the
///   signature block (BEGIN through END inclusive).
/// * **Just a signature block** appended to a regular body. Drop only
///   the BEGIN..END block.
///
/// Markers must sit at column 0 so quoted references to them don't
/// trigger a strip. The early `contains` cheap-out keeps the common
/// no-armor path allocation-free.
fn strip_pgp_armor(input: &str) -> Cow<'_, str> {
    if !input.contains("-----BEGIN PGP") {
        return Cow::Borrowed(input);
    }

    let mut out = String::with_capacity(input.len());
    let mut tail: &str = input;

    // Stage 1: drop the clearsigned header block.
    if let Some(start) = at_line_start(tail, "-----BEGIN PGP SIGNED MESSAGE-----") {
        out.push_str(&tail[..start]);
        // First blank line ends the header; body begins after.
        if let Some(blank) = tail[start..].find("\n\n") {
            tail = &tail[start + blank + 2..];
        } else {
            // Malformed: header without body. Drop the rest.
            return Cow::Owned(out);
        }
    }

    // Stage 2: drop the signature block.
    if let Some(sig_start) = at_line_start(tail, "-----BEGIN PGP SIGNATURE-----") {
        out.push_str(&tail[..sig_start]);
        let end_marker = "-----END PGP SIGNATURE-----";
        if let Some(end_off) = tail[sig_start..].find(end_marker) {
            let after = &tail[sig_start + end_off + end_marker.len()..];
            // Swallow the line break right after END so we don't leave
            // a stray blank line behind.
            let after = after.strip_prefix('\n').unwrap_or(after);
            out.push_str(after);
        }
        // No END marker -> silently drop the malformed tail.
    } else {
        out.push_str(tail);
    }

    Cow::Owned(out)
}

/// Find `marker` at the start of any line in `s`. Returns the absolute
/// byte offset of the match, or `None`. Used to anchor PGP markers so a
/// quoted reference inside a body line never triggers a strip.
fn at_line_start(s: &str, marker: &str) -> Option<usize> {
    if s.starts_with(marker) {
        return Some(0);
    }
    let mut i = 0;
    let bytes = s.as_bytes();
    while let Some(rel) = s[i..].find(marker) {
        let abs = i + rel;
        if bytes[abs - 1] == b'\n' {
            return Some(abs);
        }
        i = abs + 1;
    }
    None
}

/// RFC 3676 sec 4.3: a line of exactly `-- ` separates the body from
/// the sender's signature. Everything past it is contact-info
/// boilerplate that adds noise to embedding/search.
fn strip_signature(input: &str) -> &str {
    if input.starts_with("-- \n") {
        return "";
    }
    match input.find("\n-- \n") {
        Some(idx) => &input[..idx],
        None => input,
    }
}

/// Mailman writes a separator line of 30+ underscores before its
/// unsubscribe / list-info block. Detect a line that is *entirely*
/// underscores (after trimming the trailing newline) and cut everything
/// from that line onward.
fn strip_mailman_footer(input: &str) -> &str {
    let mut pos = 0;
    for line in input.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        if body.len() >= 30 && body.bytes().all(|b| b == b'_') {
            return &input[..pos];
        }
        pos += line.len();
    }
    input
}

/// Drop "On <date>, <name> wrote:" attribution lines and the blank
/// line that conventionally follows. Not a formal spec, but every
/// major English-language MUA (Gmail, Apple Mail, Outlook,
/// Thunderbird, Mailman) emits the same anchor: starts `On `, ends
/// ` wrote:`. Date formats vary too wildly to parse, so the anchor
/// alone covers the vast majority.
///
/// 200-char cap defends against a body line that coincidentally
/// starts with `On ` and contains ` wrote:` mid-sentence after a wrap.
pub fn strip_attribution_lines(text: &str) -> Cow<'_, str> {
    // Cheap precheck so the common no-attribution path skips line
    // splitting and reallocation entirely.
    if !text.contains(" wrote:") {
        return Cow::Borrowed(text);
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < lines.len() {
        if is_attribution_line(lines[i]) {
            i += 1;
            // Eat the conventional blank line that follows.
            if i < lines.len() && lines[i].trim().is_empty() {
                i += 1;
            }
            continue;
        }
        out.push_str(lines[i]);
        if i + 1 < lines.len() {
            out.push('\n');
        }
        i += 1;
    }
    Cow::Owned(out)
}

fn is_attribution_line(line: &str) -> bool {
    if line.len() > 200 || !line.starts_with("On ") {
        return false;
    }
    line.trim_end().ends_with(" wrote:")
}

/// Conservative non-flowed paragraph unwrap. Decides per line break
/// whether it's a soft wrap (insert a space) or intentional (insert a
/// newline). Bias is toward NOT joining; false joins mash unrelated
/// content together, false breaks are merely uglier.
fn unwrap_paragraphs(input: &str) -> String {
    // `split('\n')` round-trips line count: a trailing `\n` produces a
    // trailing empty entry that we re-emit as `\n`.
    let lines: Vec<&str> = input.split('\n').collect();
    let mut out = String::with_capacity(input.len());

    for (i, line) in lines.iter().enumerate() {
        out.push_str(line);
        if i + 1 == lines.len() {
            break;
        }
        if should_join(line, lines[i + 1]) {
            out.push(' ');
        } else {
            out.push('\n');
        }
    }
    out
}

/// True iff `curr` and `next` should be joined. All branches err
/// toward *break* -- see module docstring.
fn should_join(curr: &str, next: &str) -> bool {
    // Blank line on either side -> paragraph boundary, never join.
    if curr.trim().is_empty() || next.trim().is_empty() {
        return false;
    }

    // Leading whitespace suggests preformatted content (code, ASCII
    // tables, indented lists); don't reflow.
    if starts_indented(curr) || starts_indented(next) {
        return false;
    }

    // Structural markers either side: list bullets, separator rules,
    // residual `>` quote leaders, headings, table pipes.
    if starts_structural(curr) || starts_structural(next) {
        return false;
    }

    // Sentence-final punctuation means an intentional break (or a
    // multi-line address block where each line ends in `.` for "Inc."
    // / "St." -- both want a break).
    let last = curr.trim_end().chars().last();
    if matches!(last, Some('.' | '!' | '?' | ':' | ';')) {
        return false;
    }

    // `curr` must be plausibly wrapped; short lines tend to be
    // intentional (titles, sign-offs, aphorisms). 50 chars catches
    // typical 60-78-col wraps even after word-boundary jitter while
    // skipping the obvious shorts.
    if curr.chars().count() < 50 {
        return false;
    }

    true
}

fn starts_indented(line: &str) -> bool {
    line.starts_with("  ") || line.starts_with('\t')
}

fn starts_structural(line: &str) -> bool {
    let trimmed = line.trim_start();
    let Some(first) = trimmed.chars().next() else {
        return false;
    };
    if matches!(first, '-' | '*' | '>' | '#' | '|' | '+' | '=' | '_') {
        return true;
    }
    // Numbered list (`1.`, `2)`, `10.` etc).
    if first.is_ascii_digit() {
        let after_digits = trimmed.trim_start_matches(|c: char| c.is_ascii_digit());
        return after_digits.starts_with('.') || after_digits.starts_with(')');
    }
    false
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unflow_joins_soft_wrapped_lines() {
        let input = "This is paragraph text that is \nmeant to be flowed across \nseveral lines.\n";
        assert_eq!(
            unflow(input, false),
            "This is paragraph text that is meant to be flowed across several lines.\n",
        );
    }

    #[test]
    fn unflow_with_delsp_drops_joining_space() {
        // CJK-style: trailing space marks soft wrap but isn't kept.
        let input = "abc \ndef\n";
        assert_eq!(unflow(input, true), "abcdef\n");
    }

    #[test]
    fn unflow_keeps_hard_breaks() {
        let input = "Line one.\nLine two.\n";
        assert_eq!(unflow(input, false), "Line one.\nLine two.\n");
    }

    #[test]
    fn unflow_trailing_soft_wrap_no_break() {
        // No final `\n` on the input -- and none on the output either.
        // unflow preserves "did this end in a newline" rather than
        // adding one; whitespace tidy-up is `clean_body_text`'s job.
        let input = "abc \ndef";
        assert_eq!(unflow(input, false), "abc def");
    }

    #[test]
    fn unflow_preserves_sigdash() {
        // The sigdash line ends in a space but is NOT a soft wrap; if
        // unflow joined it with "John Doe", strip_signature couldn't find
        // it. Make sure that's preserved.
        let input = "Body\n-- \nJohn Doe\n";
        assert_eq!(unflow(input, false), "Body\n-- \nJohn Doe\n");
    }

    #[test]
    fn strip_signature_cuts_at_dash_dash_space() {
        let input = "Hello world.\n\n-- \nJohn Doe\njohn@example.com\n";
        assert_eq!(strip_signature(input), "Hello world.\n");
    }

    #[test]
    fn strip_signature_leaves_alone_when_absent() {
        let input = "Body without a signature.\n";
        assert_eq!(strip_signature(input), input);
    }

    #[test]
    fn strip_signature_does_not_match_dashes_in_text() {
        // Three-dash line isn't the sigdash; must be exactly `-- `.
        let input = "Some text.\n---\nMore text.\n";
        assert_eq!(strip_signature(input), input);
    }

    #[test]
    fn strip_mailman_footer_cuts_at_underscore_separator() {
        let sep = "_".repeat(53);
        let input = format!("Question?\n\nThanks!\n{sep}\nList -- list@example.com\n");
        assert_eq!(strip_mailman_footer(&input), "Question?\n\nThanks!\n");
    }

    #[test]
    fn strip_mailman_footer_ignores_short_underscore_runs() {
        let input = "var __init__ = ...\nNot a separator: ____\n";
        assert_eq!(strip_mailman_footer(input), input);
    }

    #[test]
    fn unwrap_joins_long_continuation_lines() {
        let input = "I noticed that the function foo() in module bar isn't\n\
                     handling the edge case where the input is empty. Could\n\
                     we add a check for that?\n";
        let out = unwrap_paragraphs(input);
        assert_eq!(
            out,
            "I noticed that the function foo() in module bar isn't \
             handling the edge case where the input is empty. Could \
             we add a check for that?\n",
        );
    }

    #[test]
    fn unwrap_preserves_paragraph_breaks() {
        let input = "First paragraph that is long enough to be plausibly wrapped at standard widths.\n\
                     Second part of first paragraph after a soft wrap continues here.\n\
                     \n\
                     New paragraph after blank line.\n";
        let out = unwrap_paragraphs(input);
        assert!(out.contains("\n\nNew paragraph"));
    }

    #[test]
    fn unwrap_keeps_short_lines_alone() {
        let input = "Best regards,\nJane Doe\n";
        // Both lines short -> never join.
        assert_eq!(unwrap_paragraphs(input), input);
    }

    #[test]
    fn unwrap_keeps_address_block_alone() {
        let input = "Acme Inc.\n123 Main St., Suite 400\nSpringfield, IL 12345\n";
        // Each line ends in `.`, or is short -> never join.
        assert_eq!(unwrap_paragraphs(input), input);
    }

    #[test]
    fn unwrap_skips_list_items() {
        // Bulleted list -- even if items are long the leading marker
        // forces a break.
        let input = "- First item that is long enough to look like a wrap candidate maybe\n\
                     - Second item that is also long enough to look like one\n";
        assert_eq!(unwrap_paragraphs(input), input);
    }

    #[test]
    fn unwrap_skips_indented_code() {
        let input = "    fn foo() -> Result<()> {\n    Ok(())\n    }\n";
        assert_eq!(unwrap_paragraphs(input), input);
    }

    #[test]
    fn unwrap_skips_numbered_list() {
        let input = "1. First item that runs to a respectable length on its own line\n\
                     2. Second item that also looks reasonably wrapped today\n";
        assert_eq!(unwrap_paragraphs(input), input);
    }

    #[test]
    fn unwrap_skips_separator_rules() {
        let input = "Heading line that is long enough to trigger join consideration\n\
                     ============================================\n\
                     Body that follows the heading\n";
        let out = unwrap_paragraphs(input);
        // The heading shouldn't merge with the rule line; the rule line
        // shouldn't merge with the body either.
        assert!(out.contains("\n=====") || out.contains("===\n"));
        assert!(out.contains("\nBody"));
    }

    #[test]
    fn attribution_strip_removes_on_wrote_line_and_blank() {
        let input =
            "I disagree.\n\nOn 3/27/2026 2:09 PM, Piergiorgio Sartor wrote:\n\nremaining body";
        assert_eq!(
            strip_attribution_lines(input).as_ref(),
            "I disagree.\n\nremaining body",
        );
    }

    #[test]
    fn attribution_strip_handles_via_listname_variant() {
        let input = "ack\n\nOn 3/27/2026 2:09 PM, Bob via Python-list wrote:\n\nrest";
        assert_eq!(strip_attribution_lines(input).as_ref(), "ack\n\nrest",);
    }

    #[test]
    fn attribution_strip_borrows_when_no_wrote_marker() {
        let input = "Body without any reply attribution at all.\n";
        assert!(matches!(strip_attribution_lines(input), Cow::Borrowed(_)));
    }

    #[test]
    fn attribution_strip_ignores_mid_sentence_wrote() {
        // Body line that coincidentally contains "wrote:" but isn't an
        // attribution. Must not be stripped.
        let input = "She wrote: stop. He listened.\nNext line.\n";
        assert_eq!(strip_attribution_lines(input).as_ref(), input);
    }

    #[test]
    fn attribution_strip_skips_runaway_long_line() {
        // A 250-char line starting with "On " and ending with "wrote:"
        // is almost certainly NOT an attribution; bail rather than nuke
        // a real paragraph.
        let line = format!("On {}, somebody wrote:", "x".repeat(240));
        let input = format!("{line}\nbody\n");
        assert_eq!(strip_attribution_lines(&input).as_ref(), input);
    }

    #[test]
    fn attribution_strip_handles_attribution_at_end_of_input() {
        let input = "Hello.\n\nOn 1/1/2026, Bob wrote:";
        assert_eq!(strip_attribution_lines(input).as_ref(), "Hello.\n\n");
    }

    #[test]
    fn outlook_strip_removes_citation_header_block() {
        let input = "Reply text.\n\n\
                     -----Original Message-----\n\
                     From: Michael Torrie via Python-list\n\
                     <python-list@python.org>\n\
                     Sent: Friday, February 27, 2026 11:27 PM\n\
                     To: python-list@python.org\n\
                     Subject: Re: foo\n\
                     \n\
                     Quoted body text follows here.\n";
        assert_eq!(
            strip_outlook_citation(input).as_ref(),
            "Reply text.\n\nQuoted body text follows here.\n",
        );
    }

    #[test]
    fn outlook_strip_borrows_when_no_marker() {
        let input = "A normal email body without any citation marker.\n";
        assert!(matches!(strip_outlook_citation(input), Cow::Borrowed(_)));
    }

    #[test]
    fn outlook_strip_handles_chained_forwards() {
        let input = "Top reply.\n\n\
                     -----Original Message-----\n\
                     From: A\n\
                     Subject: x\n\
                     \n\
                     Second-level reply.\n\n\
                     -----Original Message-----\n\
                     From: B\n\
                     Subject: y\n\
                     \n\
                     Original body.\n";
        assert_eq!(
            strip_outlook_citation(input).as_ref(),
            "Top reply.\n\nSecond-level reply.\n\nOriginal body.\n",
        );
    }

    #[test]
    fn outlook_strip_preserves_malformed_marker() {
        // Marker without a following blank-line header block -- leave the
        // input unchanged rather than nuke whatever follows.
        let input = "Reply.\n-----Original Message-----\nFrom: nobody";
        assert_eq!(strip_outlook_citation(input).as_ref(), input);
    }

    #[test]
    fn outlook_strip_does_not_match_marker_inside_a_line() {
        // The marker must be at column 0 -- a quoted reference shouldn't
        // trigger a strip.
        let input = "He said: '-----Original Message-----' and laughed.\n";
        assert!(matches!(strip_outlook_citation(input), Cow::Borrowed(_)));
    }

    #[test]
    fn flow_params_reads_format_and_delsp() {
        use std::collections::BTreeMap;
        let mut p = BTreeMap::new();
        p.insert("format".to_string(), "flowed".to_string());
        p.insert("delsp".to_string(), "yes".to_string());
        assert_eq!(flow_params(&p), (true, true));

        let mut p2 = BTreeMap::new();
        p2.insert("format".to_string(), "FLOWED".to_string()); // case-insensitive
        assert_eq!(flow_params(&p2), (true, false));

        let p3 = BTreeMap::new();
        assert_eq!(flow_params(&p3), (false, false));
    }

    #[test]
    fn normalize_full_pipeline() {
        // Flowed mail with a sigdash and a Mailman footer in one body.
        // strip_signature consumes the leading '\n' of its delimiter (the
        // end of the blank line before "-- "), so the output retains a
        // single trailing `\n` rather than two -- `clean_body_text` would
        // normalize either way.
        let sep = "_".repeat(50);
        let input = format!(
            "This is a wrapped \nparagraph that should join.\n\
             \n\
             -- \n\
             John Doe\n\
             {sep}\n\
             unsubscribe footer\n",
        );
        let out = normalize(&input, true, false);
        assert_eq!(out, "This is a wrapped paragraph that should join.\n");
    }
}
