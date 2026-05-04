//! User configuration, persisted as `~/.mailwise/config.toml`.
//!
//! Loaded once per process via [`active`] (an `OnceLock`) so call sites
//! can treat it as a static. Defaults are usable, but `clients` starts
//! empty: the indexer bails with a "run `mailwise config`" message
//! rather than guessing what to index.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use crate::clients::Source;

/// Prepended to every saved config so the file is self-documenting:
/// the running indexer caches via `OnceLock` and won't see manual edits
/// until restart, but `mailwise config` handles the bounce.
const HEADER: &str = "\
# mailwise configuration
#
# Prefer `mailwise config` for changes -- it bounces the launchd agent so
# your edits take effect immediately. Manual edits are fine, but a
# running indexer won't pick them up until it restarts.

";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Mail clients to index, in preference order. The first available
    /// client wins when opening a message that exists in multiple sources.
    pub clients: Vec<Source>,

    /// Directory containing plain mbox files. Only consulted when
    /// `"plain-mbox"` is in [`Self::clients`].
    pub plain_mbox_path: Option<PathBuf>,

    /// Open messages exported from Google Takeout (those carrying
    /// `X-GM-THRID` / `X-GM-MSGID`) in the Gmail web UI rather than the
    /// `$TMPDIR` preview. Per-message: messages without those headers
    /// fall through to the preview path either way.
    pub open_google_takeout_in_gmail: bool,

    pub index: IndexSettings,
    pub search: SearchSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IndexSettings {
    /// Seconds to sleep between sync cycles in the steady-state poll loop.
    pub poll_interval: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchSettings {
    pub result_limit: usize,
    /// Body preview snippet length, in chars.
    pub preview_length: usize,
    /// Output format used when `search` is invoked without `--format`.
    /// Set to `"json"` for launcher-driven workflows (Alfred, Raycast).
    pub default_format: SearchFormat,
}

/// Output format for `search`. Lives in config because it doubles as a
/// per-user default on top of the CLI flag.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum SearchFormat {
    /// Human-readable table.
    #[default]
    Text,
    /// JSON array on stdout. Always emits `[]` for empty hits so
    /// downstream parsers (Alfred, Raycast, jq, ...) don't have to
    /// special-case the no-results path.
    Json,
}

impl Default for IndexSettings {
    fn default() -> Self {
        Self { poll_interval: 60 }
    }
}

impl Default for SearchSettings {
    fn default() -> Self {
        Self {
            result_limit: 10,
            preview_length: 118,
            default_format: SearchFormat::Text,
        }
    }
}

impl Settings {
    pub fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.index.poll_interval)
    }

    pub fn path() -> Result<PathBuf> {
        Ok(mailwise_dir()?.join("config.toml"))
    }

    /// Returns defaults if the file is missing; propagates parse errors
    /// so the user sees them rather than silently running with defaults
    /// that don't reflect their intent.
    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&body).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        let body = format!("{HEADER}{}", toml::to_string_pretty(self)?);
        std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Resolved plain-mbox root with `~/` expanded. Done at the boundary
    /// because manual edits commonly contain `~/...` and we don't want
    /// every consumer to remember.
    pub fn plain_mbox_root(&self) -> Option<PathBuf> {
        self.plain_mbox_path.as_deref().map(expand_tilde)
    }
}

/// Process-lifetime cached config. Parse failures fall back to defaults
/// with a tracing warning -- the indexer will then bail on the empty
/// client list, so the user sees a clear next step.
pub fn active() -> &'static Settings {
    static SETTINGS: OnceLock<Settings> = OnceLock::new();
    SETTINGS.get_or_init(|| match Settings::load() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Failed to load config, using defaults: {e:#}");
            Settings::default()
        }
    })
}

/// Single source of truth for the data directory. db.rs and
/// embeddings/model.rs both route through this
pub fn mailwise_dir() -> Result<PathBuf> {
    let home = std::env::home_dir().context("HOME not set")?;
    let dir = home.join(".mailwise");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

fn expand_tilde(p: &Path) -> PathBuf {
    if let Some(rest) = p.to_str().and_then(|s| s.strip_prefix("~/"))
        && let Some(home) = std::env::home_dir()
    {
        return home.join(rest);
    }
    p.to_path_buf()
}
