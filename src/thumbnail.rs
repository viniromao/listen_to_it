use crate::app::AppMessage;
use anyhow::Result;
use image::DynamicImage;
use tokio::sync::mpsc::UnboundedSender;

pub async fn load(video_id: String, url: String, tx: UnboundedSender<AppMessage>) {
    match fetch(&url).await {
        Ok(img) => {
            let _ = tx.send(AppMessage::ThumbnailLoaded { video_id, image: img });
        }
        Err(_) => {
            let _ = tx.send(AppMessage::ThumbnailFailed(video_id));
        }
    }
}

async fn fetch(url: &str) -> Result<DynamicImage> {
    let bytes = reqwest::get(url).await?.bytes().await?;
    // Decoding a 480×360 JPEG is CPU work, not IO. Left inline it occupies a
    // runtime worker for the whole decode, which is exactly the thread that
    // should be draining a yt-dlp pipe or driving the UI.
    Ok(tokio::task::spawn_blocking(move || image::load_from_memory(&bytes)).await??)
}
