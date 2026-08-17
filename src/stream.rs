//! Turning a YouTube watch URL into something mpv can play *now*.
//!
//! Previously mpv did this itself: it was handed the watch URL and its
//! `ytdl_hook` shelled out to yt-dlp before a single byte of audio moved.
//! That extraction is the whole cost of starting a track — measured at
//! ~2.5 s against YouTube (webpage + two player-client API calls + an m3u8
//! manifest), versus ~0.8 s for mpv opening an already-resolved media URL.
//!
//! Doing the extraction here instead buys three things that hook can't:
//!
//!   - the result is cached, so a replay, a retry after a failed attempt, or
//!     stepping back through history doesn't pay for it again;
//!   - it can run *ahead of time* (see [`prefetch`]) while the previous track
//!     is still playing or while the user is reading a search row, which is
//!     what makes a track change feel instant rather than "loading…";
//!   - chapters come back from the same extraction that produced the URL,
//!     instead of a second, redundant yt-dlp run per track.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::app::Chapter;

/// Audio-only, best available. Video formats are never wanted (the player
/// runs mpv with `--no-video`) and skipping them keeps the URL small and the
/// bandwidth honest.
const FORMAT: &str = "bestaudio/best";

/// Signed YouTube URLs carry their own `expire=`, typically ~6 h out. Treat
/// one as spent a little early rather than handing mpv a URL that dies partway
/// through a long track.
const EXPIRY_MARGIN: Duration = Duration::from_secs(15 * 60);

/// Lifetime for a resolved URL that carries no `expire=` of its own — live
/// HLS manifests, mostly. Short, because there's nothing to trust here.
const DEFAULT_TTL: Duration = Duration::from_secs(30 * 60);

/// A track resolved down to a media URL mpv can open directly, plus what came
/// along with it in the same extraction.
pub struct Stream {
    pub media_url: String,
    /// Request headers yt-dlp used to obtain the URL. Some are load-bearing:
    /// the signed URL is issued to a particular player client and can 403 when
    /// fetched with a mismatched `User-Agent`.
    pub headers: Vec<(String, String)>,
    pub chapters: Vec<Chapter>,
    expires_at: SystemTime,
}

impl Stream {
    fn is_fresh(&self) -> bool {
        SystemTime::now() < self.expires_at
    }
}

