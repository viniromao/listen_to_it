use crate::youtube::VideoResult;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// One track as it is stored on disk.
///
/// Deliberately its own type rather than a serialized `VideoResult`: the file
/// on disk outlives any one version of the app, so the format shouldn't drift
/// every time an internal field is added. Everything here is enough to play
/// the track again without asking YouTube what it is.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedTrack {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
}

impl From<&VideoResult> for SavedTrack {
    fn from(v: &VideoResult) -> Self {
        Self {
            id: v.id.clone(),
            title: v.title.clone(),
            url: v.url.clone(),
            duration: v.duration,
            channel: v.channel.clone().or_else(|| v.uploader.clone()),
            thumbnail: v.thumbnail.clone(),
        }
    }
}

impl From<&SavedTrack> for VideoResult {
    fn from(t: &SavedTrack) -> Self {
        Self {
            id: t.id.clone(),
            title: t.title.clone(),
            url: t.url.clone(),
            duration: t.duration,
            view_count: None,
            channel: t.channel.clone(),
            uploader: None,
            thumbnail: t.thumbnail.clone(),
            is_playlist: false,
            playlist_count: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedPlaylist {
    pub name: String,
    /// Unix seconds, used only to order new playlists sensibly in the list.
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub tracks: Vec<SavedTrack>,
}

impl SavedPlaylist {
    /// Total runtime, in seconds, of the tracks that know their own duration.
    pub fn total_duration(&self) -> f64 {
        self.tracks.iter().filter_map(|t| t.duration).sum()
    }

    pub fn contains(&self, video_id: &str) -> bool {
        self.tracks.iter().any(|t| t.id == video_id)
    }
}

/// Every playlist the user has saved, plus where it came from on disk.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Library {
    #[serde(default)]
    pub playlists: Vec<SavedPlaylist>,

    /// Where `save` writes. `None` when neither `XDG_DATA_HOME` nor `HOME` is
    /// set, in which case saving reports an error rather than quietly
    /// pretending to have worked.
    #[serde(skip)]
    path: Option<PathBuf>,
}

/// Saved playlists are the user's own data, not a cache: they live under
/// `XDG_DATA_HOME` (`~/.local/share`), not the `~/.cache/listen_to_it`
/// directory the managed yt-dlp binary and the debug log use, so clearing
/// caches can't take someone's playlists with it.
fn data_file() -> Result<PathBuf> {
    let dir = match std::env::var_os("XDG_DATA_HOME") {
        Some(base) if !base.is_empty() => PathBuf::from(base),
        _ => PathBuf::from(std::env::var("HOME").context("neither XDG_DATA_HOME nor HOME is set")?)
            .join(".local")
            .join("share"),
    };
    Ok(dir.join("listen_to_it").join("playlists.json"))
}

impl Library {
    /// Read the saved playlists, or start empty if there are none yet.
    ///
    /// Never fails: a library that can't be read must not stop the player
    /// from starting. A file that exists but doesn't parse is moved aside
    /// rather than left in place, so the first save can't silently overwrite
    /// something the user might still want to recover.
    pub fn load() -> Self {
        let path = match data_file() {
            Ok(p) => p,
            Err(e) => {
                crate::logline!("library: no data directory ({e}); playlists will not persist");
                return Self::default();
            }
        };

        let playlists = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Library>(&text) {
                Ok(lib) => lib.playlists,
                Err(e) => {
                    let backup = path.with_extension("json.bak");
                    let _ = std::fs::rename(&path, &backup);
                    crate::logline!(
                        "library: {} is not readable ({e}); moved to {}",
                        path.display(),
                        backup.display()
                    );
                    Vec::new()
                }
            },
            // Not there yet is the normal first-run case, not a problem.
            Err(_) => Vec::new(),
        };

        crate::logline!("library: loaded {} playlist(s) from {}", playlists.len(), path.display());
        Self { playlists, path: Some(path) }
    }

    /// A library backed by an explicit file, for tests.
    #[cfg(test)]
    pub fn at(path: PathBuf) -> Self {
        Self { playlists: Vec::new(), path: Some(path) }
    }

    /// Write the library out, replacing the previous file atomically.
    ///
    /// Written to a sibling temp file and renamed into place: a crash or a
    /// full disk halfway through then leaves the previous playlists intact
    /// instead of a half-written file that won't parse.
    pub fn save(&self) -> Result<()> {
        let path = self.path.as_ref().context("no writable data directory")?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).with_context(|| format!("could not write {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("could not replace {}", path.display()))?;
        Ok(())
    }

    /// Create an empty playlist and return its index.
    pub fn create(&mut self, name: &str) -> usize {
        let name = self.unique_name(name.trim(), None);
        self.playlists.push(SavedPlaylist {
            name,
            created_at: now_secs(),
            tracks: Vec::new(),
        });
        self.playlists.len() - 1
    }

    pub fn remove(&mut self, index: usize) -> Option<SavedPlaylist> {
        (index < self.playlists.len()).then(|| self.playlists.remove(index))
    }

    pub fn rename(&mut self, index: usize, name: &str) {
        let name = self.unique_name(name.trim(), Some(index));
        if let Some(p) = self.playlists.get_mut(index) {
            p.name = name;
        }
    }

    /// Append a track. Returns false if that video is already in the
    /// playlist — adding the same song twice by mistake is far more likely
    /// than doing it on purpose.
    pub fn add_track(&mut self, index: usize, track: &VideoResult) -> bool {
        let Some(playlist) = self.playlists.get_mut(index) else {
            return false;
        };
        if playlist.contains(&track.id) {
            return false;
        }
        playlist.tracks.push(SavedTrack::from(track));
        true
    }

    pub fn remove_track(&mut self, index: usize, track_index: usize) {
        if let Some(p) = self.playlists.get_mut(index) {
            if track_index < p.tracks.len() {
                p.tracks.remove(track_index);
            }
        }
    }

    /// Move a track one place up (`-1`) or down (`1`), returning where it
    /// ended up so the selection can follow it.
    pub fn move_track(&mut self, index: usize, track_index: usize, delta: isize) -> usize {
        let Some(p) = self.playlists.get_mut(index) else {
            return track_index;
        };
        let target = track_index as isize + delta;
        if target < 0 || target >= p.tracks.len() as isize || track_index >= p.tracks.len() {
            return track_index;
        }
        let target = target as usize;
        p.tracks.swap(track_index, target);
        target
    }

    /// The playlist's tracks, ready to hand to the queue.
    pub fn tracks_of(&self, index: usize) -> Vec<VideoResult> {
        self.playlists
            .get(index)
            .map(|p| p.tracks.iter().map(VideoResult::from).collect())
            .unwrap_or_default()
    }

    /// A name no other playlist is using, so two playlists can never be told
    /// apart only by their position in the list. `skip` is the playlist being
    /// renamed, which doesn't count as a clash with itself.
    fn unique_name(&self, name: &str, skip: Option<usize>) -> String {
        let base = if name.is_empty() { "Untitled" } else { name };
        let taken = |candidate: &str| {
            self.playlists
                .iter()
                .enumerate()
                .any(|(i, p)| Some(i) != skip && p.name == candidate)
        };
        if !taken(base) {
            return base.to_string();
        }
        (2..)
            .map(|n| format!("{base} ({n})"))
            .find(|candidate| !taken(candidate))
            .expect("an unused suffix always exists")
    }
}

/// What a delete confirmation will remove if it is accepted.
pub enum DeleteTarget {
    Playlist(usize),
    /// `(playlist index, track index)`.
    Track(usize, usize),
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(id: &str) -> VideoResult {
        VideoResult {
            id: id.to_string(),
            title: format!("song {id}"),
            url: Some(format!("https://www.youtube.com/watch?v={id}")),
            duration: Some(120.0),
            view_count: Some(99),
            channel: Some("a channel".to_string()),
            uploader: None,
            thumbnail: None,
            is_playlist: false,
            playlist_count: None,
        }
    }

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("listen_to_it_test_{tag}_{}.json", std::process::id()))
    }

