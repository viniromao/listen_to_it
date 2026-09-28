use anyhow::{Context, Result};
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

use crate::app::AppMessage;

const SOCKET_PATH: &str = "/tmp/listen_to_it_mpv.sock";

/// Label of the loudness-measuring filter, which names its metadata property.
const LEVEL_FILTER: &str = "level";

enum PlayerCmd {
    Play(PlayRequest),
    TogglePause,
    SetVolume(i32),
    SeekAbs(f64),
    Stop,
    Quit,
    /// A line mpv pushed over the IPC socket, forwarded by the event reader.
    Event(serde_json::Value),
}

/// A media URL that is ready to play, with the request headers it was issued
/// against. Resolution happens in [`crate::stream`]; by the time it reaches
/// the player thread there is nothing left to look up.
struct PlayRequest {
    url: String,
    headers: Vec<(String, String)>,
}

pub struct Player {
    tx: Option<mpsc::SyncSender<PlayerCmd>>,
}

impl Player {
    pub fn new(event_tx: UnboundedSender<AppMessage>) -> Self {
        let (tx, rx) = mpsc::sync_channel::<PlayerCmd>(32);
        let self_tx = tx.clone();
        std::thread::Builder::new()
            .name("audio".into())
            .spawn(move || player_thread(rx, self_tx, event_tx))
            .expect("failed to spawn audio thread");
        Self { tx: Some(tx) }
    }

    pub async fn play(&self, stream: &crate::stream::Stream) -> Result<()> {
        self.send(PlayerCmd::Play(PlayRequest {
            url: stream.media_url.clone(),
            headers: stream.headers.clone(),
        }));
        Ok(())
    }

    pub async fn toggle_pause(&self) -> Result<()> {
        self.send(PlayerCmd::TogglePause);
        Ok(())
    }

    pub async fn set_volume(&self, volume: i32) -> Result<()> {
        self.send(PlayerCmd::SetVolume(volume));
        Ok(())
    }

    pub async fn seek_abs(&self, seconds: f64) -> Result<()> {
        self.send(PlayerCmd::SeekAbs(seconds.max(0.0)));
        Ok(())
    }

    pub async fn stop(&mut self) {
        self.send(PlayerCmd::Stop);
    }

