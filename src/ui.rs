use crate::app::{App, AppMode, LibraryFocus, NameTarget, View};
use crate::library::DeleteTarget;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use ratatui_image::{protocol::StatefulProtocol, Resize, StatefulImage};

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    let queue_height = if app.queue.is_empty() {
        0
    } else {
        (app.queue.len() as u16 + 2).min(7)
    };
    let progress_height: u16 = if app.now_playing.is_some() { 1 } else { 0 };
    let pos_now = app.current_position();
    let has_chapter = app.now_playing.is_some()
        && app.chapters.iter()
            .filter(|c| c.start_time <= pos_now)
            .last()
            .map(|c| !c.title.is_empty())
            .unwrap_or(false);
    let status_height: u16 = if has_chapter { 4 } else { 3 };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),               // search bar
            Constraint::Min(0),                  // content
            Constraint::Length(status_height),   // now playing
            Constraint::Length(queue_height),    // queue panel
            Constraint::Length(progress_height), // progress bar
        ])
        .split(area);

    render_search_bar(frame, app, chunks[0]);
    match app.view {
        View::Search => render_content(frame, app, chunks[1]),
        View::Library => render_library(frame, app, chunks[1]),
    }
    render_status_bar(frame, app, chunks[2]);
    if queue_height > 0 {
        render_queue(frame, app, chunks[3]);
    }
    if progress_height > 0 {
        render_progress(frame, app, chunks[4]);
    }

    match app.mode {
        AppMode::Confirming => render_confirm_dialog(frame, app, area),
        AppMode::Naming => render_name_dialog(frame, app, area),
        AppMode::PickingPlaylist => render_playlist_picker(frame, app, area),
        AppMode::ConfirmingDelete => render_delete_dialog(frame, app, area),
        AppMode::Help => render_help(frame, app, area),
        AppMode::Normal | AppMode::Searching => {}
    }
}

fn render_search_bar(frame: &mut Frame, app: &App, area: Rect) {
    let active = app.mode == AppMode::Searching;
    let border_style = if active {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::White)
    };

    // The search bar is the one block drawn in every mode and both views, so
    // its top-right corner is where a hint is always on screen — including
    // mid-song, when the status bar is busy with the track.
    let help_badge = Line::from(vec![
        Span::styled(
            " [?] ",
            Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" help ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
    ])
    .right_aligned();

    let mut block = Block::default()
        .title(" Search  [/] focus  [Esc] cancel ")
        .title_top(help_badge)
        .borders(Borders::ALL)
        .border_style(border_style);

    if let Some(version) = &app.updated_to {
        block = block.title_bottom(
            Line::from(Span::styled(
                format!(" updated to v{version} — restart to run it "),
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            ))
            .right_aligned(),
        );
    }

    let widget = Paragraph::new(app.search.value.clone()).block(block);
    frame.render_widget(widget, area);

    if active {
        // Place the real terminal cursor over the character the field's cursor
        // is on so it blinks natively and sits on top of the text instead of
        // pushing it aside. +1 for the left border.
        frame.set_cursor_position((area.x + 1 + app.search.cursor_width(), area.y + 1));
    }
}

fn render_content(frame: &mut Frame, app: &mut App, area: Rect) {
    if app.search_results.is_empty() {
        let lines: Vec<Line> = if app.is_searching {
            vec![Line::from("Searching YouTube...")]
        } else if let Some(ref m) = app.status_message {
            vec![Line::from(m.as_str())]
        } else {
            vec![
                Line::from("Press / to search for music on YouTube"),
                Line::from(""),
                Line::from(Span::styled(
                    "Press [?] at any time for every key and what it does",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from("[j/k or arrows] navigate  [Enter] play now  [f] add to queue  [a] save to a playlist"),
                Line::from("[p] your saved playlists  [Space] pause  [h/l] seek  [+/-] volume  [q] quit"),
            ]
        };
        let vert = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(30),
                Constraint::Min(0),
                Constraint::Percentage(30),
            ])
            .split(area);
        let p = Paragraph::new(lines)
            .block(Block::default().borders(Borders::NONE))
            .style(Style::default().fg(Color::DarkGray))
            .wrap(Wrap { trim: true });
        frame.render_widget(p, vert[1]);
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);

    render_results(frame, app, chunks[0]);
    render_preview(frame, app, chunks[1]);
}

