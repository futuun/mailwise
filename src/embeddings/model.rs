use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

const MODEL_URL: &str = "https://huggingface.co/jinaai/jina-embeddings-v5-text-nano-retrieval-GGUF/resolve/main/v5-nano-retrieval-Q8_0.gguf";
const MODEL_SHA256: &str = "86b6e6279e9b9e71389f02a082764a2ac2b15a50e37482c26f98d69092f12442";

/// `~/.mailwise/models/model.gguf`, downloading on first call.
pub fn ensure_model() -> Result<PathBuf> {
    let path = crate::settings::mailwise_dir()?.join("models/model.gguf");
    if !path.exists() {
        download(&path)?;
    }
    Ok(path)
}

/// Resumable download via `curl -C -`. Writes to `.gguf.part` and only
/// renames into place after SHA-256 verification; a failed checksum
/// removes the partial so the next run restarts clean.
fn download(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension("gguf.part");

    let existing_bytes = tmp.metadata().map(|m| m.len()).unwrap_or(0);
    if existing_bytes > 0 {
        tracing::info!("Resuming model download from {existing_bytes} bytes...");
    } else {
        tracing::info!("Downloading embedding model (~230 MB)...");
    }

    let status = Command::new("curl")
        .args(["-fSL", "-C", "-", "--progress-bar", "-o"])
        .arg(&tmp)
        .arg(MODEL_URL)
        .status()
        .context("Failed to run curl")?;
    if !status.success() {
        // Don't delete tmp; it may be partially valid and a retry can resume.
        anyhow::bail!("Download failed (curl exit code: {status})");
    }

    tracing::info!("Verifying download integrity...");
    let actual = sha256_hex(&tmp)?;
    if !actual.eq_ignore_ascii_case(MODEL_SHA256) {
        let _ = std::fs::remove_file(&tmp);
        anyhow::bail!(
            "Downloaded model failed integrity check.\n  expected: {MODEL_SHA256}\n  got:      {actual}\n\
             Partial file removed; re-run to retry."
        );
    }

    std::fs::rename(&tmp, path).context("Failed to move downloaded model into place")?;
    tracing::info!("Model saved to {}", path.display());
    Ok(())
}

/// Shells out to `shasum -a 256` rather than pulling in a hashing
/// crate for a single use. macOS ships `shasum` as part of Perl, and
/// the shell-out matches the existing curl pattern.
fn sha256_hex(path: &Path) -> Result<String> {
    let output = Command::new("shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        .context("Failed to run `shasum -a 256` (required for model integrity verification)")?;
    if !output.status.success() {
        anyhow::bail!(
            "shasum failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let line = String::from_utf8(output.stdout).context("shasum returned non-UTF-8 output")?;
    let hex = line
        .split_whitespace()
        .next()
        .context("empty shasum output")?;
    Ok(hex.to_string())
}
