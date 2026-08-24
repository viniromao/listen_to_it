use crate::app::AppMessage;
use anyhow::{Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone)]
pub struct VideoResult {
    pub id: String,
    pub title: String,
    pub url: Option<String>,
    pub duration: Option<f64>,
    pub view_count: Option<u64>,
    pub channel: Option<String>,
    pub uploader: Option<String>,
    pub thumbnail: Option<String>,
    pub is_playlist: bool,
    /// Number of videos in the playlist, filled in lazily for playlist results.
    pub playlist_count: Option<u64>,
}

/// Playlist-level metadata fetched lazily to flesh out a playlist search row.
pub struct PlaylistMeta {
    pub channel: Option<String>,
    pub count: Option<u64>,
    pub view_count: Option<u64>,
}

impl VideoResult {
    pub fn watch_url(&self) -> String {
        self.url.clone().unwrap_or_else(|| {
            if self.is_playlist {
                format!("https://www.youtube.com/playlist?list={}", self.id)
            } else {
                format!("https://www.youtube.com/watch?v={}", self.id)
            }
        })
    }

    pub fn thumbnail_url(&self) -> String {
        // Upgrade any ytimg.com URL to hqdefault (480×360): large enough for Fit to fill
        // the cell area height, always exists, and loads fast (~50KB vs ~200KB maxresdefault).
        self.thumbnail
            .as_deref()
            .and_then(|url| {
                if url.contains("i.ytimg.com/vi/") {
                    url.rfind('/').map(|pos| format!("{}/hqdefault.jpg", &url[..pos]))
                } else {
                    Some(url.to_string())
                }
            })
            .unwrap_or_else(|| format!("https://i.ytimg.com/vi/{}/hqdefault.jpg", self.id))
    }

    pub fn channel_name(&self) -> &str {
        self.channel
            .as_deref()
            .or(self.uploader.as_deref())
            .unwrap_or("Unknown")
    }
}

