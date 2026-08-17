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
use std::sync::Arc;
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
    /// A track requested for playback has been resolved to a playable stream.
    StreamReady { watch_url: String, stream: Arc<crate::stream::Stream> },
    /// Resolving a track requested for playback failed.
    StreamFailed { watch_url: String, error: String },
    /// Debounce tick: the highlighted row has stayed put long enough to be
    /// worth resolving ahead of time. Carries the generation it was scheduled
    /// with, so ticks from rows the user has already scrolled past are ignored.
    PrefetchSelected(u64),
    MoreResults(Vec<VideoResult>),
    PlaylistLoaded { videos: Vec<VideoResult>, play_immediately: bool },
    /// Lazily-fetched metadata for a playlist search row.
    PlaylistMetaLoaded { id: String, meta: crate::youtube::PlaylistMeta },
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
    /// Whether `status_message` reports a failure, as opposed to progress the
    /// user asked for. Only the former earns an alarming colour — a search in
    /// flight is not an error. Kept in sync by `set_status`/`set_error`.
    pub status_is_error: bool,

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
    /// Playlist ids whose metadata has already been requested, to fetch once.
    pub playlist_meta_requested: HashSet<String>,
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

    /// Stable clock used only to drive ASCII animation frames (buffering
    /// spinner, playing equalizer) — never reset, just sampled for elapsed time.
    pub started_at: Instant,

    /// Consecutive playback failures without a real position report in
    /// between. Caps the auto-skip-on-error cascade so a systemic failure
    /// (e.g. YouTube throttling every track) can't silently drain the whole
    /// queue — see `MAX_CONSECUTIVE_FAILURES`.
    pub consecutive_failures: u32,

    /// How many times the current track has already been retried after an
    /// `AudioError`. YouTube's signed CDN URLs are prone to a transient
    /// 403 that has nothing to do with the video — measured empirically at
    /// roughly a 1-in-3 failure rate per attempt on an otherwise-fine video,
    /// so a single retry alone (~11% chance both attempts miss) still
    /// leaves a real dent — see `MAX_RETRIES_PER_TRACK`. Only counts as a
    /// real failure, and advances to the next track, once retries are used up.
    pub retries_current_track: u32,

    /// When the current track was requested, until its first real position
    /// report. Only used to log how long starting a track actually took, which
    /// is the number to watch when playback feels slow.
    track_started_at: Option<Instant>,

    /// Bumped every time the highlighted search row changes. A scheduled
    /// prefetch tick only acts if its generation is still current, which
    /// debounces resolving while the user scrolls through results.
    prefetch_gen: u64,
}

/// After this many playback failures in a row, stop auto-advancing and leave
/// the remaining queue intact instead of racing through it.
const MAX_CONSECUTIVE_FAILURES: u32 = 3;

/// How many times to retry the same track after an `AudioError` before
/// giving up on it. Measured per-attempt failure rate on a known-good video
/// was ~1-in-3, so 2 retries (3 attempts total) drops the odds of every
/// attempt missing to roughly 1-in-27.
const MAX_RETRIES_PER_TRACK: u32 = 2;

/// Pause before retrying the same track, giving whatever transient condition
/// caused the failure (signed-URL race, brief CDN hiccup) a moment to clear.
const RETRY_DELAY: Duration = Duration::from_millis(500);

