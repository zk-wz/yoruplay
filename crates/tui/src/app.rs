use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use yoruplay_ipc::IpcClient;
use yoruplay_protocol::{
    AuthState, Command, CommandResult, ErrorInfo, Event, Language, NoticeLevel, PlayMode,
    PlaySource, PlaybackState, PlaybackStatus, PlaylistKind, PlaylistRef, PlaylistSummary,
    QueueSnapshot, SearchItems, SearchKind, SearchPage, SongsView, StateSnapshot, LOGIN_URL,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Menu,
    Detail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    DailySongs,
    DailyPlaylists,
    PersonalFm,
    Radar,
    Liked,
    MyPlaylists,
    Subscribed,
    Recent,
    Search,
    Queue,
    Account,
}

#[derive(Debug, Clone)]
pub struct MenuEntry {
    pub header: bool,
    pub label: String,
    pub action: Option<MenuAction>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaylistsOrigin {
    Daily,
    Created,
    Subscribed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SongsOrigin {
    Daily,
    Liked,
    Recent,
    Playlist(u64),
    Radar(u64),
    Artist(u64),
    Album(u64),
    Other,
}

#[derive(Debug, Clone)]
pub enum Detail {
    Landing,
    Playlists {
        title: String,
        origin: PlaylistsOrigin,
        items: Vec<PlaylistSummary>,
        selected: usize,
    },
    Songs {
        view: SongsView,
        origin: SongsOrigin,
        selected: usize,
    },
    Search {
        query: String,
        kind: SearchKind,
        page: Option<SearchPage>,
        selected: usize,
        loading_more: bool,
        has_more: bool,
    },
    Queue {
        selected: usize,
    },
    Account,
    Loading {
        title: String,
    },
    Message {
        title: String,
        body: String,
    },
}

impl Detail {
    pub fn title(&self) -> String {
        match self {
            Detail::Landing => String::new(),
            Detail::Playlists { title, .. } => title.clone(),
            Detail::Songs { view, .. } => view.title.clone(),
            Detail::Search { query, kind, .. } => {
                format!(
                    "Search \"{query}\" [{}]",
                    search_kind_label(Language::En, *kind)
                )
            }
            Detail::Queue { .. } => "Queue".into(),
            Detail::Account => "Account".into(),
            Detail::Loading { title } => title.clone(),
            Detail::Message { title, .. } => title.clone(),
        }
    }

    pub fn len(&self, queue_len: usize) -> usize {
        match self {
            Detail::Playlists { items, .. } => items.len(),
            Detail::Songs { view, .. } => view.songs.len(),
            Detail::Search { page, .. } => page.as_ref().map(|p| p.items.len()).unwrap_or(0),
            Detail::Queue { .. } => queue_len,
            _ => 0,
        }
    }

    pub fn selected(&self) -> usize {
        match self {
            Detail::Playlists { selected, .. } => *selected,
            Detail::Songs { selected, .. } => *selected,
            Detail::Search { selected, .. } => *selected,
            Detail::Queue { selected } => *selected,
            _ => 0,
        }
    }

    pub fn set_selected(&mut self, value: usize) {
        match self {
            Detail::Playlists {
                selected, items, ..
            } => {
                *selected = value.min(items.len().saturating_sub(1));
            }
            Detail::Songs { selected, view, .. } => {
                *selected = value.min(view.songs.len().saturating_sub(1));
            }
            Detail::Search { selected, page, .. } => {
                let len = page.as_ref().map(|p| p.items.len()).unwrap_or(0);
                *selected = value.min(len.saturating_sub(1));
            }
            Detail::Queue { selected } => *selected = value,
            _ => {}
        }
    }
}

#[derive(Debug, Clone)]
pub struct StatusMessage {
    pub level: NoticeLevel,
    pub text: String,
    pub at: Instant,
}

#[derive(Debug, Clone)]
pub struct InputState {
    pub prompt: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct LoginState {
    pub status: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmAction {
    Logout,
}

#[derive(Debug, Clone)]
pub struct ConfirmState {
    pub action: ConfirmAction,
    pub message: String,
}

#[derive(Debug, Clone)]
pub enum RequestKind {
    InitialState,
    RefreshState,
    DailySongs {
        refresh: bool,
    },
    DailyPlaylists {
        refresh: bool,
    },
    Playlists {
        origin: PlaylistsOrigin,
    },
    PlaylistTracks {
        id: u64,
        title: String,
    },
    LikedSongs,
    RecentSongs,
    Radar {
        refresh: bool,
    },
    ArtistSongs {
        id: u64,
        name: String,
    },
    AlbumSongs {
        id: u64,
        name: String,
    },
    Search {
        generation: u64,
        query: String,
        kind: SearchKind,
        offset: usize,
    },
    StartPersonalFm,
    BrowserLogin,
    Logout,
    Generic {
        label: String,
    },
}

#[derive(Debug)]
pub struct UiResponse {
    pub kind: RequestKind,
    pub result: Result<CommandResult, ErrorInfo>,
}

pub struct App {
    pub lang: Language,
    pub menu: Vec<MenuEntry>,
    pub menu_index: usize,
    pub focus: Focus,
    pub detail: Detail,
    pub queue: QueueSnapshot,
    pub playback: PlaybackState,
    pub auth: AuthState,
    pub warnings: Vec<String>,
    pub status: Option<StatusMessage>,
    pub input: Option<InputState>,
    pub login: Option<LoginState>,
    pub help: bool,
    pub confirm: Option<ConfirmState>,
    pub quit: bool,
    pub dirty: bool,
    pub list_height: usize,
    pub spinner: usize,
    pub width: u16,
    pub height: u16,
    pub search_generation: u64,
    pub need_daily_after_login: bool,
    client: Arc<IpcClient>,
    responses: mpsc::Sender<UiResponse>,
}

impl App {
    pub fn new(
        client: Arc<IpcClient>,
        responses: mpsc::Sender<UiResponse>,
        lang: Language,
    ) -> Self {
        let menu = build_menu(lang);
        let menu_index = menu
            .iter()
            .position(|entry| entry.action == Some(MenuAction::DailySongs))
            .unwrap_or(0);
        Self {
            lang,
            menu,
            menu_index,
            focus: Focus::Menu,
            detail: Detail::Landing,
            queue: QueueSnapshot {
                items: Vec::new(),
                current: None,
                source: PlaySource::none(),
                mode: PlayMode::ListLoop,
            },
            playback: default_playback(),
            auth: AuthState {
                logged_in: false,
                profile: None,
                session_expired: false,
            },
            warnings: Vec::new(),
            status: None,
            input: None,
            login: None,
            help: false,
            confirm: None,
            quit: false,
            dirty: true,
            list_height: 10,
            spinner: 0,
            width: 100,
            height: 30,
            search_generation: 0,
            need_daily_after_login: true,
            client,
            responses,
        }
    }

    pub fn initial_load(&mut self) {
        self.send(RequestKind::InitialState, Command::GetState);
    }

    pub fn send_external(&self, kind: RequestKind, command: Command) {
        self.send(kind, command);
    }

    fn send(&self, kind: RequestKind, command: Command) {
        let client = self.client.clone();
        let tx = self.responses.clone();
        tokio::spawn(async move {
            let result = client.request(command).await;
            let _ = tx.send(UiResponse { kind, result }).await;
        });
    }

    pub fn set_status(&mut self, level: NoticeLevel, text: impl Into<String>) {
        self.status = Some(StatusMessage {
            level,
            text: text.into(),
            at: Instant::now(),
        });
        self.dirty = true;
    }

    fn status_error(&mut self, error: ErrorInfo) {
        let mut text = error.message;
        if let Some(hint) = error.hint {
            text.push_str(" — ");
            text.push_str(&hint);
        }
        self.set_status(NoticeLevel::Error, text);
    }

    pub fn handle_event(&mut self, event: Event) {
        match event {
            Event::Playback(playback) => {
                let status_changed = self.playback.status != playback.status;
                self.playback = *playback;
                if status_changed {
                    self.dirty = true;
                }
                self.dirty = true;
            }
            Event::Queue(queue) => {
                self.queue = *queue;
                if let Detail::Queue { selected } = &mut self.detail {
                    *selected = (*selected).min(self.queue.items.len().saturating_sub(1));
                }
                self.dirty = true;
            }
            Event::Auth(auth) => {
                let logged_in = auth.logged_in;
                let expired = auth.session_expired;
                self.auth = auth;
                if logged_in {
                    if self.login.is_some() {
                        self.login = None;
                    }
                    if self.need_daily_after_login {
                        self.need_daily_after_login = false;
                        self.load_daily_songs(false);
                    }
                } else if expired && self.login.is_none() {
                    self.set_status(
                        NoticeLevel::Warning,
                        "Your NetEase session expired. Press L to log in.",
                    );
                    self.open_login();
                }
                self.dirty = true;
            }
            Event::Notice(notice) => {
                self.set_status(notice.level, notice.message);
            }
            Event::Shutdown => {
                self.quit = true;
                self.set_status(NoticeLevel::Warning, "The YoruPlay daemon stopped.");
            }
        }
    }

    pub fn handle_response(&mut self, response: UiResponse) {
        let UiResponse { kind, result } = response;
        match kind {
            RequestKind::InitialState => match result {
                Ok(CommandResult::State(snapshot)) => {
                    self.apply_snapshot(*snapshot);
                    if !self.auth.logged_in || self.auth.session_expired {
                        self.open_login();
                    } else if self.need_daily_after_login {
                        self.need_daily_after_login = false;
                        self.load_daily_songs(false);
                    }
                }
                Ok(other) => {
                    self.detail = Detail::Message {
                        title: "Initial state".into(),
                        body: format!("Unexpected response: {other:?}"),
                    };
                }
                Err(error) => self.status_error(error),
            },
            RequestKind::RefreshState => {
                if let Ok(CommandResult::State(snapshot)) = result {
                    self.apply_snapshot(*snapshot);
                }
            }
            RequestKind::DailySongs { .. } => match result {
                Ok(CommandResult::Songs(view)) => {
                    self.detail = Detail::Songs {
                        view,
                        origin: SongsOrigin::Daily,
                        selected: 0,
                    };
                    self.focus = Focus::Detail;
                }
                Ok(other) => self.show_unexpected(other),
                Err(error) => self.status_error(error),
            },
            RequestKind::DailyPlaylists { .. } => match result {
                Ok(CommandResult::Playlists(items)) => {
                    self.detail = Detail::Playlists {
                        title: tr(self.lang, "Daily Playlists", "每日推荐歌单").into(),
                        origin: PlaylistsOrigin::Daily,
                        items,
                        selected: 0,
                    };
                    self.focus = Focus::Detail;
                }
                Ok(other) => self.show_unexpected(other),
                Err(error) => self.status_error(error),
            },
            RequestKind::Playlists { origin } => match result {
                Ok(CommandResult::Playlists(items)) => {
                    let title = match origin {
                        PlaylistsOrigin::Daily => tr(self.lang, "Daily Playlists", "每日推荐歌单"),
                        PlaylistsOrigin::Created => tr(self.lang, "My Playlists", "我创建的歌单"),
                        PlaylistsOrigin::Subscribed => {
                            tr(self.lang, "Subscribed Playlists", "我收藏的歌单")
                        }
                    };
                    self.detail = Detail::Playlists {
                        title: title.into(),
                        origin,
                        items,
                        selected: 0,
                    };
                    self.focus = Focus::Detail;
                }
                Ok(other) => self.show_unexpected(other),
                Err(error) => self.status_error(error),
            },
            RequestKind::PlaylistTracks { id, title } => match result {
                Ok(CommandResult::PlaylistTracks(tracks)) => {
                    let context = Some(PlaylistRef {
                        id: tracks.playlist.id,
                        name: tracks.playlist.name.clone(),
                    });
                    self.detail = Detail::Songs {
                        view: SongsView {
                            title: tracks.playlist.name.clone(),
                            description: tracks.playlist.description.clone(),
                            songs: tracks.songs,
                            context,
                        },
                        origin: SongsOrigin::Playlist(id),
                        selected: 0,
                    };
                    self.focus = Focus::Detail;
                    if self.detail.title().is_empty() {
                        self.set_status(NoticeLevel::Info, title);
                    }
                }
                Ok(other) => self.show_unexpected(other),
                Err(error) => self.status_error(error),
            },
            RequestKind::LikedSongs => match result {
                Ok(CommandResult::Songs(view)) => {
                    self.detail = Detail::Songs {
                        view,
                        origin: SongsOrigin::Liked,
                        selected: 0,
                    };
                    self.focus = Focus::Detail;
                }
                Ok(other) => self.show_unexpected(other),
                Err(error) => self.status_error(error),
            },
            RequestKind::RecentSongs => match result {
                Ok(CommandResult::Songs(view)) => {
                    self.detail = Detail::Songs {
                        view,
                        origin: SongsOrigin::Recent,
                        selected: 0,
                    };
                    self.focus = Focus::Detail;
                }
                Ok(other) => self.show_unexpected(other),
                Err(error) => self.status_error(error),
            },
            RequestKind::Radar { .. } => match result {
                Ok(CommandResult::PlaylistTracks(tracks)) => {
                    let id = tracks.playlist.id;
                    let context = Some(PlaylistRef {
                        id,
                        name: tracks.playlist.name.clone(),
                    });
                    self.detail = Detail::Songs {
                        view: SongsView {
                            title: tracks.playlist.name.clone(),
                            description: tracks.playlist.description.clone(),
                            songs: tracks.songs,
                            context,
                        },
                        origin: SongsOrigin::Radar(id),
                        selected: 0,
                    };
                    self.focus = Focus::Detail;
                }
                Ok(other) => self.show_unexpected(other),
                Err(error) => self.status_error(error),
            },
            RequestKind::ArtistSongs { id, name } => match result {
                Ok(CommandResult::Songs(mut view)) => {
                    view.title = format!("{} — {}", name, view.title);
                    self.detail = Detail::Songs {
                        view,
                        origin: SongsOrigin::Artist(id),
                        selected: 0,
                    };
                    self.focus = Focus::Detail;
                }
                Ok(other) => self.show_unexpected(other),
                Err(error) => self.status_error(error),
            },
            RequestKind::AlbumSongs { id, name } => match result {
                Ok(CommandResult::Songs(mut view)) => {
                    view.title = format!("{} — {}", name, view.title);
                    self.detail = Detail::Songs {
                        view,
                        origin: SongsOrigin::Album(id),
                        selected: 0,
                    };
                    self.focus = Focus::Detail;
                }
                Ok(other) => self.show_unexpected(other),
                Err(error) => self.status_error(error),
            },
            RequestKind::Search {
                generation,
                query,
                kind,
                offset,
            } => {
                if generation != self.search_generation {
                    return;
                }
                match result {
                    Ok(CommandResult::Search(page)) => {
                        if let Detail::Search {
                            query: current_query,
                            kind: current_kind,
                            page: current_page,
                            selected,
                            loading_more,
                            has_more,
                        } = &mut self.detail
                        {
                            if current_query != &query || *current_kind != kind {
                                return;
                            }
                            if offset == 0 || current_page.is_none() {
                                *selected = 0;
                                *current_page = Some(page);
                            } else if let Some(existing) = current_page {
                                append_page(existing, page);
                            }
                            *has_more = current_page.as_ref().map(|p| p.has_more).unwrap_or(false);
                            *loading_more = false;
                        }
                        self.focus = Focus::Detail;
                    }
                    Err(error) => {
                        self.status_error(error);
                        if let Detail::Search {
                            query,
                            kind,
                            page,
                            selected,
                            loading_more,
                            has_more,
                        } = &mut self.detail
                        {
                            if page.is_none() {
                                *page = Some(empty_search_page(query, *kind));
                                *selected = 0;
                            }
                            *loading_more = false;
                            *has_more = page.as_ref().is_some_and(|p| p.has_more);
                        }
                    }
                    _ => {}
                }
            }
            RequestKind::StartPersonalFm => match result {
                Ok(CommandResult::Ack { message }) => {
                    self.set_status(
                        NoticeLevel::Info,
                        message.unwrap_or_else(|| {
                            tr(self.lang, "Personal FM started", "私人漫游已开始").into()
                        }),
                    );
                    self.detail = Detail::Queue { selected: 0 };
                    self.focus = Focus::Detail;
                }
                Ok(other) => self.show_unexpected(other),
                Err(error) => self.status_error(error),
            },
            RequestKind::BrowserLogin => match result {
                Ok(CommandResult::Auth(auth)) => {
                    self.auth = auth;
                    self.login = None;
                    self.set_status(NoticeLevel::Info, tr(self.lang, "Logged in.", "登录成功。"));
                    self.need_daily_after_login = false;
                    self.load_daily_songs(false);
                }
                Err(error) => {
                    if let Some(login) = &mut self.login {
                        login.status = error.message.clone();
                    }
                    self.status_error(error);
                }
                Ok(other) => self.show_unexpected(other),
            },
            RequestKind::Logout => match result {
                Ok(CommandResult::Auth(auth)) => {
                    self.auth = auth;
                    self.login = None;
                    self.set_status(
                        NoticeLevel::Info,
                        tr(self.lang, "Logged out.", "已退出登录。"),
                    );
                }
                Ok(other) => self.show_unexpected(other),
                Err(error) => self.status_error(error),
            },
            RequestKind::Generic { label } => match result {
                Ok(result) => match result {
                    CommandResult::Ack { message } => {
                        if let Some(message) = message {
                            self.set_status(NoticeLevel::Info, message);
                        } else if !label.is_empty() {
                            self.set_status(NoticeLevel::Info, label);
                        }
                    }
                    other => self.show_unexpected(other),
                },
                Err(error) => self.status_error(error),
            },
        }
        self.dirty = true;
    }

    fn show_unexpected(&mut self, result: CommandResult) {
        self.set_status(
            NoticeLevel::Error,
            format!("Unexpected daemon response: {result:?}"),
        );
    }

    pub fn apply_snapshot(&mut self, snapshot: StateSnapshot) {
        self.playback = snapshot.playback;
        self.queue = snapshot.queue;
        self.auth = snapshot.auth;
        self.warnings = snapshot.warnings;
        self.dirty = true;
    }

    pub fn open_login(&mut self) {
        if self.login.is_none() {
            let status = match self.lang {
                Language::Zh => format!(
                    "请在火狐浏览器打开 {LOGIN_URL} 登录，然后按 i 导入 Cookie；按 b 打开页面。"
                ),
                Language::En => format!(
                    "Log in at {LOGIN_URL} in Firefox, then press i to import the cookie. Press b to open the page."
                ),
            };
            self.login = Some(LoginState { status });
        }
        self.dirty = true;
    }

    fn import_browser_cookie(&mut self) {
        if let Some(login) = &mut self.login {
            login.status = tr(
                self.lang,
                "Reading the Firefox cookie database…",
                "正在读取火狐 Cookie 数据库…",
            )
            .into();
        }
        self.send(RequestKind::BrowserLogin, Command::ImportBrowserCookie);
        self.dirty = true;
    }

    fn open_login_page(&mut self) {
        let result = std::process::Command::new("xdg-open")
            .arg(LOGIN_URL)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Some(login) = &mut self.login {
            login.status = match result {
                Ok(_) => tr(
                    self.lang,
                    "Opened the NetEase login page. Log in, then press i.",
                    "已打开网易云登录页。登录后按 i 导入。",
                )
                .into(),
                Err(e) => format!("could not open browser: {e}"),
            };
        }
        self.dirty = true;
    }

    pub fn tick(&mut self) {
        self.spinner = (self.spinner + 1) % 8;
        if let Some(status) = &self.status {
            if status.at.elapsed() > Duration::from_secs(8) {
                self.status = None;
                self.dirty = true;
            }
        }
        if matches!(self.detail, Detail::Loading { .. }) {
            self.dirty = true;
        }
    }

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        if key.kind != crossterm::event::KeyEventKind::Press {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            self.quit = true;
            return;
        }
        if self.help {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q')
            ) {
                self.help = false;
                self.dirty = true;
            }
            return;
        }
        if let Some(confirm) = self.confirm.clone() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.confirm = None;
                    match confirm.action {
                        ConfirmAction::Logout => {
                            self.send(RequestKind::Logout, Command::Logout);
                        }
                    }
                    self.dirty = true;
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.confirm = None;
                    self.dirty = true;
                }
                _ => {}
            }
            return;
        }
        if self.input.is_some() {
            match key.code {
                KeyCode::Esc => {
                    self.input = None;
                    self.dirty = true;
                }
                KeyCode::Enter => {
                    if let Some(input) = self.input.take() {
                        let query = input.value.trim().to_string();
                        if !query.is_empty() {
                            let kind = match &self.detail {
                                Detail::Search { kind, .. } => *kind,
                                _ => SearchKind::Songs,
                            };
                            self.run_search(query, kind);
                        }
                    }
                    self.dirty = true;
                }
                KeyCode::Backspace => {
                    if let Some(input) = &mut self.input {
                        input.value.pop();
                    }
                    self.dirty = true;
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(input) = &mut self.input {
                        input.value.push(c);
                    }
                    self.dirty = true;
                }
                _ => {}
            }
            return;
        }
        if self.login.is_some() {
            match key.code {
                KeyCode::Esc => {
                    self.login = None;
                    self.dirty = true;
                }
                KeyCode::Char('i') => self.import_browser_cookie(),
                KeyCode::Char('b') => self.open_login_page(),
                KeyCode::Char('o') => {
                    self.confirm = Some(ConfirmState {
                        action: ConfirmAction::Logout,
                        message: tr(
                            self.lang,
                            "Log out of the NetEase account? (y/n)",
                            "确定退出网易云账号登录？(y/n)",
                        )
                        .into(),
                    });
                    self.dirty = true;
                }
                KeyCode::Char('q') => self.quit = true,
                _ => {}
            }
            return;
        }

