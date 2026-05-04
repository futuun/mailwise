//! HTML email bodies -> text for embedding. Marketing email HTML is
//! brutal: nested layout tables, CSS-hidden mobile dupes of the same
//! content, vendor footer boilerplate, tracking pixels parked in
//! `<style>`. We parse via scraper/html5ever (malformed input is
//! repaired per spec, never rejected), prune the noise, fold tables
//! into `Header: Value` lines so the embedder sees something
//! semantic, then run a byte-level cleanup pass over the serialized
//! text.
//!
//! [`extract_body_text`] is the entry. [`clean_body_text`] is also
//! public so the plaintext path can call it directly without going
//! through the DOM (which would eat literal `<...>` tokens).

use ego_tree::NodeId;
use scraper::{Html, Node, Selector};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Strip HTML and clean the result for embedding.
pub fn extract_body_text(body: &str) -> String {
    let mut doc = Html::parse_document(body);

    prune_unwanted_nodes(&mut doc);
    strip_marked_wrappers(&mut doc);
    fold_tables_bottom_up(&mut doc);
    let text = serialize_to_text(&doc);

    clean_body_text(&text)
}

// ---------------------------------------------------------------------------
// Pruning
// ---------------------------------------------------------------------------

fn prune_unwanted_nodes(doc: &mut Html) {
    let mut to_remove: Vec<NodeId> = Vec::new();

    for node_ref in doc.tree.nodes() {
        match node_ref.value() {
            Node::Comment(_) => {
                to_remove.push(node_ref.id());
            }
            Node::Element(el) => {
                let name = el.name.local.as_ref();
                if name == "script" || name == "style" || name == "head" {
                    to_remove.push(node_ref.id());
                }
            }
            _ => {}
        }
    }

    let bq_sel = Selector::parse("blockquote").unwrap();
    for el in doc.select(&bq_sel) {
        to_remove.push(el.id());
    }

    for id in to_remove {
        let mut node = doc.tree.get_mut(id).unwrap();
        node.detach();
    }
}

// ---------------------------------------------------------------------------
// Wrapper stripping
// ---------------------------------------------------------------------------

/// Class/ID substrings tagging `<table>`/`<div>`/`<td>` wrappers we
/// drop wholesale. Two categories live together because the action is
/// identical:
///
/// - Mobile-only dupes (`mobile-hide` and friends) -- responsive
///   templates ship the desktop content AND a CSS-hidden mobile copy
///   of the same thing; both reach the parser, embedding double-counts
///   without this drop.
/// - Vendor footer boilerplate (`footer-copyright`, `moe-hide`) --
///   marketing-platform tags that consistently bracket
///   unsubscribe/legal blocks that hurt embedding signal.
///
/// Substring (not exact) match because vendors append arbitrary
/// suffixes (`mobile-hide-md`, `moe-hide-on-mobile`, etc).
const DROP_WRAPPER_PATTERNS: &[&str] = &[
    "mobile-hide",
    "hidemobile",
    "hide-mobile",
    "footer-copyright",
    "moe-hide",
];

fn strip_marked_wrappers(doc: &mut Html) {
    let mut candidate_ids: Vec<NodeId> = Vec::new();

    for node_ref in doc.tree.nodes() {
        let el = match node_ref.value().as_element() {
            Some(el) => el,
            None => continue,
        };

        let name = el.name.local.as_ref();
        if name != "table" && name != "div" && name != "td" {
            continue;
        }

        let class_val = el.attr("class").unwrap_or("");
        let id_val = el.attr("id").unwrap_or("");
        let by_class_or_id = DROP_WRAPPER_PATTERNS
            .iter()
            .any(|pat| contains_ignore_case(class_val, pat) || contains_ignore_case(id_val, pat));

        // Sonar (a third-party email template platform used by Amazon
        // and others) tags its boilerplate footer wrapper with a
        // stable `data-sonar-role="footer"` attribute. Distinct enough
        // to handle out-of-band rather than polluting the substring
        // list.
        let by_sonar_attr = el
            .attr("data-sonar-role")
            .is_some_and(|v| v.eq_ignore_ascii_case("footer"));

        if by_class_or_id || by_sonar_attr {
            candidate_ids.push(node_ref.id());
        }
    }

    for id in candidate_ids {
        // Skip if a parent was detached earlier in this pass.
        if doc.tree.get(id).unwrap().parent().is_some() {
            doc.tree.get_mut(id).unwrap().detach();
        }
    }
}

fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    if needle.len() > haystack.len() {
        return false;
    }
    let needle_lower: Vec<u8> = needle.bytes().map(|b| b.to_ascii_lowercase()).collect();
    haystack.as_bytes().windows(needle.len()).any(|w| {
        w.iter()
            .zip(needle_lower.iter())
            .all(|(a, b)| a.to_ascii_lowercase() == *b)
    })
}

// ---------------------------------------------------------------------------
// Table folding
// ---------------------------------------------------------------------------

/// Bottom-up so a parent table sees its children's serialized text by
/// the time its turn comes -- gives us "Header: Value" pairing across
/// nested table layouts that marketing emails love.
fn fold_tables_bottom_up(doc: &mut Html) {
    let table_sel = Selector::parse("table").unwrap();

    loop {
        let leaf_id = doc
            .select(&table_sel)
            .find(|t| t.select(&table_sel).next().is_none())
            .map(|t| t.id());

        match leaf_id {
            Some(id) => {
                let serialized = serialize_table_element(doc, id);
                replace_node_with_text(doc, id, &serialized);
            }
            None => break,
        }
    }
}

fn serialize_table_element(doc: &Html, table_id: NodeId) -> String {
    let table_ref = match scraper::ElementRef::wrap(doc.tree.get(table_id).unwrap()) {
        Some(r) => r,
        None => return String::new(),
    };

    let tr_sel = Selector::parse("tr").unwrap();
    let th_sel = Selector::parse("th").unwrap();
    let td_sel = Selector::parse("td").unwrap();
    let tfoot_sel = Selector::parse("tfoot").unwrap();

    // tfoot rows take a separate formatting path -- thead headers (like
    // "Product", "Qty") shouldn't pair against tfoot rows (like "Total").
    let tfoot_row_ids: std::collections::HashSet<NodeId> = table_ref
        .select(&tfoot_sel)
        .flat_map(|tfoot| tfoot.select(&tr_sel))
        .map(|tr| tr.id())
        .collect();

    let mut headers: Vec<String> = Vec::new();
    let mut data_rows: Vec<Vec<String>> = Vec::new();
    let mut tfoot_rows: Vec<Vec<String>> = Vec::new();

    for tr in table_ref.select(&tr_sel) {
        let is_tfoot = tfoot_row_ids.contains(&tr.id());

        let ths: Vec<String> = tr.select(&th_sel).map(cell_text).collect();
        let tds: Vec<String> = tr.select(&td_sel).map(cell_text).collect();

        if is_tfoot {
            let mut all = ths;
            all.extend(tds);
            if !all.is_empty() {
                tfoot_rows.push(all);
            }
        } else if !ths.is_empty() && tds.is_empty() {
            headers.extend(ths.into_iter().filter(|s| !s.is_empty()));
        } else if !tds.is_empty() {
            data_rows.push(tds);
        }
    }

    let mut lines = Vec::new();

    if !headers.is_empty() {
        for row in &data_rows {
            let pairs: Vec<String> = headers
                .iter()
                .zip(row.iter())
                .filter(|(_, v)| !v.is_empty())
                .map(|(h, v)| format!("{}: {}", h, v))
                .collect();
            if !pairs.is_empty() {
                lines.push(pairs.join(", "));
            }
        }
        // Zurb Foundation fallback: headers but no data rows matched
        if lines.is_empty() {
            for h in &headers {
                if !h.is_empty() {
                    lines.push(h.clone());
                }
            }
        }
    } else {
        format_rows_no_headers(&data_rows, &mut lines);
    }

    format_rows_no_headers(&tfoot_rows, &mut lines);

    if !lines.is_empty() {
        return lines.join("\n");
    }

    // Fallback: raw text so content isn't lost.
    table_ref.text().collect::<Vec<_>>().join(" ")
}

