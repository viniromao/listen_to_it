use anyhow::{Context, Result};
use serde_json::Value;

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

pub async fn fetch_playlist(url: &str) -> Result<Vec<VideoResult>> {
    let output = tokio::process::Command::new(crate::ytdlp::path())
        .args(["-J", "--flat-playlist", "--no-warnings", url])
        .output()
        .await
        .context("failed to run yt-dlp")?;

    if !output.status.success() {
        anyhow::bail!(
            "yt-dlp playlist fetch failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let json: Value =
        serde_json::from_slice(&output.stdout).context("failed to parse yt-dlp output")?;

    Ok(json["entries"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| {
                    // Skip nested playlists inside playlists
                    if e["ie_key"].as_str() == Some("YoutubePlaylist") {
                        return None;
                    }
                    let id = e["id"].as_str()?.to_string();
                    let title = e["title"].as_str().unwrap_or("Unknown").to_string();
                    let url = e["url"]
                        .as_str()
                        .or_else(|| e["webpage_url"].as_str())
                        .map(|s| s.to_string());
                    let channel = e["channel"]
                        .as_str()
                        .or_else(|| e["uploader"].as_str())
                        .map(|s| s.to_string());
                    Some(VideoResult {
                        id,
                        title,
                        url,
                        duration: e["duration"].as_f64(),
                        view_count: e["view_count"].as_u64(),
                        channel,
                        uploader: None,
                        thumbnail: e["thumbnail"].as_str().map(|s| s.to_string()),
                        is_playlist: false,
                        playlist_count: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default())
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
