use anyhow::Result;
use rusty_ytdl::search::{SearchOptions, SearchResult, SearchType, YouTube};

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
}

impl VideoResult {
    pub fn watch_url(&self) -> String {
        self.url
            .clone()
            .unwrap_or_else(|| format!("https://www.youtube.com/watch?v={}", self.id))
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
    let yt = YouTube::new()?;

    let raw = yt
        .search(
            query,
            Some(&SearchOptions {
                limit: max_results as u64,
                search_type: SearchType::Video,
                safe_search: false,
            }),
        )
        .await?;

    let results = raw
        .into_iter()
        .filter_map(|r| {
            if let SearchResult::Video(v) = r {
                // duration is in milliseconds
                let duration_secs = if v.duration > 0 {
                    Some(v.duration as f64 / 1000.0)
                } else {
                    None
                };

                let thumbnail = v.thumbnails.into_iter().next().map(|t| t.url);

                Some(VideoResult {
                    id: v.id,
                    title: v.title,
                    url: Some(v.url),
                    duration: duration_secs,
                    view_count: Some(v.views),
                    channel: Some(v.channel.name),
                    uploader: None,
                    thumbnail,
                })
            } else {
                None
            }
        })
        .collect();

    Ok(results)
}