pub async fn search(query: &str, max_results: usize) -> Result<Vec<VideoResult>> {
    // Hit YouTube's regular results page rather than the `ytsearch:` prefix.
    // `ytsearch:` only ever returns videos; the results page returns videos
    // and playlists interleaved in YouTube's own ranking order, so playlists
    // show up exactly where YouTube places them instead of being pinned.
    let url = format!(
        "https://www.youtube.com/results?search_query={}",
        percent_encode(query)
    );
    let output = tokio::process::Command::new(crate::ytdlp::path())
        .args([
            "-J",
            "--flat-playlist",
            "--no-warnings",
            "--playlist-end",
            &max_results.to_string(),
            &url,
        ])
        .output()
        .await
        .context("failed to run yt-dlp")?;

    if !output.status.success() {
        anyhow::bail!(
            "yt-dlp search failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let json: Value =
        serde_json::from_slice(&output.stdout).context("failed to parse yt-dlp output")?;

    Ok(json["entries"]
        .as_array()
        .map(|entries| entries.iter().filter_map(parse_entry).collect())
        .unwrap_or_default())
}

/// Minimal percent-encoding for a YouTube search query string.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// How many entries to emit before the rest of the playlist has arrived.
///
/// One would be enough to start playback, but the queue would then be empty at
/// `start_track` time and the *second* track would miss its prefetch. Three
/// costs nothing extra — they come off the same first page yt-dlp yields.
const PLAYLIST_HEAD: usize = 3;

/// Entries per follow-up batch. Batching keeps a 5000-track playlist from
/// sending 5000 individual messages through the UI channel.
const PLAYLIST_BATCH: usize = 100;

/// Fetch a playlist, emitting entries to the UI as yt-dlp produces them.
///
/// The obvious spelling — `-J`, one JSON document — makes yt-dlp walk the
/// *entire* playlist before it prints a single byte. Measured on a 960-track
/// playlist: 6.6 s of silence before the first track could start, and that
/// number grows with the playlist. `-j --lazy-playlist` prints one JSON object
/// per entry as each page comes back instead, so the first entries land in
/// ~1.5 s and playback starts there while the tail keeps streaming in behind
/// it.
///
/// `token` is echoed back in every message so the app can drop entries from a
/// playlist the user has already navigated away from — the tail of a long
/// playlist can still be arriving well after they queued something else.
pub async fn fetch_playlist_streamed(
    url: &str,
    token: u64,
    play_immediately: bool,
    tx: UnboundedSender<AppMessage>,
) -> Result<()> {
    let mut child = tokio::process::Command::new(crate::ytdlp::path())
        .args(["-j", "--flat-playlist", "--lazy-playlist", "--no-warnings", url])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to run yt-dlp")?;

    let stdout = child.stdout.take().context("yt-dlp produced no stdout")?;
    let stderr = child.stderr.take().context("yt-dlp produced no stderr")?;
    // Drained concurrently: yt-dlp writes warnings there, and a full pipe
    // buffer would stall the entry stream we actually care about.
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            buf.push_str(&line);
            buf.push('\n');
        }
        buf
    });

    let mut lines = BufReader::new(stdout).lines();
    let mut pending: Vec<VideoResult> = Vec::new();
    let mut head_sent = false;
    let mut total = 0usize;

    while let Some(line) = lines.next_line().await? {
        let Ok(entry) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(video) = parse_playlist_entry(&entry) else { continue };
        pending.push(video);
        total += 1;

        let ready = if head_sent { PLAYLIST_BATCH } else { PLAYLIST_HEAD };
        if pending.len() >= ready {
            let videos = std::mem::take(&mut pending);
            let msg = if head_sent {
                AppMessage::PlaylistTail { token, videos }
            } else {
                head_sent = true;
                AppMessage::PlaylistHead { token, videos, play_immediately }
            };
            // The receiver is gone (app shutting down) — stop rather than
            // leaving yt-dlp walking a 5000-track playlist for nobody.
            if tx.send(msg).is_err() {
                let _ = child.start_kill();
                return Ok(());
            }
        }
    }

    let status = child.wait().await?;
    let stderr = stderr_task.await.unwrap_or_default();

    if !head_sent && !status.success() {
        anyhow::bail!("yt-dlp playlist fetch failed: {}", stderr.trim());
    }

    // Flush the remainder — and, for a playlist shorter than PLAYLIST_HEAD or
    // an empty one, send the head message that never reached its threshold so
    // the app isn't left waiting on a load that already finished.
    let videos = std::mem::take(&mut pending);
    let msg = if head_sent {
        AppMessage::PlaylistTail { token, videos }
    } else {
        AppMessage::PlaylistHead { token, videos, play_immediately }
    };
    let _ = tx.send(msg);
    crate::logline!("youtube: playlist {url} streamed {total} entries");
    Ok(())
}

/// One flat-playlist entry as a track, or `None` for things that aren't one
/// (nested playlists show up inside playlists and can't be queued).
fn parse_playlist_entry(e: &Value) -> Option<VideoResult> {
    if e["ie_key"].as_str() == Some("YoutubePlaylist") {
        return None;
    }
    Some(VideoResult {
        id: e["id"].as_str()?.to_string(),
        title: e["title"].as_str().unwrap_or("Unknown").to_string(),
        url: e["url"]
            .as_str()
            .or_else(|| e["webpage_url"].as_str())
            .map(|s| s.to_string()),
        duration: e["duration"].as_f64(),
        view_count: e["view_count"].as_u64(),
        channel: e["channel"]
            .as_str()
            .or_else(|| e["uploader"].as_str())
            .map(|s| s.to_string()),
        uploader: None,
        thumbnail: e["thumbnail"].as_str().map(|s| s.to_string()),
        is_playlist: false,
        playlist_count: None,
    })
}

