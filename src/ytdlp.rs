use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

// The release asset to fetch, and the executable inside it.
//
// These are yt-dlp's **onedir** builds (the `.zip` assets), not the
// single-file binaries next to them, and the difference is not cosmetic.
//
// A onefile PyInstaller build unpacks its entire ~35 MB payload — 161 files,
// including every shared library — into a fresh temp directory on *every*
// invocation, then deletes it. On Linux that costs about 0.23 s a call
// (measured: 0.436 s onefile vs 0.203 s onedir for `--version`). On macOS it
// is catastrophic: because each launch writes brand-new executable files, the
// kernel's code-signature validation can never reuse its per-file cache and
// re-validates the whole payload every time. Measured on an Apple Silicon Mac:
// **15.4 s for a single `yt-dlp --version`**, paid before yt-dlp had even
// spoken to YouTube — on every track resolved and every playlist opened.
//
// The onedir build writes those files once, at install time, so the OS caches
// what it needs and startup stays flat.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const ASSET: Asset = Asset { zip: "yt-dlp_linux.zip", exe: "yt-dlp_linux" };
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const ASSET: Asset = Asset { zip: "yt-dlp_linux_aarch64.zip", exe: "yt-dlp_linux_aarch64" };
#[cfg(target_os = "macos")]
const ASSET: Asset = Asset { zip: "yt-dlp_macos.zip", exe: "yt-dlp_macos" };
#[cfg(target_os = "windows")]
const ASSET: Asset = Asset { zip: "yt-dlp_win.zip", exe: "yt-dlp.exe" };

