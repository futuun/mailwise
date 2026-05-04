//! `mailwise config` -- interactive setup, also the canonical way to
//! edit settings later.
//!
//! Walks the user through enabled clients, the plain-mbox path (if any),
//! the open-preference order, and the tunables. Writes to
//! `~/.mailwise/config.toml` and bounces the launchd agent on change so
//! the new settings take effect immediately.

use anyhow::Result;
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input, MultiSelect, Select, Sort};
use std::path::{Path, PathBuf};

use super::agent;
use crate::clients;
use crate::clients::{MailClient, Source};
use crate::process_lock;
use crate::settings::{IndexSettings, SearchFormat, SearchSettings, Settings};

pub fn run() -> Result<()> {
    let theme = ColorfulTheme::default();

    // Re-running `config` should feel like editing the existing settings, not
    // resetting it: every prompt below uses the saved value as its default.
    // `Settings::load()` returns defaults when the file is missing, so the
    // first-run path naturally falls through to the same code.
    let existing = Settings::load().unwrap_or_default();
    let first_run = existing.clients.is_empty();

    println!("Mailwise interactive setup\n");

    // Don't go through `PlainMbox::new()` here -- it reads the (not-yet-
    // written) config and would pin a stale `plain_mbox_path` into the
    // process-wide OnceLock.
    let availability: Vec<bool> = Source::ALL
        .iter()
        .map(|s| match s {
            Source::AppleMail => clients::apple_mail::AppleMail::new().is_available(),
            Source::Thunderbird => clients::thunderbird::Thunderbird::new().is_available(),
            Source::PlainMbox => true,
        })
        .collect();

    let labels: Vec<String> = Source::ALL
        .iter()
        .zip(availability.iter())
        .map(|(s, available)| {
            if *available {
                s.pretty().to_string()
            } else {
                format!("{} (not detected)", s.pretty())
            }
        })
        .collect();

    // First run: pre-check detected GUI clients (plain-mbox stays opt-in
    // because it needs a path). Re-run: pre-check whatever's saved,
    // ignoring detection -- lets users keep a temporarily-unavailable
    // client in their config.
    let defaults: Vec<bool> = if first_run {
        Source::ALL
            .iter()
            .zip(availability.iter())
            .map(|(s, available)| *available && *s != Source::PlainMbox)
            .collect()
    } else {
        Source::ALL
            .iter()
            .map(|s| existing.clients.contains(s))
            .collect()
    };

    let chosen = MultiSelect::with_theme(&theme)
        .with_prompt("Select mail clients to index (space to toggle, enter to confirm)")
        .items(&labels)
        .defaults(&defaults)
        .interact()?;
    if chosen.is_empty() {
        anyhow::bail!("At least one client must be selected.");
    }

    let mut selected: Vec<Source> = chosen.iter().map(|i| Source::ALL[*i]).collect();

    let (plain_mbox_path, open_google_takeout_in_gmail) = if selected.contains(&Source::PlainMbox) {
        let default_path = existing
            .plain_mbox_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| {
                std::env::home_dir()
                    .map(|h| h.join(".mailwise/mbox_folder"))
                    .unwrap_or_else(|| PathBuf::from("./mbox_folder"))
                    .to_string_lossy()
                    .into_owned()
            });
        let raw: String = Input::with_theme(&theme)
            .with_prompt("Path to plain mbox directory")
            .default(default_path)
            .interact_text()?;
        let open_in_gmail = Confirm::with_theme(&theme)
            .with_prompt("Open Google Takeout messages in Gmail web (when X-GM-* headers present)?")
            .default(existing.open_google_takeout_in_gmail)
            .interact()?;
        (Some(PathBuf::from(expand_tilde(raw.trim()))), open_in_gmail)
    } else {
        (None, false)
    };

    if selected.len() > 1 {
        // Pre-sort to match the saved order so the Sort dialog opens
        // where the user left it; newly-enabled clients append at the
        // end.
        let mut ordered: Vec<Source> = Vec::with_capacity(selected.len());
        for s in &existing.clients {
            if selected.contains(s) {
                ordered.push(*s);
            }
        }
        for s in &selected {
            if !ordered.contains(s) {
                ordered.push(*s);
            }
        }
        selected = ordered;

        let labels: Vec<&'static str> = selected.iter().map(|s| s.pretty()).collect();
        let order = Sort::with_theme(&theme)
            .with_prompt(
                "Order to try when opening (most preferred first; space to grab, arrows to move)",
            )
            .items(&labels)
            .interact()?;
        selected = order.iter().map(|i| selected[*i]).collect();
    }

    let poll_interval: u64 = Input::with_theme(&theme)
        .with_prompt("Poll interval (seconds)")
        .default(existing.index.poll_interval)
        .interact_text()?;

    let result_limit: usize = Input::with_theme(&theme)
        .with_prompt("Number of search results to show")
        .default(existing.search.result_limit)
        .interact_text()?;

    let preview_length: usize = Input::with_theme(&theme)
        .with_prompt("Length of body preview snippets (characters)")
        .default(existing.search.preview_length)
        .interact_text()?;

    // Order must match the variant-mapping `match` below.
    let format_labels = ["text", "json"];
    let format_default = match existing.search.default_format {
        SearchFormat::Text => 0,
        SearchFormat::Json => 1,
    };
    let format_choice = Select::with_theme(&theme)
        .with_prompt("Default search output format (overridable per-call with --format)")
        .items(format_labels)
        .default(format_default)
        .interact()?;
    let default_format = match format_choice {
        0 => SearchFormat::Text,
        _ => SearchFormat::Json,
    };

    let settings = Settings {
        clients: selected,
        plain_mbox_path,
        open_google_takeout_in_gmail,
        index: IndexSettings { poll_interval },
        search: SearchSettings {
            result_limit,
            preview_length,
            default_format,
        },
    };

    // `Settings` doesn't derive PartialEq; serialise both to canonical
    // TOML and string-compare. Skipping the bounce on a no-op keeps
    // `mailwise config` side-effect-free when used to just inspect.
    let settings_changed = toml::to_string(&settings)? != toml::to_string(&existing)?;

    // Stop the launchd agent first so it picks up the new config when we
    // restart it below. A foreground `mailwise index` in another terminal
    // we can't kill safely -- detect it via the indexer lock and refuse
    // rather than racing.
    let agent_was_running = if settings_changed {
        agent::stop_if_installed()?
    } else {
        false
    };

    if settings_changed && process_lock::is_held()? {
        if agent_was_running {
            agent::start()?;
        }
        anyhow::bail!(
            "A foreground `mailwise index` is running in another terminal. \
             Stop it (Ctrl-C in that window), then re-run `mailwise config`."
        );
    }

    settings.save()?;
    println!("\nConfiguration saved to {}", Settings::path()?.display());

    if agent_was_running {
        agent::start()?;
        println!("Restarted launchd agent with new configuration.");
    } else if !agent::is_installed()? {
        println!("Run `mailwise index` to start indexing.");
    }
    Ok(())
}

fn expand_tilde(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = std::env::home_dir()
    {
        return Path::new(&home).join(rest).to_string_lossy().into_owned();
    }
    s.to_string()
}