fn cache() -> &'static Mutex<HashMap<String, Arc<Stream>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<Stream>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Per-URL lock, so a prefetch already in flight and a play request for the
/// same track don't each spawn their own yt-dlp: the second one waits and then
/// finds the first one's result in the cache.
fn url_lock(watch_url: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = locks.lock().expect("stream lock map poisoned");
    guard
        .entry(watch_url.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// An already-resolved, still-valid stream for `watch_url`, if there is one.
pub fn cached(watch_url: &str) -> Option<Arc<Stream>> {
    let guard = cache().lock().ok()?;
    guard.get(watch_url).filter(|s| s.is_fresh()).cloned()
}

/// Drop a cached stream — called when playing it failed, so the retry goes
/// back to yt-dlp for a fresh signed URL instead of replaying the dead one.
pub fn invalidate(watch_url: &str) {
    if let Ok(mut guard) = cache().lock() {
        guard.remove(watch_url);
    }
}

/// Resolve `watch_url`, using (and filling) the cache.
pub async fn resolve(watch_url: &str) -> Result<Arc<Stream>> {
    if let Some(hit) = cached(watch_url) {
        return Ok(hit);
    }

    let lock = url_lock(watch_url);
    let _guard = lock.lock().await;
    // Whoever held the lock may have been resolving this very URL.
    if let Some(hit) = cached(watch_url) {
        return Ok(hit);
    }

    let started = std::time::Instant::now();
    let output = tokio::process::Command::new(crate::ytdlp::path())
        .args([
            "-j",
            "-f",
            FORMAT,
            "--no-playlist",
            "--no-warnings",
            watch_url,
        ])
        .output()
        .await
        .context("failed to run yt-dlp")?;

    if !output.status.success() {
        anyhow::bail!("{}", first_error_line(&output.stderr));
    }

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("failed to parse yt-dlp output")?;
    let media_url = json["url"]
        .as_str()
        .context("yt-dlp returned no playable stream url")?
        .to_string();

    let headers = json["http_headers"]
        .as_object()
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();

    let chapters = json["chapters"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    Some(Chapter {
                        start_time: c["start_time"].as_f64()?,
                        title: c["title"].as_str().unwrap_or("").to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let stream = Arc::new(Stream {
        expires_at: expiry_of(&media_url),
        media_url,
        headers,
        chapters,
    });

    crate::logline!(
        "stream: resolved {watch_url} in {:.2}s",
        started.elapsed().as_secs_f64()
    );

    if let Ok(mut guard) = cache().lock() {
        guard.retain(|_, s| s.is_fresh());
        guard.insert(watch_url.to_string(), stream.clone());
    }
    Ok(stream)
}

/// Resolve `watch_url` in the background purely to warm the cache. Failures
/// are logged and dropped: nothing is waiting on this, and if the track is
/// actually played later it will be resolved again then, with its error
/// surfaced properly.
pub fn prefetch(watch_url: String) {
    if cached(&watch_url).is_some() {
        return;
    }
    tokio::spawn(async move {
        if let Err(e) = resolve(&watch_url).await {
            crate::logline!("stream: prefetch of {watch_url} failed: {e}");
        }
    });
}

/// The `expire=` a signed googlevideo URL carries, minus a safety margin.
fn expiry_of(media_url: &str) -> SystemTime {
    let parsed = media_url
        .split(['?', '&'])
        .find_map(|p| p.strip_prefix("expire="))
        .and_then(|v| v.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|secs| secs.parse::<u64>().ok())
        .map(|secs| UNIX_EPOCH + Duration::from_secs(secs))
        .and_then(|at| at.checked_sub(EXPIRY_MARGIN));

    parsed.unwrap_or_else(|| SystemTime::now() + DEFAULT_TTL)
}

/// yt-dlp's stderr, reduced to the one line that names the problem — the rest
/// is traceback noise the status bar has no room for.
fn first_error_line(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    text.lines()
        .map(str::trim)
        .find(|l| l.contains("ERROR"))
        .map(|l| l.trim_start_matches("ERROR:").trim().to_string())
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| "yt-dlp could not resolve this track".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_comes_from_the_signed_url() {
        let at = expiry_of("https://x.googlevideo.com/videoplayback?expire=2000000000&ei=abc");
        let expected = UNIX_EPOCH + Duration::from_secs(2_000_000_000) - EXPIRY_MARGIN;
        assert_eq!(at, expected);
    }

    #[test]
    fn url_without_expire_falls_back_to_a_short_ttl() {
        let at = expiry_of("https://x.googlevideo.com/manifest.m3u8");
        assert!(at > SystemTime::now() + DEFAULT_TTL - Duration::from_secs(5));
        assert!(at <= SystemTime::now() + DEFAULT_TTL);
    }

    #[test]
    fn error_line_is_the_one_naming_the_problem() {
        let stderr = b"WARNING: something\nERROR: [youtube] abc: Video unavailable\nTraceback...";
        assert_eq!(first_error_line(stderr), "[youtube] abc: Video unavailable");
        assert_eq!(first_error_line(b""), "yt-dlp could not resolve this track");
    }

    /// Hits YouTube for real, so it isn't part of the default run:
    /// `cargo test -- --ignored --nocapture` when you want to check that
    /// extraction still works against whatever YouTube is serving today.
    #[tokio::test]
    #[ignore = "requires network and a working yt-dlp"]
    async fn resolves_and_caches_a_real_video() {
        crate::ytdlp::ensure().await.unwrap();
        let url = "https://www.youtube.com/watch?v=NolF1yCK33c";
        let first = resolve(url).await.unwrap();
        assert!(first.media_url.starts_with("https://"));
        assert!(first.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("user-agent")));
        let started = std::time::Instant::now();
        let second = resolve(url).await.unwrap();
        assert!(Arc::ptr_eq(&first, &second), "second resolve should hit the cache");
        assert!(started.elapsed() < Duration::from_millis(50));
    }
}