fn render_results(frame: &mut Frame, app: &App, area: Rect) {
    let playing_id = app.now_playing.as_ref().map(|np| np.id.as_str());
    let queued_ids: std::collections::HashSet<&str> =
        app.queue.iter().map(|v| v.id.as_str()).collect();

    let items: Vec<ListItem> = app
        .search_results
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let is_playing = playing_id == Some(r.id.as_str());
            let in_queue = queued_ids.contains(r.id.as_str());
            let has_thumb = app.thumbnail_protocols.contains_key(&r.id);
            let loading_thumb = app.thumbnails_loading.contains(&r.id);

            let play_icon = if is_playing { ">> " } else if in_queue { "+  " } else { "   " };
            let thumb_icon = if r.is_playlist {
                "≡  "
            } else if !app.has_image_support {
                "  "
            } else if has_thumb {
                "[] "
            } else if loading_thumb {
                ".. "
            } else {
                "   "
            };

            let num = format!("{:2}. ", i + 1);
            let title = truncate(&r.title, if r.is_playlist { 26 } else { 36 });
            let channel = r.channel_name();
            let dur = r.duration.map(|d| fmt_duration(d as u64)).unwrap_or_default();
            let views = r.view_count.map(fmt_views).unwrap_or_default();

            let (title_style, play_icon_style) = if is_playing {
                (
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                    Style::default().fg(Color::Green),
                )
            } else if in_queue {
                (
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                    Style::default().fg(Color::Yellow),
                )
            } else if r.is_playlist {
                (
                    Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                    Style::default().fg(Color::Magenta),
                )
            } else {
                (
                    Style::default().add_modifier(Modifier::BOLD),
                    Style::default(),
                )
            };

            let thumb_color = if r.is_playlist { Color::Magenta } else { Color::Blue };
            let mut title_spans = vec![
                Span::styled(play_icon, play_icon_style),
                Span::styled(thumb_icon, Style::default().fg(thumb_color)),
                Span::raw(num),
                Span::styled(title, title_style),
            ];
            if r.is_playlist {
                title_spans.push(Span::styled(
                    " [PLAYLIST]",
                    Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                ));
            }
            let title_line = Line::from(title_spans);

            let info_text = if r.is_playlist {
                let owner = if r.channel.is_some() { channel } else { "…" };
                let tracks = r
                    .playlist_count
                    .map(|n| format!("{} tracks", n))
                    .unwrap_or_else(|| "playlist".to_string());
                if views.is_empty() {
                    format!("         {} . {}", owner, tracks)
                } else {
                    format!("         {} . {} . {} views", owner, tracks, views)
                }
            } else {
                format!("         {} . {} . {}", channel, dur, views)
            };
            let info_line = Line::from(Span::styled(
                info_text,
                Style::default().fg(Color::DarkGray),
            ));

            ListItem::new(vec![title_line, info_line])
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(app.selected_index));

    let queue_hint = if app.queue.is_empty() {
        String::new()
    } else {
        format!(" | Queue: {} ", app.queue.len())
    };
    let list = List::new(items)
        .block(
            Block::default()
                .title(format!(" Results ({}){} ", app.search_results.len(), queue_hint))
                .borders(Borders::ALL),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );

    frame.render_stateful_widget(list, area, &mut state);
}

