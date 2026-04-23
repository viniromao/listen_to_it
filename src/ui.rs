use crate::app::{App, AppMode};

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
            Constraint::Length(queue_height),    // queue panel
            Constraint::Length(progress_height), // progress bar
            Constraint::Length(status_height),   // status bar
        ])
        .split(area);

    render_search_bar(frame, app, chunks[0]);
    render_content(frame, app, chunks[1]);
    if queue_height > 0 {
        render_queue(frame, app, chunks[2]);
    }
    if progress_height > 0 {
        render_progress(frame, app, chunks[3]);
    }
    render_status_bar(frame, app, chunks[4]);

    if app.mode == AppMode::Confirming {
        render_confirm_dialog(frame, app, area);
    }
}

fn render_search_bar(frame: &mut Frame, app: &App, area: Rect) {
    let active = app.mode == AppMode::Searching;
    let border_style = if active {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::White)
    };

    let cursor = if active { "█" } else { "" };
    let content = format!("{}{}", app.search_input, cursor);

    let widget = Paragraph::new(content).block(
        Block::default()
            .title(" Search  [/] focus  [Esc] cancel ")
            .borders(Borders::ALL)
            .border_style(border_style),
    );
    frame.render_widget(widget, area);
}

fn render_content(frame: &mut Frame, app: &mut App, area: Rect) {
    if app.search_results.is_empty() {
        let msg = if app.is_searching {
            "Searching YouTube..."
        } else if let Some(ref m) = app.status_message {
            m.as_str()
        } else {
            "Press / to search for music on YouTube\n\n[j/k or arrows] navigate  [Enter] play now  [f] add to queue  [Space] pause  [</>] seek  [+/-] volume  [q] quit"
        };
        let vert = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(30),
                Constraint::Min(0),
                Constraint::Percentage(30),
            ])
            .split(area);
        let p = Paragraph::new(msg)
            .block(Block::default().borders(Borders::NONE))
            .style(Style::default().fg(Color::DarkGray))
            .wrap(Wrap { trim: true });
        frame.render_widget(p, vert[1]);
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
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
            let title = truncate(&r.title, 36);
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
            } else {
                (
                    Style::default().add_modifier(Modifier::BOLD),
                    Style::default(),
                )
            };

            let title_line = Line::from(vec![
                Span::styled(play_icon, play_icon_style),
                Span::styled(thumb_icon, Style::default().fg(Color::Blue)),
                Span::raw(num),
                Span::styled(title, title_style),
            ]);
            let info_line = Line::from(Span::styled(
                format!("         {} . {} . {}", channel, dur, views),
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
            .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
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

    let info = vec![
        Line::from(Span::styled(
            truncate(&result.title, 38),
            Style::default().add_modifier(Modifier::BOLD).fg(Color::White),
        )),
        Line::from(vec![
            Span::styled("Channel:  ", Style::default().fg(Color::Cyan)),
            Span::raw(channel),
        ]),
        Line::from(vec![
            Span::styled("Duration: ", Style::default().fg(Color::Cyan)),
            Span::raw(duration),
        ]),
        Line::from(vec![
            Span::styled("Views:    ", Style::default().fg(Color::Cyan)),
            Span::raw(views),
        ]),
        Line::from(""),
        Line::from(Span::styled("[Enter] Play now (clear queue)", Style::default().fg(Color::Green))),
        Line::from(Span::styled("[f]     Add to queue",           Style::default().fg(Color::Yellow))),
        Line::from(""),
        Line::from(Span::styled("[Space] Pause / resume",         Style::default().fg(Color::DarkGray))),
        Line::from(Span::styled("[h/l]   Seek -/+5s",             Style::default().fg(Color::DarkGray))),
        Line::from(Span::styled("[[ ]]   Prev / next track",      Style::default().fg(Color::DarkGray))),
        Line::from(Span::styled("[{/}]   Prev / next chapter",    Style::default().fg(Color::DarkGray))),
        Line::from(Span::styled("[+/-]   Volume",                 Style::default().fg(Color::DarkGray))),
        Line::from(Span::styled("[r]     Toggle loop",            Style::default().fg(Color::DarkGray))),
        Line::from(Span::styled("[d]     Toggle thumbnails",      Style::default().fg(Color::DarkGray))),
        Line::from(Span::styled("[q]     Quit",                   Style::default().fg(Color::DarkGray))),
    ];

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
            let channel = v.channel_name().to_string();
            let line = Line::from(vec![
                Span::styled(
                    format!("{:2}. ", i + 1),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(truncate(&v.title, 50), Style::default().fg(Color::White)),
                Span::styled(
                    format!("  {} · {}", channel, dur),
                    Style::default().fg(Color::DarkGray),
                ),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .title(format!(" Queue ({}) — [[] prev  []] next ", app.queue.len()))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow)),
    );
    frame.render_widget(list, area);
}

fn render_progress(frame: &mut Frame, app: &mut App, area: Rect) {
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

fn render_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL);

    if let Some(ref track) = app.now_playing {
        let icon = if app.is_paused { "|| " } else { ">  " };
        let pos_val = app.current_position();
        let pos = fmt_duration(pos_val as u64);
        let queue_info = if app.queue.is_empty() {
            String::new()
        } else {
            format!("  |  Next: {}", truncate(&app.queue[0].title, 25))
        };
        let loop_info = if app.loop_mode { "  |  [LOOP]" } else { "" };

        let mut lines = vec![Line::from(Span::raw(format!(
            " {} {}  |  {} elapsed  |  vol {}%{}{}",
            icon,
            truncate(&track.title, 45),
            pos,
            app.volume,
            queue_info,
            loop_info,
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
    } else {
        let p = Paragraph::new(" No track playing  |  [/] search  [j/k] navigate  [Enter] play")
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