fn format_rows_no_headers(rows: &[Vec<String>], lines: &mut Vec<String>) {
    let is_kv = !rows.is_empty()
        && rows
            .iter()
            .all(|r| r.iter().filter(|c| !c.is_empty()).count() <= 2);

    for row in rows {
        let non_empty: Vec<&str> = row
            .iter()
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .collect();
        if non_empty.is_empty() {
            continue;
        }
        if is_kv && non_empty.len() == 2 {
            let key = non_empty[0];
            if key.ends_with(':') {
                lines.push(format!("{} {}", key, non_empty[1]));
            } else {
                lines.push(format!("{}: {}", key, non_empty[1]));
            }
        } else if non_empty.len() == 1 {
            lines.push(non_empty[0].to_string());
        } else {
            lines.push(non_empty.join(", "));
        }
    }
}

fn cell_text(el: scraper::ElementRef) -> String {
    el.text()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn replace_node_with_text(doc: &mut Html, node_id: NodeId, text: &str) {
    let content = format!("\n{}\n", text);
    let text_node = Node::Text(scraper::node::Text {
        text: content.into(),
    });
    let text_id = doc.tree.orphan(text_node).id();

    {
        let mut target = doc.tree.get_mut(node_id).unwrap();
        if target.parent().is_some() {
            target.insert_id_before(text_id);
        }
    }
    doc.tree.get_mut(node_id).unwrap().detach();
}

// ---------------------------------------------------------------------------
// DOM serialization
// ---------------------------------------------------------------------------

fn serialize_to_text(doc: &Html) -> String {
    let mut result = String::new();

    for edge in doc.tree.root().traverse() {
        match edge {
            ego_tree::iter::Edge::Open(node) => match node.value() {
                Node::Text(text) => {
                    result.push_str(text);
                }
                Node::Element(el) if el.name.local.as_ref() == "br" && !result.ends_with('\n') => {
                    result.push('\n');
                }
                _ => {}
            },
            ego_tree::iter::Edge::Close(node) => {
                if let Node::Element(el) = node.value()
                    && is_block_element(el.name.local.as_ref())
                    && !result.ends_with('\n')
                {
                    result.push('\n');
                }
            }
        }
    }

    result
}

fn is_block_element(name: &str) -> bool {
    matches!(
        name,
        "p" | "div"
            | "tr"
            | "td"
            | "th"
            | "li"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "section"
            | "article"
            | "pre"
    )
}

// ---------------------------------------------------------------------------
// Text cleanup
// ---------------------------------------------------------------------------

/// Clean body text for embedding: strip URLs, invisible characters,
/// normalize whitespace, drop `>` quoted lines. Public so the
/// plaintext path can call it directly without going through the HTML
/// pipeline (which would eat literal `<...>` tokens in plain text).
pub fn clean_body_text(text: &str) -> String {
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut result = Vec::with_capacity(len);
    let mut i = 0;

    // Pass 1: strip URLs, decode HTML entities, drop invisible Unicode and NULs.
    while i < len {
        // quoted-printable `=00` decodes to NUL which breaks tokenizers.
        if bytes[i] == 0 {
            i += 1;
            continue;
        }

        if i + 7 < len
            && (bytes[i..].starts_with(b"http://") || bytes[i..].starts_with(b"https://"))
        {
            while i < len
                && !bytes[i].is_ascii_whitespace()
                && bytes[i] != b'>'
                && bytes[i] != b'"'
                && bytes[i] != b')'
            {
                i += 1;
            }
            continue;
        }

        if bytes[i] == b'&'
            && let Some((decoded, advance)) = decode_entity(bytes, i)
        {
            result.extend_from_slice(decoded.as_bytes());
            i += advance;
            continue;
        }

        if bytes[i] >= 0xC2 {
            if let Some(skip) = skip_invisible_utf8(bytes, i) {
                i += skip;
                continue;
            }
            // U+00A0 (NBSP) = 0xC2 0xA0 -- emit regular space.
            if bytes[i] == 0xC2 && i + 1 < len && bytes[i + 1] == 0xA0 {
                result.push(b' ');
                i += 2;
                continue;
            }
        }

        result.push(bytes[i]);
        i += 1;
    }

    // Pass 2: remove empty parens left behind by URL stripping.
    let mut cleaned = Vec::with_capacity(result.len());
    let mut j = 0;
    while j < result.len() {
        if result[j] == b'(' {
            let mut k = j + 1;
            while k < result.len() && result[k].is_ascii_whitespace() {
                k += 1;
            }
            if k < result.len() && result[k] == b')' {
                j = k + 1;
                continue;
            }
        }
        cleaned.push(result[j]);
        j += 1;
    }

    // Pass 3: normalize whitespace, strip '>' quoted lines.
    let mut out = Vec::with_capacity(cleaned.len());
    let mut consecutive_newlines = 0u32;
    let mut line_start = true;
    let mut prev_space = false;
    let mut skip_line = false;

    let mut k = 0;
    while k < cleaned.len() {
        let b = cleaned[k];

        if b == b'\n' {
            consecutive_newlines += 1;
            if consecutive_newlines <= 2 {
                out.push(b'\n');
            }
            line_start = true;
            prev_space = false;
            skip_line = false;
            k += 1;
            continue;
        }

        if skip_line {
            k += 1;
            continue;
        }

        if line_start && (b == b' ' || b == b'\t') {
            k += 1;
            continue;
        }
        if line_start && b == b'>' {
            // Require `> `, `>>`, or `>\n`. A bare `>text` is usually
            // a rendering artifact, not a real email quote marker.
            let next = if k + 1 < cleaned.len() {
                cleaned[k + 1]
            } else {
                0
            };
            if next == b' ' || next == b'\t' || next == b'>' || next == b'\n' {
                skip_line = true;
                k += 1;
                continue;
            }
        }

        consecutive_newlines = 0;
        line_start = false;

        if b == b' ' || b == b'\t' {
            if !prev_space {
                out.push(b' ');
            }
            prev_space = true;
            k += 1;
            continue;
        }

        prev_space = false;
        out.push(b);
        k += 1;
    }

    // SAFETY: every byte pushed to `out` is either ASCII or part of a
    // whole UTF-8 sequence emitted by `decode_entity` /
    // `skip_invisible_utf8`. Multi-byte sequences are added or skipped
    // atomically -- never split.
    let s = unsafe { String::from_utf8_unchecked(out) };

    // Pass 4: with `>`-quoted blocks gone, any "On <date>, <name>
    // wrote:" attribution would dangle as a header above empty space.
    let s = match super::plaintext::strip_attribution_lines(&s) {
        std::borrow::Cow::Borrowed(_) => s,
        std::borrow::Cow::Owned(stripped) => stripped,
    };
    let trimmed = s.trim();
    if trimmed.len() == s.len() {
        s
    } else {
        trimmed.to_string()
    }
}

enum DecodedEntity {
    Static(&'static [u8]),
    Char([u8; 4], u8),
}

impl DecodedEntity {
    fn as_bytes(&self) -> &[u8] {
        match self {
            DecodedEntity::Static(s) => s,
            DecodedEntity::Char(buf, len) => &buf[..*len as usize],
        }
    }

    fn from_char(c: char) -> Self {
        let mut buf = [0u8; 4];
        let s = c.encode_utf8(&mut buf);
        let len = s.len() as u8;
        DecodedEntity::Char(buf, len)
    }
}

#[inline]
fn decode_entity(bytes: &[u8], pos: usize) -> Option<(DecodedEntity, usize)> {
    let remaining = &bytes[pos..];

    let semi = remaining[1..].iter().take(10).position(|&b| b == b';')?;
    let semi = semi + 1;
    let inner = &remaining[1..semi];

    if inner.starts_with(b"#") {
        let num_str = &inner[1..];
        let code = if num_str.starts_with(b"x") || num_str.starts_with(b"X") {
            std::str::from_utf8(&num_str[1..])
                .ok()
                .and_then(|s| u32::from_str_radix(s, 16).ok())
        } else {
            std::str::from_utf8(num_str)
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
        };
        let cp = code?;
        if is_invisible_codepoint(cp) {
            return Some((DecodedEntity::Static(b""), semi + 1));
        }
        let c = char::from_u32(cp)?;
        return Some((DecodedEntity::from_char(c), semi + 1));
    }

    let entity_str = std::str::from_utf8(&remaining[..semi + 1]).ok()?;
    let decoded = html_escape::decode_html_entities(entity_str);
    match decoded {
        std::borrow::Cow::Borrowed(_) => None,
        std::borrow::Cow::Owned(s) => {
            let c = s.chars().next()?;
            let cp = c as u32;
            if is_invisible_codepoint(cp) {
                return Some((DecodedEntity::Static(b""), semi + 1));
            }
            Some((DecodedEntity::from_char(c), semi + 1))
        }
    }
}

#[inline]
fn is_invisible_codepoint(cp: u32) -> bool {
    matches!(
        cp,
        0x034F          // Combining Grapheme Joiner
        | 0x00AD        // Soft Hyphen
        | 0x200B        // Zero Width Space
        | 0x200C        // Zero Width Non-Joiner
        | 0x200D        // Zero Width Joiner
        | 0x200E        // Left-to-Right Mark
        | 0x200F        // Right-to-Left Mark
        | 0x2028        // Line Separator
        | 0x2029        // Paragraph Separator
        | 0xFEFF // BOM / Zero Width No-Break Space
    )
}

#[inline]
fn skip_invisible_utf8(bytes: &[u8], i: usize) -> Option<usize> {
    let b0 = bytes[i];
    let remaining = bytes.len() - i;

    if remaining >= 2 {
        let b1 = bytes[i + 1];
        match (b0, b1) {
            (0xC2, 0xAD) => return Some(2), // U+00AD Soft Hyphen
            (0xCD, 0x8F) => return Some(2), // U+034F Combining Grapheme Joiner
            _ => {}
        }
    }

    if remaining >= 3 {
        let b1 = bytes[i + 1];
        let b2 = bytes[i + 2];
        match (b0, b1, b2) {
            (0xE2, 0x80, 0x8B) => return Some(3), // U+200B Zero Width Space
            (0xE2, 0x80, 0x8C) => return Some(3), // U+200C Zero Width Non-Joiner
            (0xE2, 0x80, 0x8D) => return Some(3), // U+200D Zero Width Joiner
            (0xE2, 0x80, 0x8E) => return Some(3), // U+200E Left-to-Right Mark
            (0xE2, 0x80, 0x8F) => return Some(3), // U+200F Right-to-Left Mark
            (0xE2, 0x80, 0xA8) => return Some(3), // U+2028 Line Separator
            (0xE2, 0x80, 0xA9) => return Some(3), // U+2029 Paragraph Separator
            (0xEF, 0xBB, 0xBF) => return Some(3), // U+FEFF BOM
            _ => {}
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- HTML parsing --

    #[test]
    fn strip_html_basic() {
        let text = extract_body_text("<html><body><p>Hello <b>world</b></p></body></html>");
        assert!(text.contains("Hello world"), "got: {text}");
    }

    #[test]
    fn strip_html_entities() {
        let text = extract_body_text("<p>A &amp; B &lt; C</p>");
        assert!(text.contains("A & B < C"), "got: {text}");
    }

    #[test]
    fn strip_html_script_and_style() {
        let text = extract_body_text(
            "<style>body{color:red}</style><script>alert('hi');</script><p>Content</p>",
        );
        assert_eq!(text.trim(), "Content");
    }

    #[test]
    fn strip_html_head() {
        let text =
            extract_body_text("<html><head><title>T</title></head><body>Content</body></html>");
        assert_eq!(text.trim(), "Content");
    }

    #[test]
    fn strip_html_comments() {
        let text = extract_body_text("Before<!-- hidden -->After");
        assert!(text.contains("Before") && text.contains("After"));
        assert!(!text.contains("hidden"));
    }

    #[test]
    fn strip_html_blockquote() {
        let text = extract_body_text("<p>My reply</p><blockquote><p>Original</p></blockquote>");
        assert!(text.contains("My reply"));
        assert!(!text.contains("Original"));
    }

    #[test]
    fn strip_html_br_newline() {
        let text = extract_body_text("Line one<br>Line two<BR/>Line three");
        assert!(
            text.contains("Line one") && text.contains("Line two") && text.contains("Line three")
        );
    }

    // -- Table serialization --

    #[test]
    fn table_with_headers() {
        let text = extract_body_text(
            "<table><tr><th>Item</th><th>Qty</th></tr><tr><td>MacBook</td><td>1</td></tr></table>",
        );
        assert!(text.contains("Item: MacBook, Qty: 1"), "got: {text}");
    }

    #[test]
    fn table_key_value() {
        let text = extract_body_text(
            "<table><tr><td>Status:</td><td>Active</td></tr><tr><td>Date:</td><td>2021-10-20</td></tr></table>",
        );
        assert!(text.contains("Status: Active"));
        assert!(text.contains("Date: 2021-10-20"));
    }

    #[test]
    fn table_key_value_no_double_colon() {
        let text = extract_body_text("<table><tr><td>Status:</td><td>Active</td></tr></table>");
        assert!(!text.contains("Status:: "), "double colon: {text}");
    }

    #[test]
    fn table_nested() {
        let text = extract_body_text(
            "<table><tr><td><table><tr><td>Inner</td></tr></table></td></tr></table>",
        );
        assert!(text.contains("Inner"));
    }

    #[test]
    fn table_empty_cells_filtered() {
        let text = extract_body_text(
            "<table><tr><th>Name</th><th>Value</th></tr><tr><td>Foo</td><td></td></tr></table>",
        );
        assert!(text.contains("Name: Foo"));
        assert!(!text.contains("Value:"), "empty value leaked: {text}");
    }

    #[test]
    fn table_tfoot_separate_from_thead() {
        let html = r#"<table>
            <thead><tr><th>Product</th><th>Qty</th><th>Price</th></tr></thead>
            <tbody><tr><td>Widget</td><td>2</td><td>10€</td></tr></tbody>
            <tfoot><tr><td>Total:</td><td></td><td>25€</td></tr></tfoot>
        </table>"#;
        let text = extract_body_text(html);
        assert!(text.contains("Product: Widget"));
        assert!(
            !text.contains("Product: Total"),
            "tfoot got thead headers: {text}"
        );
        assert!(text.contains("Total: 25"));
    }

    #[test]
    fn table_zurb_foundation_th_only() {
        let text = extract_body_text(
            r#"<table><tbody><tr><th>Important</th><th class="expander"></th></tr></tbody></table>"#,
        );
        assert!(text.contains("Important"), "lost <th> content: {text}");
    }

    // -- Hidden-on-mobile stripping --

    #[test]
    fn strip_mobile_hide_class() {
        let text = extract_body_text(
            r#"<p>Visible</p><div class="mobile-hide">Hidden copy</div><p>Bye</p>"#,
        );
        assert!(text.contains("Visible") && text.contains("Bye"));
        assert!(
            !text.contains("Hidden copy"),
            "mobile-hide not stripped: {text}"
        );
    }

    #[test]
    fn strip_mobile_hide_case_insensitive() {
        let text = extract_body_text(r#"<div CLASS="HideMobile">gone</div><p>ok</p>"#);
        assert!(!text.contains("gone"));
    }

    #[test]
    fn strip_moe_hide_class() {
        let text = extract_body_text(
            r#"<p>Order body</p><table><tr><td class="moe-hide">PRAWO KONSUMENCKIE legal warranty text that is display:none on render</td></tr></table>"#,
        );
        assert!(text.contains("Order body"));
        assert!(
            !text.contains("PRAWO KONSUMENCKIE"),
            "moe-hide not stripped: {text}"
        );
    }

    #[test]
    fn strip_footer_copyright_class() {
        let text = extract_body_text(
            r#"<p>Order body</p><table><tr><td class="footer-copyright-td">Apple Distribution International Ltd., Hollyhill, Cork</td></tr><tr><td class="footer-copyright-td"><div class="footer-copyright-div">Prawa autorskie 2021 Apple Inc.</div></td></tr></table>"#,
        );
        assert!(text.contains("Order body"));
        assert!(
            !text.contains("Apple Distribution") && !text.contains("Prawa autorskie"),
            "footer-copyright not stripped: {text}"
        );
    }

    #[test]
    fn strip_sonar_footer_role() {
        let text = extract_body_text(
            r#"<p>Order details</p><table data-sonar-role="footer"><tr><td>Legal disclaimer text and unsubscribe links</td></tr></table>"#,
        );
        assert!(text.contains("Order details"));
        assert!(
            !text.contains("Legal disclaimer"),
            "sonar footer not stripped: {text}"
        );
    }

    #[test]
    fn strip_no_match_preserves_content() {
        let text = extract_body_text(r#"<p>Hi</p><div class="header">Nav</div>"#);
        assert!(text.contains("Hi") && text.contains("Nav"));
    }

    // -- clean_body_text --

    #[test]
    fn clean_strips_https_url() {
        assert_eq!(
            clean_body_text("Check https://example.com/x?utm=1 for details"),
            "Check for details"
        );
    }

    #[test]
    fn clean_strips_http_url() {
        assert_eq!(
            clean_body_text("Visit http://example.com and see"),
            "Visit and see"
        );
    }

    #[test]
    fn clean_removes_empty_parens() {
        assert_eq!(
            clean_body_text("click here ( ) for more"),
            "click here for more"
        );
    }

    #[test]
    fn clean_url_in_parens() {
        assert_eq!(
            clean_body_text("a lib ( https://example.com/foo ) does stuff"),
            "a lib does stuff"
        );
    }

    #[test]
    fn clean_collapses_newlines() {
        assert_eq!(clean_body_text("First\n\n\n\n\nSecond"), "First\n\nSecond");
    }

    #[test]
    fn clean_collapses_spaces() {
        assert_eq!(clean_body_text("hello    world   test"), "hello world test");
    }

    #[test]
    fn clean_strips_quoted_lines() {
        assert_eq!(
            clean_body_text("My reply\n> Previous\n> More\nMy text"),
            "My reply\n\nMy text"
        );
    }

    #[test]
    fn clean_preserves_gt_without_space() {
        // Bare `>text` is not a quote marker; must not delete the line.
        let result = clean_body_text("Line one\n>Not a quote\nLine three");
        assert!(result.contains("Not a quote"), "got: {result}");
    }

    #[test]
    fn clean_preserves_utf8() {
        assert_eq!(clean_body_text("Héllo wörld café"), "Héllo wörld café");
    }
}
