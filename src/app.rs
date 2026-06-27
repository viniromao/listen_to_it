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
    /// Real playback position (seconds) reported by mpv via IPC
    Position(f64),
    ChaptersLoaded { url: String, chapters: Vec<Chapter> },
    MoreResults(Vec<VideoResult>),
    PlaylistLoaded { videos: Vec<VideoResult>, play_immediately: bool },
}

#[derive(Debug, Clone)]
pub struct Chapter {
    pub start_time: f64,
    pub title: String,
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
    /// While set (and recent), mpv position reports are ignored so a manual
    /// seek isn't briefly overwritten by a stale pre-seek sample.
    pub seek_guard: Option<Instant>,

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

    pub loop_mode: bool,
    pub chapters: Vec<Chapter>,
    pub search_query: String,
    pub is_loading_more: bool,
    pub search_cursor: usize,
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
            seek_guard: None,

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

            loop_mode: false,
            chapters: Vec::new(),
            search_query: String::new(),
            is_loading_more: false,
            search_cursor: 0,
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
                if self.search_cursor > 0 {
                    self.search_cursor -= 1;
                    let byte_pos = self.search_input.char_indices()
                        .nth(self.search_cursor)
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    self.search_input.remove(byte_pos);
                }
            }
            KeyCode::Delete => {
                let len = self.search_input.chars().count();
                if self.search_cursor < len {
                    let byte_pos = self.search_input.char_indices()
                        .nth(self.search_cursor)
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    self.search_input.remove(byte_pos);
                }
            }
            KeyCode::Left => {
                if self.search_cursor > 0 {
                    self.search_cursor -= 1;
                }
            }
            KeyCode::Right => {
                if self.search_cursor < self.search_input.chars().count() {
                    self.search_cursor += 1;
                }
            }
            KeyCode::Home => {
                self.search_cursor = 0;
            }
            KeyCode::End => {
                self.search_cursor = self.search_input.chars().count();
            }
            KeyCode::Char(c) => {
                let byte_pos = self.search_input.char_indices()
                    .nth(self.search_cursor)
                    .map(|(i, _)| i)
                    .unwrap_or(self.search_input.len());
                self.search_input.insert(byte_pos, c);
                self.search_cursor += 1;
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
                self.search_cursor = self.search_input.chars().count();
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
                    let remaining = self.search_results.len().saturating_sub(self.selected_index + 1);
                    if remaining <= 2 && !self.is_loading_more && !self.search_query.is_empty() {
                        self.load_more_results();
                    }
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
            KeyCode::Char('r') => {
                self.loop_mode = !self.loop_mode;
                let msg = if self.loop_mode { "Loop ON" } else { "Loop OFF" };
                self.status_message = Some(msg.to_string());
            }
            KeyCode::Char('}') => {
                self.seek_to_next_chapter().await?;
            }
            KeyCode::Char('{') => {
                self.seek_to_prev_chapter().await?;
            }
            _ => {}
        }
        Ok(false)
    }

    async fn start_search(&mut self) {
        self.is_searching = true;
        self.is_loading_more = false;
        self.status_message = Some("Searching...".to_string());
        self.search_results.clear();
        self.selected_index = 0;
        self.thumbnail_protocols.clear();
        self.thumbnails_loading.clear();
        self.thumbnails_failed.clear();

        self.search_query = self.search_input.clone();
        let query = self.search_query.clone();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            match crate::youtube::search(&query, 20).await {
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

    fn load_more_results(&mut self) {
        self.is_loading_more = true;
        let query = self.search_query.clone();
        let total = self.search_results.len() + 3;
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let results = crate::youtube::search(&query, total).await.unwrap_or_default();
            let _ = tx.send(AppMessage::MoreResults(results));
        });
    }

    fn fetch_chapters_for(&self, url: &str) {
        let tx = self.msg_tx.clone();
        let url = url.to_string();
        tokio::spawn(async move {
            let output = tokio::process::Command::new(crate::ytdlp::path())
                .args(["-j", "--no-playlist", "--no-warnings", &url])
                .output()
                .await;
            let chapters = match output {
                Ok(out) if out.status.success() => {
                    let json: serde_json::Value = serde_json::from_slice(&out.stdout)
                        .unwrap_or(serde_json::Value::Null);
                    json["chapters"]
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
                        .unwrap_or_default()
                }
                _ => vec![],
            };
            let _ = tx.send(AppMessage::ChaptersLoaded { url, chapters });
        });
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
                if self.loop_mode {
                    if let Some(ref track) = self.now_playing {
                        let url = track.watch_url();
                        self.is_paused = false;
                        self.play_start = Some(Instant::now());
                        self.paused_elapsed = 0.0;
                        self.player.play_url(&url).await?;
                        self.update_media_controls();
                    }
                } else {
                    if let Some(done) = self.now_playing.take() {
                        self.history.push(done);
                    }
                    if let Some(next) = self.queue.pop_front() {
                        let url = next.watch_url();
                        self.now_playing = Some(next);
                        self.is_paused = false;
                        self.play_start = Some(Instant::now());
                        self.paused_elapsed = 0.0;
                        self.chapters.clear();
                        self.player.play_url(&url).await?;
                        self.fetch_chapters_for(&url);
                    } else {
                        self.is_paused = false;
                        self.play_start = None;
                        self.chapters.clear();
                    }
                    self.update_media_controls();
                }
            }
            AppMessage::Position(pos) => {
                // Re-anchor the local clock to mpv's real position. This keeps
                // the counter glued to the stream across pause, seek, and system
                // suspend, while the Instant extrapolation in current_position()
                // keeps motion smooth between these ~5 Hz samples.
                let seeking_recently = self
                    .seek_guard
                    .map(|t| t.elapsed() < Duration::from_millis(400))
                    .unwrap_or(false);
                if self.now_playing.is_some() && !seeking_recently {
                    self.paused_elapsed = pos;
                    self.play_start = if self.is_paused {
                        None
                    } else {
                        Some(Instant::now())
                    };
                }
            }
            AppMessage::ChaptersLoaded { url, chapters } => {
                if self.now_playing.as_ref().map(|t| t.watch_url()) == Some(url) {
                    self.chapters = chapters;
                }
            }
            AppMessage::PlaylistLoaded { videos, play_immediately } => {
                self.status_message = None;
                if videos.is_empty() {
                    self.status_message = Some("Playlist is empty or could not be loaded.".to_string());
                    return Ok(());
                }
                let total = videos.len();
                if play_immediately {
                    if let Some(prev) = self.now_playing.take() {
                        self.history.push(prev);
                    }
                    self.queue.clear();
                    let mut iter = videos.into_iter();
                    if let Some(first) = iter.next() {
                        let url = first.watch_url();
                        self.now_playing = Some(first);
                        self.is_paused = false;
                        self.play_start = Some(Instant::now());
                        self.paused_elapsed = 0.0;
                        self.chapters.clear();
                        self.player.play_url(&url).await?;
                        self.fetch_chapters_for(&url);
                    }
                    for video in iter {
                        self.queue.push_back(video);
                    }
                    self.update_media_controls();
                } else {
                    for video in videos {
                        self.queue.push_back(video);
                    }
                    self.status_message = Some(format!("Added {} tracks to queue", total));
                }
            }
            AppMessage::MoreResults(all_results) => {
                self.is_loading_more = false;
                let existing: std::collections::HashSet<String> =
                    self.search_results.iter().map(|r| r.id.clone()).collect();
                let new_results: Vec<_> = all_results
                    .into_iter()
                    .filter(|r| !existing.contains(&r.id))
                    .collect();
                self.search_results.extend(new_results);
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
            if result.is_playlist {
                let url = result.watch_url();
                self.status_message = Some(format!("Loading playlist \"{}\"...", &result.title.chars().take(35).collect::<String>()));
                let tx = self.msg_tx.clone();
                tokio::spawn(async move {
                    match crate::youtube::fetch_playlist(&url).await {
                        Ok(videos) => { let _ = tx.send(AppMessage::PlaylistLoaded { videos, play_immediately: true }); }
                        Err(e) => { let _ = tx.send(AppMessage::SearchError(e.to_string())); }
                    }
                });
                return Ok(());
            }
            if let Some(prev) = self.now_playing.take() {
                self.history.push(prev);
            }
            self.queue.clear();
            let url = result.watch_url();
            self.now_playing = Some(result);
            self.is_paused = false;
            self.play_start = Some(Instant::now());
            self.paused_elapsed = 0.0;
            self.chapters.clear();
            self.player.play_url(&url).await?;
            self.fetch_chapters_for(&url);
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
            self.chapters.clear();
            self.player.play_url(&url).await?;
            self.fetch_chapters_for(&url);
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
            self.chapters.clear();
            self.player.play_url(&url).await?;
            self.fetch_chapters_for(&url);
            self.update_media_controls();
        }
        Ok(())
    }

    async fn queue_selected(&mut self) -> Result<()> {
        if let Some(result) = self.search_results.get(self.selected_index).cloned() {
            if result.is_playlist {
                let play_immediately = self.now_playing.is_none();
                let url = result.watch_url();
                self.status_message = Some(format!("Loading playlist \"{}\"...", &result.title.chars().take(35).collect::<String>()));
                let tx = self.msg_tx.clone();
                tokio::spawn(async move {
                    match crate::youtube::fetch_playlist(&url).await {
                        Ok(videos) => { let _ = tx.send(AppMessage::PlaylistLoaded { videos, play_immediately }); }
                        Err(e) => { let _ = tx.send(AppMessage::SearchError(e.to_string())); }
                    }
                });
                return Ok(());
            }
            if self.now_playing.is_none() {
                // Nothing playing — start immediately without touching the queue.
                let url = result.watch_url();
                self.now_playing = Some(result);
                self.is_paused = false;
                self.play_start = Some(Instant::now());
                self.paused_elapsed = 0.0;
                self.chapters.clear();
                self.player.play_url(&url).await?;
                self.fetch_chapters_for(&url);
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

    async fn seek_to(&mut self, pos: f64) -> Result<()> {
        let pos = pos.max(0.0);
        self.paused_elapsed = pos;
        self.seek_guard = Some(Instant::now());
        if !self.is_paused {
            self.play_start = Some(Instant::now());
        }
        self.player.seek_abs(pos).await.ok();
        Ok(())
    }

    async fn seek_by(&mut self, delta: f64) -> Result<()> {
        let new_pos = (self.current_position() + delta).max(0.0);
        self.seek_to(new_pos).await
    }

    async fn seek_to_next_chapter(&mut self) -> Result<()> {
        let pos = self.current_position();
        if let Some(ch) = self.chapters.iter().find(|c| c.start_time > pos + 0.5) {
            self.seek_to(ch.start_time).await?;
        }
        Ok(())
    }

    async fn seek_to_prev_chapter(&mut self) -> Result<()> {
        let pos = self.current_position();
        let current = self.chapters.iter().filter(|c| c.start_time <= pos).last().cloned();
        if let Some(ch) = current {
            if pos - ch.start_time > 3.0 {
                self.seek_to(ch.start_time).await?;
            } else {
                let prev_start = self.chapters.iter()
                    .filter(|c| c.start_time < ch.start_time)
                    .last()
                    .map(|c| c.start_time);
                self.seek_to(prev_start.unwrap_or(0.0)).await?;
            }
        }
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
        self.seek_guard = Some(Instant::now());
        if !self.is_paused {
            self.play_start = Some(Instant::now());
        }
        self.player.seek_abs(target).await.ok();
        Ok(())
    }
}
