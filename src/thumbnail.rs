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
    Ok(image::load_from_memory(&bytes)?)
}
