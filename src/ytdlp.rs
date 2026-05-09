use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const ASSET_NAME: &str = "yt-dlp_linux";
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const ASSET_NAME: &str = "yt-dlp_linux_aarch64";
#[cfg(target_os = "macos")]
const ASSET_NAME: &str = "yt-dlp_macos";
#[cfg(target_os = "windows")]
const ASSET_NAME: &str = "yt-dlp.exe";

const RELEASE_URL: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download";

static BIN: OnceLock<PathBuf> = OnceLock::new();

pub fn path() -> &'static Path {
    BIN.get()
        .expect("ytdlp::ensure() must be called before ytdlp::path()")
        .as_path()
}

fn cache_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let dir = PathBuf::from(home).join(".cache").join("listen_to_it");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub async fn ensure() -> Result<()> {
    if BIN.get().is_some() {
        return Ok(());
    }

    if probe_path().await {
        let _ = BIN.set(PathBuf::from("yt-dlp"));
        return Ok(());
    }

    let cached = cache_dir()?.join(if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" });
    if !cached.exists() {
        eprintln!("Downloading yt-dlp ({ASSET_NAME})...");
        let url = format!("{RELEASE_URL}/{ASSET_NAME}");
        let bytes = reqwest::get(&url)
            .await
            .with_context(|| format!("download request to {url} failed"))?
            .error_for_status()?
            .bytes()
            .await?;
        std::fs::write(&cached, &bytes).context("failed to write yt-dlp binary")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&cached)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&cached, perms)?;
        }
    }

    let _ = BIN.set(cached);
    Ok(())
}

async fn probe_path() -> bool {
    matches!(
        tokio::process::Command::new("yt-dlp")
            .arg("--version")
            .output()
            .await,
        Ok(out) if out.status.success()
    )
}