struct Asset {
    /// Release asset holding the onedir build.
    zip: &'static str,
    /// Executable inside it, alongside the `_internal/` directory it needs.
    exe: &'static str,
}

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
    // The onedir bundle lives in its own directory: the executable is useless
    // without the `_internal/` tree unpacked beside it.
    let dist = dir.join("dist");
    let cached = dist.join(ASSET.exe);
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

    // A previous version installed the single-file build here. It is ~38 MB of
    // nothing now, and on macOS it is the slow path we just moved off.
    let _ = std::fs::remove_file(dir.join(if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" }));

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

/// Download the current onedir build and unpack it so that `dest` (the
/// executable inside it) is runnable.
async fn download_latest(dest: &Path) -> Result<()> {
    eprintln!("Downloading yt-dlp ({})...", ASSET.zip);
    let url = format!("{RELEASE_URL}/{}", ASSET.zip);
    let bytes = reqwest::get(&url)
        .await
        .with_context(|| format!("download request to {url} failed"))?
        .error_for_status()?
        .bytes()
        .await?;

    let dist = dest.parent().context("yt-dlp destination has no parent")?;
    // Unpack beside the target and swap it in at the end, so an interrupted
    // download can't leave a half-extracted bundle that looks installed.
    let staging = dist.with_extension("incoming");
    let _ = std::fs::remove_dir_all(&staging);
    // Unpacking ~40 MB across 160-odd files is blocking filesystem work, and
    // this runs while the UI is already up on an update check.
    let staging = tokio::task::spawn_blocking(move || -> Result<PathBuf> {
        unpack(&bytes, &staging)?;
        Ok(staging)
    })
    .await??;

    let _ = std::fs::remove_dir_all(dist);
    std::fs::rename(&staging, dist).context("failed to install unpacked yt-dlp")?;

    anyhow::ensure!(
        dest.exists(),
        "{} was not present in {}",
        ASSET.exe,
        ASSET.zip
    );
    Ok(())
}

/// Extract a onedir zip into `into`, preserving the executable bit.
///
/// Hand-rolled rather than taken from a crate. `flate2` is already in the tree
/// — `image` decodes PNG through it — so reading an archive this plain (no
/// encryption, no zip64, one disk) costs no new dependencies at all, and a
/// machine that cannot reach crates.io can still build the project.
fn unpack(bytes: &[u8], into: &Path) -> Result<()> {
    use std::io::Read;

    for entry in central_directory(bytes)? {
        let Some(rel) = safe_path(&entry.name) else {
            anyhow::bail!("zip entry would write outside the target: {}", entry.name);
        };
        let out = into.join(rel);
        if entry.name.ends_with('/') {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let raw = entry_data(bytes, &entry)?;
        let data = match entry.method {
            METHOD_STORE => raw.to_vec(),
            METHOD_DEFLATE => {
                let mut buf = Vec::with_capacity(entry.uncompressed_size);
                flate2::read::DeflateDecoder::new(raw)
                    .read_to_end(&mut buf)
                    .with_context(|| format!("failed to inflate {}", entry.name))?;
                buf
            }
            other => anyhow::bail!("unsupported compression method {other} for {}", entry.name),
        };

        // This is an executable about to be run, fetched over the network:
        // worth confirming it arrived intact rather than discovering it as a
        // mystery crash later.
        anyhow::ensure!(
            data.len() == entry.uncompressed_size,
            "size mismatch for {}",
            entry.name
        );
        let mut crc = flate2::Crc::new();
        crc.update(&data);
        anyhow::ensure!(crc.sum() == entry.crc32, "checksum mismatch for {}", entry.name);

        std::fs::write(&out, &data)
            .with_context(|| format!("failed to write {}", out.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // The bundle ships its own modes; the launcher and the shared
            // libraries beside it are useless without the executable bit.
            if let Some(mode) = entry.unix_mode {
                std::fs::set_permissions(&out, std::fs::Permissions::from_mode(mode))?;
            }
        }
    }
    Ok(())
}

const METHOD_STORE: u16 = 0;
const METHOD_DEFLATE: u16 = 8;

/// One file in the archive, as described by its central-directory record.
struct ZipEntry {
    name: String,
    method: u16,
    crc32: u32,
    compressed_size: usize,
    uncompressed_size: usize,
    local_offset: usize,
    unix_mode: Option<u32>,
}

/// The archive's central directory — the authoritative index. Sizes in the
/// local headers can legally be left zero and deferred to a trailing data
/// descriptor, so they are read from here instead.
fn central_directory(bytes: &[u8]) -> Result<Vec<ZipEntry>> {
    const EOCD_SIG: &[u8] = b"PK\x05\x06";
    const CENTRAL_SIG: u32 = 0x0201_4b50;

    // The trailing comment may be up to 64 KiB, so the record can sit that far
    // from the end.
    let window = bytes.len().min(66 * 1024);
    let from = bytes.len() - window;
    let eocd = bytes[from..]
        .windows(4)
        .rposition(|w| w == EOCD_SIG)
        .map(|at| from + at)
        .context("not a zip archive: no end-of-central-directory record")?;

    let count = u16_at(bytes, eocd + 10)?;
    let offset = u32_at(bytes, eocd + 16)?;
    anyhow::ensure!(
        count != u16::MAX && offset != u32::MAX,
        "zip64 archives are not supported"
    );

    let mut entries = Vec::with_capacity(count as usize);
    let mut at = offset as usize;
    for _ in 0..count {
        anyhow::ensure!(
            u32_at(bytes, at)? == CENTRAL_SIG,
            "malformed zip central directory"
        );
        let name_len = u16_at(bytes, at + 28)? as usize;
        let extra_len = u16_at(bytes, at + 30)? as usize;
        let comment_len = u16_at(bytes, at + 32)? as usize;
        // The high half of the external attributes carries the unix mode, and
        // is zero for archives written on Windows.
        let external = u32_at(bytes, at + 38)? >> 16;
        let name = bytes
            .get(at + 46..at + 46 + name_len)
            .context("truncated zip central directory")?;

        entries.push(ZipEntry {
            name: String::from_utf8(name.to_vec()).context("zip entry name is not utf-8")?,
            method: u16_at(bytes, at + 10)?,
            crc32: u32_at(bytes, at + 16)?,
            compressed_size: u32_at(bytes, at + 20)? as usize,
            uncompressed_size: u32_at(bytes, at + 24)? as usize,
            local_offset: u32_at(bytes, at + 42)? as usize,
            unix_mode: (external != 0).then_some(external),
        });
        at += 46 + name_len + extra_len + comment_len;
    }
    Ok(entries)
}

/// The raw (still compressed) bytes of one entry.
fn entry_data<'a>(bytes: &'a [u8], entry: &ZipEntry) -> Result<&'a [u8]> {
    const LOCAL_SIG: u32 = 0x0403_4b50;

    let at = entry.local_offset;
    anyhow::ensure!(
        u32_at(bytes, at)? == LOCAL_SIG,
        "malformed local header for {}",
        entry.name
    );
    // The local header carries its own name and extra lengths, and the extra
    // field is routinely a different size from the central directory's.
    let name_len = u16_at(bytes, at + 26)? as usize;
    let extra_len = u16_at(bytes, at + 28)? as usize;
    let from = at + 30 + name_len + extra_len;
    bytes
        .get(from..from + entry.compressed_size)
        .with_context(|| format!("truncated zip data for {}", entry.name))
}

/// A relative path that cannot escape the directory being extracted into.
fn safe_path(name: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for part in name.split('/') {
        match part {
            "" | "." => continue,
            ".." => return None,
            // A backslash or drive letter would be a path separator on Windows.
            p if p.contains('\\') || p.contains(':') => return None,
            p => out.push(p),
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

fn u16_at(bytes: &[u8], at: usize) -> Result<u16> {
    let b = bytes.get(at..at + 2).context("truncated zip archive")?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32> {
    let b = bytes.get(at..at + 4).context("truncated zip archive")?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Extraction has to be exact: this bundle is an executable that gets run.
    /// Checked against the real release archive rather than a synthetic one,
    /// so entry modes, deflate/store mixes and directory records are all the
    /// genuine article. Needs the network, so it is opt-in.
    #[tokio::test]
    #[ignore = "requires network"]
    async fn unpacks_a_real_release_archive() {
        let url = format!("{RELEASE_URL}/{}", ASSET.zip);
        let bytes = reqwest::get(&url).await.unwrap().bytes().await.unwrap();

        let into = std::env::temp_dir().join("listen_to_it_unpack_test");
        let _ = std::fs::remove_dir_all(&into);
        unpack(&bytes, &into).unwrap();

        let exe = into.join(ASSET.exe);
        assert!(exe.is_file(), "{} missing from the bundle", ASSET.exe);
        assert!(into.join("_internal").is_dir(), "_internal/ missing");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&exe).unwrap().permissions().mode();
            assert!(mode & 0o111 != 0, "launcher is not executable: {mode:o}");
        }

        // The point of the whole exercise: it runs, and it runs fast.
        let started = std::time::Instant::now();
        let out = tokio::process::Command::new(&exe)
            .arg("--version")
            .output()
            .await
            .unwrap();
        assert!(out.status.success(), "unpacked yt-dlp did not run");
        let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
        println!("unpacked yt-dlp {version} started in {:.3}s", started.elapsed().as_secs_f64());
        assert!(!version.is_empty());

        let _ = std::fs::remove_dir_all(&into);
    }

    #[test]
    fn traversal_paths_are_refused() {
        assert!(safe_path("../../etc/passwd").is_none());
        assert!(safe_path("a/../../b").is_none());
        assert!(safe_path("C:/windows").is_none());
        assert_eq!(safe_path("_internal/lib.so").unwrap(), PathBuf::from("_internal/lib.so"));
        assert_eq!(safe_path("./a/./b").unwrap(), PathBuf::from("a/b"));
        assert!(safe_path("/").is_none());
    }
}