    fn send(&self, cmd: PlayerCmd) {
        if let Some(ref tx) = self.tx {
            let _ = tx.send(cmd);
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.send(PlayerCmd::Quit);
    }
}

// ── Player thread ─────────────────────────────────────────────────────────────

/// One mpv for the whole session, idling between tracks and handed each new
/// one with `loadfile`. Restarting mpv per track made every track open a
/// fresh audio output, and on macOS that came back quieter than the system
/// volume until the volume was touched again; a long-lived mpv keeps the
/// same output open across tracks.
struct MpvProcess {
    child: Child,
    /// Connection that sends commands and receives mpv's events. Commands on
    /// one connection run in order, so a track's headers are set before its
    /// `loadfile` runs.
    control: UnixStream,
    /// Most useful line mpv logged (stdout or stderr) for the current track,
    /// kept updated by background reader threads so a failure can be reported
    /// with a real reason instead of mpv's generic "loading failed". Locks
    /// onto the first line naming an actual error rather than always tracking
    /// the very last line, since mpv's housekeeping lines that follow are
    /// generic and would otherwise clobber it. Cleared for each track.
    ///
    /// Note: `--no-terminal` doesn't just hide the interactive status line —
    /// it silences mpv's logging entirely (verified empirically: with it set,
    /// a failing URL exits non-zero with *nothing* on stdout or stderr). So
    /// terminal mode is left enabled here; that's safe because stdin/stdout
    /// are redirected away from our real tty (Stdio::null / Stdio::piped),
    /// so mpv sees a non-terminal and never tries to take over the console.
    /// What log output there is lands on stdout, not stderr.
    last_output: Arc<Mutex<LastOutput>>,
}

#[derive(Default)]
struct LastOutput {
    text: String,
    is_specific: bool,
}

impl MpvProcess {
    /// Start an idle mpv and connect to it. Its events come back through
    /// `self_tx` as [`PlayerCmd::Event`].
    fn spawn(self_tx: mpsc::SyncSender<PlayerCmd>) -> Result<Self> {
        let _ = std::fs::remove_file(SOCKET_PATH);
        let mut cmd = Command::new("mpv");
        cmd.args([
            "--no-video",
            "--quiet",
            // Stay alive with nothing loaded, waiting for the next `loadfile`.
            "--idle=yes",
            // The URL is already a direct media URL — letting ytdl_hook run
            // would send it back through yt-dlp for nothing, which is exactly
            // the ~2.5 s of startup latency resolving it ourselves avoids.
            "--no-ytdl",
            &format!("--input-ipc-server={SOCKET_PATH}"),
            // Measure how loud the track is, for the spectrum bars. Appended so
            // any filters in the user's mpv.conf still apply. It sits in
            // the filter chain ahead of mpv's own volume, so it reads the
            // music itself: a quiet passage reads quiet whatever the volume
            // is set to. Stats reset every 4 frames (~85 ms at 48 kHz), about
            // one poll's worth.
            &format!(
                "--af-append=@{LEVEL_FILTER}:lavfi=[astats=metadata=1:reset=4:measure_perchannel=none:measure_overall=RMS_level]"
            ),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

        let mut child = cmd.spawn().context("failed to spawn mpv")?;
        let last_output = Arc::new(Mutex::new(LastOutput::default()));
        for stream in [
            child.stdout.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
            child.stderr.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let last_output = last_output.clone();
            std::thread::Builder::new()
                .name("mpv-log".into())
                .spawn(move || {
                    for line in BufReader::new(stream).lines().map_while(Result::ok) {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        if let Ok(mut guard) = last_output.lock() {
                            // Once a line that actually names the problem shows up
                            // (e.g. an "ERROR: ..." line), lock it in — don't let
                            // mpv's generic housekeeping lines that follow
                            // ("Errors when loading file") overwrite it with
                            // something less specific.
                            if !guard.is_specific {
                                if trimmed.contains("ERROR") {
                                    guard.is_specific = true;
                                }
                                guard.text = trimmed.to_string();
                            }
                        }
                    }
                })
                .expect("failed to spawn mpv-log reader thread");
        }

        let mut proc = Self {
            child,
            // Placeholder until the socket is up; replaced below.
            control: UnixStream::pair()?.0,
            last_output,
        };
        if !wait_for_socket(Duration::from_secs(5)) {
            proc.kill();
            anyhow::bail!("mpv socket timeout");
        }
        proc.control = UnixStream::connect(SOCKET_PATH).context("failed to connect to mpv")?;
        let events = proc.control.try_clone()?;
        std::thread::Builder::new()
            .name("mpv-events".into())
            .spawn(move || {
                for line in BufReader::new(events).lines().map_while(Result::ok) {
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                        continue;
                    };
                    // Replies to our own commands carry no event; only events matter.
                    if v.get("event").is_some() && self_tx.send(PlayerCmd::Event(v)).is_err() {
                        break;
                    }
                }
            })
            .expect("failed to spawn mpv-events reader thread");
        Ok(proc)
    }

    fn command(&mut self, cmd: serde_json::Value) {
        let mut msg = json!({ "command": cmd }).to_string();
        msg.push('\n');
        let _ = self.control.write_all(msg.as_bytes());
    }

    /// Replace whatever is playing with `req`, unpaused. The headers are
    /// options rather than per-file arguments, and each `loadfile` reads
    /// whatever they are set to at that moment.
    fn load(&mut self, req: &PlayRequest, volume: i32) {
        if let Ok(mut guard) = self.last_output.lock() {
            *guard = LastOutput::default();
        }
        // Replay the headers yt-dlp used. The User-Agent has its own option;
        // the rest go in the header list as a JSON array, since values can
        // contain commas and the option's string form is comma-separated.
        let mut fields = Vec::new();
        for (name, value) in &req.headers {
            if name.eq_ignore_ascii_case("user-agent") {
                self.command(json!(["set_property", "user-agent", value]));
            } else {
                fields.push(format!("{name}: {value}"));
            }
        }
        self.command(json!(["set_property", "http-header-fields", fields]));
        self.command(json!(["set_property", "pause", false]));
        self.command(json!(["set_property", "volume", volume]));
        self.command(json!(["loadfile", req.url, "replace"]));
    }

    fn error_detail(&self, file_error: Option<&str>) -> String {
        let logged = self
            .last_output
            .lock()
            .ok()
            .map(|s| s.text.clone())
            .unwrap_or_default();
        if !logged.is_empty() {
            logged
        } else {
            file_error
                .unwrap_or("mpv could not play the stream")
                .to_string()
        }
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(SOCKET_PATH);
    }
}

/// Ask mpv for a numeric property (e.g. `time-pos`) over the IPC socket and
/// return its value. Returns `None` if mpv is unreachable or the property is
/// currently unavailable (e.g. before playback has actually started).
fn ipc_query_f64(prop: &str) -> Option<f64> {
    ipc_query(prop)?.as_f64()
}

/// Loudness of the audio mpv is playing right now, as RMS in dBFS
/// (`-inf` for silence), from the filter set up in [`MpvProcess::spawn`].
fn query_level() -> Option<f64> {
    let data = ipc_query(&format!("af-metadata/{LEVEL_FILTER}"))?;
    parse_level(&data)
}

fn parse_level(data: &serde_json::Value) -> Option<f64> {
    data.get("lavfi.astats.Overall.RMS_level")?
        .as_str()?
        .parse()
        .ok()
}

fn ipc_query(prop: &str) -> Option<serde_json::Value> {
    let stream = UnixStream::connect(SOCKET_PATH).ok()?;
    // Never block the player loop for long if mpv is unresponsive.
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .ok()?;

    let mut writer = &stream;
    let mut msg = json!({"command": ["get_property", prop], "request_id": 1}).to_string();
    msg.push('\n');
    writer.write_all(msg.as_bytes()).ok()?;

    // mpv may interleave async event lines; read until we see our reply.
    let reader = BufReader::new(&stream);
    for line in reader.lines() {
        let line = line.ok()?;
        let v: serde_json::Value = serde_json::from_str(&line).ok()?;
        if v.get("request_id").and_then(|r| r.as_i64()) == Some(1) {
            return v.get("data").cloned();
        }
    }
    None
}

fn wait_for_socket(timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if UnixStream::connect(SOCKET_PATH).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Which file mpv is playing for us, so an `end-file` can be matched to it.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Track {
    /// Nothing requested, or the last track is over.
    None,
    /// `loadfile` sent, its `start-file` not seen yet. Any `end-file` in this
    /// window belongs to the track being replaced — including one that
    /// reached its end just as the next was requested — so none of them count.
    Loading,
    /// Playing the playlist entry with this id.
    Playing(Option<i64>),
}

/// What an mpv event means for the app, given which track is current.
#[derive(PartialEq, Debug)]
enum Outcome {
    Finished,
    Failed(Option<String>),
}

fn on_event(track: &mut Track, event: &serde_json::Value) -> Option<Outcome> {
    let entry = event.get("playlist_entry_id").and_then(|v| v.as_i64());
    match event.get("event")?.as_str()? {
        "start-file" if *track == Track::Loading => {
            *track = Track::Playing(entry);
            None
        }
        "end-file" => {
            if *track != Track::Playing(entry) {
                return None;
            }
            let outcome = match event.get("reason").and_then(|r| r.as_str()) {
                Some("eof") => Outcome::Finished,
                Some("error") => Outcome::Failed(
                    event
                        .get("file_error")
                        .and_then(|e| e.as_str())
                        .map(str::to_string),
                ),
                // Stopped or replaced by us; nothing to report.
                _ => return None,
            };
            *track = Track::None;
            Some(outcome)
        }
        _ => None,
    }
}

fn player_thread(
    rx: mpsc::Receiver<PlayerCmd>,
    self_tx: mpsc::SyncSender<PlayerCmd>,
    event_tx: UnboundedSender<AppMessage>,
) {
    let mut mpv: Option<MpvProcess> = None;
    let mut track = Track::None;
    let mut volume: i32 = 100;

    loop {
        // 100 ms keeps the loudness readings fresh enough for the bars.
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(cmd) => match cmd {
                PlayerCmd::Play(req) => {
                    crate::logline!("player: Play({})", req.url);
                    let _ = event_tx.send(AppMessage::AudioLoading);

                    if mpv.is_none() {
                        match MpvProcess::spawn(self_tx.clone()) {
                            Ok(proc) => {
                                crate::logline!("player: mpv ready (pid {})", proc.child.id());
                                mpv = Some(proc);
                            }
                            Err(e) => {
                                crate::logline!("player: mpv spawn failed: {e}");
                                let _ = event_tx.send(AppMessage::AudioError(e.to_string()));
                                continue;
                            }
                        }
                    }
                    if let Some(m) = mpv.as_mut() {
                        m.load(&req, volume);
                        track = Track::Loading;
                        let _ = event_tx.send(AppMessage::AudioReady);
                    }
                }

                PlayerCmd::TogglePause => {
                    if let Some(m) = mpv.as_mut() {
                        m.command(json!(["cycle", "pause"]));
                    }
                }

                PlayerCmd::SetVolume(v) => {
                    volume = v;
                    if let Some(m) = mpv.as_mut() {
                        m.command(json!(["set_property", "volume", v]));
                    }
                }

                PlayerCmd::SeekAbs(secs) => {
                    if let Some(m) = mpv.as_mut() {
                        m.command(json!(["seek", secs, "absolute"]));
                    }
                }

                PlayerCmd::Stop => {
                    track = Track::None;
                    if let Some(m) = mpv.as_mut() {
                        m.command(json!(["stop"]));
                    }
                }

                PlayerCmd::Quit => {
                    if let Some(mut m) = mpv.take() {
                        m.kill();
                    }
                    break;
                }

                PlayerCmd::Event(event) => {
                    let Some(m) = mpv.as_ref() else { continue };
                    match on_event(&mut track, &event) {
                        Some(Outcome::Finished) => {
                            crate::logline!("player: track reached its end -> AudioFinished");
                            let _ = event_tx.send(AppMessage::AudioFinished);
                        }
                        Some(Outcome::Failed(file_error)) => {
                            // mpv couldn't play the stream — e.g. an expired
                            // URL or a network blip. Report it instead of
                            // silently acting as if the track had finished.
                            let msg = m.error_detail(file_error.as_deref());
                            crate::logline!("player: track failed -> AudioError: {msg}");
                            let _ = event_tx.send(AppMessage::AudioError(msg));
                        }
                        None => {}
                    }
                }
            },

            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Check mpv is still alive; otherwise pull the real playback
                // position straight from mpv so the UI counter stays tightly
                // coupled to the stream.
                let Some(m) = mpv.as_mut() else { continue };
                match m.child.try_wait() {
                    Ok(Some(status)) => {
                        // mpv only exits when we kill it, so this is a crash.
                        // The next Play starts a new one.
                        let msg = m.error_detail(None);
                        crate::logline!("player: mpv exited with {status}: {msg}");
                        let _ = std::fs::remove_file(SOCKET_PATH);
                        mpv = None;
                        if track != Track::None {
                            track = Track::None;
                            let _ = event_tx.send(AppMessage::AudioError(format!(
                                "mpv exited unexpectedly ({status}): {msg}"
                            )));
                        }
                    }
                    // Not while a new track is loading: until its start-file,
                    // these would still describe the one it replaces.
                    Ok(None) if matches!(track, Track::Playing(_)) => {
                        if let Some(pos) = ipc_query_f64("time-pos") {
                            let _ = event_tx.send(AppMessage::Position(pos));
                        }
                        if let Some(idle) = ipc_query("core-idle").and_then(|v| v.as_bool()) {
                            let _ = event_tx.send(AppMessage::AudioIdle(idle));
                        }
                        if let Some(db) = query_level() {
                            let _ = event_tx.send(AppMessage::AudioLevel(db));
                        }
                    }
                    _ => {}
                }
            }

            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn a_track_that_plays_to_its_end_finishes() {
        let mut track = Track::Loading;
        assert_eq!(
            on_event(
                &mut track,
                &json!({"event": "start-file", "playlist_entry_id": 2})
            ),
            None
        );
        assert_eq!(track, Track::Playing(Some(2)));
        let end = json!({"event": "end-file", "reason": "eof", "playlist_entry_id": 2});
        assert_eq!(on_event(&mut track, &end), Some(Outcome::Finished));
        assert_eq!(track, Track::None);
    }

    /// Skipping ahead just as the current track ends: its `eof` arrives after
    /// the next `loadfile` went out and must not skip the new track too.
    #[test]
    fn the_end_of_a_replaced_track_is_ignored() {
        // Track 1 was playing when Play went out for the next one.
        let mut track = Track::Loading;
        let old_end = json!({"event": "end-file", "reason": "eof", "playlist_entry_id": 1});
        assert_eq!(on_event(&mut track, &old_end), None);
        on_event(
            &mut track,
            &json!({"event": "start-file", "playlist_entry_id": 2}),
        );
        assert_eq!(on_event(&mut track, &old_end), None);
        assert_eq!(track, Track::Playing(Some(2)));
    }

    #[test]
    fn a_stream_that_fails_to_load_is_an_error() {
        let mut track = Track::Loading;
        on_event(
            &mut track,
            &json!({"event": "start-file", "playlist_entry_id": 3}),
        );
        let end = json!({"event": "end-file", "reason": "error", "playlist_entry_id": 3, "file_error": "loading failed"});
        assert_eq!(
            on_event(&mut track, &end),
            Some(Outcome::Failed(Some("loading failed".into())))
        );
    }

    #[test]
    fn stopping_reports_nothing() {
        let mut track = Track::Playing(Some(4));
        let end = json!({"event": "end-file", "reason": "stop", "playlist_entry_id": 4});
        assert_eq!(on_event(&mut track, &end), None);
    }

    #[test]
    fn reads_the_level_mpv_reports() {
        let data = json!({"lavfi.astats.Overall.RMS_level": "-9.030004"});
        assert_eq!(parse_level(&data), Some(-9.030004));
        let silence = json!({"lavfi.astats.Overall.RMS_level": "-inf"});
        assert_eq!(parse_level(&silence), Some(f64::NEG_INFINITY));
        assert_eq!(parse_level(&json!({})), None);
    }

    /// Through a real mpv but no network: tracks are played one after another
    /// by the same mpv, a track that ends reports it, and one that's replaced
    /// before its end doesn't. `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "requires mpv"]
    async fn one_mpv_plays_track_after_track() {
        let mpv_home = std::env::temp_dir().join("listen_to_it_test_mpv");
        std::fs::create_dir_all(&mpv_home).unwrap();
        std::fs::write(mpv_home.join("mpv.conf"), "ao=null\n").unwrap();
        std::env::set_var("MPV_HOME", &mpv_home);

        let track = |secs: u32| {
            PlayerCmd::Play(PlayRequest {
                url: format!("av://lavfi:sine=f=440:d={secs}"),
                headers: vec![("User-Agent".into(), "test".into())],
            })
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let player = Player::new(tx);

        // Replaced before its end, then a short one that plays out.
        player.send(track(30));
        tokio::time::sleep(Duration::from_millis(700)).await;
        player.send(track(1));

        let mut finished = 0;
        let deadline = Instant::now() + Duration::from_secs(6);
        while Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
                Ok(Some(AppMessage::AudioFinished)) => finished += 1,
                Ok(Some(AppMessage::AudioError(e))) => panic!("playback failed: {e}"),
                Ok(Some(_)) | Err(_) => {}
                Ok(None) => break,
            }
        }
        assert_eq!(finished, 1, "exactly the track that played out should finish");
    }

    /// End-to-end through a real mpv: resolve a track, hand the player the
    /// stream, and check mpv actually opens it and reports positions and
    /// loudness — which is what proves the direct URL, `--no-ytdl`, the
    /// replayed request headers and the level filter all hold together. Hits the network, so it's opt-in:
    /// `cargo test -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "requires network, yt-dlp and mpv"]
    async fn plays_a_resolved_stream_without_touching_yt_dlp_again() {
        // Keep the test silent: mpv reads ao=null from a throwaway config dir.
        let mpv_home = std::env::temp_dir().join("listen_to_it_test_mpv");
        std::fs::create_dir_all(&mpv_home).unwrap();
        std::fs::write(mpv_home.join("mpv.conf"), "ao=null\n").unwrap();
        std::env::set_var("MPV_HOME", &mpv_home);

        crate::ytdlp::ensure().await.unwrap();
        let stream = crate::stream::resolve("https://www.youtube.com/watch?v=jNQXAC9IVRw")
            .await
            .unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let player = Player::new(tx);
        player.play(&stream).await.unwrap();

        let deadline = Instant::now() + Duration::from_secs(20);
        let mut ready = false;
        let mut position = None;
        let mut level = None;
        while Instant::now() < deadline && (position.is_none() || level.is_none()) {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Some(AppMessage::AudioReady)) => ready = true,
                Ok(Some(AppMessage::Position(p))) if p > 0.0 => position = Some(p),
                Ok(Some(AppMessage::AudioLevel(db))) => level = Some(db),
                Ok(Some(AppMessage::AudioError(e))) => panic!("playback failed: {e}"),
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        assert!(ready, "mpv never came up");
        assert!(position.is_some(), "mpv never reported a playback position");
        assert!(level.is_some(), "mpv never reported how loud the audio is");
    }
}
