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

/// yt-dlp's **nightly** channel, not stable.
///
/// This is deliberate and load-bearing. YouTube continuously rotates which
/// player clients hand out playable media URLs; when yt-dlp falls behind, it
/// still resolves a URL but that URL gets an intermittent HTTP 403 when mpv
/// fetches it — playback fails "randomly" with no useful error. Measured on
/// 2026-08-15: stable (2026.07.04, six weeks old) played 3 of 8 attempts,
/// while nightly (2026.08.04) played 8 of 8 on the same videos, because the
/// newer build had already moved to a client YouTube still honours.
/// Stable releases lag these changes by weeks, which is far longer than a
/// music player can be broken for.
const RELEASE_URL: &str =
    "https://github.com/yt-dlp/yt-dlp-nightly-builds/releases/latest/download";

/// A system-wide `yt-dlp` this old is assumed to predate YouTube's current
/// client requirements, so the app prefers its own managed nightly instead.
/// Kept generous: the goal is to skip clearly-stale installs, not to fight
/// users who keep yt-dlp current themselves.
const MAX_SYSTEM_YTDLP_AGE_DAYS: i64 = 14;

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

/// How long a downloaded (non-PATH) yt-dlp binary is trusted before we check
/// for a newer release. YouTube changes its anti-bot defenses often enough
/// that a stale yt-dlp is the single most common cause of "every video fails
/// to load" — worth re-checking periodically rather than only once ever.
const UPDATE_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

pub async fn ensure() -> Result<()> {
    if BIN.get().is_some() {
        return Ok(());
    }

    // A system yt-dlp is only trusted if it's recent enough to still resolve
    // playable YouTube URLs — see MAX_SYSTEM_YTDLP_AGE_DAYS. An outdated one
    // is worse than no yt-dlp at all: it resolves URLs that then 403 on
    // fetch, which surfaces as random playback failures rather than a clear
    // "please update" error.
    let system_version = probe_path().await;
    match system_version {
        Some(ref version) if !version_is_stale(version) => {
            crate::logline!("ytdlp: using system yt-dlp {version}");
            let _ = BIN.set(PathBuf::from("yt-dlp"));
            return Ok(());
        }
        Some(ref version) => {
            crate::logline!(
                "ytdlp: system yt-dlp {version} is older than {MAX_SYSTEM_YTDLP_AGE_DAYS} days, \
                 preferring managed nightly build"
            );
        }
        None => {}
    }

    let dir = cache_dir()?;
    let cached = dir.join(if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" });
    let stamp = dir.join("yt-dlp.checked_at");

    let is_missing = !cached.exists();
    let is_stale = !is_missing && checked_at_is_stale(&stamp);

    if is_missing || is_stale {
        crate::logline!(
            "ytdlp: {} at {}, checking for latest release",
            if is_missing { "nothing cached" } else { "cached copy is stale" },
            cached.display()
        );
        match download_latest(&cached).await {
            Ok(()) => {
                let _ = std::fs::write(&stamp, b"");
                crate::logline!("ytdlp: updated to latest release at {}", cached.display());
            }
            // Nothing cached to fall back on. A stale system yt-dlp is still
            // better than refusing to start, so use it if there is one and
            // let playback errors speak for themselves.
            Err(e) if is_missing => {
                return match system_version {
                    Some(version) => {
                        crate::logline!(
                            "ytdlp: download failed ({e}), falling back to stale system yt-dlp {version}"
                        );
                        let _ = BIN.set(PathBuf::from("yt-dlp"));
                        Ok(())
                    }
                    None => Err(e),
                };
            }
            Err(e) => {
                // Couldn't check/update, but we still have a working (if
                // possibly stale) binary — keep going rather than blocking
                // startup on a network hiccup.
                crate::logline!("ytdlp: update check failed ({e}), keeping existing cached binary");
            }
        }
    }

    let _ = BIN.set(cached);
    Ok(())
}

fn checked_at_is_stale(stamp: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(stamp) else {
        return true; // no record of ever having checked — treat as stale
    };
    let Ok(modified) = meta.modified() else { return true };
    modified.elapsed().map(|age| age > UPDATE_CHECK_INTERVAL).unwrap_or(true)
}

async fn download_latest(dest: &Path) -> Result<()> {
    eprintln!("Downloading yt-dlp ({ASSET_NAME})...");
    let url = format!("{RELEASE_URL}/{ASSET_NAME}");
    let bytes = reqwest::get(&url)
        .await
        .with_context(|| format!("download request to {url} failed"))?
        .error_for_status()?
        .bytes()
        .await?;
    std::fs::write(dest, &bytes).context("failed to write yt-dlp binary")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(dest)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(dest, perms)?;
    }
    Ok(())
}

/// Version string of a `yt-dlp` on PATH, if there is a working one.
async fn probe_path() -> Option<String> {
    let out = tokio::process::Command::new("yt-dlp")
        .arg("--version")
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!version.is_empty()).then_some(version)
}

/// yt-dlp versions are date-stamped (`2026.08.04`, nightly adds a build
/// suffix like `2026.08.04.234419`), so age is readable straight off the
/// version string without asking the network. An unparseable version is
/// treated as stale: better to fall back to a build we know the age of.
fn version_is_stale(version: &str) -> bool {
    let mut parts = version.split('.');
    let (Some(y), Some(m), Some(d)) = (parts.next(), parts.next(), parts.next()) else {
        return true;
    };
    let (Ok(y), Ok(m), Ok(d)) = (y.parse::<i64>(), m.parse::<u32>(), d.parse::<u32>()) else {
        return true;
    };
    let Some(released) = days_from_civil(y, m, d) else {
        return true;
    };
    let now_days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64 / 86400)
        .unwrap_or(0);
    now_days - released > MAX_SYSTEM_YTDLP_AGE_DAYS
}

/// Days since the Unix epoch for a calendar date (Howard Hinnant's
/// `days_from_civil`), so version dates can be compared without pulling in a
/// date library.
fn days_from_civil(y: i64, m: u32, d: u32) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe as i64 - 719468)
}

/// How many *speculative* yt-dlp processes may run at once.
///
/// yt-dlp here is a PyInstaller bundle: ~0.4 s of pure startup before it makes
/// a single request (measured), and tens of megabytes resident while it runs.
/// Background work asks for a lot of them — a search with ten playlist rows in
/// it used to spawn ten at the instant results landed, all racing the track the
/// user actually pressed Enter on. Two is enough to keep prefetching useful
/// without letting it own the machine.
const BACKGROUND_YTDLP_LIMIT: usize = 2;

/// A permit for speculative yt-dlp work (hover prefetch, playlist row
/// metadata). Foreground resolution — the track being played right now —
/// deliberately does *not* take one, so it can never queue behind guesses.
pub async fn background_permit() -> tokio::sync::SemaphorePermit<'static> {
    static SEM: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    SEM.get_or_init(|| tokio::sync::Semaphore::new(BACKGROUND_YTDLP_LIMIT))
        .acquire()
        .await
        .expect("background yt-dlp semaphore is never closed")
}
