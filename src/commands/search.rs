use crate::settings::SearchFormat;
use crate::{clients, db, embeddings};
use anyhow::Result;
use chrono::DateTime;
use serde::Serialize;

/// One ranked hit. Generic on purpose -- launchers (Alfred, Raycast)
/// reshape into their own item formats.
#[derive(Debug, Serialize)]
struct SearchResult {
    rank: usize,
    score: f64,
    message_id: String,
    from: String,
    to: String,
    subject: String,
    /// RFC 3339 UTC, or `null` if the source had no parseable date.
    date: Option<String>,
    /// Truncated to `config.search.preview_length` chars, newlines
    /// flattened to spaces -- matches the text format's per-row preview.
    body_preview: String,
    is_mailing_list: bool,
}

pub fn search_emails(
    query: &str,
    open_n: Option<usize>,
    format: SearchFormat,
    limit: usize,
    preview_len: usize,
) -> Result<()> {
    let db_path = db::default_db_path()?;
    let conn = db::initialize(&db_path)?;

    tracing::info!("Loading embedding model...");
    let mut embedder = embeddings::Embedder::new()?;

    let query_vector = embedder.embed_query(query)?;

    // Over-fetch by 3x so the length-factor rerank below has room to
    // demote thin matches without truncating below the user's limit.
    let results = db::search_vectors(&conn, &query_vector, limit * 3)?;

    // Length-factor rerank: trivially-short bodies ("hi", "thanks") can
    // hit very high cosine similarity on short queries and crowd out
    // richer hits. Scale (1 - distance) by a factor that ramps linearly
    // from 0.3 at 0 chars to 1.0 at >=50 chars of subject+body.
    let mut scored_results: Vec<(f64, crate::parser::Email)> = results
        .iter()
        .filter_map(|(id, distance)| {
            let email = db::get_email_by_id(&conn, *id).ok()??;
            let content_len = email.subject.len() + email.body_text.len();
            let length_factor = (0.3 + 0.7 * (content_len as f64 / 50.0)).min(1.0);
            Some(((1.0 - distance) * length_factor, email))
        })
        .collect();
    scored_results.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored_results.truncate(limit);

    match format {
        SearchFormat::Text => print_text(&scored_results, preview_len),
        SearchFormat::Json => print_json(&scored_results, preview_len)?,
    }

    if let Some(n) = open_n {
        if scored_results.is_empty() {
            tracing::error!("Nothing to open -- no results.");
        } else if n == 0 || n > scored_results.len() {
            tracing::error!(
                "Invalid result number {}. Choose between 1 and {}.",
                n,
                scored_results.len()
            );
        } else {
            let email = &scored_results[n - 1].1;
            if let Err(e) = clients::open_message(&conn, &email.message_id) {
                tracing::error!("{e}");
            }
        }
    }

    Ok(())
}

fn print_text(scored: &[(f64, crate::parser::Email)], preview_len: usize) {
    if scored.is_empty() {
        println!("No results found. Have you indexed your emails yet?");
        return;
    }

    println!(
        "\n{:<1} {:<7} {:<20} {:<30} Subject",
        "#", "Score", "Date", "From"
    );
    println!("{}", "-".repeat(120));

    for (rank, (score, email)) in scored.iter().enumerate() {
        let from_short: String = bare_address(&email.from).chars().take(29).collect();
        let subject_short: String = email.subject.chars().take(58).collect();

        let date_display = DateTime::from_timestamp(email.date, 0)
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_default();

        println!(
            "{:<1} {:<7.4} {:<20} {:<30} {}",
            rank + 1,
            score,
            date_display,
            from_short,
            subject_short
        );

        let body_preview = build_preview(&email.body_text, preview_len);
        if !body_preview.is_empty() {
            println!("  {}", body_preview);
        }
        println!();
    }
}

fn print_json(scored: &[(f64, crate::parser::Email)], preview_len: usize) -> Result<()> {
    let hits: Vec<SearchResult> = scored
        .iter()
        .enumerate()
        .map(|(i, (score, email))| SearchResult {
            rank: i + 1,
            score: *score,
            message_id: email.message_id.clone(),
            from: email.from.clone(),
            to: email.to.clone(),
            subject: email.subject.clone(),
            date: DateTime::from_timestamp(email.date, 0).map(|dt| dt.to_rfc3339()),
            body_preview: build_preview(&email.body_text, preview_len),
            is_mailing_list: email.is_mailing_list,
        })
        .collect();

    serde_json::to_writer(std::io::stdout().lock(), &hits)?;
    println!();
    Ok(())
}

fn build_preview(body: &str, preview_len: usize) -> String {
    body.chars()
        .take(preview_len)
        .collect::<String>()
        .replace('\n', " ")
}

/// Pull the bare address out of an RFC 2822 `From:` header. Handles
/// `addr@host`, `Name <addr@host>`, `"Name" <addr@host>`. Falls back to
/// the trimmed input when no angle brackets are present.
fn bare_address(from: &str) -> &str {
    if let Some(start) = from.rfind('<')
        && let Some(end_rel) = from[start..].find('>')
    {
        return from[start + 1..start + end_rel].trim();
    }
    from.trim()
}