        if self.focus == Focus::Detail && matches!(self.detail, Detail::Account) {
            match key.code {
                KeyCode::Char('l') => {
                    self.open_login();
                    return;
                }
                KeyCode::Char('o') => {
                    self.confirm = Some(ConfirmState {
                        action: ConfirmAction::Logout,
                        message: tr(
                            self.lang,
                            "Log out of the NetEase account? (y/n)",
                            "确定退出网易云账号登录？(y/n)",
                        )
                        .into(),
                    });
                    self.dirty = true;
                    return;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Esc => {
                if self.focus == Focus::Detail {
                    self.focus = Focus::Menu;
                    self.dirty = true;
                }
            }
            KeyCode::Tab => {
                if self.focus == Focus::Detail {
                    if let Detail::Search { query, kind, .. } = &self.detail {
                        let query = query.clone();
                        let kind = kind.next();
                        self.run_search(query, kind);
                    } else {
                        self.focus = Focus::Menu;
                        self.dirty = true;
                    }
                } else {
                    self.focus = Focus::Detail;
                    self.dirty = true;
                }
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.focus = Focus::Menu;
                self.dirty = true;
            }
            KeyCode::Right | KeyCode::Char('l') => {
                if !matches!(self.detail, Detail::Landing) {
                    self.focus = Focus::Detail;
                    self.dirty = true;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::Home | KeyCode::Char('g') => self.move_to_start(),
            KeyCode::End | KeyCode::Char('G') => self.move_to_end(),
            KeyCode::Enter => {
                if self.focus == Focus::Menu {
                    self.activate_menu();
                } else {
                    self.activate_detail();
                }
            }
            KeyCode::Char(' ') => {
                self.send(
                    RequestKind::Generic {
                        label: String::new(),
                    },
                    Command::PlayPause,
                );
            }
            KeyCode::Char('n') => {
                self.send(
                    RequestKind::Generic {
                        label: String::new(),
                    },
                    Command::Next,
                );
            }
            KeyCode::Char('p') => {
                self.send(
                    RequestKind::Generic {
                        label: String::new(),
                    },
                    Command::Previous,
                );
            }
            KeyCode::Char('m') => {
                self.send(
                    RequestKind::Generic {
                        label: String::new(),
                    },
                    Command::CycleMode,
                );
            }
            KeyCode::Char('a') => self.add_selected(false),
            KeyCode::Char('A') => self.add_selected(true),
            KeyCode::Char('r') => self.refresh_detail(),
            KeyCode::Char('/') => self.open_search_input(),
            KeyCode::Char('L') => self.open_login(),
            KeyCode::Char('?') => {
                self.help = true;
                self.dirty = true;
            }
            KeyCode::Char('q') => self.quit = true,
            _ => {}
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.focus == Focus::Menu {
            let len = self.menu.len();
            if len == 0 {
                return;
            }
            let mut index = self.menu_index as isize;
            loop {
                index += delta.signum();
                if index < 0 {
                    index = (len - 1) as isize;
                } else if index >= len as isize {
                    index = 0;
                }
                if self.menu[index as usize].action.is_some() {
                    break;
                }
                if index as usize == self.menu_index {
                    break;
                }
            }
            self.menu_index = index as usize;
        } else {
            let max = self.detail.len(self.queue.items.len());
            if max == 0 {
                return;
            }
            let current = self.detail.selected() as isize;
            let next = (current + delta).clamp(0, max as isize - 1) as usize;
            self.detail.set_selected(next);
            self.maybe_load_more_search();
        }
        self.dirty = true;
    }

    fn move_to_start(&mut self) {
        if self.focus == Focus::Menu {
            if let Some(index) = self.menu.iter().position(|entry| entry.action.is_some()) {
                self.menu_index = index;
            }
        } else {
            self.detail.set_selected(0);
        }
        self.dirty = true;
    }

    fn move_to_end(&mut self) {
        if self.focus == Focus::Menu {
            if let Some(index) = self.menu.iter().rposition(|entry| entry.action.is_some()) {
                self.menu_index = index;
            }
        } else {
            self.detail.set_selected(usize::MAX);
            self.maybe_load_more_search();
        }
        self.dirty = true;
    }

    fn activate_menu(&mut self) {
        let Some(action) = self
            .menu
            .get(self.menu_index)
            .and_then(|entry| entry.action)
        else {
            return;
        };
        match action {
            MenuAction::DailySongs => self.load_daily_songs(false),
            MenuAction::DailyPlaylists => self.load_daily_playlists(false),
            MenuAction::PersonalFm => {
                self.detail = Detail::Loading {
                    title: tr(self.lang, "Starting Personal FM", "正在启动私人漫游").into(),
                };
                self.focus = Focus::Detail;
                self.send(RequestKind::StartPersonalFm, Command::PersonalFm);
            }
            MenuAction::Radar => self.load_radar(false),
            MenuAction::Liked => {
                self.detail = Detail::Loading {
                    title: tr(self.lang, "Loading liked songs", "正在加载喜欢的音乐").into(),
                };
                self.focus = Focus::Detail;
                self.send(RequestKind::LikedSongs, Command::LikedSongs);
            }
            MenuAction::MyPlaylists => self.load_playlists(PlaylistsOrigin::Created),
            MenuAction::Subscribed => self.load_playlists(PlaylistsOrigin::Subscribed),
            MenuAction::Recent => {
                self.detail = Detail::Loading {
                    title: tr(self.lang, "Loading recently played", "正在加载最近播放").into(),
                };
                self.focus = Focus::Detail;
                self.send(RequestKind::RecentSongs, Command::RecentSongs);
            }
            MenuAction::Search => self.open_search_input(),
            MenuAction::Queue => {
                self.detail = Detail::Queue { selected: 0 };
                self.focus = Focus::Detail;
                self.dirty = true;
            }
            MenuAction::Account => {
                self.detail = Detail::Account;
                self.focus = Focus::Detail;
                self.dirty = true;
            }
        }
    }

    fn activate_detail(&mut self) {
        match &self.detail {
            Detail::Songs { view, selected, .. } => {
                if view.songs.is_empty() {
                    return;
                }
                let index = (*selected).min(view.songs.len() - 1);
                let command = Command::PlaySongs {
                    songs: view.songs.clone(),
                    start_index: index,
                    context: view.context.clone(),
                    title: Some(view.title.clone()),
                };
                self.send(
                    RequestKind::Generic {
                        label: "Playing".into(),
                    },
                    command,
                );
            }
            Detail::Playlists {
                items, selected, ..
            } => {
                if let Some(playlist) = items.get(*selected) {
                    self.load_playlist(playlist.id, playlist.name.clone());
                }
            }
            Detail::Search {
                page: Some(page),
                selected,
                ..
            } => {
                let selected = *selected;
                match &page.items {
                    SearchItems::Songs(songs) => {
                        if songs.is_empty() {
                            return;
                        }
                        let index = selected.min(songs.len() - 1);
                        self.send(
                            RequestKind::Generic {
                                label: "Playing".into(),
                            },
                            Command::PlaySongs {
                                songs: songs.clone(),
                                start_index: index,
                                context: None,
                                title: Some(format!("Search: {}", page.query)),
                            },
                        );
                    }
                    SearchItems::Artists(artists) => {
                        if let Some(artist) = artists.get(selected) {
                            self.load_artist(artist.id, artist.name.clone());
                        }
                    }
                    SearchItems::Albums(albums) => {
                        if let Some(album) = albums.get(selected) {
                            self.load_album(album.id, album.name.clone());
                        }
                    }
                    SearchItems::Playlists(playlists) => {
                        if let Some(playlist) = playlists.get(selected) {
                            self.load_playlist(playlist.id, playlist.name.clone());
                        }
                    }
                }
            }
            Detail::Queue { selected } => {
                if *selected < self.queue.items.len() {
                    self.send(
                        RequestKind::Generic {
                            label: "Playing queue item".into(),
                        },
                        Command::QueuePlay { index: *selected },
                    );
                }
            }
            Detail::Account => {
                self.open_login();
            }
            _ => {}
        }
    }

    fn add_selected(&mut self, all: bool) {
        let command = match &self.detail {
            Detail::Songs { view, selected, .. } => {
                if view.songs.is_empty() {
                    return;
                }
                let songs = if all {
                    view.songs.clone()
                } else {
                    let index = (*selected).min(view.songs.len() - 1);
                    vec![view.songs[index].clone()]
                };
                Command::AddSongs {
                    songs,
                    context: view.context.clone(),
                }
            }
            Detail::Playlists {
                items, selected, ..
            } => {
                let Some(playlist) = items.get(*selected) else {
                    return;
                };
                Command::AddPlaylist { id: playlist.id }
            }
            Detail::Search {
                page: Some(page),
                selected,
                ..
            } => match &page.items {
                SearchItems::Songs(songs) => {
                    if songs.is_empty() {
                        return;
                    }
                    let songs = if all {
                        songs.clone()
                    } else {
                        let index = (*selected).min(songs.len() - 1);
                        vec![songs[index].clone()]
                    };
                    Command::AddSongs {
                        songs,
                        context: None,
                    }
                }
                SearchItems::Playlists(playlists) => {
                    let Some(playlist) = playlists.get(*selected) else {
                        return;
                    };
                    Command::AddPlaylist { id: playlist.id }
                }
                _ => return,
            },
            _ => return,
        };
        self.send(
            RequestKind::Generic {
                label: "Added to queue".into(),
            },
            command,
        );
    }

    fn refresh_detail(&mut self) {
        match self.detail.clone() {
            Detail::Songs { origin, .. } => match origin {
                SongsOrigin::Daily => self.load_daily_songs(true),
                SongsOrigin::Liked => {
                    self.detail = Detail::Loading {
                        title: tr(self.lang, "Refreshing liked songs", "正在刷新喜欢的音乐").into(),
                    };
                    self.send(RequestKind::LikedSongs, Command::LikedSongs);
                }
                SongsOrigin::Recent => {
                    self.detail = Detail::Loading {
                        title: tr(self.lang, "Refreshing recently played", "正在刷新最近播放")
                            .into(),
                    };
                    self.send(RequestKind::RecentSongs, Command::RecentSongs);
                }
                SongsOrigin::Playlist(id) | SongsOrigin::Radar(id) => {
                    self.load_playlist(id, "playlist".into());
                }
                SongsOrigin::Artist(id) => {
                    self.load_artist(id, "artist".into());
                }
                SongsOrigin::Album(id) => {
                    self.load_album(id, "album".into());
                }
                SongsOrigin::Other => {}
            },
            Detail::Playlists { origin, .. } => match origin {
                PlaylistsOrigin::Daily => self.load_daily_playlists(true),
                PlaylistsOrigin::Created => self.load_playlists(PlaylistsOrigin::Created),
                PlaylistsOrigin::Subscribed => self.load_playlists(PlaylistsOrigin::Subscribed),
            },
            Detail::Search { query, kind, .. } => {
                self.run_search(query, kind);
            }
            Detail::Queue { .. } | Detail::Account | Detail::Landing | Detail::Message { .. } => {
                self.send(RequestKind::RefreshState, Command::GetState);
            }
            Detail::Loading { .. } => {}
        }
    }

    fn load_daily_songs(&mut self, refresh: bool) {
        self.detail = Detail::Loading {
            title: tr(
                self.lang,
                "Loading daily recommendations",
                "正在加载每日推荐",
            )
            .into(),
        };
        self.focus = Focus::Detail;
        self.send(
            RequestKind::DailySongs { refresh },
            Command::DailySongs { refresh },
        );
    }

    fn load_daily_playlists(&mut self, refresh: bool) {
        self.detail = Detail::Loading {
            title: tr(
                self.lang,
                "Loading recommended playlists",
                "正在加载推荐歌单",
            )
            .into(),
        };
        self.focus = Focus::Detail;
        self.send(
            RequestKind::DailyPlaylists { refresh },
            Command::DailyPlaylists { refresh },
        );
    }

    fn load_playlists(&mut self, origin: PlaylistsOrigin) {
        self.detail = Detail::Loading {
            title: tr(self.lang, "Loading playlists", "正在加载歌单").into(),
        };
        self.focus = Focus::Detail;
        let kind = match origin {
            PlaylistsOrigin::Created => PlaylistKind::Created,
            PlaylistsOrigin::Subscribed => PlaylistKind::Subscribed,
            PlaylistsOrigin::Daily => PlaylistKind::Created,
        };
        self.send(
            RequestKind::Playlists { origin },
            Command::Playlists { kind },
        );
    }

    pub fn load_playlist(&mut self, id: u64, title: String) {
        self.detail = Detail::Loading {
            title: format!("{} {title}…", tr(self.lang, "Loading", "正在加载")),
        };
        self.focus = Focus::Detail;
        self.send(
            RequestKind::PlaylistTracks { id, title },
            Command::PlaylistTracks {
                id,
                offset: 0,
                limit: 100_000,
            },
        );
    }

    fn load_artist(&mut self, id: u64, name: String) {
        self.detail = Detail::Loading {
            title: format!("{} {name}…", tr(self.lang, "Loading", "正在加载")),
        };
        self.focus = Focus::Detail;
        self.send(
            RequestKind::ArtistSongs { id, name },
            Command::ArtistSongs {
                id,
                offset: 0,
                limit: 100,
            },
        );
    }

    fn load_album(&mut self, id: u64, name: String) {
        self.detail = Detail::Loading {
            title: format!("{} {name}…", tr(self.lang, "Loading", "正在加载")),
        };
        self.focus = Focus::Detail;
        self.send(
            RequestKind::AlbumSongs { id, name },
            Command::AlbumSongs { id },
        );
    }

    fn load_radar(&mut self, refresh: bool) {
        self.detail = Detail::Loading {
            title: tr(self.lang, "Locating Private Radar", "正在定位私人雷达").into(),
        };
        self.focus = Focus::Detail;
        self.send(RequestKind::Radar { refresh }, Command::Radar { refresh });
    }

    fn open_search_input(&mut self) {
        let current = match &self.detail {
            Detail::Search { query, .. } => query.clone(),
            _ => String::new(),
        };
        self.input = Some(InputState {
            prompt: tr(
                self.lang,
                "Search NetEase (Enter to search, Esc to cancel)",
                "搜索网易云（回车搜索，Esc 取消）",
            )
            .into(),
            value: current,
        });
        self.dirty = true;
    }

    fn run_search(&mut self, query: String, kind: SearchKind) {
        self.search_generation = self.search_generation.wrapping_add(1);
        let generation = self.search_generation;
        self.detail = Detail::Search {
            query: query.clone(),
            kind,
            page: None,
            selected: 0,
            loading_more: false,
            has_more: false,
        };
        self.focus = Focus::Detail;
        self.send(
            RequestKind::Search {
                generation,
                query: query.clone(),
                kind,
                offset: 0,
            },
            Command::Search {
                query,
                kind,
                offset: 0,
                limit: 50,
            },
        );
    }

    fn maybe_load_more_search(&mut self) {
        let Detail::Search {
            query,
            kind,
            page,
            selected,
            loading_more,
            has_more,
        } = &mut self.detail
        else {
            return;
        };
        if !*has_more || *loading_more {
            return;
        }
        let Some(page_value) = page.as_ref() else {
            return;
        };
        let len = page_value.items.len();
        if *selected + 5 < len {
            return;
        }
        *loading_more = true;
        let generation = self.search_generation;
        let query = query.clone();
        let kind = *kind;
        let offset = len;
        self.send(
            RequestKind::Search {
                generation,
                query: query.clone(),
                kind,
                offset,
            },
            Command::Search {
                query,
                kind,
                offset,
                limit: 50,
            },
        );
        self.dirty = true;
    }
}

fn empty_search_page(query: &str, kind: SearchKind) -> SearchPage {
    let items = match kind {
        SearchKind::Songs => SearchItems::Songs(Vec::new()),
        SearchKind::Artists => SearchItems::Artists(Vec::new()),
        SearchKind::Albums => SearchItems::Albums(Vec::new()),
        SearchKind::Playlists => SearchItems::Playlists(Vec::new()),
    };
    SearchPage {
        query: query.to_string(),
        offset: 0,
        limit: 0,
        total: 0,
        has_more: false,
        items,
    }
}

fn append_page(existing: &mut SearchPage, page: SearchPage) {
    existing.has_more = page.has_more;
    existing.total = page.total;
    existing.offset = page.offset;
    existing.limit = page.limit;
    match (&mut existing.items, page.items) {
        (SearchItems::Songs(a), SearchItems::Songs(mut b)) => a.append(&mut b),
        (SearchItems::Artists(a), SearchItems::Artists(mut b)) => a.append(&mut b),
        (SearchItems::Albums(a), SearchItems::Albums(mut b)) => a.append(&mut b),
        (SearchItems::Playlists(a), SearchItems::Playlists(mut b)) => a.append(&mut b),
        _ => {}
    }
}

fn build_menu(lang: Language) -> Vec<MenuEntry> {
    let header = |label: &str| MenuEntry {
        header: true,
        label: label.into(),
        action: None,
    };
    let item = |action: MenuAction, label: &str| MenuEntry {
        header: false,
        label: label.into(),
        action: Some(action),
    };
    vec![
        header(tr(lang, "Discover", "发现")),
        item(
            MenuAction::DailySongs,
            tr(lang, "Daily Songs", "每日推荐歌曲"),
        ),
        item(
            MenuAction::DailyPlaylists,
            tr(lang, "Daily Playlists", "每日推荐歌单"),
        ),
        item(MenuAction::PersonalFm, tr(lang, "Personal FM", "私人漫游")),
        item(MenuAction::Radar, tr(lang, "Personal Radar", "私人雷达")),
        header(tr(lang, "Library", "音乐库")),
        item(MenuAction::Liked, tr(lang, "Liked Songs", "我喜欢的音乐")),
        item(
            MenuAction::MyPlaylists,
            tr(lang, "My Playlists", "我创建的歌单"),
        ),
        item(
            MenuAction::Subscribed,
            tr(lang, "Subscribed Playlists", "我收藏的歌单"),
        ),
        item(MenuAction::Recent, tr(lang, "Recently Played", "最近播放")),
        header(tr(lang, "Other", "其他")),
        item(MenuAction::Search, tr(lang, "Search", "搜索")),
        item(MenuAction::Queue, tr(lang, "Queue", "播放队列")),
        item(MenuAction::Account, tr(lang, "Account", "账号")),
    ]
}

pub fn default_playback() -> PlaybackState {
    PlaybackState {
        status: PlaybackStatus::Stopped,
        position_ms: 0,
        duration_ms: 0,
        volume: 0.7,
        mode: PlayMode::ListLoop,
        source: PlaySource::none(),
        current: None,
        current_index: None,
        queue_len: 0,
        error: None,
        warning: None,
        mpv_available: false,
        url_refreshed_at_ms: None,
    }
}

pub fn tr(lang: Language, en: &'static str, zh: &'static str) -> &'static str {
    match lang {
        Language::En => en,
        Language::Zh => zh,
    }
}

pub fn mode_label(lang: Language, mode: PlayMode) -> &'static str {
    match (lang, mode) {
        (Language::En, PlayMode::ListLoop) => "List repeat",
        (Language::En, PlayMode::TrackLoop) => "Track repeat",
        (Language::En, PlayMode::Sequential) => "Sequential",
        (Language::En, PlayMode::Shuffle) => "Shuffle",
        (Language::En, PlayMode::Intelligence) => "Heartbeat",
        (Language::Zh, PlayMode::ListLoop) => "列表循环",
        (Language::Zh, PlayMode::TrackLoop) => "单曲循环",
        (Language::Zh, PlayMode::Sequential) => "按序播放",
        (Language::Zh, PlayMode::Shuffle) => "随机播放",
        (Language::Zh, PlayMode::Intelligence) => "心动模式",
    }
}

pub fn search_kind_label(lang: Language, kind: SearchKind) -> &'static str {
    match (lang, kind) {
        (Language::En, SearchKind::Songs) => "Songs",
        (Language::En, SearchKind::Artists) => "Artists",
        (Language::En, SearchKind::Albums) => "Albums",
        (Language::En, SearchKind::Playlists) => "Playlists",
        (Language::Zh, SearchKind::Songs) => "歌曲",
        (Language::Zh, SearchKind::Artists) => "歌手",
        (Language::Zh, SearchKind::Albums) => "专辑",
        (Language::Zh, SearchKind::Playlists) => "歌单",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translations_and_mode_labels_cover_both_languages() {
        assert_eq!(tr(Language::En, "Queue", "播放队列"), "Queue");
        assert_eq!(tr(Language::Zh, "Queue", "播放队列"), "播放队列");
        assert_eq!(
            mode_label(Language::En, PlayMode::Intelligence),
            "Heartbeat"
        );
        assert_eq!(mode_label(Language::Zh, PlayMode::Intelligence), "心动模式");
        assert_eq!(
            search_kind_label(Language::Zh, SearchKind::Playlists),
            "歌单"
        );
    }
}