fn render_preview(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default().title(" Preview ").borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(result) = app.search_results.get(app.selected_index).cloned() else {
        return;
    };

    let (thumb_area, info_area) = if app.has_image_support {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(75), Constraint::Percentage(25)])
            .split(inner);
        (Some(chunks[0]), chunks[1])
    } else {
        (None, inner)
    };

    // Render thumbnail if supported and visuals are enabled.
    if let Some(t_area) = thumb_area {
        if app.show_visuals {
            let vid_id = result.id.clone();
            if let Some(protocol) = app.thumbnail_protocols.get_mut(&vid_id) {
                let img = StatefulImage::<StatefulProtocol>::new().resize(Resize::Fit(None));
                frame.render_stateful_widget(img, t_area, protocol);
            } else {
                let msg = if app.thumbnails_loading.contains(&vid_id) {
                    "Loading thumbnail..."
                } else {
                    "No thumbnail available"
                };
                let p = Paragraph::new(msg).style(Style::default().fg(Color::DarkGray));
                frame.render_widget(p, t_area);
            }
        }
    }

    // Info panel
    let channel = result.channel_name().to_string();
    let duration = result
        .duration
        .map(|d| fmt_duration(d as u64))
        .unwrap_or_else(|| "Unknown".to_string());
    let views = result
        .view_count
        .map(fmt_views)
        .unwrap_or_else(|| "Unknown".to_string());

    let mut info = vec![
        Line::from(Span::styled(
            truncate(&result.title, 38),
            Style::default().add_modifier(Modifier::BOLD).fg(Color::White),
        )),
    ];
    if result.is_playlist {
        info.push(Line::from(Span::styled(
            "[PLAYLIST]",
            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        )));
        let owner = if result.channel.is_some() { channel } else { "Loading…".to_string() };
        info.push(Line::from(vec![
            Span::styled("Channel:  ", Style::default().fg(Color::Cyan)),
            Span::raw(owner),
        ]));
        info.push(Line::from(vec![
            Span::styled("Tracks:   ", Style::default().fg(Color::Cyan)),
            Span::raw(result.playlist_count.map(|n| n.to_string()).unwrap_or_else(|| "…".to_string())),
        ]));
        info.push(Line::from(vec![
            Span::styled("Views:    ", Style::default().fg(Color::Cyan)),
            Span::raw(if views == "Unknown" { "…".to_string() } else { views }),
        ]));
        info.push(Line::from(""));
        info.push(Line::from(Span::styled("[Enter] Play all (clear queue)", Style::default().fg(Color::Green))));
        info.push(Line::from(Span::styled("[f]     Queue all tracks",      Style::default().fg(Color::Yellow))));
    } else {
        info.push(Line::from(vec![
            Span::styled("Channel:  ", Style::default().fg(Color::Cyan)),
            Span::raw(channel),
        ]));
        info.push(Line::from(vec![
            Span::styled("Duration: ", Style::default().fg(Color::Cyan)),
            Span::raw(duration),
        ]));
        info.push(Line::from(vec![
            Span::styled("Views:    ", Style::default().fg(Color::Cyan)),
            Span::raw(views),
        ]));
        let saved_in = app.playlists_with(&result.id);
        if !saved_in.is_empty() {
            info.push(Line::from(vec![
                Span::styled("Saved in: ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    truncate(&saved_in.join(", "), 28),
                    Style::default().fg(Color::Magenta),
                ),
            ]));
        }
        info.push(Line::from(""));
        info.push(Line::from(Span::styled("[Enter] Play now (clear queue)", Style::default().fg(Color::Green))));
        info.push(Line::from(Span::styled("[f]     Add to queue",           Style::default().fg(Color::Yellow))));
        info.push(Line::from(Span::styled("[a]     Save to a playlist",     Style::default().fg(Color::Magenta))));
    }
    info.extend(vec![
        Line::from(""),
        Line::from(Span::styled("[Space] Pause / resume",         Style::default().fg(Color::DarkGray))),
        Line::from(Span::styled("[h/l]   Seek -/+5s",             Style::default().fg(Color::DarkGray))),
        Line::from(Span::styled("[[ ]]   Prev / next track",      Style::default().fg(Color::DarkGray))),
        Line::from(Span::styled("[{/}]   Prev / next chapter",    Style::default().fg(Color::DarkGray))),
        Line::from(Span::styled("[+/-]   Volume",                 Style::default().fg(Color::DarkGray))),
        Line::from(Span::styled("[p]     Saved playlists",        Style::default().fg(Color::DarkGray))),
        Line::from(Span::styled("[A]     Save queue as playlist", Style::default().fg(Color::DarkGray))),
        Line::from(Span::styled("[r]     Toggle loop",            Style::default().fg(Color::DarkGray))),
        Line::from(Span::styled("[d]     Toggle thumbnails",      Style::default().fg(Color::DarkGray))),
        Line::from(Span::styled("[?]     Help — all keys",       Style::default().fg(Color::Cyan))),
        Line::from(Span::styled("[q]     Quit",                   Style::default().fg(Color::DarkGray))),
    ]);

    let p = Paragraph::new(info).wrap(Wrap { trim: true });
    frame.render_widget(p, info_area);
}

