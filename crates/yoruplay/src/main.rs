use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use yoruplay_core::cache::FileCache;
use yoruplay_core::mpv::MpvController;
use yoruplay_ipc::{shutdown_any_version, IpcClient};
use yoruplay_protocol::{
    Command, CommandResult, Config, ErrorInfo, Paths, PlayMode, PlaylistKind, PlaylistTracks,
    SearchItems, SearchKind, SearchPage, Song, SongsView, StateSnapshot, LOGIN_URL,
};

#[derive(Parser, Debug)]
#[command(
    name = "yoruplay",
    version,
    about = "A lightweight Rust terminal music player for NetEase Cloud Music"
)]
struct Cli {
    /// Print machine-readable JSON instead of the human-readable view.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Subcommand, Debug)]
enum CliCommand {
    /// Open the terminal interface (default).
    Tui,
    /// Run the background service in the foreground.
    Daemon,
    /// Stop the background daemon and save playback state.
    #[command(alias = "quit")]
    Shutdown,
    /// Show daemon, login and playback status.
    Status,
    /// Resume or start playback.
    Play,
    /// Pause playback.
    Pause,
    /// Toggle pause.
    PlayPause,
    /// Stop playback and reset the position.
    Stop,
    /// Play the next track.
    Next,
    /// Play the previous track.
    Previous,
    /// Show or change the playback mode.
    Mode { mode: Option<String> },
    /// Show or set the volume (0-100).
    Volume { value: Option<f64> },
    /// Seek relative to the current position, in seconds (e.g. +10, -5).
    Seek { offset: String },
    /// Print the queue.
    Queue,
    /// Remove one queue entry by zero-based index.
    QueueRemove { index: usize },
    /// Play a queue entry by zero-based index.
    QueuePlay { index: usize },
    /// Clear the queue.
    QueueClear,
    /// Search songs, artists, albums or playlists.
    Search(SearchArgs),
    /// List your created or subscribed playlists.
    Playlists {
        #[arg(long, default_value = "created")]
        kind: String,
    },
    /// List liked songs.
    Liked,
    /// List the NetEase server's recently played songs.
    Recent,
    /// List today's recommended songs.
    Daily {
        #[arg(long)]
        refresh: bool,
    },
    /// List today's recommended playlists.
    DailyPlaylists {
        #[arg(long)]
        refresh: bool,
    },
    /// Resolve and print the Private Radar playlist.
    Radar {
        #[arg(long)]
        refresh: bool,
    },
    /// Start Personal FM.
    Fm,
    /// Print one playlist and its songs.
    Playlist {
        id: u64,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
    /// Replace the queue with a playlist and start at an index.
    StartPlaylist {
        id: u64,
        #[arg(long, default_value_t = 0)]
        start: usize,
    },
    /// Append all songs of a playlist to the queue.
    AddPlaylist { id: u64 },
    /// Log in through Firefox, or import a Cookie header from stdin.
    Login(LoginArgs),
    /// Log out and remove the saved NetEase session.
    Logout,
    /// Manage the bounded on-disk cache.
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },
    /// Install or control the systemd user service.
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// Check mpv, PipeWire, the daemon, MPRIS and file permissions.
    Doctor,
    /// Print log locations and follow commands.
    Logs,
    /// Remove service, cache and runtime state (never credentials/config).
    Uninstall {
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        remove_binary: bool,
    },
}

#[derive(Args, Debug)]
struct SearchArgs {
    query: String,
    #[arg(long, default_value = "songs")]
    kind: String,
    #[arg(long, default_value_t = 0)]
    offset: usize,
    #[arg(long, default_value_t = 30)]
    limit: usize,
}

#[derive(Args, Debug)]
struct LoginArgs {
    /// Read a Cookie header from stdin (MUSIC_U required). Avoids shell history.
    #[arg(long)]
    cookie_stdin: bool,
    /// Open the browser even when a Firefox session is already available.
    #[arg(long, visible_alias = "force")]
    switch_account: bool,
}

#[derive(Subcommand, Debug)]
enum CacheCommand {
    /// Remove all cache entries.
    Clear,
    /// Show cache size and configuration.
    Info,
}

#[derive(Subcommand, Debug)]
enum ServiceCommand {
    /// Write the user unit and reload systemd (does not enable it).
    Install,
    /// Stop, disable and remove the user unit.
    Uninstall,
    Start,
    Stop,
    Restart,
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = Paths::discover().context("discover XDG directories")?;
    let command = cli.command.unwrap_or(CliCommand::Tui);
    match command {
        CliCommand::Daemon => {
            let config = load_config(&paths)?;
            init_stderr_logging(&config.log.level);
            yoruplay_core::daemon::run(paths, config).await
        }
        CliCommand::Tui => {
            let config = load_config(&paths)?;
            init_file_logging(&paths.tui_log_file(), &config.log.level)?;
            ensure_daemon(&paths).await?;
            yoruplay_tui::run(paths, config).await
        }
        other => {
            let config = load_config(&paths)?;
            init_stderr_logging(&config.log.level);
            run_command(other, cli.json, paths, config).await
        }
    }
}

async fn run_command(command: CliCommand, json: bool, paths: Paths, config: Config) -> Result<()> {
    match command {
        CliCommand::Shutdown => shutdown_daemon(&paths, json).await,
        CliCommand::Status => {
            let result = send(&paths, Command::GetState).await?;
            if json {
                return print_json(&result);
            }
            let snapshot = expect_state(result)?;
            print_state(&snapshot);
            Ok(())
        }
        CliCommand::Queue => {
            let result = send(&paths, Command::GetState).await?;
            if json {
                return print_json(&result);
            }
            let snapshot = expect_state(result)?;
            print_queue(&snapshot);
            Ok(())
        }
        CliCommand::Play => ack(&paths, Command::Play, json).await,
        CliCommand::Pause => ack(&paths, Command::Pause, json).await,
        CliCommand::PlayPause => ack(&paths, Command::PlayPause, json).await,
        CliCommand::Stop => ack(&paths, Command::Stop, json).await,
        CliCommand::Next => ack(&paths, Command::Next, json).await,
        CliCommand::Previous => ack(&paths, Command::Previous, json).await,
        CliCommand::QueueClear => ack(&paths, Command::QueueClear, json).await,
        CliCommand::QueueRemove { index } => {
            ack(&paths, Command::QueueRemove { index }, json).await
        }
        CliCommand::QueuePlay { index } => ack(&paths, Command::QueuePlay { index }, json).await,
        CliCommand::Mode { mode } => match mode {
            Some(mode) => {
                let mode = parse_mode(&mode)?;
                ack(&paths, Command::SetMode { mode }, json).await
            }
            None => ack(&paths, Command::CycleMode, json).await,
        },
        CliCommand::Volume { value } => match value {
            Some(value) => {
                if !(0.0..=100.0).contains(&value) {
                    bail!("volume must be between 0 and 100");
                }
                ack(
                    &paths,
                    Command::SetVolume {
                        volume: value / 100.0,
                    },
                    json,
                )
                .await
            }
            None => {
                let result = send(&paths, Command::GetState).await?;
                let snapshot = expect_state(result)?;
                println!("Volume: {:.0}%", snapshot.playback.volume * 100.0);
                Ok(())
            }
        },
        CliCommand::Seek { offset } => {
            let seconds: f64 = offset
                .trim_start_matches('+')
                .parse()
                .with_context(|| format!("invalid seek offset `{offset}`"))?;
            ack(
                &paths,
                Command::SeekRelative {
                    offset_ms: (seconds * 1000.0) as i64,
                },
                json,
            )
            .await
        }
        CliCommand::Search(args) => {
            let kind = parse_search_kind(&args.kind)?;
            let result = send(
                &paths,
                Command::Search {
                    query: args.query,
                    kind,
                    offset: args.offset,
                    limit: args.limit,
                },
            )
            .await?;
            if json {
                return print_json(&result);
            }
            match result {
                CommandResult::Search(page) => {
                    print_search(&page);
                    Ok(())
                }
                other => print_result(other),
            }
        }
        CliCommand::Playlists { kind } => {
            let kind = match kind.to_ascii_lowercase().as_str() {
                "created" | "mine" => PlaylistKind::Created,
                "subscribed" | "collected" => PlaylistKind::Subscribed,
                other => bail!("unknown playlist kind `{other}`; use created or subscribed"),
            };
            let result = send(&paths, Command::Playlists { kind }).await?;
            if json {
                return print_json(&result);
            }
            match result {
                CommandResult::Playlists(playlists) => {
                    for playlist in playlists {
                        println!(
                            "{:>12}  {:>5} tracks  {}  [{}]",
                            playlist.id,
                            playlist.track_count,
                            playlist.name,
                            if playlist.subscribed {
                                "subscribed"
                            } else {
                                "created"
                            }
                        );
                    }
                    Ok(())
                }
                other => print_result(other),
            }
        }
        CliCommand::Liked => songs_command(&paths, Command::LikedSongs, json).await,
        CliCommand::Recent => songs_command(&paths, Command::RecentSongs, json).await,
        CliCommand::Daily { refresh } => {
            songs_command(&paths, Command::DailySongs { refresh }, json).await
        }
        CliCommand::DailyPlaylists { refresh } => {
            let result = send(&paths, Command::DailyPlaylists { refresh }).await?;
            if json {
                return print_json(&result);
            }
            match result {
                CommandResult::Playlists(playlists) => {
                    for (i, playlist) in playlists.iter().enumerate() {
                        println!(
                            "{:>2}. {:>12}  {:>5} tracks  {}",
                            i + 1,
                            playlist.id,
                            playlist.track_count,
                            playlist.name
                        );
                    }
                    Ok(())
                }
                other => print_result(other),
            }
        }
        CliCommand::Radar { refresh } => {
            let result = send(&paths, Command::Radar { refresh }).await?;
            if json {
                return print_json(&result);
            }
            match result {
                CommandResult::PlaylistTracks(tracks) => {
                    print_tracks(&tracks);
                    Ok(())
                }
                other => print_result(other),
            }
        }
        CliCommand::Fm => ack(&paths, Command::PersonalFm, json).await,
        CliCommand::Playlist { id, offset, limit } => {
            let result = send(&paths, Command::PlaylistTracks { id, offset, limit }).await?;
            if json {
                return print_json(&result);
            }
            match result {
                CommandResult::PlaylistTracks(tracks) => {
                    print_tracks(&tracks);
                    Ok(())
                }
                other => print_result(other),
            }
        }
        CliCommand::StartPlaylist { id, start } => {
            ack(
                &paths,
                Command::StartPlaylist {
                    id,
                    start_index: start,
                },
                json,
            )
            .await
        }
        CliCommand::AddPlaylist { id } => ack(&paths, Command::AddPlaylist { id }, json).await,
        CliCommand::Login(args) => login(&paths, args).await,
        CliCommand::Logout => {
            let result = send(&paths, Command::Logout).await?;
            if json {
                return print_json(&result);
            }
            println!("Logged out.");
            Ok(())
        }
        CliCommand::Cache { command } => cache_command(command, &paths, &config).await,
        CliCommand::Service { command } => service_command(command, &paths, &config).await,
        CliCommand::Doctor => doctor(&paths, &config).await,
        CliCommand::Logs => {
            println!("Daemon logs: journalctl --user -u yoruplay -f");
            println!(
                "Auto-started daemon log: tail -f {}",
                paths.log_file().display()
            );
            println!("TUI log: tail -f {}", paths.tui_log_file().display());
            Ok(())
        }
        CliCommand::Uninstall { yes, remove_binary } => {
            uninstall(&paths, &config, yes, remove_binary).await
        }
        CliCommand::Daemon | CliCommand::Tui => unreachable!("handled in main"),
    }
}

async fn send(paths: &Paths, command: Command) -> Result<CommandResult> {
    let client = connect(paths).await?;
    let result = client.request(command).await.map_err(ipc_error)?;
    client.close().await;
    Ok(result)
}

/// Stop the daemon whether it was auto-started by YoruPlay or by the systemd
/// user unit. The IPC shutdown path is preferred because it saves the queue and
/// reaps mpv; `systemctl stop` is only a fallback for an unresponsive unit.
async fn shutdown_daemon(paths: &Paths, json: bool) -> Result<()> {
    let message = match IpcClient::connect(paths.socket_file()).await {
        Ok(client) => {
            client.request(Command::Shutdown).await.map_err(ipc_error)?;
            client.close().await;
            if wait_for_socket_removal(paths, Duration::from_secs(10)).await {
                "Daemon stopped."
            } else {
                bail!(
                    "the daemon accepted shutdown but did not exit; try `systemctl --user stop yoruplay`"
                );
            }
        }
        Err(_) => {
            // An older daemon (for example protocol v1 during an upgrade)
            // rejects the normal handshake. Its shutdown command still has the
            // same wire shape, so stop it before falling back to systemd.
            if shutdown_any_version(paths.socket_file()).await.is_ok() {
                if !wait_for_socket_removal(paths, Duration::from_secs(10)).await {
                    bail!("the daemon accepted shutdown but did not exit");
                }
                "Daemon stopped."
            } else {
                let active = systemctl_quiet(&["is-active", "--quiet", "yoruplay"])
                    .map(|status| status.success())
                    .unwrap_or(false);
                if active {
                    systemctl(&["stop", "yoruplay"])?;
                    if !wait_for_socket_removal(paths, Duration::from_secs(10)).await {
                        bail!("systemd stopped the unit but the daemon socket is still present");
                    }
                    "Daemon stopped (systemd user service)."
                } else {
                    "Daemon is not running."
                }
            }
        }
    };
    if json {
        print_json(&CommandResult::ack_msg(message))?;
    } else {
        println!("{message}");
    }
    Ok(())
}

async fn wait_for_socket_removal(paths: &Paths, limit: Duration) -> bool {
    let deadline = std::time::Instant::now() + limit;
    while std::time::Instant::now() < deadline {
        if !paths.socket_file().exists() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    !paths.socket_file().exists()
}

async fn ack(paths: &Paths, command: Command, json: bool) -> Result<()> {
    let result = send(paths, command).await?;
    if json {
        return print_json(&result);
    }
    print_result(result)
}

async fn songs_command(paths: &Paths, command: Command, json: bool) -> Result<()> {
    let result = send(paths, command).await?;
    if json {
        return print_json(&result);
    }
    match result {
        CommandResult::Songs(view) => {
            print_songs(&view);
            Ok(())
        }
        other => print_result(other),
    }
}

async fn connect(paths: &Paths) -> Result<IpcClient> {
    IpcClient::connect(paths.socket_file()).await.map_err(|e| {
        let info = e.to_error_info();
        anyhow!(
            "{}\n\nStart it with `yoruplay daemon` or `systemctl --user start yoruplay`.",
            info.message
        )
    })
}

fn ipc_error(error: ErrorInfo) -> anyhow::Error {
    match error.hint {
        Some(hint) => anyhow!("{}\n\n{hint}", error.message),
        None => anyhow!("{}", error.message),
    }
}

async fn ensure_daemon(paths: &Paths) -> Result<IpcClient> {
    if let Ok(client) = IpcClient::connect(paths.socket_file()).await {
        return Ok(client);
    }
    paths.ensure_dirs().context("create runtime directories")?;
    let exe = std::env::current_exe().context("locate the yoruplay executable")?;
    let log_path = paths.log_file();
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log_path)
        .with_context(|| format!("open {}", log_path.display()))?;
    let mut command = ProcessCommand::new(&exe);
    command
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("start {}", exe.display()))?;
    for _ in 0..80 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(client) = IpcClient::connect(paths.socket_file()).await {
            return Ok(client);
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(anyhow!(
                "the daemon exited during startup ({status}). See {}.",
                log_path.display()
            ));
        }
    }
    let tail = tail_file(&log_path, 20).unwrap_or_default();
    Err(anyhow!(
        "the daemon did not become ready. See {}.\n{}",
        log_path.display(),
        tail
    ))
}

