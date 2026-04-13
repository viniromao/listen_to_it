use crate::app::{App, AppMode};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use ratatui_image::{protocol::StatefulProtocol, Resize, StatefulImage};

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // search bar
            Constraint::Min(0),    // content
            Constraint::Length(3), // status / now playing
        ])
        .split(area);

    render_search_bar(frame, app, chunks[0]);
    render_content(frame, app, chunks[1]);
    render_status_bar(frame, app, chunks[2]);
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
            "Press / to search for music on YouTube\n\n[j/k or arrows] navigate  [Enter] play  [Space] pause  [</>] seek  [+/-] volume  [q] quit"
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

    let items: Vec<ListItem> = app
        .search_results
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let is_playing = playing_id == Some(r.id.as_str());
            let has_thumb = app.thumbnail_protocols.contains_key(&r.id);
            let loading_thumb = app.thumbnails_loading.contains(&r.id);

            let play_icon = if is_playing { ">> " } else { "   " };
            let thumb_icon = if !app.has_image_support {
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

            let title_style = if is_playing {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            };

            let title_line = Line::from(vec![
                Span::styled(play_icon, Style::default().fg(Color::Green)),
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

    let list = List::new(items)
        .block(
            Block::default()
                .title(format!(" Results ({}) ", app.search_results.len()))
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

    // Render thumbnail if supported
    if let Some(t_area) = thumb_area {
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
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::White),
        )),
        Line::from(""),
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
        Line::from(Span::styled(
            "[Enter] Play this track",
            Style::default().fg(Color::Green),
        )),
    ];

    let p = Paragraph::new(info).wrap(Wrap { trim: true });
    frame.render_widget(p, info_area);
}

fn render_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let (text, style) = if let Some(ref track) = app.now_playing {
        let icon = if app.is_paused { "|| " } else { ">  " };
        let pos = fmt_duration(app.current_position() as u64);
        let t = format!(
            " {} {}  |  {} elapsed  |  vol {}%  |  [Space] pause  [< >] seek  [+ -] vol  [q] quit",
            icon,
            truncate(&track.title, 38),
            pos,
            app.volume,
        );
        (t, Style::default().fg(Color::Green))
    } else {
        let t = " No track playing  |  [/] search  [arrows/jk] navigate  [Enter] play  [q] quit"
            .to_string();
        (t, Style::default().fg(Color::DarkGray))
    };

    let p = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL))
        .style(style);
    frame.render_widget(p, area);
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
