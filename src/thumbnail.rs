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

pub async fn fetch(url: &str) -> Result<DynamicImage> {
    let bytes = reqwest::get(url).await?.bytes().await?;
    // Decoding a 480×360 JPEG is CPU work, not IO. Left inline it occupies a
    // runtime worker for the whole decode, which is exactly the thread that
    // should be draining a yt-dlp pipe or driving the UI.
    Ok(tokio::task::spawn_blocking(move || image::load_from_memory(&bytes)).await??)
}

/// YouTube's hqdefault is 4:3 with a 16:9 frame letterboxed inside it. In the
/// small now-playing slot those black bands eat a quarter of the height, so
/// cut them off and keep only the picture.
pub fn crop_letterbox(img: DynamicImage) -> DynamicImage {
    let (w, h) = (img.width(), img.height());
    if w * 3 != h * 4 {
        return img;
    }
    let inner = w * 9 / 16;
    img.crop_imm(0, (h - inner) / 2, w, inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letterboxed_thumbnails_are_cropped_to_16_by_9() {
        let img = crop_letterbox(DynamicImage::new_rgb8(480, 360));
        assert_eq!((img.width(), img.height()), (480, 270));
    }

    #[test]
    fn other_shapes_are_left_alone() {
        let img = crop_letterbox(DynamicImage::new_rgb8(1280, 720));
        assert_eq!((img.width(), img.height()), (1280, 720));
    }
}