    /// What is written has to come back as something playable — the point of
    /// the whole feature is that a saved playlist plays without re-searching.
    #[test]
    fn round_trips_through_disk() {
        let path = temp_path("roundtrip");
        let _ = std::fs::remove_file(&path);

        let mut lib = Library::at(path.clone());
        let idx = lib.create("Road trip");
        assert!(lib.add_track(idx, &video("aaa")));
        assert!(lib.add_track(idx, &video("bbb")));
        lib.save().unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let reloaded: Library = serde_json::from_str(&text).unwrap();
        assert_eq!(reloaded.playlists.len(), 1);
        assert_eq!(reloaded.playlists[0].name, "Road trip");

        let tracks = reloaded.tracks_of(0);
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].watch_url(), "https://www.youtube.com/watch?v=aaa");
        assert_eq!(reloaded.playlists[0].total_duration(), 240.0);

        let _ = std::fs::remove_file(&path);
    }

    /// Two playlists called the same thing are indistinguishable in the list,
    /// so the second one gets a suffix instead.
    #[test]
    fn names_are_kept_unique() {
        let mut lib = Library::default();
        lib.create("Focus");
        lib.create("Focus");
        lib.create("Focus");
        let names: Vec<&str> = lib.playlists.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Focus", "Focus (2)", "Focus (3)"]);

        // Renaming a playlist to what it is already called is not a clash.
        lib.rename(0, "Focus");
        assert_eq!(lib.playlists[0].name, "Focus");
    }

    #[test]
    fn the_same_video_is_not_added_twice() {
        let mut lib = Library::default();
        let idx = lib.create("Dupes");
        assert!(lib.add_track(idx, &video("aaa")));
        assert!(!lib.add_track(idx, &video("aaa")));
        assert_eq!(lib.playlists[idx].tracks.len(), 1);
    }

    #[test]
    fn tracks_reorder_and_the_selection_follows() {
        let mut lib = Library::default();
        let idx = lib.create("Order");
        for id in ["a", "b", "c"] {
            lib.add_track(idx, &video(id));
        }

        assert_eq!(lib.move_track(idx, 2, 1), 2, "the last track cannot move down");
        assert_eq!(lib.move_track(idx, 0, -1), 0, "the first track cannot move up");

        assert_eq!(lib.move_track(idx, 0, 1), 1);
        let ids: Vec<&str> = lib.playlists[idx].tracks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["b", "a", "c"]);
    }

    /// A file that doesn't parse must not be overwritten by the first save
    /// of the session — the user's playlists are in there somewhere.
    #[test]
    fn unreadable_file_is_moved_aside_not_clobbered() {
        let home = std::env::temp_dir().join(format!("listen_to_it_data_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("listen_to_it")).unwrap();
        let file = home.join("listen_to_it").join("playlists.json");
        std::fs::write(&file, "{ this is not json").unwrap();

        // The only test that touches this variable, so pointing it at a temp
        // directory cannot disturb the others running alongside it.
        std::env::set_var("XDG_DATA_HOME", &home);
        let mut lib = Library::load();
        assert!(lib.playlists.is_empty(), "a broken file starts an empty library");

        let backup = file.with_extension("json.bak");
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            "{ this is not json",
            "the unreadable file has to survive somewhere"
        );

        // And the fresh library is still writable over the same path.
        lib.create("Recovered");
        lib.save().unwrap();
        assert!(std::fs::read_to_string(&file).unwrap().contains("Recovered"));

        std::env::remove_var("XDG_DATA_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }
}
