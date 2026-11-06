//! Rendering for the YoruPlay TUI.

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthChar;
use yoruplay_protocol::{
    NoticeLevel, PlaybackStatus, PlaylistSummary, SearchItems, SearchKind, LOGIN_URL,
};

use crate::app::{
    mode_label, search_kind_label, tr, App, Detail, Focus, PlaylistsOrigin, SongsOrigin,
};

pub fn draw(app: &mut App, frame: &mut Frame) {
    let area = frame.area();
    app.width = area.width;
    app.height = area.height;
    if area.width == 0 || area.height == 0 {
        return;
    }
    let compact = area.height < 10;
    let chunks = if compact {
        Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area)
    } else {
        Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(4),
            Constraint::Length(1),
        ])
        .split(area)
    };
    render_header(app, frame, chunks[0]);
    render_body(app, frame, chunks[1]);
    if !compact {
        render_player(app, frame, chunks[2]);
        render_footer(app, frame, chunks[3]);
    } else {
        render_footer(app, frame, chunks[2]);
    }
    if let Some(login) = &app.login {
        render_login(app, frame, area, login);
    }
    if app.help {
        render_help(app, frame, area);
    }
    if let Some(input) = &app.input {
        render_input(app, frame, area, input);
    }
    if let Some(confirm) = &app.confirm {
        render_confirm(app, frame, area, &confirm.message);
    }
}

