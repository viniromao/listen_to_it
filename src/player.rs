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

enum PlayerCmd {
    Play(PlayRequest),
    TogglePause,
    SetVolume(i32),
    SeekAbs(f64),
    Stop,
    Quit,
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
        std::thread::Builder::new()
            .name("audio".into())
            .spawn(move || player_thread(rx, event_tx))
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

struct MpvProcess {
    child: Child,
    /// Most useful line mpv logged (stdout or stderr) for this run, kept
    /// updated by background reader threads so a failure can be reported
    /// with a real reason instead of just an exit code. Locks onto the first
    /// line naming an actual error rather than always tracking the very last
    /// line, since mpv's final housekeeping message (e.g. "Exiting... (Errors
    /// when loading file)") is generic and would otherwise clobber it.
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
    fn spawn(req: &PlayRequest) -> Result<Self> {
        let _ = std::fs::remove_file(SOCKET_PATH);
        let mut cmd = Command::new("mpv");
        cmd.args([
            "--no-video",
            "--quiet",
            // The URL is already a direct media URL — letting ytdl_hook run
            // would send it back through yt-dlp for nothing, which is exactly
            // the ~2.5 s of startup latency resolving it ourselves avoids.
            "--no-ytdl",
            &format!("--input-ipc-server={SOCKET_PATH}"),
        ]);

        // Replay the headers yt-dlp used. The User-Agent has its own option;
        // the rest go through the list option one at a time, since values can
        // contain commas and `--http-header-fields` is comma-separated.
        for (name, value) in &req.headers {
            if name.eq_ignore_ascii_case("user-agent") {
                cmd.arg(format!("--user-agent={value}"));
            } else {
                cmd.arg(format!("--http-header-fields-append={name}: {value}"));
            }
        }

        cmd.arg(&req.url)
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
                            // (e.g. the ytdl_hook "ERROR: ..." line), lock it in —
                            // don't let mpv's generic housekeeping lines that follow
                            // ("youtube-dl failed: unexpected error occurred",
                            // "Exiting... (Errors when loading file)") overwrite it
                            // with something less specific.
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
        Ok(Self { child, last_output })
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(SOCKET_PATH);
    }
}

fn ipc_send(cmd: serde_json::Value) -> Result<()> {
    let mut stream = UnixStream::connect(SOCKET_PATH)?;
    let mut msg = cmd.to_string();
    msg.push('\n');
    stream.write_all(msg.as_bytes())?;
    Ok(())
}

/// Ask mpv for a numeric property (e.g. `time-pos`) over the IPC socket and
/// return its value. Returns `None` if mpv is unreachable or the property is
/// currently unavailable (e.g. before playback has actually started).
fn ipc_query_f64(prop: &str) -> Option<f64> {
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
            return v.get("data").and_then(|d| d.as_f64());
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

fn player_thread(rx: mpsc::Receiver<PlayerCmd>, event_tx: UnboundedSender<AppMessage>) {
    let mut mpv: Option<MpvProcess> = None;
    let mut volume: i32 = 100;

    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(cmd) => match cmd {
                PlayerCmd::Play(req) => {
                    crate::logline!("player: Play({})", req.url);
                    // Kill previous instance — set mpv to None first so the
                    // polling branch never fires AudioFinished for the old process.
                    if let Some(mut m) = mpv.take() {
                        m.kill();
                    }

                    let _ = event_tx.send(AppMessage::AudioLoading);

                    match MpvProcess::spawn(&req) {
                        Err(e) => {
                            crate::logline!("player: mpv spawn failed: {e}");
                            let _ = event_tx.send(AppMessage::AudioError(e.to_string()));
                        }
                        Ok(mut proc) => {
                            if wait_for_socket(Duration::from_secs(5)) {
                                crate::logline!("player: mpv ready (pid {})", proc.child.id());
                                let _ = ipc_send(json!({
                                    "command": ["set_property", "volume", volume]
                                }));
                                let _ = event_tx.send(AppMessage::AudioReady);
                                mpv = Some(proc);
                            } else {
                                crate::logline!(
                                    "player: mpv socket timeout (pid {})",
                                    proc.child.id()
                                );
                                proc.kill();
                                let _ = event_tx
                                    .send(AppMessage::AudioError("mpv socket timeout".to_string()));
                            }
                        }
                    }
                }

                PlayerCmd::TogglePause => {
                    let _ = ipc_send(json!({"command": ["cycle", "pause"]}));
                }

                PlayerCmd::SetVolume(v) => {
                    volume = v;
                    let _ = ipc_send(json!({"command": ["set_property", "volume", v]}));
                }

                PlayerCmd::SeekAbs(secs) => {
                    let _ = ipc_send(json!({"command": ["seek", secs, "absolute"]}));
                }

                PlayerCmd::Stop => {
                    if let Some(mut m) = mpv.take() {
                        m.kill();
                    }
                }

                PlayerCmd::Quit => {
                    if let Some(mut m) = mpv.take() {
                        m.kill();
                    }
                    break;
                }
            },

            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Check if mpv exited; otherwise pull the real playback
                // position straight from mpv so the UI counter stays tightly
                // coupled to the stream.
                if let Some(ref mut m) = mpv {
                    let wait_result = m.child.try_wait();
                    let last_output = m.last_output.clone();
                    match wait_result {
                        Ok(Some(status)) => {
                            mpv = None;
                            if status.success() {
                                crate::logline!("player: mpv exited cleanly ({status}) -> AudioFinished");
                                let _ = event_tx.send(AppMessage::AudioFinished);
                            } else {
                                // mpv bailed early — e.g. yt-dlp couldn't resolve
                                // the stream (rate limit, unavailable video,
                                // network blip). Report it instead of silently
                                // acting as if the track had finished playing.
                                let detail = last_output.lock().ok().map(|s| s.text.clone()).unwrap_or_default();
                                let msg = if detail.is_empty() {
                                    format!("mpv exited unexpectedly ({status})")
                                } else {
                                    detail
                                };
                                crate::logline!("player: mpv exited with {status} -> AudioError: {msg}");
                                let _ = event_tx.send(AppMessage::AudioError(msg));
                            }
                        }
                        Ok(None) => {
                            if let Some(pos) = ipc_query_f64("time-pos") {
                                let _ = event_tx.send(AppMessage::Position(pos));
                            }
                        }
                        Err(_) => {}
                    }
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

    /// End-to-end through a real mpv: resolve a track, hand the player the
    /// stream, and check mpv actually opens it and reports positions — which
    /// is what proves the direct URL, `--no-ytdl` and the replayed request
    /// headers all hold together. Hits the network, so it's opt-in:
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
        let stream = crate::stream::resolve("https://www.youtube.com/watch?v=NolF1yCK33c")
            .await
            .unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let player = Player::new(tx);
        player.play(&stream).await.unwrap();

        let deadline = Instant::now() + Duration::from_secs(20);
        let mut ready = false;
        let mut position = None;
        while Instant::now() < deadline && position.is_none() {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Some(AppMessage::AudioReady)) => ready = true,
                Ok(Some(AppMessage::Position(p))) if p > 0.0 => position = Some(p),
                Ok(Some(AppMessage::AudioError(e))) => panic!("playback failed: {e}"),
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        assert!(ready, "mpv never came up");
        assert!(position.is_some(), "mpv never reported a playback position");
    }
}
