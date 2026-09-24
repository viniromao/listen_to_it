use crate::input::TextInput;
use crate::library::{DeleteTarget, Library, SavedPlaylist};
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
    /// Artwork for the track in the now-playing bar, fetched separately from
    /// the preview cache so a new search or eviction never takes it away.
    NowPlayingThumbnail { video_id: String, image: DynamicImage },
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
    /// Loudness of what mpv is playing, RMS in dBFS
    AudioLevel(f64),
    /// mpv's `core-idle`: true while no audio is coming out even though a
    /// track is loaded (opening the stream, stalled waiting on the network,
    /// paused).
    AudioIdle(bool),
    /// A track requested for playback has been resolved to a playable stream.
    StreamReady { watch_url: String, stream: Arc<crate::stream::Stream> },
    /// Resolving a track requested for playback failed.
    StreamFailed { watch_url: String, error: String },
    /// Debounce tick: the highlighted row has stayed put long enough to be
    /// worth resolving ahead of time. Carries the generation it was scheduled
    /// with, so ticks from rows the user has already scrolled past are ignored.
    PrefetchSelected(u64),
    MoreResults(Vec<VideoResult>),
    /// First entries of a playlist, as soon as yt-dlp yields them — playback
    /// starts here rather than waiting for the whole playlist to be walked.
    PlaylistHead { token: u64, videos: Vec<VideoResult>, play_immediately: bool },
    /// A later batch of the same playlist, appended to the queue behind the
    /// track that is already playing.
    PlaylistTail { token: u64, videos: Vec<VideoResult> },
    /// Lazily-fetched metadata for a playlist search row.
    PlaylistMetaLoaded { id: String, meta: crate::youtube::PlaylistMeta },
    Updated(String),
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
    /// "Play this now and clear the queue?"
    Confirming,
    /// Typing a name for a playlist being created or renamed.
    Naming,
    /// Choosing which saved playlist the pending tracks should go into.
    PickingPlaylist,
    /// "Delete this playlist / remove this track?"
    ConfirmingDelete,
    /// The full key reference, over whatever was on screen.
    Help,
}

/// What the main pane shows: YouTube search results, or the playlists saved
/// on this machine.
#[derive(PartialEq, Clone, Copy)]
pub enum View {
    Search,
    Library,
}

/// Which half of the library view the keys act on.
#[derive(PartialEq, Clone, Copy)]
pub enum LibraryFocus {
    Playlists,
    Tracks,
}

/// What the name currently being typed is for.
pub enum NameTarget {
    /// Create a playlist, then move `pending_tracks` into it.
    Create,
    Rename(usize),
}

pub struct App {
    pub mode: AppMode,
    pub view: View,
    pub search: TextInput,
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
    /// The now-playing track's artwork, keyed by video id. Its own protocol
    /// rather than a shared one from `thumbnail_protocols`: the bar and the
    /// preview draw at different sizes, and one protocol drawn at two sizes
    /// re-encodes the image on every frame.
    pub now_playing_thumb: Option<(String, StatefulProtocol)>,
    pub picker: Picker,

    pub msg_tx: UnboundedSender<AppMessage>,
    pub has_image_support: bool,

    // Stored on main thread only — not required to be Send.
    pub media_controls: Option<MediaControls>,

    pub show_visuals: bool,
    pub spectrum: crate::spectrum::Spectrum,
    /// Last `AudioIdle` from mpv. Starts true for each track, so the bars stay
    /// down until mpv says sound is actually coming out.
    pub audio_idle: bool,
    pub progress_bar_area: Option<Rect>,
    // Title of the track pending confirmation before playing.
    pub confirm_title: Option<String>,

    pub loop_mode: bool,
    pub shuffle: bool,
    pub updated_to: Option<String>,
    pub chapters: Vec<Chapter>,
    pub search_query: String,
    pub is_loading_more: bool,

    /// Playlists saved on this machine, and the file they came from.
    pub library: Library,
    /// Highlighted playlist in the library view.
    pub library_selected: usize,
    /// Highlighted track within that playlist.
    pub library_track_selected: usize,
    pub library_focus: LibraryFocus,

    /// The playlist name being typed, and what it will be used for.
    pub name_input: TextInput,
    pub name_target: Option<NameTarget>,
    /// Tracks waiting for a playlist to be picked (or created) for them.
    pub pending_tracks: Vec<VideoResult>,
    /// Highlighted row in the "add to which playlist?" dialog. One row past
    /// the last playlist is the "new playlist" entry.
    pub pick_selected: usize,
    pub delete_target: Option<DeleteTarget>,

    /// First visible line of the help overlay. Clamped while rendering,
    /// which is the only place the viewport height is known.
    pub help_scroll: u16,

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

    /// Bumped every time something takes over the queue. A playlist streams in
    /// over several seconds, so its tail can still be arriving after the user
    /// has queued something else; batches stamped with a spent token are
    /// dropped instead of appended to a queue they no longer belong to.
    playlist_token: u64,
    /// Whether the playlist currently streaming in was queued rather than
    /// played, so its running "added N tracks" count keeps updating.
    playlist_announces_count: bool,

    /// Insertion order for `thumbnail_protocols`, used to evict the oldest.
    thumbnail_order: VecDeque<String>,

    /// Set once a "load more" comes back with nothing new: YouTube has no more
    /// results for this query, and asking again just burns a full search.
    search_exhausted: bool,