fn render_header(app: &App, frame: &mut Frame, area: Rect) {
    let account = if app.auth.logged_in {
        app.auth
            .profile
            .as_ref()
            .map(|p| p.nickname.clone())
            .unwrap_or_else(|| tr(app.lang, "Logged in", "已登录").into())
    } else if app.auth.session_expired {
        tr(app.lang, "Session expired", "登录已过期").into()
    } else {
        tr(app.lang, "Not logged in", "未登录").into()
    };
    let mut spans = vec![
        Span::styled(
            " YoruPlay ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("│ ", Style::default().fg(Color::DarkGray)),
        Span::styled(account, Style::default().fg(Color::Gray)),
    ];
    if !app.warnings.is_empty() {
        spans.push(Span::styled(
            format!("  ⚠ {}", app.warnings.len()),
            Style::default().fg(Color::Yellow),
        ));
    }
    if app.playback.audio_available {
        spans.push(Span::styled(
            "  Rust audio",
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        spans.push(Span::styled(
            "  audio?",
            Style::default().fg(Color::DarkGray),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_body(app: &mut App, frame: &mut Frame, area: Rect) {
    let single = area.width < 90;
    if single {
        match app.focus {
            Focus::Menu => render_menu(app, frame, area),
            Focus::Detail => render_detail(app, frame, area),
        }
    } else {
        let chunks = Layout::horizontal([Constraint::Length(30), Constraint::Min(30)]).split(area);
        render_menu(app, frame, chunks[0]);
        render_detail(app, frame, chunks[1]);
    }
}

fn render_menu(app: &mut App, frame: &mut Frame, area: Rect) {
    let border_style = if app.focus == Focus::Menu {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Span::styled(
            tr(app.lang, " Navigation ", " 导航 "),
            Style::default().fg(Color::Cyan),
        ));
    let inner = block.inner(area);
    let visible = inner.height as usize;
    let len = app.menu.len();
    let offset = if len <= visible {
        0
    } else {
        app.menu_index
            .saturating_sub(visible / 2)
            .min(len.saturating_sub(visible))
    };
    let mut lines = Vec::new();
    for (index, entry) in app.menu.iter().enumerate().skip(offset).take(visible) {
        let selected = index == app.menu_index;
        let style = if entry.header {
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        } else if selected {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default()
        };
        let marker = if entry.header {
            ""
        } else if selected {
            "▸ "
        } else {
            "  "
        };
        let text = format!("{marker}{}", entry.label);
        lines.push(Line::styled(truncate(&text, inner.width as usize), style));
    }
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_detail(app: &mut App, frame: &mut Frame, area: Rect) {
    let focus_style = if app.focus == Focus::Detail {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let title = match &app.detail {
        Detail::Loading { .. } => format!(
            " {} {} ",
            spinner(app.spinner),
            tr(app.lang, "Loading", "加载中")
        ),
        Detail::Search { query, kind, .. } => format!(
            " Search \"{}\" [{}] ",
            truncate(query, 32),
            search_kind_label(app.lang, *kind)
        ),
        _ => format!(
            " {} ",
            truncate(&app.detail.title(), area.width.saturating_sub(4) as usize)
        ),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(focus_style)
        .title(Span::styled(title, Style::default().fg(Color::Cyan)));
    let inner = block.inner(area);
    let visible = inner.height as usize;
    app.list_height = visible;
    match &app.detail {
        Detail::Landing => {
            let lines = landing_lines(app);
            frame.render_widget(Paragraph::new(lines).block(block), area);
        }
        Detail::Playlists {
            title: _,
            origin,
            items,
            selected,
        } => {
            let lines = playlist_lines(app, *origin, items, *selected, visible, inner.width);
            frame.render_widget(Paragraph::new(lines).block(block), area);
        }
        Detail::Songs {
            view,
            origin,
            selected,
        } => {
            let lines = song_lines(app, *origin, view, *selected, visible, inner.width);
            frame.render_widget(Paragraph::new(lines).block(block), area);
        }
        Detail::Search {
            query: _,
            kind,
            page,
            selected,
            loading_more,
            has_more,
        } => {
            let lines = search_lines(
                app,
                *kind,
                SearchRender {
                    page: page.as_ref(),
                    selected: *selected,
                    loading_more: *loading_more,
                    has_more: *has_more,
                    visible,
                    width: inner.width,
                },
            );
            frame.render_widget(Paragraph::new(lines).block(block), area);
        }
        Detail::Queue { selected } => {
            let lines = queue_lines(app, *selected, visible, inner.width);
            frame.render_widget(Paragraph::new(lines).block(block), area);
        }
        Detail::Account => {
            let lines = account_lines(app);
            frame.render_widget(Paragraph::new(lines).block(block), area);
        }
        Detail::Loading { .. } => {
            let text = vec![
                Line::raw(""),
                Line::styled(
                    format!(
                        "{} {}",
                        spinner(app.spinner),
                        tr(app.lang, "Loading…", "加载中…")
                    ),
                    Style::default().fg(Color::Cyan),
                ),
            ];
            frame.render_widget(
                Paragraph::new(text)
                    .block(block)
                    .alignment(Alignment::Center),
                area,
            );
        }
        Detail::Message { title: _, body } => {
            let lines: Vec<Line> = body.lines().map(Line::raw).collect();
            frame.render_widget(
                Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
                area,
            );
        }
    }
}

fn landing_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::styled(
            tr(app.lang, "Welcome to YoruPlay", "欢迎使用 YoruPlay"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::raw(tr(
            app.lang,
            "Choose a section with j/k, then press Enter.",
            "使用 j/k 选择栏目，按回车进入。",
        )),
        Line::raw(tr(
            app.lang,
            "Tab switches between the navigation and the content pane.",
            "Tab 在导航栏与内容栏之间切换。",
        )),
        Line::raw(tr(
            app.lang,
            "Space toggles playback; n/p change tracks; m changes mode.",
            "空格暂停/继续；n/p 切歌；m 切换播放模式。",
        )),
        Line::raw(tr(
            app.lang,
            "/ searches; ? opens help; q closes only the TUI.",
            "/ 搜索；? 帮助；q 仅关闭 TUI，后台继续播放。",
        )),
    ];
    if !app.auth.logged_in {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            tr(
                app.lang,
                "Press L to log in through Firefox (needed for your library and playback).",
                "按 L 使用火狐浏览器登录（访问音乐库和播放需要登录）。",
            ),
            Style::default().fg(Color::Yellow),
        ));
    }
    if !app.warnings.is_empty() {
        lines.push(Line::raw(""));
        for warning in &app.warnings {
            lines.push(Line::styled(
                format!("⚠ {warning}"),
                Style::default().fg(Color::Yellow),
            ));
        }
    }
    lines
}

fn playlist_lines(
    app: &App,
    origin: PlaylistsOrigin,
    items: &[PlaylistSummary],
    selected: usize,
    visible: usize,
    width: u16,
) -> Vec<Line<'static>> {
    if items.is_empty() {
        return vec![Line::raw(tr(app.lang, "No playlists.", "暂无歌单。"))];
    }
    let _ = origin;
    rows(
        items
            .iter()
            .enumerate()
            .map(|(i, playlist)| {
                let text = format!(
                    "{:>4}  {}  ({} {})",
                    i + 1,
                    playlist.name,
                    playlist.track_count,
                    tr(app.lang, "tracks", "首")
                );
                (text, Style::default())
            })
            .collect(),
        selected,
        visible,
        width,
    )
}

fn song_lines(
    app: &App,
    origin: SongsOrigin,
    view: &yoruplay_protocol::SongsView,
    selected: usize,
    visible: usize,
    width: u16,
) -> Vec<Line<'static>> {
    let offset = app
        .playlist_page
        .filter(|page| matches!(origin, SongsOrigin::Playlist(id) if id == page.id))
        .map(|page| page.offset)
        .unwrap_or(0);
    if view.songs.is_empty() {
        return vec![Line::raw(tr(app.lang, "No songs.", "暂无歌曲。"))];
    }
    let current_id = app.playback.current.as_ref().map(|s| s.id);
    rows(
        view.songs
            .iter()
            .enumerate()
            .map(|(i, song)| {
                let playing = current_id == Some(song.id);
                let marker = if playing { "▶" } else { " " };
                let text = format!(
                    "{} {:>3}  {} — {}",
                    marker,
                    offset + i + 1,
                    song.name,
                    song.artist_text()
                );
                let style = if playing {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default()
                };
                (text, style)
            })
            .collect(),
        selected,
        visible,
        width,
    )
}

struct SearchRender<'a> {
    page: Option<&'a yoruplay_protocol::SearchPage>,
    selected: usize,
    loading_more: bool,
    has_more: bool,
    visible: usize,
    width: u16,
}

fn search_lines(app: &App, kind: SearchKind, view: SearchRender<'_>) -> Vec<Line<'static>> {
    let SearchRender {
        page,
        selected,
        loading_more,
        has_more,
        visible,
        width,
    } = view;
    let Some(page) = page else {
        return vec![Line::styled(
            format!(
                "{} {}",
                spinner(app.spinner),
                tr(app.lang, "Searching…", "搜索中…")
            ),
            Style::default().fg(Color::Cyan),
        )];
    };
    let current_id = app.playback.current.as_ref().map(|s| s.id);
    let rows_vec = match (&page.items, kind) {
        (SearchItems::Songs(songs), SearchKind::Songs) => songs
            .iter()
            .enumerate()
            .map(|(i, song)| {
                let playing = current_id == Some(song.id);
                let marker = if playing { "▶" } else { " " };
                (
                    format!(
                        "{} {:>3}  {} — {}",
                        marker,
                        i + 1,
                        song.name,
                        song.artist_text()
                    ),
                    if playing {
                        Style::default().fg(Color::Green)
                    } else {
                        Style::default()
                    },
                )
            })
            .collect(),
        (SearchItems::Artists(artists), SearchKind::Artists) => artists
            .iter()
            .enumerate()
            .map(|(i, artist)| {
                (
                    format!(
                        "{:>4}  {}  ({} albums, {} songs)",
                        i + 1,
                        artist.name,
                        artist.album_count,
                        artist.song_count
                    ),
                    Style::default(),
                )
            })
            .collect(),
        (SearchItems::Albums(albums), SearchKind::Albums) => albums
            .iter()
            .enumerate()
            .map(|(i, album)| {
                (
                    format!(
                        "{:>4}  {} — {} ({} tracks)",
                        i + 1,
                        album.name,
                        album.artist_name,
                        album.track_count
                    ),
                    Style::default(),
                )
            })
            .collect(),
        (SearchItems::Playlists(playlists), SearchKind::Playlists) => playlists
            .iter()
            .enumerate()
            .map(|(i, playlist)| {
                (
                    format!(
                        "{:>4}  {} ({} tracks)",
                        i + 1,
                        playlist.name,
                        playlist.track_count
                    ),
                    Style::default(),
                )
            })
            .collect(),
        _ => Vec::new(),
    };
    if rows_vec.is_empty() {
        let message = if loading_more {
            format!(
                "{} {}",
                spinner(app.spinner),
                tr(app.lang, "Loading more…", "加载更多…")
            )
        } else if has_more {
            tr(
                app.lang,
                "No results in this page; scroll to load more.",
                "本页无结果，请滚动加载更多。",
            )
            .into()
        } else {
            tr(app.lang, "No results.", "没有结果。").into()
        };
        return vec![Line::raw(message)];
    }
    rows(rows_vec, selected, visible, width)
}

fn queue_lines(app: &App, selected: usize, visible: usize, width: u16) -> Vec<Line<'static>> {
    if app.queue.items.is_empty() {
        return vec![Line::raw(tr(
            app.lang,
            "The queue is empty.",
            "播放队列为空。",
        ))];
    }
    let current = app.queue.current;
    rows(
        app.queue
            .items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let i = i + app.queue.offset;
                let marker = if current == Some(i) { "▶" } else { " " };
                (
                    format!(
                        "{} {:>4}  {} — {}",
                        marker,
                        i + 1,
                        item.song.name,
                        item.song.artist_text()
                    ),
                    if current == Some(i) {
                        Style::default().fg(Color::Green)
                    } else {
                        Style::default()
                    },
                )
            })
            .collect(),
        selected.saturating_sub(app.queue.offset),
        visible,
        width,
    )
}

fn account_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if app.auth.logged_in {
        if let Some(profile) = &app.auth.profile {
            lines.push(Line::styled(
                format!("{} ({})", profile.nickname, profile.user_id),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
            if let Some(signature) = &profile.signature {
                if !signature.is_empty() {
                    lines.push(Line::raw(truncate(signature, 72)));
                }
            }
        } else {
            lines.push(Line::raw(tr(app.lang, "Logged in", "已登录")));
        }
    } else if app.auth.session_expired {
        lines.push(Line::styled(
            tr(app.lang, "Session expired", "登录已过期"),
            Style::default().fg(Color::Yellow),
        ));
    } else {
        lines.push(Line::raw(tr(app.lang, "Not logged in", "未登录")));
    }
    lines.push(Line::raw(""));
    lines.push(Line::raw(tr(
        app.lang,
        "b  open the login page · i  import Firefox cookie",
        "b  打开登录页 · i  导入火狐 Cookie",
    )));
    lines.push(Line::raw(tr(app.lang, "o  log out", "o  退出登录")));
    lines.push(Line::raw(tr(
        app.lang,
        "q  close the TUI only (playback continues in the daemon)",
        "q  仅关闭 TUI（后台继续播放）",
    )));
    lines.push(Line::raw(""));
    lines.push(Line::raw(tr(
        app.lang,
        "Cookies are stored with mode 0600 and never shown in the UI or logs.",
        "Cookie 以 0600 权限保存，不会显示在界面或日志中。",
    )));
    lines
}

fn rows(
    mut rows: Vec<(String, Style)>,
    selected: usize,
    visible: usize,
    width: u16,
) -> Vec<Line<'static>> {
    if rows.is_empty() {
        return Vec::new();
    }
    let selected = selected.min(rows.len().saturating_sub(1));
    let visible = visible.max(1);
    let offset = selected
        .saturating_sub(visible / 2)
        .min(rows.len().saturating_sub(visible));
    let mut out = Vec::new();
    for (index, (text, style)) in rows.drain(..).enumerate().skip(offset).take(visible) {
        let is_selected = index == selected;
        let line_style = if is_selected {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            style
        };
        let prefix = if is_selected { "▸ " } else { "  " };
        let text = format!("{prefix}{text}");
        out.push(Line::styled(
            truncate(&text, width.saturating_sub(1) as usize),
            line_style,
        ));
    }
    out
}

fn render_player(app: &App, frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let p = &app.playback;
    let (icon, color) = match p.status {
        PlaybackStatus::Playing => ("▶", Color::Green),
        PlaybackStatus::Paused => ("⏸", Color::Yellow),
        PlaybackStatus::Stopped => ("■", Color::DarkGray),
        PlaybackStatus::Buffering => ("…", Color::Cyan),
    };
    let source = if p.source.personal_fm {
        "Personal FM".to_string()
    } else if let Some(playlist) = &p.source.playlist {
        playlist.name.clone()
    } else {
        String::new()
    };
    let mut line1 = vec![
        Span::styled(format!(" {icon} "), Style::default().fg(color)),
        Span::styled(
            p.current
                .as_ref()
                .map(|s| s.name.clone())
                .unwrap_or_else(|| tr(app.lang, "No track loaded", "未加载歌曲").into()),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(song) = &p.current {
        line1.push(Span::styled(
            format!(" — {}", song.artist_text()),
            Style::default().fg(Color::Gray),
        ));
    }
    line1.push(Span::styled(
        format!(
            "   [{}]  {:.0}%",
            mode_label(app.lang, p.mode),
            p.volume * 100.0
        ),
        Style::default().fg(Color::DarkGray),
    ));
    if !source.is_empty() {
        line1.push(Span::styled(
            format!("  · {source}"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    let text = format!(
        "{} / {}",
        format_time(p.position_ms),
        format_time(p.duration_ms)
    );
    let ratio = if p.duration_ms > 0 {
        (p.position_ms as f64 / p.duration_ms as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(inner);
    frame.render_widget(Paragraph::new(Line::from(line1)), chunks[0]);
    let gauge = Gauge::default()
        .gauge_style(Style::default().fg(Color::Cyan))
        .ratio(ratio)
        .label(text);
    frame.render_widget(gauge, chunks[1]);
}

fn render_footer(app: &App, frame: &mut Frame, area: Rect) {
    let (text, style) = if let Some(status) = &app.status {
        (
            status.text.clone(),
            Style::default().fg(match status.level {
                NoticeLevel::Info => Color::Cyan,
                NoticeLevel::Warning => Color::Yellow,
                NoticeLevel::Error => Color::Red,
            }),
        )
    } else if let Some(error) = &app.playback.error {
        (error.clone(), Style::default().fg(Color::Red))
    } else if let Some(warning) = &app.playback.warning {
        (warning.clone(), Style::default().fg(Color::Yellow))
    } else {
        (hint(app), Style::default().fg(Color::DarkGray))
    };
    frame.render_widget(
        Paragraph::new(Line::styled(truncate(&text, area.width as usize), style)),
        area,
    );
}

fn hint(app: &App) -> String {
    let base = if app.focus == Focus::Menu {
        tr(
            app.lang,
            "j/k move · Enter open · Tab pane · / search · Space pause · m mode · ? help · q quit",
            "j/k 移动 · 回车打开 · Tab 切换 · / 搜索 · 空格暂停 · m 模式 · ? 帮助 · q 退出",
        )
    } else {
        match &app.detail {
            Detail::Songs { .. } => tr(
                app.lang,
                "j/k move · Enter play · a add · A add all · r refresh · Esc back · Space pause · q quit",
                "j/k 移动 · 回车播放 · a 加入队列 · A 全部加入 · r 刷新 · Esc 返回 · 空格暂停 · q 退出",
            ),
            Detail::Playlists { .. } => tr(
                app.lang,
                "j/k move · Enter open · a/A add playlist · r refresh · Esc back · q quit",
                "j/k 移动 · 回车打开 · a/A 加入歌单 · r 刷新 · Esc 返回 · q 退出",
            ),
            Detail::Search { .. } => tr(
                app.lang,
                "j/k move · Enter open/play · Tab type · / new search · r rerun · Esc back · q quit",
                "j/k 移动 · 回车打开/播放 · Tab 类型 · / 新搜索 · r 重试 · Esc 返回 · q 退出",
            ),
            Detail::Queue { .. } => tr(
                app.lang,
                "j/k move · Enter play · r refresh · Esc back · Space pause · q quit",
                "j/k 移动 · 回车播放 · r 刷新 · Esc 返回 · 空格暂停 · q 退出",
            ),
            _ => tr(
                app.lang,
                "j/k move · Enter select · Esc back · / search · ? help · q quit",
                "j/k 移动 · 回车选择 · Esc 返回 · / 搜索 · ? 帮助 · q 退出",
            ),
        }
    };
    format!(" {base}")
}

fn render_help(app: &App, frame: &mut Frame, area: Rect) {
    let width = area.width.saturating_sub(4).clamp(20, 76);
    let height = area.height.saturating_sub(4).clamp(6, 24);
    let rect = centered(area, width, height);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            tr(app.lang, " Keyboard help ", " 键盘帮助 "),
            Style::default().fg(Color::Cyan),
        ));
    let lines = vec![
        Line::raw(tr(app.lang, "Navigation", "导航")),
        Line::raw("  j/k ↑/↓      move                 g/G  top/bottom"),
        Line::raw("  Tab h/l ←/→  switch pane          Enter open/play"),
        Line::raw("  Esc          back to navigation"),
        Line::raw(""),
        Line::raw(tr(app.lang, "Playback", "播放")),
        Line::raw("  Space        play/pause           n/p  next/previous"),
        Line::raw("  m            cycle mode           r    refresh view"),
        Line::raw("  a / A        add one / add all    q    quit TUI"),
        Line::raw(""),
        Line::raw(tr(app.lang, "Search", "搜索")),
        Line::raw("  /            open search          Enter execute"),
        Line::raw("  Tab          switch result type   r rerun"),
        Line::raw(""),
        Line::raw(tr(app.lang, "Account", "账号")),
        Line::raw("  L            login / account      o logout in account"),
        Line::raw(""),
        Line::raw(tr(
            app.lang,
            "q quits only the TUI; the daemon keeps playing.",
            "q 只退出 TUI，后台服务继续播放。",
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        rect,
    );
}

fn render_login(app: &App, frame: &mut Frame, area: Rect, login: &crate::app::LoginState) {
    let width = area.width.saturating_sub(4).clamp(24, 64);
    let height = area.height.saturating_sub(4).max(8);
    let rect = centered(area, width, height);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            tr(app.lang, " NetEase login ", " 网易云登录 "),
            Style::default().fg(Color::Cyan),
        ));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::styled(
        tr(
            app.lang,
            "Open the official login page:",
            "请打开官方登录页：",
        ),
        Style::default().fg(Color::Gray),
    ));
    for part in wrap_text(LOGIN_URL, inner.width as usize) {
        lines.push(Line::raw(part));
    }
    lines.push(Line::raw(""));
    for part in wrap_text(&login.status, inner.width as usize) {
        lines.push(Line::styled(part, Style::default().fg(Color::Gray)));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        truncate(
            tr(
                app.lang,
                "b open browser · i import Firefox cookie · o logout · Esc close",
                "b 打开浏览器 · i 导入火狐 Cookie · o 退出登录 · Esc 关闭",
            ),
            inner.width as usize,
        ),
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), inner);
}

fn render_input(app: &App, frame: &mut Frame, area: Rect, input: &crate::app::InputState) {
    let width = area.width.saturating_sub(4).clamp(24, 72);
    let height = 5;
    let rect = centered(area, width, height);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            tr(app.lang, " Search ", " 搜索 "),
            Style::default().fg(Color::Cyan),
        ));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let lines = vec![
        Line::raw(truncate(&input.prompt, inner.width as usize)),
        Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Cyan)),
            Span::raw(truncate(
                &input.value,
                inner.width.saturating_sub(2) as usize,
            )),
            Span::styled("█", Style::default().fg(Color::Cyan)),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_confirm(app: &App, frame: &mut Frame, area: Rect, message: &str) {
    let width = area.width.saturating_sub(4).clamp(20, 60);
    let height = 5;
    let rect = centered(area, width, height);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(Span::styled(
            tr(app.lang, " Confirm ", " 确认 "),
            Style::default().fg(Color::Yellow),
        ));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    frame.render_widget(
        Paragraph::new(Line::raw(truncate(message, inner.width as usize)))
            .alignment(Alignment::Center),
        inner,
    );
}

fn wrap_text(value: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    let mut used = 0;
    for ch in value.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > width && !current.is_empty() {
            out.push(current);
            current = String::new();
            used = 0;
        }
        current.push(ch);
        used += w;
    }
    if !current.is_empty() || out.is_empty() {
        out.push(current);
    }
    out
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width).max(1);
    let height = height.min(area.height).max(1);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

fn spinner(frame: usize) -> &'static str {
    const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
    FRAMES[frame % FRAMES.len()]
}

fn truncate(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in value.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out
}

fn format_time(ms: u64) -> String {
    let total = ms / 1000;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_display_width() {
        assert_eq!(truncate("abcdef", 3), "abc");
        assert_eq!(truncate("你好世界", 4), "你好");
        assert_eq!(truncate("a你b", 3), "a你");
        assert_eq!(truncate("", 4), "");
        assert_eq!(truncate("abc", 0), "");
    }

    #[test]
    fn wrap_text_splits_on_width() {
        assert_eq!(wrap_text("abcdef", 3), vec!["abc", "def"]);
        assert_eq!(wrap_text("你好世界", 4), vec!["你好", "世界"]);
        assert_eq!(wrap_text("", 4), vec![String::new()]);
    }

    #[test]
    fn time_format_handles_hours() {
        assert_eq!(format_time(0), "00:00");
        assert_eq!(format_time(65_000), "01:05");
        assert_eq!(format_time(3_665_000), "1:01:05");
    }
}
