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
mod player;
mod thumbnail;
mod ui;
mod youtube;

use app::{App, AppMessage, MediaAction};

// current_thread keeps everything on the main thread, which is required because
// MediaControls (souvlaki) is not Send on all platforms.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    enable_raw_mode()?;

    // Detect terminal image protocol while in raw mode, before alternate screen.
    let (picker, has_image_support) = match Picker::from_query_stdio() {
        Ok(p) => {
            let supported = !matches!(p.protocol_type(), ProtocolType::Halfblocks);
            (p, supported)
        }
        Err(_) => (Picker::from_fontsize((8, 12)), false),
    };

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

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
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}