    rng_state: u64,
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

/// Results asked for by the first search of a query.
const SEARCH_PAGE: usize = 25;

/// Ceiling on how deep a single query is followed. YouTube's ranking is long
/// past useful by here, and every extra row makes each re-search dearer.
const MAX_SEARCH_RESULTS: usize = 200;

/// How many rows from the bottom trigger a "load more". Deep enough that the
/// (now much rarer, but slower) re-search finishes before the user arrives.
const LOAD_MORE_LOOKAHEAD: usize = 5;

/// How far either side of the highlighted row playlist metadata is fetched.
/// Each row's "N tracks / owner / views" costs an entire yt-dlp process
/// (~1.7 s measured), so it is worth spending only on rows in view.
const META_WINDOW: usize = 6;

/// Decoded thumbnails kept in memory. Each is a full 480×360 image held by its
/// resize protocol — about 0.66 MB — so an uncapped map cost ~66 MB per
/// hundred rows browsed, on top of everything else the session is holding.
const MAX_CACHED_THUMBNAILS: usize = 32;

fn seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9e37_79b9_7f4a_7c15)
        | 1
}

impl App {
    pub fn new(msg_tx: UnboundedSender<AppMessage>, picker: Picker, has_image_support: bool) -> Self {
        // Player spawns its thread immediately and uses msg_tx to report state back.
        let player = Player::new(msg_tx.clone());

        Self {
            mode: AppMode::Normal,
            view: View::Search,
            search: TextInput::default(),
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
            now_playing_thumb: None,
            picker,

            msg_tx,
            has_image_support,
            media_controls: None,

            show_visuals: true,
            spectrum: crate::spectrum::Spectrum::new(),
            audio_idle: true,
            progress_bar_area: None,
            confirm_title: None,

            loop_mode: false,
            shuffle: false,
            updated_to: None,
            chapters: Vec::new(),
            search_query: String::new(),
            is_loading_more: false,

            library: Library::load(),
            library_selected: 0,
            library_track_selected: 0,
            library_focus: LibraryFocus::Playlists,
            name_input: TextInput::default(),
            name_target: None,
            pending_tracks: Vec::new(),
            pick_selected: 0,
            delete_target: None,
            help_scroll: 0,

            started_at: Instant::now(),
            consecutive_failures: 0,
            retries_current_track: 0,
            track_started_at: None,
            prefetch_gen: 0,
            playlist_token: 0,
            playlist_announces_count: false,
            thumbnail_order: VecDeque::new(),
            search_exhausted: false,
            rng_state: seed(),
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

    /// Whether audio is actually coming out right now. `is_buffering` alone
    /// isn't enough: while mpv opens or stalls on a stream it still answers
    /// with a (stuck) position, which looks like playback to the local clock.
    fn is_audible(&self) -> bool {
        self.now_playing.is_some() && !self.is_paused && !self.is_buffering() && !self.audio_idle
    }

    /// Advance anything that animates on its own between events.
    pub fn animate(&mut self) {
        self.spectrum.step(self.is_audible());
    }

    /// How long the main loop may sleep before the next frame is due. The
    /// spectrum bars need a smoother frame rate than the clock does.
    pub fn frame_interval(&self) -> Duration {
        if self.show_visuals && self.spectrum.is_moving(self.is_audible()) {
            Duration::from_millis(50)
        } else {
            Duration::from_millis(100)
        }
    }

    /// Make `track` the current track and start it playing.
    async fn start_track(&mut self, track: VideoResult) -> Result<()> {
        let url = track.watch_url();
        self.now_playing = Some(track);
        self.request_now_playing_thumbnail();
        self.audio_idle = true;
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

    fn next_random(&mut self) -> u64 {
        let mut x = self.rng_state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng_state = x;
        x
    }

    fn random_below(&mut self, len: usize) -> usize {
        if len <= 1 {
            return 0;
        }
        (self.next_random() % len as u64) as usize
    }

    fn enqueue(&mut self, video: VideoResult) {
        if self.shuffle {
            let at = self.random_below(self.queue.len() + 1);
            self.queue.insert(at, video);
        } else {
            self.queue.push_back(video);
        }
    }

    fn enqueue_all(&mut self, videos: impl IntoIterator<Item = VideoResult>) {
        for video in videos {
            self.enqueue(video);
        }
    }

    fn take_first(&mut self, videos: &mut Vec<VideoResult>) -> Option<VideoResult> {
        if videos.is_empty() {
            return None;
        }
        let at = if self.shuffle { self.random_below(videos.len()) } else { 0 };
        Some(videos.remove(at))
    }

    fn shuffle_queue(&mut self) {
        for i in (1..self.queue.len()).rev() {
            let j = self.random_below(i + 1);
            self.queue.swap(i, j);
        }
    }

    fn toggle_shuffle(&mut self) {
        self.shuffle = !self.shuffle;
        if self.shuffle {
            self.shuffle_queue();
            self.prefetch_next();
        }
        let msg = if self.shuffle { "Shuffle ON" } else { "Shuffle OFF" };
        self.set_status(msg.to_string());
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
        self.request_playlist_meta();
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
                    AppMode::Naming => self.handle_name_key(key.code),
                    AppMode::PickingPlaylist => self.handle_pick_key(key.code),
                    AppMode::ConfirmingDelete => self.handle_delete_key(key.code),
                    AppMode::Help => self.handle_help_key(key.code),
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
                if !self.search.is_empty() {
                    self.view = View::Search;
                    self.start_search().await;
                }
                self.mode = AppMode::Normal;
            }
            other => {
                self.search.handle_key(other);
            }
        }
        Ok(())
    }

    /// Returns true when the app should quit.
    ///
    /// The keys that act on whatever is on screen (navigating, playing,
    /// saving) belong to the current view and get first refusal; anything
    /// they don't claim falls through to the playback keys, which mean the
    /// same thing wherever you happen to be.
    async fn handle_normal_key(&mut self, key: KeyCode, _mods: KeyModifiers) -> Result<bool> {
        let claimed = match self.view {
            View::Search => self.handle_results_key(key).await?,
            View::Library => self.handle_library_key(key).await?,
        };
        if claimed {
            return Ok(false);
        }
        self.handle_global_key(key).await
    }

    /// Keys that only mean something over the search results. Returns whether
    /// the key was claimed.
    async fn handle_results_key(&mut self, key: KeyCode) -> Result<bool> {
        match key {
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
                    if remaining <= LOAD_MORE_LOOKAHEAD
                        && !self.is_loading_more
                        && !self.search_query.is_empty()
                    {
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
            KeyCode::Char('a') => {
                self.add_selected_to_playlist();
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Keys that only mean something over the saved playlists. Returns
    /// whether the key was claimed.
    async fn handle_library_key(&mut self, key: KeyCode) -> Result<bool> {
        match key {
            KeyCode::Esc => self.view = View::Search,
            KeyCode::Tab | KeyCode::BackTab => self.toggle_library_focus(),
            KeyCode::Left => self.set_library_focus(LibraryFocus::Playlists),
            KeyCode::Right => self.set_library_focus(LibraryFocus::Tracks),
            KeyCode::Up | KeyCode::Char('k') => self.move_library_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_library_selection(1),
            KeyCode::Char('K') => self.reorder_selected_track(-1),
            KeyCode::Char('J') => self.reorder_selected_track(1),
            KeyCode::Enter => self.play_library_selection(true).await?,
            KeyCode::Char('f') => self.play_library_selection(false).await?,
            KeyCode::Char('n') => self.prompt_new_playlist(),
            KeyCode::Char('R') => self.prompt_rename_playlist(),
            KeyCode::Char('x') => self.prompt_delete(),
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Playback and navigation keys, available from either view. Returns true
    /// when the app should quit.
    async fn handle_global_key(&mut self, key: KeyCode) -> Result<bool> {
        match key {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('d') => {
                self.show_visuals = !self.show_visuals;
            }
            KeyCode::Char('/') | KeyCode::Char('s') => {
                self.view = View::Search;
                self.mode = AppMode::Searching;
                self.search.cursor = self.search.len();
            }
            KeyCode::Char('p') => {
                self.toggle_library_view();
            }
            KeyCode::Char('?') | KeyCode::F(1) => {
                self.help_scroll = 0;
                self.mode = AppMode::Help;
            }
            KeyCode::Char('A') => {
                self.prompt_save_queue();
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
            KeyCode::Char('z') => {
                self.toggle_shuffle();
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
        self.thumbnail_order.clear();
        self.search_exhausted = false;

        self.search_query = self.search.value.clone();
        let query = self.search_query.clone();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            match crate::youtube::search(&query, SEARCH_PAGE).await {
                Ok(results) => {
                    let _ = tx.send(AppMessage::SearchResults(results));
                }
                Err(e) => {
                    let _ = tx.send(AppMessage::SearchError(e.to_string()));
                }
            }
        });
    }

    /// Fetch artwork for the now-playing bar, unless it already shows this
    /// track (a retry of the same song keeps what it has).
    fn request_now_playing_thumbnail(&mut self) {
        let Some(track) = &self.now_playing else { return };
        if !self.has_image_support
            || self.now_playing_thumb.as_ref().is_some_and(|(id, _)| *id == track.id)
        {
            return;
        }
        self.now_playing_thumb = None;
        let tx = self.msg_tx.clone();
        let video_id = track.id.clone();
        let url = track.thumbnail_url();
        tokio::spawn(async move {
            if let Ok(image) = crate::thumbnail::fetch(&url).await {
                let _ = tx.send(AppMessage::NowPlayingThumbnail { video_id, image });
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

    /// Kick off a one-time metadata fetch (owner, track count, total views) for
    /// the playlist rows near the selection that haven't been requested yet.
    ///
    /// Deliberately not every playlist row in the results: a typical search
    /// carries ten of them, and firing one yt-dlp process per row the instant
    /// results landed put ten Python processes on the machine at once, all
    /// racing whatever the user was actually trying to play.
    fn request_playlist_meta(&mut self) {
        let lo = self.selected_index.saturating_sub(META_WINDOW);
        let hi = (self.selected_index + META_WINDOW + 1).min(self.search_results.len());
        let pending: Vec<(String, String)> = self.search_results[lo..hi]
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

    /// Keep the decoded-thumbnail map to `MAX_CACHED_THUMBNAILS`, oldest first.
    /// Browsing a long result list otherwise accumulates every image it ever
    /// showed; the row on screen is never evicted, since it is about to be
    /// drawn again.
    fn evict_old_thumbnails(&mut self) {
        let on_screen = self
            .search_results
            .get(self.selected_index)
            .map(|r| r.id.clone());
        // Bounded by the queue length rather than `while over capacity`: the
        // on-screen id gets rotated to the back rather than dropped, and an
        // unbounded loop would spin on it if it were ever the only candidate.
        for _ in 0..self.thumbnail_order.len() {
            if self.thumbnail_protocols.len() <= MAX_CACHED_THUMBNAILS {
                break;
            }
            let Some(oldest) = self.thumbnail_order.pop_front() else { break };
            if Some(&oldest) == on_screen.as_ref() {
                self.thumbnail_order.push_back(oldest);
                continue;
            }
            self.thumbnail_protocols.remove(&oldest);
        }
    }

    fn request_selected_thumbnail(&mut self) {
        if let Some(result) = self.search_results.get(self.selected_index) {
            let id = result.id.clone();
            let url = result.thumbnail_url();
            self.request_thumbnail_for(&id, &url);
        }
    }

    /// Ask YouTube for a deeper slice of the current query.
    ///
    /// There is no resumable offset on the results page: yt-dlp always starts
    /// from the top and re-walks everything, so each call costs more than the
    /// last (measured: 1.6 s for 20 rows, 3.8 s for 41) and returns rows we
    /// already have. Growing the target geometrically pays that rising cost
    /// O(log n) times over a session instead of once every three rows, which
    /// is what made browsing get slower the longer it went on.
    fn load_more_results(&mut self) {
        let have = self.search_results.len();
        if self.search_exhausted || have >= MAX_SEARCH_RESULTS {
            return;
        }
        self.is_loading_more = true;
        let query = self.search_query.clone();
        let total = (have * 2).max(have + SEARCH_PAGE).min(MAX_SEARCH_RESULTS);
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
                if self.thumbnail_protocols.insert(video_id.clone(), protocol).is_none() {
                    self.thumbnail_order.push_back(video_id);
                }
                self.evict_old_thumbnails();
            }
            AppMessage::ThumbnailFailed(video_id) => {
                self.thumbnails_loading.remove(&video_id);
                self.thumbnails_failed.insert(video_id);
            }
            AppMessage::NowPlayingThumbnail { video_id, image } => {
                // The track may have changed while this was in flight.
                if self.now_playing.as_ref().is_some_and(|t| t.id == video_id) {
                    let image = crate::thumbnail::crop_letterbox(image);
                    let protocol = self.picker.new_resize_protocol(image);
                    self.now_playing_thumb = Some((video_id, protocol));
                }
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
            AppMessage::AudioIdle(idle) => {
                self.audio_idle = idle;
            }
            AppMessage::AudioLevel(db) => {
                self.spectrum.set_level_db(db);
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
            AppMessage::PlaylistHead { token, mut videos, play_immediately } => {
                if token != self.playlist_token {
                    return Ok(()); // a playlist the user has already moved on from
                }
                self.clear_status();
                if videos.is_empty() {
                    self.set_error("Playlist is empty or could not be loaded.".to_string());
                    return Ok(());
                }
                if play_immediately {
                    if let Some(prev) = self.now_playing.take() {
                        self.history.push(prev);
                    }
                    self.queue.clear();
                    let first = self.take_first(&mut videos);
                    self.enqueue_all(videos);
                    if let Some(first) = first {
                        self.consecutive_failures = 0;
                        self.start_track(first).await?;
                    }
                } else {
                    self.playlist_announces_count = true;
                    let start_empty = self.now_playing.is_none() && self.queue.is_empty();
                    self.enqueue_all(videos);
                    // Nothing was playing, so the queue alone would just sit
                    // there — start it on the first track that arrived.
                    if start_empty {
                        if let Some(first) = self.queue.pop_front() {
                            self.consecutive_failures = 0;
                            self.start_track(first).await?;
                        }
                    }
                    self.set_status(format!("Added {} tracks to queue", self.queue.len()));
                }
            }
            AppMessage::PlaylistTail { token, videos } => {
                if token != self.playlist_token || videos.is_empty() {
                    return Ok(());
                }
                let queue_was_empty = self.queue.is_empty();
                self.enqueue_all(videos);
                // The head was short enough that `start_track` found nothing to
                // resolve ahead; now there is.
                if queue_was_empty {
                    self.prefetch_next();
                }
                if self.playlist_announces_count && !self.status_is_error {
                    self.set_status(format!("Added {} tracks to queue", self.queue.len()));
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
                // Nothing new came back, so YouTube has no more for this query.
                // Without this the next `j` near the bottom fires another full
                // re-search, and every one after that, forever.
                self.search_exhausted = new_results.is_empty();
                self.search_results.extend(new_results);
                self.request_playlist_meta();
            }
            AppMessage::Updated(version) => {
                self.set_status(format!("Updated to v{version} — restart to run it"));
                self.updated_to = Some(version);
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
                self.audio_idle = true;
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
                self.start_playlist_load(url, true);
                return Ok(());
            }
            self.playlist_token = self.playlist_token.wrapping_add(1);
            if let Some(prev) = self.now_playing.take() {
                self.history.push(prev);
            }
            self.queue.clear();
            self.consecutive_failures = 0;
            self.start_track(result).await?;
        }
        Ok(())
    }

    /// Start streaming a playlist into the queue, invalidating any playlist
    /// still arriving from a previous request.
    fn start_playlist_load(&mut self, url: String, play_immediately: bool) {
        self.playlist_token = self.playlist_token.wrapping_add(1);
        self.playlist_announces_count = false;
        let token = self.playlist_token;
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            if let Err(e) =
                crate::youtube::fetch_playlist_streamed(&url, token, play_immediately, tx.clone())
                    .await
            {
                let _ = tx.send(AppMessage::SearchError(e.to_string()));
            }
        });
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
                let url = result.watch_url();
                self.set_status(format!("Loading playlist \"{}\"...", &result.title.chars().take(35).collect::<String>()));
                // Always "queue": the head handler starts the first track by
                // itself when nothing is playing, and by the time the tail
                // arrives that decision has already been made correctly.
                self.start_playlist_load(url, false);
                return Ok(());
            }
            if self.now_playing.is_none() {
                // Nothing playing — start immediately without touching the queue.
                self.consecutive_failures = 0;
                self.start_track(result).await?;
            } else {
                let title = result.title.clone();
                self.enqueue(result);
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

    // ── Saved playlists ──────────────────────────────────────────────────

    pub fn selected_playlist(&self) -> Option<&SavedPlaylist> {
        self.library.playlists.get(self.library_selected)
    }

    /// Names of the saved playlists a video is already in, so the preview can
    /// say so instead of letting someone add the same track twice.
    pub fn playlists_with(&self, video_id: &str) -> Vec<&str> {
        self.library
            .playlists
            .iter()
            .filter(|p| p.contains(video_id))
            .map(|p| p.name.as_str())
            .collect()
    }

    fn toggle_library_view(&mut self) {
        self.view = match self.view {
            View::Search => View::Library,
            View::Library => View::Search,
        };
        if self.view == View::Library {
            self.library_focus = LibraryFocus::Playlists;
            self.clamp_library_selection();
        }
    }

    fn set_library_focus(&mut self, focus: LibraryFocus) {
        self.library_focus = focus;
        self.clamp_library_selection();
    }

    fn toggle_library_focus(&mut self) {
        let next = match self.library_focus {
            LibraryFocus::Playlists => LibraryFocus::Tracks,
            LibraryFocus::Tracks => LibraryFocus::Playlists,
        };
        self.set_library_focus(next);
    }

    /// Keep both library cursors pointing at something that exists. Called
    /// after anything that can shorten a list — deleting, removing a track,
    /// or moving between playlists of different lengths.
    fn clamp_library_selection(&mut self) {
        let playlists = self.library.playlists.len();
        self.library_selected = self.library_selected.min(playlists.saturating_sub(1));
        let tracks = self.selected_playlist().map(|p| p.tracks.len()).unwrap_or(0);
        self.library_track_selected = self.library_track_selected.min(tracks.saturating_sub(1));
        // Nothing to point at on the right, so the focus can't live there.
        if tracks == 0 {
            self.library_focus = LibraryFocus::Playlists;
        }
    }

    fn move_library_selection(&mut self, delta: isize) {
        let count = match self.library_focus {
            LibraryFocus::Playlists => self.library.playlists.len(),
            LibraryFocus::Tracks => self.selected_playlist().map(|p| p.tracks.len()).unwrap_or(0),
        };
        if count == 0 {
            return;
        }
        let current = match self.library_focus {
            LibraryFocus::Playlists => self.library_selected,
            LibraryFocus::Tracks => self.library_track_selected,
        };
        let next = (current as isize + delta).clamp(0, count as isize - 1) as usize;
        match self.library_focus {
            LibraryFocus::Playlists => {
                if next != self.library_selected {
                    self.library_selected = next;
                    // A different playlist entirely: start at its top rather
                    // than wherever the previous one happened to be scrolled.
                    self.library_track_selected = 0;
                }
            }
            LibraryFocus::Tracks => self.library_track_selected = next,
        }
    }

    fn reorder_selected_track(&mut self, delta: isize) {
        if self.library_focus != LibraryFocus::Tracks {
            return;
        }
        let moved = self
            .library
            .move_track(self.library_selected, self.library_track_selected, delta);
        if moved != self.library_track_selected {
            self.library_track_selected = moved;
            self.persist();
        }
    }

    /// Play (or queue) the highlighted playlist.
    ///
    /// With the focus on a track, the playlist starts from that track rather
    /// than the top — playing a playlist from the middle is the normal way to
    /// use one — and queueing takes just that track.
    async fn play_library_selection(&mut self, replace: bool) -> Result<()> {
        let Some(playlist) = self.selected_playlist() else {
            self.set_error("No saved playlists yet — press [n] to make one.");
            return Ok(());
        };
        let name = playlist.name.clone();
        let mut tracks = self.library.tracks_of(self.library_selected);

        if self.library_focus == LibraryFocus::Tracks
            && self.library_track_selected < tracks.len()
        {
            if replace {
                tracks.drain(..self.library_track_selected);
            } else {
                tracks = vec![tracks.remove(self.library_track_selected)];
            }
        }

        if tracks.is_empty() {
            self.set_error(format!(
                "\"{name}\" is empty — add tracks with [a] from the search results."
            ));
            return Ok(());
        }

        // A YouTube playlist may still be streaming into the queue; stamp its
        // token spent so the tail doesn't land behind what we start here.
        self.playlist_token = self.playlist_token.wrapping_add(1);
        let count = tracks.len();

        if replace {
            if let Some(prev) = self.now_playing.take() {
                self.history.push(prev);
            }
            self.queue.clear();
            self.consecutive_failures = 0;
            let first = self
                .take_first(&mut tracks)
                .expect("checked non-empty just above");
            self.enqueue_all(tracks);
            self.start_track(first).await?;
            self.set_status(format!("Playing \"{name}\" ({count} tracks)"));
        } else {
            let start_now = self.now_playing.is_none();
            self.enqueue_all(tracks);
            if start_now {
                if let Some(first) = self.queue.pop_front() {
                    self.consecutive_failures = 0;
                    self.start_track(first).await?;
                }
            }
            self.prefetch_next();
            self.set_status(format!("Queued {count} track(s) from \"{name}\""));
        }
        Ok(())
    }

    /// Offer to save the highlighted search result to a playlist.
    fn add_selected_to_playlist(&mut self) {
        let Some(result) = self.search_results.get(self.selected_index).cloned() else {
            return;
        };
        if result.is_playlist {
            // A playlist row is an id, not tracks — they only exist here once
            // yt-dlp has walked it, which queueing already does.
            self.set_error(
                "That's a YouTube playlist — queue it with [f], then press [A] to save the queue.",
            );
            return;
        }
        self.offer_playlists(vec![result]);
    }

    /// Save what is playing plus everything queued behind it as a playlist.
    /// This is also how a YouTube playlist becomes a local one.
    fn prompt_save_queue(&mut self) {
        let tracks: Vec<VideoResult> = self
            .now_playing
            .iter()
            .chain(self.queue.iter())
            .cloned()
            .collect();
        if tracks.is_empty() {
            self.set_error("Nothing playing or queued to save.");
            return;
        }
        // Straight to naming rather than the picker: a whole queue is a new
        // playlist, not a handful of tracks to fold into an existing one.
        self.pending_tracks = tracks;
        self.begin_naming(NameTarget::Create, String::new());
    }

    /// Ask which playlist the pending tracks belong in — unless there are no
    /// playlists yet, in which case the only useful answer is a new one.
    fn offer_playlists(&mut self, tracks: Vec<VideoResult>) {
        self.pending_tracks = tracks;
        if self.library.playlists.is_empty() {
            self.begin_naming(NameTarget::Create, String::new());
        } else {
            self.pick_selected = self.library_selected.min(self.library.playlists.len() - 1);
            self.mode = AppMode::PickingPlaylist;
        }
    }

    fn prompt_new_playlist(&mut self) {
        self.pending_tracks.clear();
        self.begin_naming(NameTarget::Create, String::new());
    }

    fn prompt_rename_playlist(&mut self) {
        let Some(playlist) = self.selected_playlist() else {
            return;
        };
        let name = playlist.name.clone();
        self.begin_naming(NameTarget::Rename(self.library_selected), name);
    }

    fn begin_naming(&mut self, target: NameTarget, initial: String) {
        self.name_input = TextInput::with_value(initial);
        self.name_target = Some(target);
        self.mode = AppMode::Naming;
    }

    fn handle_name_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Esc => {
                self.mode = AppMode::Normal;
                self.name_target = None;
                self.pending_tracks.clear();
            }
            KeyCode::Enter => {
                // A nameless playlist is unusable in a list of names, so an
                // empty prompt simply doesn't submit.
                if self.name_input.is_empty() {
                    return;
                }
                let name = self.name_input.value.trim().to_string();
                match self.name_target.take() {
                    Some(NameTarget::Create) => {
                        let index = self.library.create(&name);
                        let added = self.add_pending_to(index);
                        self.library_selected = index;
                        self.library_track_selected = 0;
                        self.persist();
                        let created = self.library.playlists[index].name.clone();
                        if added > 0 {
                            self.set_status(format!("Saved {added} track(s) to \"{created}\""));
                        } else {
                            self.set_status(format!("Created playlist \"{created}\""));
                        }
                    }
                    Some(NameTarget::Rename(index)) => {
                        self.library.rename(index, &name);
                        self.persist();
                        if let Some(playlist) = self.library.playlists.get(index) {
                            self.set_status(format!("Renamed to \"{}\"", playlist.name));
                        }
                    }
                    None => {}
                }
                self.mode = AppMode::Normal;
            }
            other => {
                self.name_input.handle_key(other);
            }
        }
    }

    fn handle_pick_key(&mut self, key: KeyCode) {
        let count = self.library.playlists.len();
        match key {
            KeyCode::Esc => {
                self.mode = AppMode::Normal;
                self.pending_tracks.clear();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.pick_selected = self.pick_selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                // `count` itself is the "new playlist" row at the bottom.
                self.pick_selected = (self.pick_selected + 1).min(count);
            }
            KeyCode::Char('n') => {
                self.begin_naming(NameTarget::Create, String::new());
            }
            KeyCode::Enter => {
                if self.pick_selected >= count {
                    self.begin_naming(NameTarget::Create, String::new());
                    return;
                }
                let index = self.pick_selected;
                let added = self.add_pending_to(index);
                self.persist();
                let playlist = &self.library.playlists[index];
                let (name, total) = (playlist.name.clone(), playlist.tracks.len());
                if added > 0 {
                    self.set_status(format!("Added to \"{name}\" ({total} tracks)"));
                } else {
                    self.set_status(format!("Already in \"{name}\""));
                }
                self.mode = AppMode::Normal;
            }
            _ => {}
        }
    }

    /// Move whatever is waiting in `pending_tracks` into playlist `index`,
    /// returning how many of them were actually new to it.
    fn add_pending_to(&mut self, index: usize) -> usize {
        let tracks = std::mem::take(&mut self.pending_tracks);
        tracks
            .iter()
            .filter(|track| self.library.add_track(index, track))
            .count()
    }

    fn prompt_delete(&mut self) {
        let target = match self.library_focus {
            LibraryFocus::Playlists => self
                .selected_playlist()
                .map(|_| DeleteTarget::Playlist(self.library_selected)),
            LibraryFocus::Tracks => self
                .selected_playlist()
                .filter(|p| self.library_track_selected < p.tracks.len())
                .map(|_| DeleteTarget::Track(self.library_selected, self.library_track_selected)),
        };
        if target.is_some() {
            self.delete_target = target;
            self.mode = AppMode::ConfirmingDelete;
        }
    }

    fn handle_delete_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                match self.delete_target.take() {
                    Some(DeleteTarget::Playlist(index)) => {
                        if let Some(removed) = self.library.remove(index) {
                            self.persist();
                            self.set_status(format!("Deleted playlist \"{}\"", removed.name));
                        }
                    }
                    Some(DeleteTarget::Track(index, track)) => {
                        self.library.remove_track(index, track);
                        self.persist();
                        self.set_status("Removed track from playlist".to_string());
                    }
                    None => {}
                }
                self.clamp_library_selection();
                self.mode = AppMode::Normal;
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.delete_target = None;
                self.mode = AppMode::Normal;
            }
            _ => {}
        }
    }

    /// Scroll the help overlay, or close it.
    ///
    /// Anything that isn't a scroll closes it — someone who opened the
    /// reference by accident shouldn't have to work out how to leave, and
    /// there is nothing in here a stray key could damage.
    fn handle_help_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Down | KeyCode::Char('j') => {
                self.help_scroll = self.help_scroll.saturating_add(1)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.help_scroll = self.help_scroll.saturating_sub(1)
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                self.help_scroll = self.help_scroll.saturating_add(10)
            }
            KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(10),
            KeyCode::Home => self.help_scroll = 0,
            // Past the end; the render clamps it to the real last line.
            KeyCode::End => self.help_scroll = u16::MAX,
            _ => self.mode = AppMode::Normal,
        }
    }

    /// Write the library out, surfacing a failure rather than losing the
    /// change quietly — the whole point of a saved playlist is that it is
    /// still there next time.
    fn persist(&mut self) {
        if let Err(e) = self.library.save() {
            crate::logline!("library: save failed: {e}");
            self.set_error(format!("Could not save playlists: {e}"));
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui_image::picker::Picker;

    fn track(i: usize) -> VideoResult {
        VideoResult {
            id: format!("v{i}"),
            title: format!("track {i}"),
            url: Some(format!("https://www.youtube.com/watch?v=v{i}")),
            duration: Some(180.0),
            view_count: None,
            channel: None,
            uploader: None,
            thumbnail: None,
            is_playlist: false,
            playlist_count: None,
        }
    }

    fn app() -> App {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        // Keep the receiver alive; dropping it makes every send fail.
        std::mem::forget(rx);
        let mut app = App::new(tx, Picker::from_fontsize((8, 12)), false);
        // `App::new` loads the real user library; point every test at a
        // scratch file of its own so saving can never reach someone's
        // actual playlists, and two tests can't fight over one file.
        app.library = Library::at(scratch_library());
        app
    }

    fn scratch_library() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("listen_to_it_test_app_{}_{n}.json", std::process::id()))
    }

    fn key(c: char) -> Event {
        Event::Key(crossterm::event::KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    fn press(code: KeyCode) -> Event {
        Event::Key(crossterm::event::KeyEvent::new(code, KeyModifiers::NONE))
    }

    async fn type_name(app: &mut App, name: &str) {
        for c in name.chars() {
            app.handle_event(key(c)).await.unwrap();
        }
        app.handle_event(press(KeyCode::Enter)).await.unwrap();
    }

    /// The headline flow: pick a video out of the search results, name a
    /// playlist for it, and find the track in it afterwards.
    #[tokio::test]
    async fn a_search_result_is_saved_into_a_named_playlist() {
        let mut app = app();
        app.search_results = vec![track(1), track(2)];
        app.selected_index = 1;

        app.handle_event(key('a')).await.unwrap();
        // Nothing to choose between yet, so it goes straight to naming.
        assert!(app.mode == AppMode::Naming);

        type_name(&mut app, "Road trip").await;

        assert!(app.mode == AppMode::Normal);
        assert_eq!(app.library.playlists.len(), 1);
        assert_eq!(app.library.playlists[0].name, "Road trip");
        let ids: Vec<&str> =
            app.library.playlists[0].tracks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["v2"], "the highlighted row is the one that gets saved");
    }

    /// With playlists already there, the track has to land in the one the
    /// user picks rather than the first or the last.
    #[tokio::test]
    async fn a_track_lands_in_the_chosen_playlist() {
        let mut app = app();
        app.library.create("Focus");
        app.library.create("Gym");
        app.search_results = vec![track(7)];

        app.handle_event(key('a')).await.unwrap();
        assert!(app.mode == AppMode::PickingPlaylist);
        app.handle_event(key('j')).await.unwrap(); // down to "Gym"
        app.handle_event(press(KeyCode::Enter)).await.unwrap();

        assert!(app.library.playlists[0].tracks.is_empty());
        assert_eq!(app.library.playlists[1].tracks.len(), 1);
        assert!(app.mode == AppMode::Normal);
    }

    /// A saved playlist plays without going back to YouTube for anything: the
    /// first track starts and the rest are queued in order behind it.
    #[tokio::test]
    async fn a_saved_playlist_plays_in_order() {
        let mut app = app();
        let idx = app.library.create("Focus");
        for i in 1..=3 {
            app.library.add_track(idx, &track(i));
        }
        app.view = View::Library;

        app.handle_event(press(KeyCode::Enter)).await.unwrap();

        assert_eq!(app.now_playing.as_ref().unwrap().id, "v1");
        let queued: Vec<&str> = app.queue.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(queued, ["v2", "v3"]);
    }

    /// Playing a playlist from the middle is the normal way to use one, so
    /// Enter over a track starts there rather than at the top.
    #[tokio::test]
    async fn enter_on_a_track_starts_the_playlist_there() {
        let mut app = app();
        let idx = app.library.create("Focus");
        for i in 1..=3 {
            app.library.add_track(idx, &track(i));
        }
        app.view = View::Library;
        app.library_focus = LibraryFocus::Tracks;
        app.library_track_selected = 1;

        app.handle_event(press(KeyCode::Enter)).await.unwrap();

        assert_eq!(app.now_playing.as_ref().unwrap().id, "v2");
        let queued: Vec<&str> = app.queue.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(queued, ["v3"], "the tracks before the one picked are skipped, not queued");
    }

    /// Whatever is playing leads the playlist the queue is saved into —
    /// this is also how a YouTube playlist becomes a local one.
    #[tokio::test]
    async fn the_queue_can_be_saved_as_a_playlist() {
        let mut app = app();
        app.now_playing = Some(track(1));
        app.queue.push_back(track(2));
        app.queue.push_back(track(3));

        app.handle_event(key('A')).await.unwrap();
        assert!(app.mode == AppMode::Naming);
        type_name(&mut app, "Set").await;

        let ids: Vec<&str> =
            app.library.playlists[0].tracks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["v1", "v2", "v3"]);
    }

    /// Deleting shortens the lists the two cursors point into; neither may be
    /// left hanging past the end, and the focus can't stay on a pane that no
    /// longer has anything in it.
    #[tokio::test]
    async fn deleting_leaves_both_cursors_valid() {
        let mut app = app();
        app.library.create("one");
        let idx = app.library.create("two");
        app.library.add_track(idx, &track(1));

        app.view = View::Library;
        app.library_selected = 1;
        app.library_focus = LibraryFocus::Tracks;

        app.handle_event(key('x')).await.unwrap();
        app.handle_event(key('y')).await.unwrap();
        assert!(app.library.playlists[1].tracks.is_empty());
        assert!(
            app.library_focus == LibraryFocus::Playlists,
            "the focus cannot stay on an empty track list"
        );

        app.handle_event(key('x')).await.unwrap();
        app.handle_event(key('y')).await.unwrap();
        assert_eq!(app.library.playlists.len(), 1);
        assert_eq!(app.library_selected, 0, "the cursor must not point past the end");
    }

    /// A playlist name is typed into the same kind of field as a search, so
    /// the two must not bleed into each other.
    #[tokio::test]
    async fn naming_a_playlist_does_not_disturb_the_search_box() {
        let mut app = app();
        app.search.value = "miles davis".to_string();
        app.search.cursor = app.search.len();
        app.view = View::Library;

        app.handle_event(key('n')).await.unwrap();
        type_name(&mut app, "Jazz").await;

        assert_eq!(app.search.value, "miles davis");
        assert_eq!(app.library.playlists[0].name, "Jazz");
    }

    /// The help overlay is a reference, not a mode to get stuck in: it opens
    /// from either view, and anything that isn't a scroll leaves it again.
    #[tokio::test]
    async fn help_opens_anywhere_and_closes_on_any_key() {
        let mut app = app();

        app.handle_event(key('?')).await.unwrap();
        assert!(app.mode == AppMode::Help);

        // 'q' quits from normal mode; over the help it only closes the help.
        let quit = app.handle_event(key('q')).await.unwrap();
        assert!(!quit, "a key pressed over the help must not quit the app");
        assert!(app.mode == AppMode::Normal);

        app.view = View::Library;
        app.handle_event(press(KeyCode::F(1))).await.unwrap();
        assert!(app.mode == AppMode::Help, "the help is reachable from the playlists too");
    }

    /// The top of the help is clamped here; the bottom is clamped by the
    /// render, which is the only place that knows how many lines fit.
    #[tokio::test]
    async fn help_scrolling_stops_at_the_top() {
        let mut app = app();
        app.handle_event(key('?')).await.unwrap();

        app.handle_event(key('k')).await.unwrap();
        assert_eq!(app.help_scroll, 0, "already at the first line");

        app.handle_event(key('j')).await.unwrap();
        assert_eq!(app.help_scroll, 1);
        assert!(app.mode == AppMode::Help, "scrolling doesn't close it");

        app.handle_event(press(KeyCode::Home)).await.unwrap();
        assert_eq!(app.help_scroll, 0);
    }

    /// A playlist arrives in pieces, so the tail has to land behind the head in
    /// the queue rather than replacing it or being dropped.
    #[tokio::test]
    async fn playlist_tail_appends_behind_the_head() {
        let mut app = app();
        app.now_playing = Some(track(99)); // something already playing
        let token = app.playlist_token;

        app.handle_message(AppMessage::PlaylistHead {
            token,
            videos: vec![track(1), track(2)],
            play_immediately: false,
        })
        .await
        .unwrap();
        assert_eq!(app.queue.len(), 2);

        app.handle_message(AppMessage::PlaylistTail { token, videos: vec![track(3)] })
            .await
            .unwrap();

        let ids: Vec<&str> = app.queue.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(ids, ["v1", "v2", "v3"]);
        assert_eq!(app.status_message.as_deref(), Some("Added 3 tracks to queue"));
    }

    /// The tail of a 5000-track playlist can still be arriving seconds after
    /// the user has moved on. Those batches must not pile into the new queue.
    #[tokio::test]
    async fn a_stale_playlist_tail_is_dropped() {
        let mut app = app();
        app.now_playing = Some(track(99));
        let old_token = app.playlist_token;

        app.handle_message(AppMessage::PlaylistHead {
            token: old_token,
            videos: vec![track(1)],
            play_immediately: false,
        })
        .await
        .unwrap();

        // The user queues something else, which takes over the queue.
        app.start_playlist_load("https://example.invalid/list".to_string(), false);
        app.queue.clear();

        app.handle_message(AppMessage::PlaylistTail {
            token: old_token,
            videos: vec![track(2), track(3)],
        })
        .await
        .unwrap();

        assert!(app.queue.is_empty(), "tail from the abandoned playlist leaked in");
    }

    /// Browsing a long result list used to hold every thumbnail it ever
    /// decoded — roughly 0.66 MB apiece.
    #[tokio::test]
    async fn thumbnail_cache_stays_bounded() {
        let mut app = app();
        for i in 0..MAX_CACHED_THUMBNAILS * 3 {
            let id = format!("v{i}");
            let image = image::DynamicImage::new_rgb8(4, 4);
            app.handle_message(AppMessage::ThumbnailLoaded { video_id: id, image })
                .await
                .unwrap();
        }
        assert_eq!(app.thumbnail_protocols.len(), MAX_CACHED_THUMBNAILS);
        assert_eq!(app.thumbnail_order.len(), MAX_CACHED_THUMBNAILS);
    }

    /// Once YouTube has no more results, asking again costs a full re-search
    /// and returns nothing — it must not be retried on every keypress.
    /// While mpv opens or stalls on a stream it still reports a position,
    /// which used to make the bars twitch between playing and not. They
    /// follow mpv's own idea of whether sound is coming out instead.
    #[tokio::test]
    async fn the_bars_wait_for_mpv_to_actually_play() {
        let mut app = app();
        app.now_playing = Some(track(1));
        app.handle_message(AppMessage::Position(0.0)).await.unwrap();
        assert!(!app.is_audible(), "a position alone isn't sound");

        app.handle_message(AppMessage::AudioIdle(false)).await.unwrap();
        assert!(app.is_audible());

        app.handle_message(AppMessage::AudioIdle(true)).await.unwrap();
        assert!(!app.is_audible(), "a stall silences the bars");
    }

    #[tokio::test]
    async fn exhausted_search_stops_reloading() {
        let mut app = app();
        app.search_query = "anything".to_string();
        app.search_results = (0..5).map(track).collect();

        app.handle_message(AppMessage::MoreResults((0..5).map(track).collect()))
            .await
            .unwrap();
        assert!(app.search_exhausted, "no new ids came back, so the query is spent");

        app.load_more_results();
        assert!(!app.is_loading_more, "a spent query must not fire another search");
    }
}