async fn login(paths: &Paths, args: LoginArgs) -> Result<()> {
    let client = ensure_daemon(paths).await?;
    if args.cookie_stdin {
        let mut cookie = String::new();
        io::stdin()
            .read_to_string(&mut cookie)
            .context("read cookie from stdin")?;
        let cookie = cookie.trim();
        if cookie.is_empty() {
            bail!("stdin was empty; expected a Cookie header containing MUSIC_U");
        }
        let result = client
            .request(Command::ImportCookie {
                cookie: cookie.to_string(),
            })
            .await
            .map_err(ipc_error)?;
        client.close().await;
        print_auth_result(result)
    } else {
        // Fast path: a valid Firefox session is enough; do not open a browser.
        if !args.switch_account {
            match client
                .request(Command::ImportBrowserCookie { force: false })
                .await
            {
                Ok(result) => {
                    client.close().await;
                    return print_auth_result(result);
                }
                Err(error)
                    if matches!(
                        error.code.as_str(),
                        "browser_cookie_missing"
                            | "browser_profile_missing"
                            | "invalid_cookie"
                            | "not_logged_in"
                            | "session_expired"
                            | "api_error"
                    ) => {}
                Err(error) => {
                    client.close().await;
                    return Err(ipc_error(error));
                }
            }
        }

        println!("Opening {LOGIN_URL} in your browser.");
        open_login_page();
        println!("Log in there in Firefox. Waiting for a Firefox MUSIC_U cookie...");
        println!("Other browsers are not supported yet; use --cookie-stdin for those.");
        let mut same_account_reported = false;
        for _ in 0..150 {
            match client
                .request(Command::ImportBrowserCookie {
                    force: args.switch_account,
                })
                .await
            {
                Ok(result) => {
                    client.close().await;
                    return print_auth_result(result);
                }
                Err(error)
                    if matches!(
                        error.code.as_str(),
                        "browser_cookie_missing"
                            | "invalid_cookie"
                            | "not_logged_in"
                            | "session_expired"
                            | "api_error"
                    ) =>
                {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Err(error) if error.code == "browser_same_account" => {
                    if !same_account_reported {
                        println!(
                            "Firefox is still logged in as the current account; switch accounts there."
                        );
                        same_account_reported = true;
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Err(error) => {
                    client.close().await;
                    return Err(ipc_error(error));
                }
            }
        }
        client.close().await;
        bail!("timed out waiting for Firefox login; run `yoruplay login` again")
    }
}

fn open_login_page() {
    let _ = ProcessCommand::new("xdg-open")
        .arg(LOGIN_URL)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

fn print_auth_result(result: CommandResult) -> Result<()> {
    match result {
        CommandResult::Auth(auth) => {
            if auth.logged_in {
                if let Some(profile) = auth.profile {
                    println!("Logged in as {} ({})", profile.nickname, profile.user_id);
                } else {
                    println!("Logged in.");
                }
            } else {
                println!("Not logged in.");
            }
            Ok(())
        }
        other => print_result(other),
    }
}

async fn cache_command(command: CacheCommand, paths: &Paths, config: &Config) -> Result<()> {
    let cache = FileCache::new(paths.cache_dir.clone(), config.cache.max_mb);
    match command {
        CacheCommand::Clear => {
            if let Ok(client) = IpcClient::connect(paths.socket_file()).await {
                let result = client
                    .request(Command::ClearCache)
                    .await
                    .map_err(ipc_error)?;
                client.close().await;
                return print_result(result);
            }
            let before = cache.size_bytes();
            cache.clear()?;
            println!("Cache cleared ({before} bytes).");
            Ok(())
        }
        CacheCommand::Info => {
            println!("Cache directory: {}", cache.dir().display());
            println!("Size: {} bytes", cache.size_bytes());
            println!("Limit: {} bytes", cache.max_bytes());
            println!(
                "Lyrics TTL: {} hours; daily recommendations: 6 hours",
                config.cache.lyrics_ttl_hours
            );
            Ok(())
        }
    }
}

async fn service_command(command: ServiceCommand, paths: &Paths, config: &Config) -> Result<()> {
    match command {
        ServiceCommand::Install => service_install(paths),
        ServiceCommand::Uninstall => service_uninstall(paths, config).await,
        ServiceCommand::Start => systemctl(&["start", "yoruplay"]),
        ServiceCommand::Stop => systemctl(&["stop", "yoruplay"]),
        ServiceCommand::Restart => systemctl(&["restart", "yoruplay"]),
        ServiceCommand::Status => systemctl(&["status", "yoruplay", "--no-pager"]),
    }
}

fn service_unit_path(paths: &Paths) -> PathBuf {
    paths
        .config_dir
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("systemd/user/yoruplay.service")
}

fn service_install(paths: &Paths) -> Result<()> {
    let exe = std::env::current_exe().context("locate the yoruplay executable")?;
    let unit_path = service_unit_path(paths);
    if let Some(parent) = unit_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let unit = format!(
        r#"[Unit]
Description=YoruPlay background music service
After=pipewire-pulse.service wireplumber.service
Wants=pipewire-pulse.service

[Service]
Type=simple
ExecStart="{}" daemon
Restart=on-failure
RestartSec=2
TimeoutStopSec=10
NoNewPrivileges=true

[Install]
WantedBy=default.target
"#,
        exe.display()
    );
    fs::write(&unit_path, unit).with_context(|| format!("write {}", unit_path.display()))?;
    fs::set_permissions(&unit_path, fs::Permissions::from_mode(0o644))?;
    systemctl(&["daemon-reload"])?;
    println!("Installed {}", unit_path.display());
    println!("It is NOT enabled at login. Start it with:");
    println!("  systemctl --user start yoruplay");
    println!("Optional autostart: systemctl --user enable yoruplay");
    Ok(())
}

async fn service_uninstall(paths: &Paths, config: &Config) -> Result<()> {
    let _ = systemctl_quiet(&["disable", "--now", "yoruplay"]);
    let unit_path = service_unit_path(paths);
    if unit_path.exists() {
        fs::remove_file(&unit_path).with_context(|| format!("remove {}", unit_path.display()))?;
    }
    let _ = systemctl_quiet(&["daemon-reload"]);
    if let Ok(client) = IpcClient::connect(paths.socket_file()).await {
        let _ = client.request(Command::Shutdown).await;
        client.close().await;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let _ = fs::remove_file(paths.socket_file());
    let _ = fs::remove_file(paths.lock_file());
    let cache = FileCache::new(paths.cache_dir.clone(), config.cache.max_mb);
    let _ = cache.clear();
    println!("Removed the systemd user unit, runtime socket and cache.");
    println!(
        "Preserved credentials and configuration: {} and {}",
        paths.auth_file().display(),
        paths.config_file().display()
    );
    Ok(())
}

fn systemctl(args: &[&str]) -> Result<()> {
    let status = ProcessCommand::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("run systemctl --user")?;
    if !status.success() {
        bail!("systemctl --user {} failed ({status})", args.join(" "));
    }
    Ok(())
}

fn systemctl_quiet(args: &[&str]) -> io::Result<std::process::ExitStatus> {
    ProcessCommand::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
}

async fn doctor(paths: &Paths, config: &Config) -> Result<()> {
    println!("YoruPlay {}", env!("CARGO_PKG_VERSION"));
    println!("Config: {}", paths.config_file().display());
    println!("Auth:   {}", paths.auth_file().display());
    match fs::metadata(paths.auth_file()) {
        Ok(meta) => println!("Auth permissions: {:o}", meta.permissions().mode() & 0o777),
        Err(_) => println!("Auth permissions: not logged in"),
    }
    println!("Socket: {}", paths.socket_file().display());
    println!(
        "Cache:  {} ({} bytes, limit {})",
        paths.cache_dir.display(),
        FileCache::new(paths.cache_dir.clone(), config.cache.max_mb).size_bytes(),
        config.cache.max_mb * 1024 * 1024
    );
    match MpvController::available_version().await {
        Ok(version) => println!("mpv:    {version}"),
        Err(e) => println!("mpv:    ERROR: {}", e.user_message()),
    }
    match ProcessCommand::new("pactl").arg("info").output() {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            for line in text
                .lines()
                .filter(|l| l.starts_with("Server Name:") || l.starts_with("Default Sink:"))
            {
                println!("{line}");
            }
        }
        _ => println!("PipeWire/PulseAudio: pactl info failed"),
    }
    match IpcClient::connect(paths.socket_file()).await {
        Ok(client) => {
            let state = client.request(Command::GetState).await;
            client.close().await;
            match state {
                Ok(CommandResult::State(snapshot)) => {
                    println!(
                        "Daemon: running; mpv_available={}, status={:?}, queue={}",
                        snapshot.playback.mpv_available,
                        snapshot.playback.status,
                        snapshot.playback.queue_len
                    );
                    for warning in snapshot.warnings {
                        println!("Warning: {warning}");
                    }
                }
                _ => println!("Daemon: running but did not answer GetState"),
            }
        }
        Err(_) => println!("Daemon: not running"),
    }
    match ProcessCommand::new("busctl")
        .args(["--user", "status", "org.mpris.MediaPlayer2.yoruplay"])
        .output()
    {
        Ok(output) if output.status.success() => println!("MPRIS:  registered"),
        _ => println!("MPRIS:  not registered (busctl unavailable or daemon not running)"),
    }
    Ok(())
}

async fn uninstall(paths: &Paths, config: &Config, yes: bool, remove_binary: bool) -> Result<()> {
    if !yes {
        bail!(
            "Refusing to remove service/cache/runtime state without --yes.\n\
             Credentials and config are never removed by this command."
        );
    }
    let current = std::env::current_exe().ok();
    service_uninstall(paths, config).await?;
    println!(
        "Preserved account and configuration files. Remove them manually only if you are sure:"
    );
    println!("  {}", paths.auth_file().display());
    println!("  {}", paths.config_file().display());
    if remove_binary {
        if let Some(exe) = current {
            let home = std::env::var_os("HOME").map(PathBuf::from);
            let inside_home = home.as_ref().is_some_and(|home| exe.starts_with(home));
            if inside_home {
                fs::remove_file(&exe).with_context(|| format!("remove {}", exe.display()))?;
                println!("Removed executable {}", exe.display());
            } else {
                println!(
                    "Executable {} is outside HOME; remove it manually.",
                    exe.display()
                );
            }
        }
    } else {
        if let Some(exe) = current {
            println!(
                "Remove the executable manually if desired: {}",
                exe.display()
            );
        }
    }
    Ok(())
}

fn load_config(paths: &Paths) -> Result<Config> {
    paths.ensure_dirs().context("create XDG directories")?;
    let mut config = Config::load_or_create(&paths.config_file()).map_err(|e| anyhow!("{}", e))?;
    config.validate();
    Ok(config)
}

fn env_filter(level: &str) -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level))
}

fn init_stderr_logging(level: &str) {
    let filter = env_filter(level);
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

fn init_file_logging(path: &Path, level: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    let filter = env_filter(level);
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(false)
        .with_writer(move || file.try_clone().expect("clone log file"))
        .try_init();
    Ok(())
}

fn print_json(result: &CommandResult) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(result)?);
    Ok(())
}