/// How long a search row must stay highlighted before its stream is resolved
/// ahead of a possible play. Long enough that scrolling past a row costs
/// nothing, short enough that a row someone is actually reading is ready by
/// the time they hit Enter.
const HOVER_PREFETCH_DELAY: Duration = Duration::from_millis(900);

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
            status_is_error: false,

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
            playlist_meta_requested: HashSet::new(),
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

            started_at: Instant::now(),
            consecutive_failures: 0,
            retries_current_track: 0,
            track_started_at: None,
            prefetch_gen: 0,
        }
    }

    /// Progress the user asked for: searching, loading, queued. Informational,
    /// and rendered as such.
    fn set_status(&mut self, msg: impl Into<String>) {
        self.status_message = Some(msg.into());
        self.status_is_error = false;
    }

    /// Something actually went wrong. Rendered as an error.
    fn set_error(&mut self, msg: impl Into<String>) {
        self.status_message = Some(msg.into());
        self.status_is_error = true;
    }

    fn clear_status(&mut self) {
        self.status_message = None;
        self.status_is_error = false;
    }

    /// True once a track has been requested but mpv hasn't reported a real
    /// playback position yet (still resolving/buffering the YouTube stream).
    /// Derived rather than stored so it can never drift out of sync with
    /// `play_start`/`is_paused`.
    pub fn is_buffering(&self) -> bool {
        self.now_playing.is_some() && self.play_start.is_none() && !self.is_paused
    }

    /// Make `track` the current track and start it playing.
    async fn start_track(&mut self, track: VideoResult) -> Result<()> {
        let url = track.watch_url();
        self.now_playing = Some(track);
        self.is_paused = false;
        self.play_start = None; // anchored once mpv reports a real Position
        self.paused_elapsed = 0.0;
        self.chapters.clear();
        self.retries_current_track = 0;
        self.track_started_at = Some(Instant::now());
        self.begin_stream(&url).await?;
        self.update_media_controls();
        self.prefetch_next();
        Ok(())
    }

    /// Hand the current track's stream to mpv. A stream that was resolved
    /// ahead of time (see `crate::stream`) starts immediately; otherwise
    /// resolving runs in the background and playback starts from
    /// `StreamReady`, so the UI never sits still waiting on yt-dlp.
    async fn begin_stream(&mut self, watch_url: &str) -> Result<()> {
        if let Some(stream) = crate::stream::cached(watch_url) {
            self.chapters = stream.chapters.clone();
            self.clear_status();
            self.player.play(&stream).await?;
            return Ok(());
        }

        // Nothing cached: stop whatever is playing now rather than leaving the
        // previous track audible while the UI already shows the new one.
        self.player.stop().await;
        self.set_status("Loading stream...".to_string());
        let tx = self.msg_tx.clone();
        let watch_url = watch_url.to_string();
        tokio::spawn(async move {
            let msg = match crate::stream::resolve(&watch_url).await {
                Ok(stream) => AppMessage::StreamReady { watch_url, stream },
                Err(e) => AppMessage::StreamFailed { watch_url, error: e.to_string() },
            };
            let _ = tx.send(msg);
        });
        Ok(())
    }

    /// Resolve the next queued track while the current one is still playing,
    /// so advancing the queue costs an mpv startup instead of a fresh
    /// extraction.
    fn prefetch_next(&self) {
        if let Some(next) = self.queue.front() {
            if !next.is_playlist {
                crate::stream::prefetch(next.watch_url());
            }
        }
    }

    /// Selection moved: pull in the artwork for the new row, and line it up to
    /// be resolved if it stays put.
    fn on_selection_changed(&mut self) {
        self.request_selected_thumbnail();
        self.schedule_selection_prefetch();
    }

    fn schedule_selection_prefetch(&mut self) {
        self.prefetch_gen = self.prefetch_gen.wrapping_add(1);
        let generation = self.prefetch_gen;
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(HOVER_PREFETCH_DELAY).await;
            let _ = tx.send(AppMessage::PrefetchSelected(generation));
        });
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
                    self.on_selection_changed();
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.search_results.is_empty()
                    && self.selected_index + 1 < self.search_results.len()
                {
                    self.selected_index += 1;
                    self.on_selection_changed();
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
                self.set_status(msg.to_string());
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
        self.set_status("Searching...".to_string());
        self.search_results.clear();
        self.selected_index = 0;
        self.thumbnail_protocols.clear();
        self.thumbnails_loading.clear();
        self.thumbnails_failed.clear();
        self.playlist_meta_requested.clear();

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

    /// Kick off a one-time metadata fetch for every playlist row in the current
    /// results that hasn't been requested yet (owner, track count, total views).
    fn request_playlist_meta(&mut self) {
        let pending: Vec<(String, String)> = self
            .search_results
            .iter()
            .filter(|r| r.is_playlist && !self.playlist_meta_requested.contains(&r.id))
            .map(|r| (r.id.clone(), r.watch_url()))
            .collect();
        for (id, url) in pending {
            self.playlist_meta_requested.insert(id.clone());
            let tx = self.msg_tx.clone();
            tokio::spawn(async move {
                if let Ok(meta) = crate::youtube::fetch_playlist_meta(&url).await {
                    let _ = tx.send(AppMessage::PlaylistMetaLoaded { id, meta });
                }
            });
        }
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

    pub async fn handle_message(&mut self, msg: AppMessage) -> Result<()> {
        match msg {
            AppMessage::SearchResults(results) => {
                self.is_searching = false;
                self.clear_status();

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
                self.request_playlist_meta();
                self.schedule_selection_prefetch();
            }
            AppMessage::SearchError(e) => {
                self.is_searching = false;
                self.set_error(format!("Error: {}", e));
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
                self.set_status("Buffering audio...".to_string());
            }
            AppMessage::AudioReady => {
                self.clear_status();
            }
            AppMessage::AudioError(e) => {
                self.on_playback_error(e).await?;
            }
            AppMessage::StreamReady { watch_url, stream } => {
                if self.is_current_track(&watch_url) {
                    self.chapters = stream.chapters.clone();
                    self.clear_status();
                    self.player.play(&stream).await?;
                }
            }
            AppMessage::StreamFailed { watch_url, error } => {
                if self.is_current_track(&watch_url) {
                    self.on_playback_error(error).await?;
                }
            }
            AppMessage::PrefetchSelected(generation) => {
                if generation == self.prefetch_gen {
                    if let Some(result) = self.search_results.get(self.selected_index) {
                        if !result.is_playlist {
                            crate::stream::prefetch(result.watch_url());
                        }
                    }
                }
            }
            AppMessage::AudioFinished => {
                if self.loop_mode {
                    if let Some(track) = self.now_playing.take() {
                        self.start_track(track).await?;
                    }
                } else {
                    if let Some(done) = self.now_playing.take() {
                        self.history.push(done);
                    }
                    if let Some(next) = self.queue.pop_front() {
                        self.start_track(next).await?;
                    } else {
                        self.is_paused = false;
                        self.play_start = None;
                        self.chapters.clear();
                        self.update_media_controls();
                    }
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
                    if let Some(requested_at) = self.track_started_at.take() {
                        crate::logline!(
                            "app: audio started {:.2}s after the track was requested",
                            requested_at.elapsed().as_secs_f64()
                        );
                    }
                    self.consecutive_failures = 0;
                    self.retries_current_track = 0;
                    self.paused_elapsed = pos;
                    self.play_start = if self.is_paused {
                        None
                    } else {
                        Some(Instant::now())
                    };
                }
            }
            AppMessage::PlaylistLoaded { videos, play_immediately } => {
                self.clear_status();
                if videos.is_empty() {
                    self.set_error("Playlist is empty or could not be loaded.".to_string());
                    return Ok(());
                }
                let total = videos.len();
                if play_immediately {
                    if let Some(prev) = self.now_playing.take() {
                        self.history.push(prev);
                    }
                    self.queue.clear();
                    let mut iter = videos.into_iter();
                    let first = iter.next();
                    for video in iter {
                        self.queue.push_back(video);
                    }
                    if let Some(first) = first {
                        self.consecutive_failures = 0;
                        self.start_track(first).await?;
                    }
                } else {
                    for video in videos {
                        self.queue.push_back(video);
                    }
                    self.set_status(format!("Added {} tracks to queue", total));
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
                self.request_playlist_meta();
            }
            AppMessage::PlaylistMetaLoaded { id, meta } => {
                if let Some(r) = self.search_results.iter_mut().find(|r| r.id == id) {
                    if meta.channel.is_some() {
                        r.channel = meta.channel;
                    }
                    r.playlist_count = meta.count;
                    if meta.view_count.is_some() {
                        r.view_count = meta.view_count;
                    }
                }
            }
        }
        Ok(())
    }

    fn is_current_track(&self, watch_url: &str) -> bool {
        self.now_playing
            .as_ref()
            .is_some_and(|t| t.watch_url() == watch_url)
    }

    /// A track failed to start or died mid-stream — either yt-dlp couldn't
    /// resolve it or mpv exited non-zero on the resolved URL.
    async fn on_playback_error(&mut self, e: String) -> Result<()> {
        let failed_track = self.now_playing.take();
        self.is_paused = false;
        self.play_start = None;
        self.chapters.clear();

        // Whatever was cached for this track is suspect now — a signed URL
        // that just 403'd will keep 403ing, so drop it and make the retry go
        // back to yt-dlp for a fresh one.
        if let Some(ref track) = failed_track {
            crate::stream::invalidate(&track.watch_url());
        }

        // YouTube's signed CDN URLs are prone to a transient 403 that has
        // nothing to do with the video itself — the very same URL can fail
        // once and succeed moments later. Give the same track a couple of
        // fresh attempts (new extraction, new signed URL each time) before
        // treating this as a real failure and burning a consecutive-failure
        // slot / advancing the queue. A short pause before retrying gives
        // whatever transient condition caused the 403 a moment to clear
        // instead of immediately racing into the same failure.
        if self.retries_current_track < MAX_RETRIES_PER_TRACK {
            if let Some(track) = failed_track {
                self.retries_current_track += 1;
                self.set_error(format!(
                    "Audio error: {e} — retrying ({}/{MAX_RETRIES_PER_TRACK})",
                    self.retries_current_track
                ));
                crate::logline!(
                    "app: retrying \"{}\" after error (attempt {}/{MAX_RETRIES_PER_TRACK}): {e}",
                    track.title,
                    self.retries_current_track
                );
                tokio::time::sleep(RETRY_DELAY).await;
                let url = track.watch_url();
                self.now_playing = Some(track);
                self.paused_elapsed = 0.0;
                self.begin_stream(&url).await?;
                self.update_media_controls();
                return Ok(());
            }
        }
        self.retries_current_track = 0;

        self.consecutive_failures += 1;
        crate::logline!(
            "app: AudioError #{} (after {MAX_RETRIES_PER_TRACK} retries): {e} ({} left in queue)",
            self.consecutive_failures,
            self.queue.len()
        );
        if self.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
            // Several tracks in a row failed to play even after a retry each —
            // this is almost certainly a systemic problem (network, rate
            // limiting, broken yt-dlp), not one-off bad luck. Stop racing
            // through the queue and surface it clearly instead of silently
            // draining every track down to an empty queue.
            self.set_error(format!(
                "Audio error: {e} — {} tracks failed in a row, stopped auto-skip ({} left in queue)",
                self.consecutive_failures,
                self.queue.len()
            ));
            crate::logline!("app: hit MAX_CONSECUTIVE_FAILURES, stopping auto-skip");
            self.update_media_controls();
        } else {
            // The track never actually played, so it doesn't belong in
            // history — just drop it and, if there's more queued, move on
            // rather than leaving playback silently stalled.
            if let Some(next) = self.queue.pop_front() {
                self.start_track(next).await?;
            } else {
                self.update_media_controls();
            }
            self.set_error(format!("Audio error: {e} — skipping to next"));
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
                self.set_status(format!("Loading playlist \"{}\"...", &result.title.chars().take(35).collect::<String>()));
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
            self.consecutive_failures = 0;
            self.start_track(result).await?;
        }
        Ok(())
    }

    async fn skip_next(&mut self) -> Result<()> {
        if let Some(next) = self.queue.pop_front() {
            if let Some(current) = self.now_playing.take() {
                self.history.push(current);
            }
            self.consecutive_failures = 0;
            self.start_track(next).await?;
        }
        Ok(())
    }

    async fn skip_prev(&mut self) -> Result<()> {
        if let Some(prev) = self.history.pop() {
            if let Some(current) = self.now_playing.take() {
                self.queue.push_front(current);
            }
            self.consecutive_failures = 0;
            self.start_track(prev).await?;
        }
        Ok(())
    }

    async fn queue_selected(&mut self) -> Result<()> {
        if let Some(result) = self.search_results.get(self.selected_index).cloned() {
            if result.is_playlist {
                let play_immediately = self.now_playing.is_none();
                let url = result.watch_url();
                self.set_status(format!("Loading playlist \"{}\"...", &result.title.chars().take(35).collect::<String>()));
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
                self.consecutive_failures = 0;
                self.start_track(result).await?;
            } else {
                let title = result.title.clone();
                self.queue.push_back(result);
                self.set_status(format!(
                    "Added to queue ({} tracks): {}",
                    self.queue.len(),
                    &title[..title.len().min(40)]
                ));
                self.prefetch_next();
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