fn render_queue(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .queue
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let dur = v.duration.map(|d| fmt_duration(d as u64)).unwrap_or_default();
            // Playlist imports come back without channel info; omit it rather
            // than printing a meaningless "Unknown".
            let channel = v.channel.as_deref().or(v.uploader.as_deref());
            let meta = match (channel, dur.is_empty()) {
                (Some(c), false) => format!("  {} · {}", c, dur),
                (Some(c), true) => format!("  {}", c),
                (None, false) => format!("  {}", dur),
                (None, true) => String::new(),
            };
            let line = Line::from(vec![
                Span::styled(
                    format!("{:2}. ", i + 1),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(truncate(&v.title, 50), Style::default().fg(Color::White)),
                Span::styled(meta, Style::default().fg(Color::DarkGray)),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .title(format!(
                " Queue ({}){} — [[] prev  []] next ",
                app.queue.len(),
                if app.shuffle { " shuffled" } else { "" }
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow)),
    );
    frame.render_widget(list, area);
}

/// Classic four-frame ASCII spinner, ~120ms per frame.
const SPINNER: [char; 4] = ['|', '/', '-', '\\'];

fn render_progress(frame: &mut Frame, app: &mut App, area: Rect) {
    if app.is_buffering() {
        render_buffering_bar(frame, app, area);
        return;
    }

    let pos = app.current_position();
    let duration = app.now_playing.as_ref().and_then(|t| t.duration).unwrap_or(0.0);
    let ratio = if duration > 0.0 { (pos / duration).clamp(0.0, 1.0) } else { 0.0 };

    let label = format!(" {} / {} ", fmt_duration(pos as u64), fmt_duration(duration as u64));
    let label_w = label.chars().count() as u16;
    let bar_w = area.width.saturating_sub(label_w) as usize;

    let filled = (ratio * bar_w as f64).round() as usize;
    let filled = filled.clamp(0, bar_w);

    let marker_cols: std::collections::HashSet<usize> = if duration > 0.0 {
        app.chapters.iter()
            .filter(|c| c.start_time > 0.5)
            .map(|c| ((c.start_time / duration) * bar_w as f64).floor() as usize)
            .filter(|&p| p < bar_w)
            .collect()
    } else {
        std::collections::HashSet::new()
    };

    let bar: Vec<(char, Color)> = (0..bar_w)
        .map(|i| {
            if marker_cols.contains(&i) {
                ('▴', Color::Yellow)
            } else if filled > 0 && i + 1 == filled {
                ('╸', Color::White)
            } else if filled > 0 && i + 1 < filled {
                ('━', Color::Cyan)
            } else {
                ('╌', Color::DarkGray)
            }
        })
        .collect();

    let mut spans = Vec::new();
    if !bar.is_empty() {
        let (mut cur_color, mut buf) = (bar[0].1, String::new());
        for (c, color) in &bar {
            if *color == cur_color {
                buf.push(*c);
            } else {
                spans.push(Span::styled(buf.clone(), Style::default().fg(cur_color)));
                buf = c.to_string();
                cur_color = *color;
            }
        }
        if !buf.is_empty() {
            spans.push(Span::styled(buf, Style::default().fg(cur_color)));
        }
    }
    spans.push(Span::styled(label, Style::default().fg(Color::White)));

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    app.progress_bar_area = Some(Rect { width: bar_w as u16, ..area });
}

/// A "Larson scanner" style bar (like KITT's dashboard light) that sweeps
/// back and forth while mpv is still resolving/buffering the stream — stands
/// in for the progress bar until a real position is known.
fn render_buffering_bar(frame: &mut Frame, app: &mut App, area: Rect) {
    let label = " buffering... ";
    let label_w = label.chars().count() as u16;
    let bar_w = area.width.saturating_sub(label_w) as usize;
    app.progress_bar_area = None; // nothing to seek into yet

    if bar_w == 0 {
        return;
    }

    let t = app.started_at.elapsed().as_millis() as usize;
    let period = bar_w * 2;
    let x = (t / 35) % period;
    let head = if x < bar_w { x } else { period - x - 1 };

    let bar: String = (0..bar_w)
        .map(|i| match (i as isize - head as isize).unsigned_abs() {
            0 => '#',
            1 => '=',
            2 => '-',
            _ => '.',
        })
        .collect();

    let spans = vec![
        Span::styled(bar, Style::default().fg(Color::Yellow)),
        Span::styled(label, Style::default().fg(Color::Yellow)),
    ];
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL);

    if let Some(ref track) = app.now_playing {
        if app.is_buffering() {
            let t = app.started_at.elapsed().as_millis() as usize;
            let spinner = SPINNER[(t / 120) % SPINNER.len()];
            let dots = ".".repeat((t / 300) % 4);
            let line = Line::from(vec![
                Span::styled(format!(" {} ", spinner), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::raw("Fetching from YouTube: "),
                Span::styled(truncate(&track.title, 45), Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(format!("{:<3}", dots)),
            ]);
            let p = Paragraph::new(line)
                .block(block)
                .style(Style::default().fg(Color::Yellow));
            frame.render_widget(p, area);
            return;
        }

        // Both icons occupy the same width so the title doesn't shift when
        // playback is paused and resumed.
        let icon = if app.is_paused { "||  " } else { "▶   " };
        let pos_val = app.current_position();
        let pos = fmt_duration(pos_val as u64);
        let queue_info = if app.queue.is_empty() {
            String::new()
        } else {
            format!("  |  Next: {}", truncate(&app.queue[0].title, 25))
        };
        let loop_info = if app.loop_mode { "  |  [LOOP]" } else { "" };
        let shuffle_info = if app.shuffle { "  |  [SHUFFLE]" } else { "" };

        let mut lines = vec![Line::from(Span::raw(format!(
            " {} {}  |  {} elapsed  |  vol {}%{}{}{}",
            icon,
            truncate(&track.title, 45),
            pos,
            app.volume,
            queue_info,
            loop_info,
            shuffle_info,
        )))];

        if let Some(ch) = app.chapters.iter()
            .filter(|c| c.start_time <= pos_val)
            .last()
            .filter(|c| !c.title.is_empty())
        {
            lines.push(Line::from(vec![
                Span::styled("   ♪ ", Style::default().fg(Color::Yellow)),
                Span::styled(truncate(&ch.title, 70), Style::default().fg(Color::Yellow)),
            ]));
        }

        let p = Paragraph::new(lines)
            .block(block)
            .style(Style::default().fg(Color::Green));
        frame.render_widget(p, area);
    } else if let Some(ref msg) = app.status_message {
        // Red is reserved for things that actually failed. Searching, loading a
        // playlist or queueing a track are ordinary progress and shouldn't
        // paint the status bar like a fault.
        let color = if app.status_is_error { Color::Red } else { Color::Cyan };
        let p = Paragraph::new(format!(" {msg}"))
            .block(block)
            .style(Style::default().fg(color));
        frame.render_widget(p, area);
    } else {
        let p = Paragraph::new(" No track playing  |  [/] search  [j/k] navigate  [Enter] play  |  [?] help")
            .block(block)
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(p, area);
    }
}

fn render_confirm_dialog(frame: &mut Frame, app: &App, area: Rect) {
    let title = app.confirm_title.as_deref().unwrap_or("this track");
    let truncated = truncate(title, 40);

    let popup = centered_rect(54, 7, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" Play now? ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let content = vec![
        Line::from(vec![
            Span::raw("  Play "),
            Span::styled(format!("\"{}\"", truncated), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(Span::styled(
            "  and clear the queue?",
            Style::default().fg(Color::White),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  [Y] Yes  ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::styled("(default)", Style::default().fg(Color::DarkGray)),
            Span::styled("     [n] No", Style::default().fg(Color::Red)),
        ]),
    ];

    frame.render_widget(Paragraph::new(content), inner);
}

/// The saved playlists: names on the left, the highlighted playlist's tracks
/// on the right, with the focused side outlined and a key hint underneath.
fn render_library(frame: &mut Frame, app: &App, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    if app.library.playlists.is_empty() {
        render_empty_library(frame, rows[0]);
    } else {
        let panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(rows[0]);
        render_playlist_list(frame, app, panes[0]);
        render_playlist_tracks(frame, app, panes[1]);
    }

    let hint = if app.library.playlists.is_empty() {
        " [n] new playlist   [A] save the queue as a playlist   [?] help   [Esc] back to search"
    } else if app.library_focus == LibraryFocus::Tracks {
        " [Enter] play from here  [f] queue track  [J/K] reorder  [x] remove  [Tab] playlists  [Esc] back"
    } else {
        " [Enter] play all  [f] queue all  [n] new  [R] rename  [x] delete  [Tab] tracks  [Esc] back"
    };
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(Color::DarkGray)),
        rows[1],
    );
}

fn render_empty_library(frame: &mut Frame, area: Rect) {
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(30), Constraint::Min(0), Constraint::Percentage(30)])
        .split(area);
    let lines = vec![
        Line::from(Span::styled(
            "No saved playlists yet.",
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("Search for a song and press [a] to save it into a playlist of your own,"),
        Line::from("or queue a few tracks and press [A] to save the whole queue at once."),
    ];
    let p = Paragraph::new(lines)
        .style(Style::default().fg(Color::DarkGray))
        .wrap(Wrap { trim: true });
    frame.render_widget(p, vert[1]);
}

fn render_playlist_list(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.library_focus == LibraryFocus::Playlists;

    let items: Vec<ListItem> = app
        .library
        .playlists
        .iter()
        .map(|playlist| {
            let total = playlist.total_duration();
            let count = tracks_label(playlist.tracks.len());
            let info = if total > 0.0 {
                format!("  {count} · {}", fmt_duration(total as u64))
            } else {
                format!("  {count}")
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("  {}", truncate(&playlist.name, 22)),
                    Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                ),
                Span::styled(info, Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(app.library_selected));

    let list = List::new(items)
        .block(
            Block::default()
                .title(format!(" Playlists ({}) ", app.library.playlists.len()))
                .borders(Borders::ALL)
                .border_style(pane_border(focused)),
        )
        .highlight_style(highlight(focused));

    frame.render_stateful_widget(list, area, &mut state);
}

fn render_playlist_tracks(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.library_focus == LibraryFocus::Tracks;
    let playing_id = app.now_playing.as_ref().map(|np| np.id.as_str());

    let Some(playlist) = app.selected_playlist() else {
        return;
    };

    let items: Vec<ListItem> = playlist
        .tracks
        .iter()
        .enumerate()
        .map(|(i, track)| {
            let is_playing = playing_id == Some(track.id.as_str());
            let marker = if is_playing { ">> " } else { "   " };
            let dur = track.duration.map(|d| fmt_duration(d as u64)).unwrap_or_default();
            let meta = match (track.channel.as_deref(), dur.is_empty()) {
                (Some(c), false) => format!("  {} · {}", c, dur),
                (Some(c), true) => format!("  {}", c),
                (None, false) => format!("  {}", dur),
                (None, true) => String::new(),
            };
            let title_style = if is_playing {
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            ListItem::new(Line::from(vec![
                Span::styled(marker, Style::default().fg(Color::Green)),
                Span::styled(format!("{:2}. ", i + 1), Style::default().fg(Color::DarkGray)),
                Span::styled(truncate(&track.title, 34), title_style),
                Span::styled(meta, Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();

    let block = Block::default()
        .title(format!(" {} ", truncate(&playlist.name, 30)))
        .borders(Borders::ALL)
        .border_style(pane_border(focused));

    if items.is_empty() {
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new("Empty — press [a] on a search result to add tracks here.")
                .style(Style::default().fg(Color::DarkGray))
                .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }

    let mut state = ListState::default();
    state.select(Some(app.library_track_selected));
    let list = List::new(items).block(block).highlight_style(highlight(focused));
    frame.render_stateful_widget(list, area, &mut state);
}

/// The focused pane is outlined in yellow; the other one recedes, so which
/// side the keys apply to is visible without reading the hint line.
fn pane_border(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn highlight(focused: bool) -> Style {
    if focused {
        Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    }
}

fn render_name_dialog(frame: &mut Frame, app: &App, area: Rect) {
    let (title, prompt) = match app.name_target {
        Some(NameTarget::Rename(_)) => (" Rename playlist ", "New name:".to_string()),
        _ if !app.pending_tracks.is_empty() => (
            " New playlist ",
            format!("Name for {} track(s):", app.pending_tracks.len()),
        ),
        _ => (" New playlist ", "Name:".to_string()),
    };

    let popup = centered_rect(56, 7, area);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let content = vec![
        Line::from(Span::styled(format!("  {prompt}"), Style::default().fg(Color::Cyan))),
        Line::from(""),
        Line::from(vec![
            Span::styled("  > ", Style::default().fg(Color::Magenta)),
            Span::styled(
                app.name_input.value.clone(),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  [Enter] save  ", Style::default().fg(Color::Green)),
            Span::styled("  [Esc] cancel", Style::default().fg(Color::Red)),
        ]),
    ];
    frame.render_widget(Paragraph::new(content), inner);
    // "  > " is four columns wide, before whatever has been typed.
    frame.set_cursor_position((inner.x + 4 + app.name_input.cursor_width(), inner.y + 2));
}

fn render_playlist_picker(frame: &mut Frame, app: &App, area: Rect) {
    // One row per playlist, plus the "new playlist" row at the bottom.
    let rows = (app.library.playlists.len() + 1).min(8) as u16;
    let popup = centered_rect(56, rows + 4, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(format!(" Add {} track(s) to… ", app.pending_tracks.len()))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let mut items: Vec<ListItem> = app
        .library
        .playlists
        .iter()
        .map(|playlist| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(" {}", truncate(&playlist.name, 34)),
                    Style::default().fg(Color::White),
                ),
                Span::styled(
                    format!("  ({})", tracks_label(playlist.tracks.len())),
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        })
        .collect();
    items.push(ListItem::new(Line::from(Span::styled(
        " + New playlist…",
        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
    ))));

    let mut state = ListState::default();
    state.select(Some(app.pick_selected.min(items.len() - 1)));
    let list = List::new(items).highlight_style(
        Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD),
    );
    frame.render_stateful_widget(list, chunks[0], &mut state);

    frame.render_widget(
        Paragraph::new(" [Enter] add   [n] new playlist   [Esc] cancel")
            .style(Style::default().fg(Color::DarkGray)),
        chunks[1],
    );
}

fn render_delete_dialog(frame: &mut Frame, app: &App, area: Rect) {
    let question = match app.delete_target {
        Some(DeleteTarget::Playlist(index)) => app
            .library
            .playlists
            .get(index)
            .map(|p| format!("Delete \"{}\" and its {} tracks?", truncate(&p.name, 30), p.tracks.len())),
        Some(DeleteTarget::Track(index, track)) => app
            .library
            .playlists
            .get(index)
            .and_then(|p| p.tracks.get(track).map(|t| (p, t)))
            .map(|(p, t)| {
                format!(
                    "Remove \"{}\" from \"{}\"?",
                    truncate(&t.title, 28),
                    truncate(&p.name, 20)
                )
            }),
        None => None,
    };
    let Some(question) = question else { return };

    let popup = centered_rect(58, 7, area);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(" Are you sure? ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let content = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  {question}"),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  [Y] Yes  ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Span::styled("     [n] Keep it", Style::default().fg(Color::Green)),
        ]),
    ];
    frame.render_widget(Paragraph::new(content).wrap(Wrap { trim: true }), inner);
}

/// The full key reference, over whatever was on screen.
///
/// Takes `&mut App` so the scroll position can be clamped here: this is the
/// only place that knows how many lines fit, so the key handler is free to
/// just add and subtract without tracking the end of the list.
fn render_help(frame: &mut Frame, app: &mut App, area: Rect) {
    let lines = help_lines();

    let popup = centered_rect(80, area.height.saturating_sub(2), area);
    frame.render_widget(Clear, popup);

    let visible = popup.height.saturating_sub(2); // the block's own borders
    let max_scroll = (lines.len() as u16).saturating_sub(visible);
    app.help_scroll = app.help_scroll.min(max_scroll);

    let more = if app.help_scroll < max_scroll { " ▼ more " } else { " " };
    let block = Block::default()
        .title(format!(" Help — [j/k] scroll  [any other key] close {more}"))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    frame.render_widget(Paragraph::new(lines).scroll((app.help_scroll, 0)), inner);
}

/// A section heading in the help overlay.
fn help_head(title: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!(" {title}"),
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
    ))
}

/// One "keys — what they do" row, with the keys in a fixed-width column so
/// the descriptions line up down the page.
fn help_key(keys: &str, what: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("   {:<14}", keys),
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        ),
        Span::styled(what.to_string(), Style::default().fg(Color::White)),
    ])
}

fn help_note(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("   {text}"),
        Style::default().fg(Color::DarkGray),
    ))
}

fn help_lines() -> Vec<Line<'static>> {
    let mut lines = vec![Line::from("")];

    lines.push(help_head("SEARCHING YOUTUBE"));
    lines.push(help_key("/  or  s", "Open the search box"));
    lines.push(help_key("Enter", "Run the search (while the box is open)"));
    lines.push(help_key("Esc", "Close the search box without searching"));
    lines.push(help_key("← → Home End", "Move the text cursor while typing"));
    lines.push(help_key("j / k  ↓ / ↑", "Move through the results"));
    lines.push(help_key("Enter", "Play the highlighted result (clears the queue)"));
    lines.push(help_key("f", "Add the highlighted result to the queue"));
    lines.push(help_key("a", "Save the highlighted track to one of your playlists"));
    lines.push(help_note("More results load by themselves as you scroll down."));
    lines.push(help_note(
        "Rows marked [PLAYLIST] are YouTube playlists: Enter plays the whole thing,",
    ));
    lines.push(help_note("f queues it."));
    lines.push(Line::from(""));

    lines.push(help_head("PLAYBACK"));
    lines.push(help_key("Space", "Pause / resume"));
    lines.push(help_key("h / l", "Seek 5 seconds back / forward (← / → over results)"));
    lines.push(help_key("[  /  ]", "Previous track (history) / next track (queue)"));
    lines.push(help_key("{  /  }", "Previous / next chapter of the current track"));
    lines.push(help_key("+  /  -", "Volume up / down"));
    lines.push(help_key("r", "Loop the current track on / off"));
    lines.push(help_key("z", "Shuffle on / off — reorders what is queued, and new"));
    lines.push(help_note("tracks land at a random spot. Each one plays once and"));
    lines.push(help_note("leaves the queue."));
    lines.push(help_note("Media keys (play/pause/stop) work system-wide over MPRIS2."));
    lines.push(Line::from(""));

    lines.push(help_head("YOUR OWN PLAYLISTS"));
    lines.push(help_key("p", "Show your saved playlists (p or Esc goes back)"));
    lines.push(help_key("a", "Save the highlighted search result into a playlist"));
    lines.push(help_key("A", "Save the current track + queue as a new playlist"));
    lines.push(Line::from(""));
    lines.push(help_note("Inside the playlist view:"));
    lines.push(help_key("Tab  ← / →", "Switch between the playlist list and its tracks"));
    lines.push(help_key("Enter", "Play the playlist — over a track, start from there"));
    lines.push(help_key("f", "Queue the playlist — over a track, just that track"));
    lines.push(help_key("n", "Create a new, empty playlist"));
    lines.push(help_key("R", "Rename the highlighted playlist"));
    lines.push(help_key("x", "Delete the playlist / remove the track (asks first)"));
    lines.push(help_key("J / K", "Move the highlighted track down / up"));
    lines.push(Line::from(""));
    lines.push(help_note("Playlists are kept on this machine, in"));
    lines.push(help_note("~/.local/share/listen_to_it/playlists.json, and play without"));
    lines.push(help_note("searching YouTube again. To keep a YouTube playlist as your own,"));
    lines.push(help_note("queue it with f and then press A."));
    lines.push(Line::from(""));

    lines.push(help_head("THE REST"));
    lines.push(help_key("d", "Show / hide thumbnails"));
    lines.push(help_key("?  or  F1", "This help"));
    lines.push(help_key("q", "Quit"));
    lines.push(help_key("click", "Click the progress bar to jump to that point"));
    lines.push(Line::from(""));
    lines.push(help_note("A track that fails to start is retried a couple of times before"));
    lines.push(help_note("the queue moves on — YouTube's stream URLs 403 at random."));
    lines.push(help_note("The next track is resolved while the current one plays, so"));
    lines.push(help_note("changing tracks is usually instant."));
    lines.push(Line::from(""));
    lines.push(help_note("An installed copy updates itself: once a day it checks for a"));
    lines.push(help_note("newer release and installs it in the background, for the next"));
    lines.push(help_note("time you start. LISTEN_TO_IT_NO_UPDATE=1 turns that off."));
    lines.push(Line::from(""));

    lines
}

fn centered_rect(width_pct: u16, height: u16, area: Rect) -> Rect {
    let w = (area.width * width_pct / 100).min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

/// "1 track" / "4 tracks" — a count that reads like English wherever it lands.
fn tracks_label(n: usize) -> String {
    if n == 1 {
        "1 track".to_string()
    } else {
        format!("{n} tracks")
    }
}

pub fn fmt_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{:02}:{:02}:{:02}", h, m, s)
    } else {
        format!("{:02}:{:02}", m, s)
    }
}

pub fn fmt_views(v: u64) -> String {
    if v >= 1_000_000_000 {
        format!("{:.1}B", v as f64 / 1_000_000_000.0)
    } else if v >= 1_000_000 {
        format!("{:.1}M", v as f64 / 1_000_000.0)
    } else if v >= 1_000 {
        format!("{:.1}K", v as f64 / 1_000.0)
    } else {
        format!("{}", v)
    }
}

fn truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() > max {
        let t: String = chars[..max.saturating_sub(1)].iter().collect();
        format!("{}...", t)
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, AppMode, LibraryFocus, View};
    use crate::library::Library;
    use crate::youtube::VideoResult;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use ratatui_image::picker::Picker;

    fn app() -> App {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        // Keep the receiver alive; dropping it makes every send fail.
        std::mem::forget(rx);
        let mut app = App::new(tx, Picker::from_fontsize((8, 12)), false);
        // `App::new` loads the real user library; a render test must not
        // depend on what happens to be saved on the machine running it.
        app.library = Library::default();
        app
    }

    fn video(id: &str, title: &str) -> VideoResult {
        VideoResult {
            id: id.to_string(),
            title: title.to_string(),
            url: None,
            duration: Some(210.0),
            view_count: Some(1234),
            channel: Some("a channel".to_string()),
            uploader: None,
            thumbnail: None,
            is_playlist: false,
            playlist_count: None,
        }
    }

    /// Render into an off-screen terminal and return what landed on it, row
    /// by row.
    fn draw_rows(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }

    fn draw(app: &mut App, width: u16, height: u16) -> String {
        draw_rows(app, width, height).concat()
    }

    /// Every mode the app can be sitting in.
    const MODES: [AppMode; 7] = [
        AppMode::Normal,
        AppMode::Searching,
        AppMode::Confirming,
        AppMode::Naming,
        AppMode::PickingPlaylist,
        AppMode::ConfirmingDelete,
        AppMode::Help,
    ];

    /// The key handler is free to scroll past the end of the help; the render
    /// is the only place that knows how many lines fit, so it has to be what
    /// pulls the scroll back.
    #[test]
    fn help_scroll_is_clamped_to_the_last_line() {
        let mut app = app();
        app.mode = AppMode::Help;
        app.help_scroll = u16::MAX;

        let screen = draw(&mut app, 120, 40);

        assert!(app.help_scroll < u16::MAX, "End must not scroll into blank space");
        assert!(
            screen.contains("THE REST"),
            "scrolled to the end, the last section is what's on screen"
        );
    }

    #[test]
    fn help_opens_on_the_first_section() {
        let mut app = app();
        app.mode = AppMode::Help;
        let screen = draw(&mut app, 120, 40);
        assert!(screen.contains("SEARCHING YOUTUBE"));
        assert!(screen.contains("Help"), "the title says what this is");
    }

    /// Both playlist panes, and the tracks of the highlighted playlist.
    #[test]
    fn the_library_view_shows_playlists_and_their_tracks() {
        let mut app = app();
        let idx = app.library.create("Road trip");
        app.library.add_track(idx, &video("v1", "first song"));
        app.library.add_track(idx, &video("v2", "second song"));
        app.view = View::Library;

        let screen = draw(&mut app, 120, 40);

        assert!(screen.contains("Road trip"));
        assert!(screen.contains("2 tracks"));
        assert!(screen.contains("first song"));
        assert!(screen.contains("second song"));
    }

    #[test]
    fn an_empty_library_says_how_to_start_one() {
        let mut app = app();
        app.view = View::Library;
        let screen = draw(&mut app, 120, 40);
        assert!(screen.contains("No saved playlists yet"));
    }

    /// `?` can't be discovered by someone who doesn't already know about it,
    /// so the badge lives on the top line of the search block — the one thing
    /// drawn in every mode and both views, and the one place no dialog and no
    /// playing track can push it off.
    #[test]
    fn the_help_badge_is_always_on_the_top_line() {
        let mut app = app();
        app.now_playing = Some(video("v1", "playing now"));
        app.queue.push_back(video("v2", "up next"));
        app.library.create("Focus");
        app.pending_tracks = vec![video("v2", "up next")];
        app.delete_target = Some(DeleteTarget::Playlist(0));

        for view in [View::Search, View::Library] {
            for mode in MODES {
                app.view = view;
                app.mode = mode;
                let rows = draw_rows(&mut app, 100, 30);
                assert!(
                    rows[0].contains("[?]"),
                    "the help badge went missing from the top line in one of the modes"
                );
            }
        }
    }

    /// Every view and dialog has to survive a render at whatever size the
    /// terminal happens to be — a panic in here takes the terminal down with
    /// the app, mid-song.
    #[test]
    fn every_screen_renders_at_any_size() {
        let mut app = app();
        let idx = app.library.create("Focus");
        app.library.add_track(idx, &video("v1", "a song"));
        app.search_results = vec![video("v2", "a result")];
        app.now_playing = Some(video("v3", "playing now"));
        app.queue.push_back(video("v4", "up next"));
        app.pending_tracks = vec![video("v2", "a result")];
        app.confirm_title = Some("a result".to_string());
        app.delete_target = Some(DeleteTarget::Playlist(0));

        for view in [View::Search, View::Library] {
            for focus in [LibraryFocus::Playlists, LibraryFocus::Tracks] {
                for mode in MODES {
                    app.view = view;
                    app.library_focus = focus;
                    app.mode = mode;
                    for (w, h) in [(120, 40), (80, 24), (40, 12), (20, 6)] {
                        draw(&mut app, w, h);
                    }
                }
            }
        }
    }
}
