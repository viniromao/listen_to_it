use anyhow::{Context, Result};
use serde_json::json;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

use crate::app::AppMessage;

const SOCKET_PATH: &str = "/tmp/listen_to_it_mpv.sock";

enum PlayerCmd {
    Play(String),
    TogglePause,
    SetVolume(i32),
    SeekAbs(f64),
    Stop,
    Quit,
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

    pub async fn play_url(&self, url: &str) -> Result<()> {
        self.send(PlayerCmd::Play(url.to_string()));
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
}

impl MpvProcess {
    fn spawn(url: &str) -> Result<Self> {
        let _ = std::fs::remove_file(SOCKET_PATH);
        let child = Command::new("mpv")
            .args([
                "--no-video",
                "--no-terminal",
                "--quiet",
                &format!("--input-ipc-server={SOCKET_PATH}"),
                url,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to spawn mpv")?;
        Ok(Self { child })
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
                PlayerCmd::Play(url) => {
                    // Kill previous instance — set mpv to None first so the
                    // polling branch never fires AudioFinished for the old process.
                    if let Some(mut m) = mpv.take() {
                        m.kill();
                    }

                    let _ = event_tx.send(AppMessage::AudioLoading);

                    match MpvProcess::spawn(&url) {
                        Err(e) => {
                            let _ = event_tx.send(AppMessage::AudioError(e.to_string()));
                        }
                        Ok(mut proc) => {
                            if wait_for_socket(Duration::from_secs(5)) {
                                let _ = ipc_send(json!({
                                    "command": ["set_property", "volume", volume]
                                }));
                                let _ = event_tx.send(AppMessage::AudioReady);
                                mpv = Some(proc);
                            } else {
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
                // Check if mpv exited naturally (track finished).
                if let Some(ref mut m) = mpv {
                    if let Ok(Some(_)) = m.child.try_wait() {
                        mpv = None;
                        let _ = event_tx.send(AppMessage::AudioFinished);
                    }
                }
            }

            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}
