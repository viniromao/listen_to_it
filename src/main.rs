use anyhow::Result;
use crossterm::{
    event::EventStream,
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use ratatui::{backend::CrosstermBackend, Terminal};
use ratatui_image::picker::{Picker, ProtocolType};
use souvlaki::{MediaControlEvent, MediaControls, PlatformConfig};
use std::io;
use tokio::sync::mpsc;
use tokio::time::Duration;

mod app;
#[macro_use]
mod logging;
mod player;
mod stream;
mod thumbnail;
mod ui;
mod youtube;
mod ytdlp;

use app::{App, AppMessage, MediaAction};

async fn check_mpv() -> Result<()> {
    match tokio::process::Command::new("mpv")
        .arg("--version")
        .output()
        .await
    {
        Ok(out) if out.status.success() => Ok(()),
        _ => {
            anyhow::bail!(
                "mpv not found on PATH. Please install it:\n  \
                 Debian/Ubuntu: sudo apt install mpv\n  \
                 Arch:          sudo pacman -S mpv\n  \
                 Fedora:        sudo dnf install mpv\n  \
                 macOS:         brew install mpv\n  \
                 Windows:       winget install mpv"
            )
        }
    }
}

/// A multi-threaded runtime, deliberately.
///
/// Everything here shares one process: the UI loop, the yt-dlp child processes
/// whose output has to be drained, the thumbnail fetches, and the JPEG decodes.
/// On a `current_thread` runtime they all take turns on the *same* thread, so a
/// decode or a slow render stalled the very futures that were meant to be
/// fetching the next track in the background. `main` still runs on this thread
/// via `block_on`, so the non-`Send` bits it holds (media controls, the image
/// picker) are unaffected.
#[tokio::main]
async fn main() -> Result<()> {
    match logging::init() {
        Ok(path) => eprintln!("Logging to {}", path.display()),
        Err(e) => eprintln!("Could not open debug log: {e}"),
    }

    check_mpv().await?;
    ytdlp::ensure().await?;
    logline!("using yt-dlp at {}", ytdlp::path().display());

    enable_raw_mode()?;

    let (picker, has_image_support) = match Picker::from_query_stdio() {
        Ok(p) => {
            let supported = !matches!(p.protocol_type(), ProtocolType::Halfblocks);
            (p, supported)
        }
        Err(_) => (Picker::from_fontsize((8, 12)), false),
    };

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, crossterm::event::EnableMouseCapture)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let (msg_tx, mut msg_rx) = mpsc::unbounded_channel::<AppMessage>();

    // App creates its own player thread inside new().
    let mut app = App::new(msg_tx.clone(), picker, has_image_support);

    // ── Media controls (MPRIS2 / Now Playing) ────────────────────────────────
    let (media_tx, mut media_rx) = mpsc::unbounded_channel::<MediaAction>();
    match MediaControls::new(PlatformConfig {
        dbus_name: "listen_to_it",
        display_name: "Listen To It",
        hwnd: None,
    }) {
        Ok(mut controls) => {
            let tx = media_tx;
            if let Err(e) = controls.attach(move |event: MediaControlEvent| {
                let action = match event {
                    MediaControlEvent::Play => MediaAction::Play,
                    MediaControlEvent::Pause => MediaAction::Pause,
                    MediaControlEvent::Toggle => MediaAction::Toggle,
                    MediaControlEvent::Stop => MediaAction::Stop,
                    _ => return,
                };
                let _ = tx.send(action);
            }) {
                eprintln!("media controls attach failed: {e}");
            }
            app.media_controls = Some(controls);
        }
        Err(e) => eprintln!("media controls unavailable: {e}"),
    }

    // ── Keyboard event forwarding ─────────────────────────────────────────────
    // Forward EventStream into an unbounded channel so the main loop can use
    // try_recv() to drain *all* pending key events before each render.
    // This prevents the "one key per frame" lag that causes missed keystrokes.
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<crossterm::event::Event>();
    tokio::spawn(async move {
        let mut stream = EventStream::new();
        while let Some(Ok(event)) = stream.next().await {
            if ev_tx.send(event).is_err() {
                break;
            }
        }
    });

    'outer: loop {
        // ── Drain ALL immediately-available events before rendering ──────────
        loop {
            match ev_rx.try_recv() {
                Ok(event) => {
                    if app.handle_event(event).await? {
                        break 'outer;
                    }
                }
                Err(_) => break, // channel empty right now
            }
        }
        while let Ok(msg) = msg_rx.try_recv() {
            app.handle_message(msg).await?;
        }
        while let Ok(action) = media_rx.try_recv() {
            app.handle_media_action(action).await?;
        }

        // ── Render ──────────────────────────────────────────────────────────
        terminal.draw(|frame| ui::render(frame, &mut app))?;

        // ── Wait for next event (100 ms timeout keeps the clock updating) ───
        tokio::select! {
            biased;
            Some(event) = ev_rx.recv() => {
                if app.handle_event(event).await? { break; }
            }
            Some(msg) = msg_rx.recv() => { app.handle_message(msg).await?; }
            Some(action) = media_rx.recv() => { app.handle_media_action(action).await?; }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
    }

    app.player.stop().await;
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, crossterm::event::DisableMouseCapture)?;
    terminal.show_cursor()?;

    Ok(())
}
