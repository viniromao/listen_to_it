use crate::player::Player;
use crate::youtube::VideoResult;
use anyhow::Result;
use crossterm::event::{
    Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use image::DynamicImage;
use ratatui_image::{picker::Picker, protocol::StatefulProtocol};
use souvlaki::{MediaControls, MediaMetadata, MediaPlayback};
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

pub enum AppMessage {
    SearchResults(Vec<VideoResult>),
    SearchError(String),
    ThumbnailLoaded { video_id: String, image: DynamicImage },
    ThumbnailFailed(String),
    /// Audio thread started downloading
    AudioLoading,
    /// Audio thread finished buffering and started playback
    AudioReady,
    /// Audio thread encountered an error
    AudioError(String),
    /// mpv process exited naturally (track finished)
    AudioFinished,
}

/// Simple Send-safe enum used to forward media key events from the
/// souvlaki callback (which may run on a background thread) into the
/// async main loop.
#[derive(Debug)]
pub enum MediaAction {
    Play,
    Pause,
    Toggle,
    Stop,
}

#[derive(PartialEq)]
pub enum AppMode {
    Normal,
    Searching,
    Confirming,
}

pub struct App {
    pub mode: AppMode,
    pub search_input: String,
    pub search_results: Vec<VideoResult>,
    pub selected_index: usize,
    pub is_searching: bool,
    pub status_message: Option<String>,

    pub player: Player,
    pub now_playing: Option<VideoResult>,
    pub queue: VecDeque<VideoResult>,
    pub history: Vec<VideoResult>,
    pub is_paused: bool,
    pub volume: i32,
    pub play_start: Option<Instant>,
    pub paused_elapsed: f64,

    pub thumbnail_protocols: HashMap<String, StatefulProtocol>,
    pub thumbnails_loading: HashSet<String>,
    pub thumbnails_failed: HashSet<String>,
    pub picker: Picker,

    pub msg_tx: UnboundedSender<AppMessage>,
    pub has_image_support: bool,

    // Stored on main thread only — not required to be Send.
    pub media_controls: Option<MediaControls>,

    pub show_visuals: bool,
    pub progress_bar_area: Option<Rect>,
    // Title of the track pending confirmation before playing.
    pub confirm_title: Option<String>,
}

impl App {
    pub fn new(msg_tx: UnboundedSender<AppMessage>, picker: Picker, has_image_support: bool) -> Self {
        // Player spawns its thread immediately and uses msg_tx to report state back.
        let player = Player::new(msg_tx.clone());

        Self {
            mode: AppMode::Normal,
            search_input: String::new(),
            search_results: Vec::new(),
            selected_index: 0,
            is_searching: false,
            status_message: None,

            player,
            now_playing: None,
            queue: VecDeque::new(),
            history: Vec::new(),
            is_paused: false,
            volume: 100,
            play_start: None,
            paused_elapsed: 0.0,

            thumbnail_protocols: HashMap::new(),
            thumbnails_loading: HashSet::new(),
            thumbnails_failed: HashSet::new(),
            picker,

            msg_tx,
            has_image_support,
            media_controls: None,

            show_visuals: true,
            progress_bar_area: None,
            confirm_title: None,
        }
    }

    /// Returns true when the app should quit.
    pub async fn handle_event(&mut self, event: Event) -> Result<bool> {
        match event {
            Event::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    return Ok(false);
                }
                match self.mode {
                    AppMode::Searching => self.handle_search_key(key.code).await?,
                    AppMode::Normal => {
                        if self.handle_normal_key(key.code, key.modifiers).await? {
                            return Ok(true);
                        }
                    }
                    AppMode::Confirming => self.handle_confirm_key(key.code).await?,
                }
            }
            Event::Mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column, row, .. }) => {
                self.handle_mouse_click(column, row).await?;
            }
            _ => {}
        }
        Ok(false)
    }

    async fn handle_search_key(&mut self, key: KeyCode) -> Result<()> {
        match key {
            KeyCode::Esc => {
                self.mode = AppMode::Normal;
            }
            KeyCode::Enter => {
                if !self.search_input.is_empty() {
                    self.start_search().await;
                }
                self.mode = AppMode::Normal;
            }
            KeyCode::Backspace => {
                self.search_input.pop();
            }
            KeyCode::Char(c) => {
                self.search_input.push(c);
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle_normal_key(&mut self, key: KeyCode, _mods: KeyModifiers) -> Result<bool> {
        match key {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('d') => {
                self.show_visuals = !self.show_visuals;
            }
            KeyCode::Char('/') | KeyCode::Char('s') => {
                self.mode = AppMode::Searching;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected_index > 0 {
                    self.selected_index -= 1;
                    self.request_selected_thumbnail();
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.search_results.is_empty()
                    && self.selected_index + 1 < self.search_results.len()
                {
                    self.selected_index += 1;
                    self.request_selected_thumbnail();
                }
            }
            KeyCode::Enter => {
                // Ask for confirmation only when it would disrupt playback or clear a queue.
                let needs_confirm = self.now_playing.is_some() || !self.queue.is_empty();
                if needs_confirm {
                    if let Some(result) = self.search_results.get(self.selected_index) {
                        self.confirm_title = Some(result.title.clone());
                        self.mode = AppMode::Confirming;
                    }
                } else {
                    self.play_selected().await?;
                }
            }
            KeyCode::Char('f') => {
                self.queue_selected().await?;
            }
            KeyCode::Char(']') => {
                self.skip_next().await?;
            }
            KeyCode::Char('[') => {
                self.skip_prev().await?;
            }
            KeyCode::Char(' ') => {
                self.toggle_pause().await?;
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.change_volume(5).await?;
            }
            KeyCode::Char('-') => {
                self.change_volume(-5).await?;
            }
            KeyCode::Char('h') | KeyCode::Left => {
                self.seek_by(-5.0).await?;
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.seek_by(5.0).await?;
            }
            _ => {}
        }
        Ok(false)
    }

    async fn start_search(&mut self) {
        self.is_searching = true;
        self.status_message = Some("Searching...".to_string());
        self.search_results.clear();
        self.selected_index = 0;
        self.thumbnail_protocols.clear();
        self.thumbnails_loading.clear();
        self.thumbnails_failed.clear();

        let query = self.search_input.clone();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            match crate::youtube::search(&query, 10).await {
                Ok(results) => {
                    let _ = tx.send(AppMessage::SearchResults(results));
                }
                Err(e) => {
                    let _ = tx.send(AppMessage::SearchError(e.to_string()));
                }
            }
        });
    }

    fn request_thumbnail_for(&mut self, video_id: &str, url: &str) {
        if !self.has_image_support
            || self.thumbnail_protocols.contains_key(video_id)
            || self.thumbnails_loading.contains(video_id)
            || self.thumbnails_failed.contains(video_id)
        {
            return;
        }
        self.thumbnails_loading.insert(video_id.to_string());
        let tx = self.msg_tx.clone();
        let vid = video_id.to_string();
        let url = url.to_string();
        tokio::spawn(async move {
            crate::thumbnail::load(vid, url, tx).await;
        });
    }

    fn request_selected_thumbnail(&mut self) {
        if let Some(result) = self.search_results.get(self.selected_index) {
            let id = result.id.clone();
            let url = result.thumbnail_url();
            self.request_thumbnail_for(&id, &url);
        }
    }

    pub async fn handle_message(&mut self, msg: AppMessage) -> Result<()> {
        match msg {
            AppMessage::SearchResults(results) => {
                self.is_searching = false;
                self.status_message = None;

                let preload: Vec<(String, String)> = results
                    .iter()
                    .take(5)
                    .map(|r| (r.id.clone(), r.thumbnail_url()))
                    .collect();

                self.search_results = results;
                self.selected_index = 0;

                for (id, url) in preload {
                    self.request_thumbnail_for(&id, &url);
                }
            }
            AppMessage::SearchError(e) => {
                self.is_searching = false;
                self.status_message = Some(format!("Error: {}", e));
            }
            AppMessage::ThumbnailLoaded { video_id, image } => {
                self.thumbnails_loading.remove(&video_id);
                let protocol = self.picker.new_resize_protocol(image);
                self.thumbnail_protocols.insert(video_id, protocol);
            }
            AppMessage::ThumbnailFailed(video_id) => {
                self.thumbnails_loading.remove(&video_id);
                self.thumbnails_failed.insert(video_id);
            }
            AppMessage::AudioLoading => {
                self.status_message = Some("Buffering audio...".to_string());
            }
            AppMessage::AudioReady => {
                self.status_message = None;
            }
            AppMessage::AudioError(e) => {
                self.status_message = Some(format!("Audio error: {}", e));
                self.now_playing = None;
                self.is_paused = false;
                self.play_start = None;
                self.update_media_controls();
            }
            AppMessage::AudioFinished => {
                if let Some(done) = self.now_playing.take() {
                    self.history.push(done);
                }
                if let Some(next) = self.queue.pop_front() {
                    let url = next.watch_url();
                    self.now_playing = Some(next);
                    self.is_paused = false;
                    self.play_start = Some(Instant::now());
                    self.paused_elapsed = 0.0;
                    self.player.play_url(&url).await?;
                } else {
                    self.is_paused = false;
                    self.play_start = None;
                }
                self.update_media_controls();
            }
        }
        Ok(())
    }

    /// Handle a media key event forwarded from the souvlaki callback.
    pub async fn handle_media_action(&mut self, action: MediaAction) -> Result<()> {
        match action {
            MediaAction::Play => {
                if self.is_paused && self.now_playing.is_some() {
                    self.toggle_pause().await?;
                }
            }
            MediaAction::Pause => {
                if !self.is_paused && self.now_playing.is_some() {
                    self.toggle_pause().await?;
                }
            }
            MediaAction::Toggle => {
                if self.now_playing.is_some() {
                    self.toggle_pause().await?;
                }
            }
            MediaAction::Stop => {
                if self.now_playing.is_some() {
                    if !self.is_paused {
                        self.toggle_pause().await?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Push current playback state and metadata to the OS media centre (MPRIS2 / Now Playing).
    pub fn update_media_controls(&mut self) {
        let Some(ref mut controls) = self.media_controls else {
            return;
        };

        match self.now_playing.as_ref() {
            None => {
                let _ = controls.set_playback(MediaPlayback::Stopped);
            }
            Some(track) => {
                // Clone strings so we aren't holding a borrow of self while
                // mutably accessing self.media_controls.
                let title = track.title.clone();
                let artist = track
                    .channel
                    .clone()
                    .or_else(|| track.uploader.clone())
                    .unwrap_or_default();
                let duration = track
                    .duration
                    .map(|d| Duration::from_secs_f64(d));

                let _ = controls.set_metadata(MediaMetadata {
                    title: Some(title.as_str()),
                    artist: Some(artist.as_str()),
                    album: None,
                    cover_url: None,
                    duration,
                });

                let playback = if self.is_paused {
                    MediaPlayback::Paused { progress: None }
                } else {
                    MediaPlayback::Playing { progress: None }
                };
                let _ = controls.set_playback(playback);
            }
        }
    }

    async fn play_selected(&mut self) -> Result<()> {
        if let Some(result) = self.search_results.get(self.selected_index).cloned() {
            if let Some(prev) = self.now_playing.take() {
                self.history.push(prev);
            }
            self.queue.clear();
            let url = result.watch_url();
            self.now_playing = Some(result);
            self.is_paused = false;
            self.play_start = Some(Instant::now());
            self.paused_elapsed = 0.0;
            self.player.play_url(&url).await?;
            self.update_media_controls();
        }
        Ok(())
    }

    async fn skip_next(&mut self) -> Result<()> {
        if let Some(next) = self.queue.pop_front() {
            if let Some(current) = self.now_playing.take() {
                self.history.push(current);
            }
            let url = next.watch_url();
            self.now_playing = Some(next);
            self.is_paused = false;
            self.play_start = Some(Instant::now());
            self.paused_elapsed = 0.0;
            self.player.play_url(&url).await?;
            self.update_media_controls();
        }
        Ok(())
    }

    async fn skip_prev(&mut self) -> Result<()> {
        if let Some(prev) = self.history.pop() {
            if let Some(current) = self.now_playing.take() {
                self.queue.push_front(current);
            }
            let url = prev.watch_url();
            self.now_playing = Some(prev);
            self.is_paused = false;
            self.play_start = Some(Instant::now());
            self.paused_elapsed = 0.0;
            self.player.play_url(&url).await?;
            self.update_media_controls();
        }
        Ok(())
    }

    async fn queue_selected(&mut self) -> Result<()> {
        if let Some(result) = self.search_results.get(self.selected_index).cloned() {
            if self.now_playing.is_none() {
                // Nothing playing — start immediately without touching the queue.
                let url = result.watch_url();
                self.now_playing = Some(result);
                self.is_paused = false;
                self.play_start = Some(Instant::now());
                self.paused_elapsed = 0.0;
                self.player.play_url(&url).await?;
                self.update_media_controls();
            } else {
                let title = result.title.clone();
                self.queue.push_back(result);
                self.status_message = Some(format!(
                    "Added to queue ({} tracks): {}",
                    self.queue.len(),
                    &title[..title.len().min(40)]
                ));
            }
        }
        Ok(())
    }

    async fn toggle_pause(&mut self) -> Result<()> {
        if self.now_playing.is_some() {
            if self.is_paused {
                self.is_paused = false;
                self.play_start = Some(Instant::now());
            } else {
                self.paused_elapsed = self.current_position();
                self.play_start = None;
                self.is_paused = true;
            }
            self.player.toggle_pause().await.ok();
            self.update_media_controls();
        }
        Ok(())
    }

    async fn change_volume(&mut self, delta: i32) -> Result<()> {
        self.volume = (self.volume + delta).clamp(0, 130);
        self.player.set_volume(self.volume).await.ok();
        Ok(())
    }

    pub fn current_position(&self) -> f64 {
        let running = self
            .play_start
            .map(|s| s.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        self.paused_elapsed + running
    }

    async fn handle_confirm_key(&mut self, key: KeyCode) -> Result<()> {
        match key {
            // Enter or 'y'/'Y' → confirmed, play now.
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.mode = AppMode::Normal;
                self.confirm_title = None;
                self.play_selected().await?;
            }
            // 'n'/'N' or Esc → cancelled.
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.mode = AppMode::Normal;
                self.confirm_title = None;
            }
            _ => {}
        }
        Ok(())
    }

    async fn seek_by(&mut self, delta: f64) -> Result<()> {
        let new_pos = (self.current_position() + delta).max(0.0);
        self.paused_elapsed = new_pos;
        if !self.is_paused {
            self.play_start = Some(Instant::now());
        }
        self.player.seek_abs(new_pos).await.ok();
        Ok(())
    }

    async fn handle_mouse_click(&mut self, col: u16, row: u16) -> Result<()> {
        let Some(area) = self.progress_bar_area else { return Ok(()); };
        if row < area.y || row >= area.y + area.height { return Ok(()); }
        if col < area.x || col >= area.x + area.width { return Ok(()); }
        let Some(duration) = self.now_playing.as_ref().and_then(|t| t.duration) else {
            return Ok(());
        };
        let ratio = (col - area.x) as f64 / area.width as f64;
        let target = (ratio * duration).max(0.0);
        self.paused_elapsed = target;
        if !self.is_paused {
            self.play_start = Some(Instant::now());
        }
        self.player.seek_abs(target).await.ok();
        Ok(())
    }
}
