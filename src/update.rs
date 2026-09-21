use crate::app::AppMessage;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

const REPO: &str = "viniromao/listen_to_it";
const INSTALLER: &str = "listen_to_it-installer.sh";
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 3600);

pub fn start(tx: UnboundedSender<AppMessage>) {
    if !self_update_applies() {
        return;
    }
    tokio::spawn(async move {
        match install_newer().await {
            Ok(Some(version)) => {
                let _ = tx.send(AppMessage::Updated(version));
            }
            Ok(None) => {}
            Err(e) => crate::logline!("update: {e:#}"),
        }
    });
}

fn self_update_applies() -> bool {
    if !cfg!(unix) {
        return false;
    }
    if std::env::var_os("LISTEN_TO_IT_NO_UPDATE").is_some() {
        crate::logline!("update: disabled by LISTEN_TO_IT_NO_UPDATE");
        return false;
    }
    match receipt() {
        Some(path) if path.exists() => true,
        _ => {
            crate::logline!("update: no install receipt, leaving this copy alone");
            false
        }
    }
}

fn receipt() -> Option<PathBuf> {
    let config = match std::env::var_os("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(config.join("listen_to_it").join("listen_to_it-receipt.json"))
}

async fn install_newer() -> Result<Option<String>> {
    let stamp = crate::ytdlp::cache_dir()?.join("update.checked_at");
    if checked_recently(&stamp) {
        return Ok(None);
    }

    let latest = latest_release(REPO).await?;
    let _ = std::fs::write(&stamp, b"");

    let current = env!("CARGO_PKG_VERSION");
    if !is_newer(&latest, current) {
        crate::logline!("update: {current} is the latest release");
        return Ok(None);
    }

    crate::logline!("update: installing {latest} over {current}");
    run_installer().await?;
    Ok(Some(latest.trim_start_matches('v').to_string()))
}

fn checked_recently(stamp: &Path) -> bool {
    std::fs::metadata(stamp)
        .and_then(|meta| meta.modified())
        .map(|modified| modified.elapsed().map(|age| age < CHECK_INTERVAL).unwrap_or(false))
        .unwrap_or(false)
}

async fn latest_release(repo: &str) -> Result<String> {
    let url = format!("https://github.com/{repo}/releases/latest");
    let landed = reqwest::get(&url)
        .await
        .with_context(|| format!("release check against {url} failed"))?
        .error_for_status()?
        .url()
        .clone();

    let tag = landed
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .unwrap_or_default()
        .to_string();

    anyhow::ensure!(
        version_numbers(&tag).iter().any(|n| *n > 0),
        "no release to update to ({landed} does not name a version)"
    );
    Ok(tag)
}

async fn run_installer() -> Result<()> {
    let url = format!("https://github.com/{REPO}/releases/latest/download/{INSTALLER}");
    let script = reqwest::get(&url)
        .await
        .with_context(|| format!("downloading {url} failed"))?
        .error_for_status()?
        .text()
        .await?;

    let path = crate::ytdlp::cache_dir()?.join(INSTALLER);
    std::fs::write(&path, script).context("could not stage the installer")?;

    let run = tokio::process::Command::new("sh")
        .arg(&path)
        .env("INSTALLER_NO_MODIFY_PATH", "1")
        .env("INSTALLER_PRINT_QUIET", "1")
        .output()
        .await
        .context("could not run the installer");
    let _ = std::fs::remove_file(&path);

    let run = run?;
    anyhow::ensure!(
        run.status.success(),
        "installer failed ({}): {}",
        run.status,
        String::from_utf8_lossy(&run.stderr).trim()
    );
    Ok(())
}

fn is_newer(latest: &str, current: &str) -> bool {
    version_numbers(latest) > version_numbers(current)
}

fn version_numbers(version: &str) -> Vec<u64> {
    version
        .trim()
        .trim_start_matches('v')
        .split('-')
        .next()
        .unwrap_or_default()
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}
