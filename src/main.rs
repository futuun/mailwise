mod clients;
mod commands;
mod db;
mod embeddings;
mod parser;
mod process_lock;
mod settings;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "mailwise", about = "Semantic email search")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Interactive setup / settings editor. Writes ~/.mailwise/config.toml
    /// and restarts the launchd agent (if installed) so changes take effect.
    Config,
    /// Index existing emails and watch for new ones
    Index {
        /// Also embed mailing list emails (excluded by default)
        #[arg(long)]
        include_mailing_lists: bool,
    },
    /// Search indexed emails by meaning
    Search {
        /// Natural language query
        query: String,

        /// Open the nth result in the configured mail client
        #[arg(long)]
        open: Option<usize>,

        /// Output format. `text` is human-readable; `json` emits a JSON
        /// array on stdout for launchers (Alfred, Raycast) and pipelines.
        /// Defaults to `search.default_format` from `~/.mailwise/config.toml`
        /// when omitted.
        #[arg(long, value_enum)]
        format: Option<settings::SearchFormat>,

        /// Maximum number of results to return.
        /// Defaults to `search.result_limit` from `~/.mailwise/config.toml`
        /// when omitted.
        #[arg(long)]
        limit: Option<usize>,

        /// Number of body characters to include in each result's preview.
        /// Defaults to `search.preview_length` from `~/.mailwise/config.toml`
        /// when omitted.
        #[arg(long)]
        preview_length: Option<usize>,
    },
    /// Open an indexed email in the configured mail client by Message-ID.
    /// Pair with `search --format=json` from a launcher.
    Open {
        /// RFC 2822 Message-ID, angle brackets stripped (as it appears in
        /// the JSON output of `search`).
        message_id: String,
    },
    /// Install a launchd agent that keeps `index` running in the background
    InstallAgent,
    /// Uninstall the launchd agent
    UninstallAgent,
}

fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    // Must run before any Connection::open call so the auto-extension hook
    // is in place when sqlite installs the vec0 virtual table module.
    db::register_sqlite_vec();

    match cli.command {
        Commands::Config => commands::config::run()?,
        Commands::Index {
            include_mailing_lists,
        } => commands::index::run(include_mailing_lists)?,
        Commands::Search {
            query,
            open,
            format,
            limit,
            preview_length,
        } => {
            let cfg = settings::active();
            let format = format.unwrap_or(cfg.search.default_format);
            let limit = limit.unwrap_or(cfg.search.result_limit);
            let preview_length = preview_length.unwrap_or(cfg.search.preview_length);
            commands::search::search_emails(&query, open, format, limit, preview_length)?
        }
        Commands::Open { message_id } => commands::open::run(&message_id)?,
        Commands::InstallAgent => commands::agent::install()?,
        Commands::UninstallAgent => commands::agent::uninstall()?,
    }

    Ok(())
}

/// TTY runs get compact, untimestamped output; pipes (launchd-redirected)
/// get timestamps so the daemon log is useful on its own.
///
/// The default filter silences html5ever/markup5ever/selectors -- scraper
/// runs over every body and malformed marketing HTML floods WARN with
/// "foster parenting not implemented" and friends. `RUST_LOG` overrides.
fn init_tracing() {
    use std::io::IsTerminal as _;
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,html5ever=off,markup5ever=off,selectors=off"));
    let base = fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .compact();
    if std::io::stderr().is_terminal() {
        base.without_time().init();
    } else {
        base.init();
    }
}