fn parse_entry(e: &Value) -> Option<VideoResult> {
    let ie_key = e["ie_key"].as_str().unwrap_or("");
    let url = e["url"]
        .as_str()
        .or_else(|| e["webpage_url"].as_str())
        .map(|s| s.to_string());

    // The results page mixes videos, playlists, channels and "mixes". Videos
    // come as "Youtube"; everything else comes as "YoutubeTab"/"YoutubePlaylist".
    // A tab entry is a playlist only when its URL carries a `list=` id —
    // channels share the same ie_key but link to /channel/ or /@handle, so
    // they're dropped.
    let is_playlist = match ie_key {
        "Youtube" | "" => false,
        "YoutubePlaylist" => true,
        "YoutubeTab" => url.as_deref().is_some_and(|u| u.contains("list=")),
        _ => return None,
    };
    if ie_key == "YoutubeTab" && !is_playlist {
        return None; // a channel, not a playlist
    }

    let id = e["id"].as_str()?.to_string();
    let title = e["title"].as_str().unwrap_or("Unknown").to_string();
    let channel = e["channel"]
        .as_str()
        .or_else(|| e["uploader"].as_str())
        .map(|s| s.to_string());
    let duration = e["duration"].as_f64();
    let view_count = e["view_count"].as_u64();
    let thumbnail = e["thumbnail"]
        .as_str()
        .or_else(|| e["thumbnails"][0]["url"].as_str())
        .map(|s| s.to_string());

    Some(VideoResult {
        id,
        title,
        url,
        duration,
        view_count,
        channel,
        uploader: None,
        thumbnail,
        is_playlist,
        playlist_count: None,
    })
}

/// Fetch playlist-level metadata (owner, track count, total views) with a cheap
/// single-item flat request, used to flesh out a playlist row after it appears.
pub async fn fetch_playlist_meta(url: &str) -> Result<PlaylistMeta> {
    // Purely decorative — it fills in "N tracks / owner / views" on a row.
    // Takes a background permit so a screenful of playlist rows can't fire a
    // process per row and crowd out the track the user is waiting on.
    let _permit = crate::ytdlp::background_permit().await;
    let output = tokio::process::Command::new(crate::ytdlp::path())
        .args([
            "-J",
            "--flat-playlist",
            "--no-warnings",
            "--playlist-items",
            "1",
            url,
        ])
        .output()
        .await
        .context("failed to run yt-dlp")?;

    if !output.status.success() {
        anyhow::bail!(
            "yt-dlp playlist meta failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let json: Value =
        serde_json::from_slice(&output.stdout).context("failed to parse yt-dlp output")?;

    Ok(PlaylistMeta {
        channel: json["channel"]
            .as_str()
            .or_else(|| json["uploader"].as_str())
            .map(|s| s.to_string()),
        count: json["playlist_count"].as_u64(),
        view_count: json["view_count"].as_u64(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of `fetch_playlist_streamed`: the first tracks must be
    /// usable long before the playlist has finished being walked. Hits YouTube,
    /// so it is opt-in: `cargo test -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "requires network and a working yt-dlp"]
    async fn head_of_a_long_playlist_arrives_before_the_tail() {
        crate::ytdlp::ensure().await.unwrap();
        let url = "https://www.youtube.com/playlist?list=PLOzDu-MXXLliO9fBNZOQTBDddoA3FzZUo";
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let started = std::time::Instant::now();
        let task = tokio::spawn({
            let url = url.to_string();
            async move { fetch_playlist_streamed(&url, 7, true, tx).await }
        });

        let mut head_at = None;
        let mut total = 0usize;
        while let Some(msg) = rx.recv().await {
            match msg {
                AppMessage::PlaylistHead { token, videos, play_immediately } => {
                    assert_eq!(token, 7, "token must be echoed back");
                    assert!(play_immediately);
                    assert!(!videos.is_empty(), "head must carry playable tracks");
                    assert!(videos.iter().all(|v| !v.is_playlist));
                    head_at = Some(started.elapsed());
                    total += videos.len();
                }
                AppMessage::PlaylistTail { token, videos } => {
                    assert_eq!(token, 7);
                    total += videos.len();
                }
                _ => panic!("unexpected message variant"),
            }
        }
        task.await.unwrap().unwrap();

        let head_at = head_at.expect("no head was ever emitted");
        let all_at = started.elapsed();
        println!("head after {head_at:.2?}, all {total} entries after {all_at:.2?}");
        assert!(total > 100, "expected a long playlist, got {total}");
        assert!(
            head_at < all_at / 2,
            "head ({head_at:.2?}) should land well before the whole playlist ({all_at:.2?})"
        );
    }
}