fn print_result(result: CommandResult) -> Result<()> {
    match result {
        CommandResult::Ack { message } => {
            if let Some(message) = message {
                println!("{message}");
            } else {
                println!("OK");
            }
        }
        CommandResult::State(snapshot) => print_state(&snapshot),
        CommandResult::Auth(auth) => {
            if auth.logged_in {
                if let Some(profile) = auth.profile {
                    println!("Logged in as {} ({})", profile.nickname, profile.user_id);
                } else {
                    println!("Logged in.");
                }
            } else if auth.session_expired {
                println!("Session expired. Log in again.");
            } else {
                println!("Not logged in.");
            }
        }
        CommandResult::Songs(view) => print_songs(&view),
        CommandResult::Playlists(playlists) => {
            for playlist in playlists {
                println!(
                    "{}  {}  ({} tracks)",
                    playlist.id, playlist.name, playlist.track_count
                );
            }
        }
        CommandResult::PlaylistTracks(tracks) => print_tracks(&tracks),
        CommandResult::Search(page) => print_search(&page),
    }
    Ok(())
}

fn expect_state(result: CommandResult) -> Result<StateSnapshot> {
    match result {
        CommandResult::State(snapshot) => Ok(*snapshot),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

fn print_state(snapshot: &StateSnapshot) {
    let p = &snapshot.playback;
    match &snapshot.auth.profile {
        Some(profile) if snapshot.auth.logged_in => {
            println!("Account: {} ({})", profile.nickname, profile.user_id)
        }
        _ if snapshot.auth.session_expired => println!("Account: session expired"),
        _ => println!("Account: not logged in"),
    }
    if let Some(song) = &p.current {
        println!(
            "Track:   {} — {} [{}]",
            song.name,
            song.artist_text(),
            song.id
        );
        println!(
            "Status:  {:?} {}/{}  {}  queue {}",
            p.status,
            format_time(p.position_ms),
            format_time(p.duration_ms),
            p.mode.short(),
            p.queue_len
        );
    } else {
        println!("Track:   (none)");
        println!(
            "Status:  {:?}  {}  queue {}",
            p.status,
            p.mode.short(),
            p.queue_len
        );
    }
    if p.source.personal_fm {
        println!("Source:  Personal FM");
    } else if let Some(playlist) = &p.source.playlist {
        println!("Source:  {} [{}]", playlist.name, playlist.id);
    }
    println!(
        "Volume:  {:.0}%   mpv: {}",
        p.volume * 100.0,
        if p.mpv_available {
            "available"
        } else {
            "missing/stopped"
        }
    );
    if let Some(error) = &p.error {
        println!("Error:   {error}");
    }
    if let Some(warning) = &p.warning {
        println!("Warning: {warning}");
    }
    for warning in &snapshot.warnings {
        println!("Warning: {warning}");
    }
}

fn print_queue(snapshot: &StateSnapshot) {
    if snapshot.queue.items.is_empty() {
        println!("Queue is empty.");
        return;
    }
    for (index, item) in snapshot.queue.items.iter().enumerate() {
        let marker = if Some(index) == snapshot.queue.current {
            ">"
        } else {
            " "
        };
        println!(
            "{marker} {:>4}  {} — {} [{}]",
            index,
            item.song.name,
            item.song.artist_text(),
            item.song.id
        );
    }
    println!(
        "{} tracks, mode {}, source {}",
        snapshot.queue.items.len(),
        snapshot.queue.mode.short(),
        queue_source(&snapshot.queue.source)
    );
}

fn queue_source(source: &yoruplay_protocol::PlaySource) -> String {
    if source.personal_fm {
        "Personal FM".into()
    } else if let Some(playlist) = &source.playlist {
        format!("{} [{}]", playlist.name, playlist.id)
    } else {
        "queue".into()
    }
}

fn print_songs(view: &SongsView) {
    println!("{} ({} songs)", view.title, view.songs.len());
    if let Some(description) = &view.description {
        println!("{description}");
    }
    for (i, song) in view.songs.iter().enumerate() {
        print_song_line(i + 1, song);
    }
}

fn print_tracks(tracks: &PlaylistTracks) {
    println!(
        "{} [{}] — {} of {} tracks",
        tracks.playlist.name,
        tracks.playlist.id,
        tracks.songs.len(),
        tracks.total
    );
    if let Some(description) = &tracks.playlist.description {
        if !description.is_empty() {
            println!("{description}");
        }
    }
    for (i, song) in tracks.songs.iter().enumerate() {
        print_song_line(tracks.offset + i + 1, song);
    }
}

fn print_search(page: &SearchPage) {
    println!(
        "Search \"{}\" ({}) — {} results, showing {}{}",
        page.query,
        page.items.kind().label(),
        page.total,
        page.items.len(),
        if page.has_more {
            " (more available)"
        } else {
            ""
        }
    );
    match &page.items {
        SearchItems::Songs(songs) => {
            for (i, song) in songs.iter().enumerate() {
                print_song_line(i + 1, song);
            }
        }
        SearchItems::Artists(artists) => {
            for (i, artist) in artists.iter().enumerate() {
                println!(
                    "{:>3}. {:>12}  {}  ({} albums, {} songs)",
                    i + 1,
                    artist.id,
                    artist.name,
                    artist.album_count,
                    artist.song_count
                );
            }
        }
        SearchItems::Albums(albums) => {
            for (i, album) in albums.iter().enumerate() {
                println!(
                    "{:>3}. {:>12}  {} — {} ({} tracks)",
                    i + 1,
                    album.id,
                    album.name,
                    album.artist_name,
                    album.track_count
                );
            }
        }
        SearchItems::Playlists(playlists) => {
            for (i, playlist) in playlists.iter().enumerate() {
                println!(
                    "{:>3}. {:>12}  {} ({} tracks)",
                    i + 1,
                    playlist.id,
                    playlist.name,
                    playlist.track_count
                );
            }
        }
    }
}

fn print_song_line(index: usize, song: &Song) {
    let playable = match song.playable {
        Some(false) => " !",
        _ => "",
    };
    println!(
        "{index:>4}. {:>12}  {} — {}{}",
        song.id,
        song.name,
        song.artist_text(),
        playable
    );
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

fn parse_mode(value: &str) -> Result<PlayMode> {
    match value.to_ascii_lowercase().as_str() {
        "list" | "list_loop" | "list-loop" | "repeat" => Ok(PlayMode::ListLoop),
        "one" | "single" | "track" | "track_loop" | "track-loop" => Ok(PlayMode::TrackLoop),
        "sequential" | "seq" | "order" => Ok(PlayMode::Sequential),
        "shuffle" | "random" | "rand" => Ok(PlayMode::Shuffle),
        "intelligence" | "heartbeat" | "heart" => Ok(PlayMode::Intelligence),
        other => bail!("unknown mode `{other}`"),
    }
}

fn parse_search_kind(value: &str) -> Result<SearchKind> {
    match value.to_ascii_lowercase().as_str() {
        "song" | "songs" | "1" => Ok(SearchKind::Songs),
        "artist" | "artists" | "100" => Ok(SearchKind::Artists),
        "album" | "albums" | "10" => Ok(SearchKind::Albums),
        "playlist" | "playlists" | "1000" => Ok(SearchKind::Playlists),
        other => bail!("unknown search type `{other}`; use songs, artists, albums or playlists"),
    }
}

fn tail_file(path: &Path, lines: usize) -> io::Result<String> {
    let text = fs::read_to_string(path)?;
    let mut all: Vec<&str> = text.lines().collect();
    if all.len() > lines {
        all = all.split_off(all.len() - lines);
    }
    Ok(all.join("\n"))
}
