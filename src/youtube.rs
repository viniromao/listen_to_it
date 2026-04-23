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
        self.thumbnail
            .clone()
            .unwrap_or_else(|| format!("https://i.ytimg.com/vi/{}/mqdefault.jpg", self.id))
    }

    pub fn channel_name(&self) -> &str {
        self.channel
            .as_deref()
            .or(self.uploader.as_deref())
            .unwrap_or("Unknown")
    }
}

pub async fn search(query: &str, max_results: usize) -> Result<Vec<VideoResult>> {
    let search_term = format!("ytsearch{}:{}", max_results, query);
    let output = tokio::process::Command::new("yt-dlp")
        .args(["-J", "--flat-playlist", "--no-warnings", &search_term])
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

pub async fn fetch_playlist(url: &str) -> Result<Vec<VideoResult>> {
    let output = tokio::process::Command::new("yt-dlp")
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
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

fn parse_entry(e: &Value) -> Option<VideoResult> {
    let ie_key = e["ie_key"].as_str().unwrap_or("");
    // Accept videos and playlists; skip channels and other types
    if !ie_key.is_empty() && ie_key != "Youtube" && ie_key != "YoutubePlaylist" {
        return None;
    }
    let is_playlist = ie_key == "YoutubePlaylist";

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
    let duration = e["duration"].as_f64();
    let view_count = e["view_count"].as_u64();
    let thumbnail = e["thumbnail"].as_str().map(|s| s.to_string());

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
    })
}
