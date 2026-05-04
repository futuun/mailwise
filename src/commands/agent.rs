//! launchd agent install/uninstall for fully hands-off background indexing.
//!
//! Writes `~/Library/LaunchAgents/com.mailwise.indexer.plist` pointing at
//! the current binary's `index` subcommand, with `RunAtLoad=true` and
//! `KeepAlive=true` so it starts on login and respawns on crash. Logs land
//! in `~/.mailwise/logs/`.
//!
//! We use the legacy `launchctl load -w` / `unload -w` form rather than the
//! newer `bootstrap`/`bootout` pair: it's still supported on every macOS
//! version mailwise targets and doesn't require knowing the user's UID.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

const LABEL: &str = "com.mailwise.indexer";

pub fn install() -> Result<()> {
    let cfg = crate::settings::active();
    if cfg.clients.is_empty() {
        anyhow::bail!("No mail clients configured. Run `mailwise config` first.");
    }

    let exe = std::env::current_exe().context("locating mailwise binary")?;
    let plist_path = plist_path()?;
    let log_dir = crate::settings::mailwise_dir()?.join("logs");
    std::fs::create_dir_all(&log_dir).with_context(|| format!("creating {}", log_dir.display()))?;

    // If we're reinstalling, unload any prior agent first so `load -w`
    // doesn't fail with "Operation already in progress".
    if plist_path.exists() {
        let _ = run_launchctl(&["unload", "-w"], &plist_path);
    }

    let plist = render_plist(&exe, &log_dir);
    std::fs::create_dir_all(plist_path.parent().unwrap())?;
    std::fs::write(&plist_path, &plist)
        .with_context(|| format!("writing {}", plist_path.display()))?;

    run_launchctl(&["load", "-w"], &plist_path)?;

    println!("Installed launchd agent");
    println!("  Plist: {}", plist_path.display());
    println!("  Logs:  {}/indexer.log", log_dir.display());
    println!("\nThe indexer is now running in the background.");
    println!(
        "Tail the log with: tail -f {}/indexer.log",
        log_dir.display()
    );

    // launchd is the responsible process for the agent, so any FDA grant on
    // the parent terminal doesn't carry over -- the binary itself needs FDA
    // for Apple Mail to be readable.
    if cfg.clients.contains(&crate::clients::Source::AppleMail) {
        println!("\nApple Mail note: the launchd agent runs without inheriting your");
        println!("terminal's Full Disk Access. Grant FDA to the mailwise binary at:");
        println!("  {}", exe.display());
        println!(
            "Open the right pane: open 'x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles'"
        );
    }
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let plist_path = plist_path()?;
    if !plist_path.exists() {
        anyhow::bail!("No launchd agent installed at {}", plist_path.display());
    }
    let _ = run_launchctl(&["unload", "-w"], &plist_path);
    std::fs::remove_file(&plist_path)
        .with_context(|| format!("removing {}", plist_path.display()))?;
    println!("Uninstalled launchd agent");
    Ok(())
}

/// True if the plist exists. Plist presence == "installed" because launchd
/// will respawn the indexer on next login regardless of current process
/// state -- callers planning destructive ops need that signal.
pub fn is_installed() -> Result<bool> {
    Ok(plist_path()?.exists())
}

/// Stop the launchd agent if installed; returns `true` when the unload
/// fired so the caller knows whether to reload later. `launchctl unload
/// -w` is synchronous (SIGTERM then wait), which is what `config` needs:
/// the indexer's flock and DB connection are released before we touch
/// the file.
pub fn stop_if_installed() -> Result<bool> {
    let plist_path = plist_path()?;
    if !plist_path.exists() {
        return Ok(false);
    }
    run_launchctl(&["unload", "-w"], &plist_path)?;
    Ok(true)
}

/// Reload an agent that was previously stopped by [`stop_if_installed`].
/// Bails if the plist no longer exists (caller bug).
pub fn start() -> Result<()> {
    let plist_path = plist_path()?;
    if !plist_path.exists() {
        anyhow::bail!("No launchd agent installed at {}", plist_path.display());
    }
    run_launchctl(&["load", "-w"], &plist_path)
}

fn plist_path() -> Result<PathBuf> {
    let home = std::env::home_dir().context("HOME not set")?;
    Ok(home.join(format!("Library/LaunchAgents/{LABEL}.plist")))
}

fn render_plist(exe: &Path, log_dir: &Path) -> String {
    let exe_s = xml_escape(&exe.to_string_lossy());
    let log_s = xml_escape(&log_dir.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe_s}</string>
        <string>index</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log_s}/indexer.log</string>
    <key>StandardErrorPath</key>
    <string>{log_s}/indexer.error.log</string>
</dict>
</plist>
"#
    )
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&apos;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

fn run_launchctl(args: &[&str], plist: &Path) -> Result<()> {
    let status = Command::new("launchctl")
        .args(args)
        .arg(plist)
        .status()
        .context("running launchctl")?;
    if !status.success() {
        anyhow::bail!("launchctl {} failed (exit {status})", args.join(" "));
    }
    Ok(())
}
