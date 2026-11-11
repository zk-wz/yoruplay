use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use fs2::FileExt;
use termtree::Tree;
use yoruplay_core::audio_cache::AudioCache;
use yoruplay_core::cache::FileCache;
use yoruplay_ipc::{shutdown_any_version, IpcClient};
use yoruplay_protocol::config::{ConfigKey, CONFIG_KEYS};
use yoruplay_protocol::{
    Command, CommandResult, Config, ErrorInfo, Paths, PlayMode, PlaylistKind, PlaylistTracks,
    Quality, SearchItems, SearchKind, SearchPage, Song, SongsView, StateSnapshot, LOGIN_URL,
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
    Tui,
    /// Manage the background daemon.
    Daemon(DaemonArgs),
    /// Playback controls.
    Control(ControlArgs),
    /// Queue management.
    Queue(QueueArgs),
    /// Browse the NetEase library.
    Library(LibraryArgs),
    /// Search songs, artists, albums or playlists.
    Search(SearchArgs),
    /// Account login and logout.
    Auth(AuthArgs),
    /// Manage metadata and complete offline audio.
    Cache(CacheArgs),
    /// Inspect or atomically update typed configuration.
    Config(ConfigArgs),
    /// List all generated paths without creating anything.
    Listpath,
    /// Check the Rust audio device, cache filesystem and daemon.
    Doctor,
    /// Print log file locations and tail commands.
    Logs,
    /// Remove cache and runtime state (never credentials/config).
    Uninstall {
        /// Confirm removal without prompting.
        #[arg(long)]
        yes: bool,
        /// Also delete the installed executable when it is under HOME.
        #[arg(long)]
        remove_binary: bool,
    },
    /// Internal foreground daemon (hidden; used by `daemon start`).
    #[command(name = "__daemon", hide = true)]
    InternalDaemon,
}

#[derive(Args, Debug)]
struct DaemonArgs {
    #[command(subcommand)]
    action: Option<DaemonAction>,
}

#[derive(Subcommand, Debug)]
enum DaemonAction {
    /// Start a detached daemon and wait until its socket is ready.
    Start,
    /// Gracefully stop the daemon and release audio resources.
    Stop,
    /// Show lightweight daemon and playback status.
    Status,
}

#[derive(Args, Debug)]
struct ControlArgs {
    #[command(subcommand)]
    action: Option<ControlAction>,
}

#[derive(Subcommand, Debug)]
enum ControlAction {
    /// Start or resume playback.
    Resume,
    /// Pause playback.
    Pause,
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
}

#[derive(Args, Debug)]
struct QueueArgs {
    #[command(subcommand)]
    action: Option<QueueAction>,
}

#[derive(Subcommand, Debug)]
enum QueueAction {
    /// Print a page of the queue.
    List {
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Play one complete cached song without network access.
    PlayCache { id: u64 },
    /// Play a queue entry by zero-based index.
    Play { index: usize },
    /// Remove one queue entry by zero-based index.
    Remove { index: usize },
    /// Clear the queue.
    Clear,
    /// Replace the queue with a playlist and start at an index.
    StartPlaylist {
        id: u64,
        #[arg(long, default_value_t = 0)]
        start: usize,
    },
    /// Append all songs of a playlist to the queue.
    AddPlaylist { id: u64 },
}

#[derive(Args, Debug)]
struct LibraryArgs {
    #[command(subcommand)]
    action: Option<LibraryAction>,
}

#[derive(Subcommand, Debug)]
enum LibraryAction {
    /// List your created or subscribed playlists.
    Playlists {
        #[arg(long, default_value = "created")]
        kind: String,
    },
    /// List liked songs.
    Liked {
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
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
}

#[derive(Args, Debug)]
struct AuthArgs {
    #[command(subcommand)]
    action: Option<AuthAction>,
}

#[derive(Subcommand, Debug)]
enum AuthAction {
    /// Log in through Firefox, or import a Cookie header from stdin.
    Login(LoginArgs),
    /// Log out and remove the saved NetEase session.
    Logout,
}

#[derive(Args, Debug)]
struct CacheArgs {
    #[command(subcommand)]
    action: Option<CacheAction>,
}

#[derive(Subcommand, Debug)]
enum CacheAction {
    /// Remove metadata only unless an explicit audio/all scope is selected.
    Clear {
        #[arg(long, conflicts_with = "all")]
        audio: bool,
        #[arg(long, conflicts_with = "audio")]
        all: bool,
    },
    /// Show metadata, audio, index, scratch and log accounting.
    Info,
    /// List complete offline audio summaries (at most 100 per page).
    List {
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Page size, from 1 to 100.
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Explicitly upgrade a cached song; requires a running daemon.
    Upgrade {
        id: u64,
        #[arg(long)]
        quality: Option<String>,
    },
}

#[derive(Args, Debug)]
struct ConfigArgs {
    #[command(subcommand)]
    action: Option<ConfigAction>,
}

#[derive(Subcommand, Debug)]
enum ConfigAction {
    /// List all typed keys, effective values and application boundaries.
    List,
    /// Show one effective setting and its source.
    Get { key: String },
    /// Validate and atomically persist one setting, retaining comments.
    Set { key: String, value: String },
    /// Reset one key; resetting every key requires --all --yes.
    Reset {
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        key: Option<String>,
        #[arg(long, conflicts_with = "key", requires = "yes")]
        all: bool,
        #[arg(long, requires = "all")]
        yes: bool,
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

/// Intercept only root/parent help. Leaf commands keep clap's detailed
/// `--help` output. Filtering `--json` keeps global option placement flexible.
fn intercept_help() -> bool {
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|arg| arg != "--json")
        .collect();
    let topic = match args.as_slice() {
        [] => Some(HelpTopic::Root),
        [arg] if matches!(arg.as_str(), "--help" | "-h") => Some(HelpTopic::Root),
        [name] if name == "help" => Some(HelpTopic::Root),
        [name] => match name.as_str() {
            "daemon" => Some(HelpTopic::Daemon),
            "control" => Some(HelpTopic::Control),
            "queue" => Some(HelpTopic::Queue),
            "library" => Some(HelpTopic::Library),
            "auth" => Some(HelpTopic::Auth),
            "cache" => Some(HelpTopic::Cache),
            "config" => Some(HelpTopic::Config),
            _ => None,
        },
        [name, flag] if matches!(flag.as_str(), "--help" | "-h" | "help") => match name.as_str() {
            "daemon" => Some(HelpTopic::Daemon),
            "control" => Some(HelpTopic::Control),
            "queue" => Some(HelpTopic::Queue),
            "library" => Some(HelpTopic::Library),
            "auth" => Some(HelpTopic::Auth),
            "cache" => Some(HelpTopic::Cache),
            "config" => Some(HelpTopic::Config),
            _ => None,
        },
        [name, sub] if name == "help" => match sub.as_str() {
            "daemon" => Some(HelpTopic::Daemon),
            "control" => Some(HelpTopic::Control),
            "queue" => Some(HelpTopic::Queue),
            "library" => Some(HelpTopic::Library),
            "auth" => Some(HelpTopic::Auth),
            "cache" => Some(HelpTopic::Cache),
            "config" => Some(HelpTopic::Config),
            _ => None,
        },
        _ => None,
    };
    if let Some(topic) = topic {
        print_help(topic);
        true
    } else {
        false
    }
}

#[tokio::main(worker_threads = 2)]
async fn main() -> Result<()> {
    if intercept_help() {
        return Ok(());
    }
    let cli = Cli::parse();
    let paths = Paths::discover().context("discover XDG directories")?;
    let command = cli.command.expect("checked above");
    match command {
        CliCommand::InternalDaemon => {
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
            let config = Config::load_readonly(&paths.config_file())?;
            init_stderr_logging(&config.log.level);
            run_command(other, cli.json, paths, config).await
        }
    }
}

async fn run_command(command: CliCommand, json: bool, paths: Paths, config: Config) -> Result<()> {
    match command {
        CliCommand::Daemon(args) => daemon_command(args, &paths, json).await,
        CliCommand::Control(args) => control_command(args, &paths, json).await,
        CliCommand::Queue(args) => queue_command(args, &paths, json).await,
        CliCommand::Library(args) => library_command(args, &paths, json).await,
        CliCommand::Search(args) => search_command(args, &paths, json).await,
        CliCommand::Auth(args) => auth_command(args, &paths, json).await,
        CliCommand::Cache(args) => cache_command(args, &paths, &config, json).await,
        CliCommand::Config(args) => config_command(args, &paths, &config, json).await,
        CliCommand::Listpath => listpath(&paths, &config, json).await,
        CliCommand::Doctor => doctor(&paths, &config, json).await,
        CliCommand::Logs => {
            let value = serde_json::json!({"daemon":paths.log_file(),"tui":paths.tui_log_file()});
            if json {
                print_json(&value)
            } else {
                println!("Daemon log: tail -f {}", paths.log_file().display());
                println!("TUI log: tail -f {}", paths.tui_log_file().display());
                Ok(())
            }
        }
        CliCommand::Uninstall { yes, remove_binary } => {
            uninstall(&paths, &config, yes, remove_binary, json).await
        }
        CliCommand::Tui | CliCommand::InternalDaemon => unreachable!("handled in main"),
    }
}

async fn daemon_command(args: DaemonArgs, paths: &Paths, json: bool) -> Result<()> {
    if args.action.is_none() {
        print_help(HelpTopic::Daemon);
        return Ok(());
    }
    match args.action.expect("checked above") {
        DaemonAction::Start => match IpcClient::connect(paths.socket_file()).await {
            Ok(client) => {
                client.close().await;
                if json {
                    print_json(&CommandResult::ack_msg("Daemon is already running."))?;
                } else {
                    println!("Daemon is already running.");
                }
                Ok(())
            }
            Err(_) => {
                let client = ensure_daemon(paths).await?;
                client.close().await;
                if json {
                    print_json(&CommandResult::ack_msg("Daemon started in the background."))?;
                } else {
                    println!("Daemon started in the background.");
                }
                Ok(())
            }
        },
        DaemonAction::Stop => shutdown_daemon(paths, json).await,
        DaemonAction::Status => status_command(paths, json).await,
    }
}

async fn status_command(paths: &Paths, json: bool) -> Result<()> {
    let result = send(paths, Command::GetPlayback).await?;
    if json {
        return print_json(&result);
    }
    print_result(result)
}

async fn control_command(args: ControlArgs, paths: &Paths, json: bool) -> Result<()> {
    if args.action.is_none() {
        print_help(HelpTopic::Control);
        return Ok(());
    }
    match args.action.expect("checked above") {
        ControlAction::Resume => ack(paths, Command::Play, json).await,
        ControlAction::Pause => ack(paths, Command::Pause, json).await,
        ControlAction::Stop => ack(paths, Command::Stop, json).await,
        ControlAction::Next => ack(paths, Command::Next, json).await,
        ControlAction::Previous => ack(paths, Command::Previous, json).await,
        ControlAction::Mode { mode } => match mode {
            Some(mode) => {
                let mode = parse_mode(&mode)?;
                ack(paths, Command::SetMode { mode }, json).await
            }
            None => ack(paths, Command::CycleMode, json).await,
        },
        ControlAction::Volume { value } => match value {
            Some(value) => {
                if !(0.0..=100.0).contains(&value) {
                    bail!("volume must be between 0 and 100");
                }
                ack(
                    paths,
                    Command::SetVolume {
                        volume: value / 100.0,
                    },
                    json,
                )
                .await
            }
            None => {
                let result = send(paths, Command::GetPlayback).await?;
                if json {
                    return print_json(&result);
                }
                let CommandResult::Playback(playback) = result else {
                    bail!("unexpected daemon response");
                };
                println!("Volume: {:.0}%", playback.volume * 100.0);
                Ok(())
            }
        },
        ControlAction::Seek { offset } => {
            let seconds: f64 = offset
                .trim_start_matches('+')
                .parse()
                .with_context(|| format!("invalid seek offset `{offset}`"))?;
            ack(
                paths,
                Command::SeekRelative {
                    offset_ms: (seconds * 1000.0) as i64,
                },
                json,
            )
            .await
        }
    }
}

async fn queue_command(args: QueueArgs, paths: &Paths, json: bool) -> Result<()> {
    if args.action.is_none() {
        print_help(HelpTopic::Queue);
        return Ok(());
    }
    match args.action.expect("checked above") {
        QueueAction::List { offset, limit } => {
            let result = send(paths, Command::GetQueue { offset, limit }).await?;
            if json {
                return print_json(&result);
            }
            match result {
                CommandResult::Queue(queue) => print_queue(&queue),
                other => return print_result(other),
            }
            Ok(())
        }
        QueueAction::PlayCache { id } => {
            ack(paths, Command::PlayCached { song_id: id }, json).await
        }
        QueueAction::Play { index } => ack(paths, Command::QueuePlay { index }, json).await,
        QueueAction::Remove { index } => ack(paths, Command::QueueRemove { index }, json).await,
        QueueAction::Clear => ack(paths, Command::QueueClear, json).await,
        QueueAction::StartPlaylist { id, start } => {
            ack(
                paths,
                Command::StartPlaylist {
                    id,
                    start_index: start,
                    start_song_id: None,
                },
                json,
            )
            .await
        }
        QueueAction::AddPlaylist { id } => ack(paths, Command::AddPlaylist { id }, json).await,
    }
}

async fn library_command(args: LibraryArgs, paths: &Paths, json: bool) -> Result<()> {
    if args.action.is_none() {
        print_help(HelpTopic::Library);
        return Ok(());
    }
    match args.action.expect("checked above") {
        LibraryAction::Playlists { kind } => {
            let kind = match kind.to_ascii_lowercase().as_str() {
                "created" | "mine" => PlaylistKind::Created,
                "subscribed" | "collected" => PlaylistKind::Subscribed,
                other => bail!("unknown playlist kind `{other}`; use created or subscribed"),
            };
            let result = send(paths, Command::Playlists { kind }).await?;
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
        LibraryAction::Liked { offset, limit } => {
            validate_page_limit(limit)?;
            tracks_command(paths, Command::LikedTracks { offset, limit }, json).await
        }
        LibraryAction::Recent => songs_command(paths, Command::RecentSongs, json).await,
        LibraryAction::Daily { refresh } => {
            songs_command(paths, Command::DailySongs { refresh }, json).await
        }
        LibraryAction::DailyPlaylists { refresh } => {
            let result = send(paths, Command::DailyPlaylists { refresh }).await?;
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
        LibraryAction::Radar { refresh } => {
            tracks_command(paths, Command::Radar { refresh }, json).await
        }
        LibraryAction::Fm => ack(paths, Command::PersonalFm, json).await,
        LibraryAction::Playlist { id, offset, limit } => {
            validate_page_limit(limit)?;
            tracks_command(paths, Command::PlaylistTracks { id, offset, limit }, json).await
        }
    }
}

async fn search_command(args: SearchArgs, paths: &Paths, json: bool) -> Result<()> {
    let kind = parse_search_kind(&args.kind)?;
    let result = send(
        paths,
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

async fn auth_command(args: AuthArgs, paths: &Paths, json: bool) -> Result<()> {
    if args.action.is_none() {
        print_help(HelpTopic::Auth);
        return Ok(());
    }
    match args.action.expect("checked above") {
        AuthAction::Login(args) => login(paths, args, json).await,
        AuthAction::Logout => {
            let result = send(paths, Command::Logout).await?;
            if json {
                return print_json(&result);
            }
            println!("Logged out.");
            Ok(())
        }
    }
}

#[derive(Clone, Copy)]
enum HelpTopic {
    Root,
    Daemon,
    Control,
    Queue,
    Library,
    Auth,
    Cache,
    Config,
}

fn help_item(name: &str, description: &str) -> String {
    format!("{name:<24} {description}")
}

fn help_daemon(root: bool) -> Tree<String> {
    let label = if root {
        help_item("daemon", "Manage the background daemon")
    } else {
        "yoruplay daemon".to_string()
    };
    Tree::new(label).with_leaves([
        help_item("start", "Start a detached daemon and wait for its socket"),
        help_item("stop", "Gracefully stop the daemon and release audio"),
        help_item("status", "Show lightweight daemon and playback status"),
    ])
}

fn help_control(root: bool) -> Tree<String> {
    let label = if root {
        help_item("control", "Playback controls")
    } else {
        "yoruplay control".to_string()
    };
    Tree::new(label).with_leaves([
        help_item("resume", "Start or resume playback"),
        help_item("pause", "Pause playback"),
        help_item("stop", "Stop playback and reset the position"),
        help_item("next", "Play the next track"),
        help_item("previous", "Play the previous track"),
        help_item("mode [mode]", "Show or set the playback mode"),
        help_item("volume [value]", "Show or set the volume (0-100)"),
        help_item("seek <offset>", "Seek relative to the current position"),
    ])
}

fn help_queue(root: bool) -> Tree<String> {
    let label = if root {
        help_item("queue", "Queue management")
    } else {
        "yoruplay queue".to_string()
    };
    Tree::new(label).with_leaves([
        help_item("list", "Print a queue page (--offset, --limit)"),
        help_item(
            "play-cache <id>",
            "Play cached audio without network access",
        ),
        help_item("play <index>", "Play a queue entry"),
        help_item("remove <index>", "Remove a queue entry"),
        help_item("clear", "Clear the queue"),
        help_item(
            "start-playlist <id>",
            "Replace the queue with a playlist and play",
        ),
        help_item("add-playlist <id>", "Append a playlist to the queue"),
    ])
}

fn help_library(root: bool) -> Tree<String> {
    let label = if root {
        help_item("library", "Browse the NetEase library")
    } else {
        "yoruplay library".to_string()
    };
    Tree::new(label).with_leaves([
        help_item("playlists", "List created or subscribed playlists"),
        help_item("liked", "List liked songs (--offset, --limit)"),
        help_item("recent", "List server recently played songs"),
        help_item("daily", "List today's recommended songs"),
        help_item("daily-playlists", "List today's recommended playlists"),
        help_item("radar", "Resolve and print Private Radar"),
        help_item("fm", "Start Personal FM"),
        help_item("playlist <id>", "Print one playlist and its songs"),
    ])
}

fn help_auth(root: bool) -> Tree<String> {
    let label = if root {
        help_item("auth", "Account login and logout")
    } else {
        "yoruplay auth".to_string()
    };
    Tree::new(label).with_leaves([
        help_item("login", "Log in through Firefox or stdin Cookie"),
        help_item("logout", "Remove the saved NetEase session"),
    ])
}

fn help_cache(root: bool) -> Tree<String> {
    let label = if root {
        help_item("cache", "Metadata and complete offline audio")
    } else {
        "yoruplay cache".to_string()
    };
    Tree::new(label).with_leaves([
        help_item(
            "clear [--audio|--all]",
            "Default scope: metadata only; never logs",
        ),
        help_item("info", "Show audio, metadata, scratch and index accounting"),
        help_item("list", "List complete audio (--offset, --limit 1..100)"),
        help_item("upgrade <id>", "Explicit network upgrade (--quality)"),
    ])
}

fn help_config(root: bool) -> Tree<String> {
    let label = if root {
        help_item("config", "Typed configuration; no daemon autostart")
    } else {
        "yoruplay config".to_string()
    };
    let mut tree = Tree::new(label).with_leaves([
        help_item(
            "list",
            "List effective settings, types and apply boundaries",
        ),
        help_item("get <key>", "Read one setting without creating files"),
        help_item("set <key> <value>", "Persist one typed value atomically"),
        help_item("reset [key]", "Reset one key, or --all --yes"),
    ]);
    let mut keys = Tree::new("Configuration keys (cache.audio.max_gb also accepts G)".to_string());
    for key in CONFIG_KEYS {
        keys.push(help_item(
            key.key,
            &format!("{:?}; {}; {}", key.value_type, key.unit, key.apply),
        ));
    }
    tree.push(keys);
    tree
}

fn help_root() -> Tree<String> {
    let mut root = Tree::new("yoruplay".to_string());
    root.push(help_item(
        "tui",
        "Open the terminal interface (auto-starts daemon)",
    ));
    root.push(help_daemon(true));
    root.push(help_control(true));
    root.push(help_queue(true));
    root.push(help_library(true));
    root.push(help_item(
        "search <query>",
        "Search songs, artists, albums or playlists",
    ));
    root.push(help_auth(true));
    root.push(help_cache(true));
    root.push(help_config(true));
    root.push(help_item(
        "listpath",
        "List all generated paths without creating files",
    ));
    root.push(help_item(
        "doctor",
        "Check Rust audio, cache filesystem and daemon",
    ));
    root.push(help_item("logs", "Print log paths and tail commands"));
    root.push(help_item("uninstall", "Remove cache and runtime state"));
    root.push(help_item("help [command]", "Print this command tree"));
    root
}

fn print_help(topic: HelpTopic) {
    let tree = match topic {
        HelpTopic::Root => help_root(),
        HelpTopic::Daemon => help_daemon(false),
        HelpTopic::Control => help_control(false),
        HelpTopic::Queue => help_queue(false),
        HelpTopic::Library => help_library(false),
        HelpTopic::Auth => help_auth(false),
        HelpTopic::Cache => help_cache(false),
        HelpTopic::Config => help_config(false),
    };
    print!("{tree}");
    println!();
    println!("Global option: --json   print machine-readable JSON");
    if matches!(topic, HelpTopic::Root) {
        println!("Run `yoruplay <command> --help` for leaf options.");
    }
}

async fn send(paths: &Paths, command: Command) -> Result<CommandResult> {
    let client = connect(paths).await?;
    let result = client.request(command).await.map_err(ipc_error)?;
    client.close().await;
    Ok(result)
}

/// Stop the daemon. IPC shutdown saves the queue and releases audio; the raw
/// handshake is only for an older daemon during an upgrade.
async fn shutdown_daemon(paths: &Paths, json: bool) -> Result<()> {
    let message = match IpcClient::connect(paths.socket_file()).await {
        Ok(client) => {
            client.request(Command::Shutdown).await.map_err(ipc_error)?;
            client.close().await;
            if wait_for_socket_removal(paths, Duration::from_secs(10)).await {
                "Daemon stopped."
            } else {
                bail!("the daemon accepted shutdown but did not exit");
            }
        }
        Err(_) => {
            if shutdown_any_version(paths.socket_file()).await.is_ok() {
                if !wait_for_socket_removal(paths, Duration::from_secs(10)).await {
                    bail!("the daemon accepted shutdown but did not exit");
                }
                "Daemon stopped."
            } else {
                "Daemon is not running."
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

fn validate_page_limit(limit: usize) -> Result<()> {
    if (1..=1000).contains(&limit) {
        Ok(())
    } else {
        bail!("limit must be between 1 and 1000")
    }
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

async fn tracks_command(paths: &Paths, command: Command, json: bool) -> Result<()> {
    let result = send(paths, command).await?;
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

async fn connect(paths: &Paths) -> Result<IpcClient> {
    IpcClient::connect(paths.socket_file()).await.map_err(|e| {
        let info = e.to_error_info();
        anyhow!(
            "{}\n\nStart it with `yoruplay daemon` or open the TUI, which auto-starts it.",
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
        .arg("__daemon")
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

async fn login(paths: &Paths, args: LoginArgs, json: bool) -> Result<()> {
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
        print_auth_result(result, json)
    } else {
        // Fast path: a valid Firefox session is enough; do not open a browser.
        if !args.switch_account {
            match client
                .request(Command::ImportBrowserCookie { force: false })
                .await
            {
                Ok(result) => {
                    client.close().await;
                    return print_auth_result(result, json);
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

        eprintln!("Opening {LOGIN_URL} in your browser.");
        open_login_page();
        eprintln!("Log in there in Firefox. Waiting for a Firefox MUSIC_U cookie...");
        eprintln!("Other browsers are not supported yet; use --cookie-stdin for those.");
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
                    return print_auth_result(result, json);
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
                        eprintln!(
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

fn print_auth_result(result: CommandResult, json: bool) -> Result<()> {
    if json {
        return print_json(&result);
    }
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

async fn cache_command(args: CacheArgs, paths: &Paths, config: &Config, json: bool) -> Result<()> {
    let Some(action) = args.action else {
        print_help(HelpTopic::Cache);
        return Ok(());
    };
    if let CacheAction::List { limit, .. } = &action {
        if *limit == 0 || *limit > 100 {
            bail!("limit must be between 1 and 100");
        }
    }
    let command = match &action {
        CacheAction::Info => Command::CacheInfo,
        CacheAction::List { offset, limit } => Command::CacheList {
            offset: *offset,
            limit: *limit,
        },
        CacheAction::Clear { audio: true, all } => Command::ClearAudioCache { all: *all },
        CacheAction::Clear { all: true, .. } => Command::ClearAudioCache { all: true },
        CacheAction::Clear { .. } => Command::ClearCache,
        CacheAction::Upgrade { id, quality } => Command::UpgradeAudio {
            song_id: *id,
            quality: quality.as_deref().map(parse_quality).transpose()?,
        },
    };
    if let Ok(client) = IpcClient::connect(paths.socket_file()).await {
        let response = client.request(command).await;
        client.close().await;
        let mut result = response.map_err(ipc_error)?;
        if matches!(action, CacheAction::Info) {
            let CommandResult::Data(info) = &mut result else {
                bail!("unexpected cache information response");
            };
            let metadata = FileCache::new(paths.metadata_dir(), config.cache.max_mb);
            info["daemon_running"] = serde_json::json!(true);
            info["persisted_config"] = serde_json::json!(config);
            info["configured_audio_dir"] = serde_json::json!(paths.audio_dir(config));
            info["metadata"] = serde_json::json!({"dir":metadata.dir(),"bytes":metadata.size_bytes(),"configured_limit_bytes":metadata.max_bytes(),"configured_lyrics_ttl_hours":config.cache.lyrics_ttl_hours});
            info["logs"] = serde_json::json!({"daemon_bytes":file_bytes(&paths.log_file()),"tui_bytes":file_bytes(&paths.tui_log_file()),"included_in_cache":false});
        }
        return if json {
            print_json(&result)
        } else {
            print_result(result)
        };
    }
    let dir = paths.audio_dir(config);
    let max_bytes = audio_budget(config);
    let metadata = FileCache::new(paths.metadata_dir(), config.cache.max_mb);
    let value = match action {
        CacheAction::Info => {
            let audio = AudioCache::inspect(&dir, max_bytes, config.cache.audio.enabled)?;
            serde_json::json!({
                "daemon_running": false,
                "metadata": {"dir":metadata.dir(),"bytes":metadata.size_bytes(),"limit_bytes":metadata.max_bytes(),"lyrics_ttl_hours":config.cache.lyrics_ttl_hours},
                "audio":audio,
                "logs":{"daemon_bytes":file_bytes(&paths.log_file()),"tui_bytes":file_bytes(&paths.tui_log_file()),"included_in_cache":false},
                "persisted_config":config,
                "active_config":null,
                "pending":"daemon_not_running"
            })
        }
        CacheAction::List { offset, limit } => {
            let entries = AudioCache::list_offline(&dir, offset, limit)?;
            serde_json::json!({"entries":entries,"offset":offset,"limit":limit,"daemon_running":false})
        }
        CacheAction::Clear { audio, all } => {
            if audio || all {
                let cache = AudioCache::open(dir, max_bytes, config.cache.audio.enabled)?;
                let audio_result = cache.clear()?;
                let before = metadata.size_bytes();
                if all { metadata.clear()?; }
                serde_json::json!({"audio":audio_result,"metadata_removed_bytes":if all { before.saturating_sub(metadata.size_bytes()) } else { 0 },"scope":if all { "all" } else { "audio" },"logs_preserved":true})
            } else {
                // Do not open/reconfigure the audio owner: metadata clear must never trim audio.
                fs::create_dir_all(&dir)?;
                let lock = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(paths.cache_lock_file(config))?;
                lock.try_lock_exclusive().context("cache is owned by a daemon; retry using its IPC connection")?;
                let before = metadata.size_bytes();
                metadata.clear()?;
                serde_json::json!({"scope":"metadata","removed_bytes":before.saturating_sub(metadata.size_bytes()),"audio_preserved":true,"logs_preserved":true})
            }
        }
        CacheAction::Upgrade { .. } => bail!("cache upgrade requires a running daemon; start it explicitly with `yoruplay daemon start`"),
    };
    if json {
        print_json(&CommandResult::Data(value))
    } else {
        print_json(&value)
    }
}

fn audio_budget(config: &Config) -> u64 {
    (config.cache.audio.max_gb * 1024.0 * 1024.0 * 1024.0) as u64
}

fn file_bytes(path: &Path) -> u64 {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

fn parse_quality(input: &str) -> Result<Quality> {
    match input {
        "standard" => Ok(Quality::Standard),
        "higher" => Ok(Quality::Higher),
        "exhigh" => Ok(Quality::Exhigh),
        "lossless" => Ok(Quality::Lossless),
        "hires" => Ok(Quality::Hires),
        _ => bail!("invalid quality `{input}`; use standard, higher, exhigh, lossless or hires"),
    }
}

/// Local IPC only. Absence and connection/reload failures remain distinguishable.
async fn daemon_data(
    paths: &Paths,
    command: Command,
) -> (Option<serde_json::Value>, Option<String>) {
    if !paths.socket_file().exists() {
        return (None, None);
    }
    match IpcClient::connect(paths.socket_file()).await {
        Ok(client) => {
            let result = client.request(command).await;
            client.close().await;
            match result {
                Ok(CommandResult::Data(value)) => (Some(value), None),
                Ok(other) => (None, Some(format!("unexpected daemon response: {other:?}"))),
                Err(error) => (None, Some(ipc_error(error).to_string())),
            }
        }
        Err(error) => (None, Some(error.to_string())),
    }
}

fn dotted_value<'a>(mut value: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    for part in key.split('.') {
        value = value.get(part)?;
    }
    Some(value)
}

async fn config_command(
    args: ConfigArgs,
    paths: &Paths,
    config: &Config,
    json: bool,
) -> Result<()> {
    let Some(action) = args.action else {
        print_help(HelpTopic::Config);
        return Ok(());
    };
    match action {
        ConfigAction::List | ConfigAction::Get { .. } => {
            let selected = match &action {
                ConfigAction::Get { key } => Some(ConfigKey::find(key)?.key),
                _ => None,
            };
            let persisted = Config::persisted_keys(&paths.config_file())?;
            let defaults = Config::default();
            let (runtime, runtime_error) = daemon_data(paths, Command::CacheInfo).await;
            let active_config = runtime
                .as_ref()
                .and_then(|value| value.get("active_config"));
            let mut entries = Vec::new();
            for entry in CONFIG_KEYS
                .iter()
                .filter(|entry| selected.is_none_or(|key| entry.key == key))
            {
                let value = config.value(entry.key)?;
                let active = active_config.and_then(|active| dotted_value(active, entry.key));
                let pending = match active {
                    Some(active) => serde_json::json!(active != &value),
                    None => serde_json::Value::Null,
                };
                entries.push(serde_json::json!({
                    "key":entry.key,"value":value,"default":defaults.value(entry.key)?,
                    "type":entry.value_type,"unit":entry.unit,"choices":entry.choices,
                    "source":if persisted.iter().any(|key| key == entry.key) { "file" } else { "default" },
                    "apply":entry.apply,"active":active,"pending":pending,
                    "runtime_state":if active_config.is_some() { "reported_by_daemon" } else if runtime_error.is_some() { "unavailable" } else { "daemon_not_running" },
                    "environment_override":if entry.key == "log.level" { std::env::var("RUST_LOG").ok() } else { None }
                }));
            }
            let value = serde_json::json!({"config_file":paths.config_file(),"entries":entries,"runtime_error":runtime_error});
            if json {
                print_json(&value)
            } else {
                for entry in value["entries"].as_array().expect("entries array") {
                    println!(
                        "{} = {}  [{}; {}; source={}; apply={}; active={}; pending={}; runtime={}]",
                        entry["key"].as_str().unwrap_or(""),
                        entry["value"],
                        entry["type"],
                        entry["unit"],
                        entry["source"],
                        entry["apply"],
                        entry["active"],
                        entry["pending"],
                        entry["runtime_state"]
                    );
                    if !entry["environment_override"].is_null() {
                        println!("  RUST_LOG override: {}", entry["environment_override"]);
                    }
                }
                if let Some(error) = value["runtime_error"].as_str() {
                    println!("Runtime unavailable: {error}");
                }
                Ok(())
            }
        }
        ConfigAction::Set { key, value } => {
            let updated = Config::set(&paths.config_file(), &key, &value)?;
            report_config_write(paths, &updated, Some(&key), json).await
        }
        ConfigAction::Reset { key, all, yes } => {
            let updated = if all && yes {
                Config::reset_all(&paths.config_file())?
            } else if let Some(key) = key.as_deref() {
                Config::reset(&paths.config_file(), key)?
            } else {
                bail!("reset requires a key, or --all --yes");
            };
            report_config_write(paths, &updated, key.as_deref(), json).await
        }
    }
}

async fn report_config_write(
    paths: &Paths,
    config: &Config,
    key: Option<&str>,
    json: bool,
) -> Result<()> {
    let (runtime, error) = daemon_data(paths, Command::ReloadConfig).await;
    let value = serde_json::json!({
        "persisted":true,"config_file":paths.config_file(),"key":key,
        "value":key.map(|key| config.value(key)).transpose()?,
        "runtime":runtime,"runtime_error":error,
        "status":if runtime.is_some() { "daemon_reported" } else if error.is_some() { "persisted_reload_failed" } else { "persisted_daemon_not_running" },
        "applied":runtime.as_ref().and_then(|value| value.get("applied")),
        "pending":runtime.as_ref().and_then(|value| value.get("pending")).cloned().unwrap_or_else(|| serde_json::json!("daemon_start_required")),
        "RUST_LOG":std::env::var("RUST_LOG").ok(),
    });
    if json {
        print_json(&value)?;
    } else {
        println!("Persisted: {}", paths.config_file().display());
        println!("Runtime: {}", value["status"]);
        println!(
            "Applied: {}; pending: {}",
            value["applied"], value["pending"]
        );
        if let Some(error) = value["runtime_error"].as_str() {
            println!("Reload failed: {error}");
        }
    }
    if error.is_some() {
        bail!("configuration persisted, but the running daemon could not reload it");
    }
    Ok(())
}

async fn listpath(paths: &Paths, config: &Config, json: bool) -> Result<()> {
    let (runtime, runtime_error) = daemon_data(paths, Command::CacheInfo).await;
    let active = runtime
        .as_ref()
        .and_then(|value| value.get("active_audio_dir"))
        .cloned();
    let mut entries = paths.inventory(config);
    if let Some(path) = active.as_ref().and_then(|value| value.as_str()) {
        let path = PathBuf::from(path);
        if path != paths.audio_dir(config) {
            entries.push(yoruplay_protocol::paths::PathEntry {
                exists: Some(path.exists()),
                path,
                kind: "directory",
                purpose: "Active daemon audio directory (configured path pending restart)",
                lifecycle: "old cache preserved; no automatic migration or deletion",
            });
        }
    }
    let value = serde_json::json!({
        "paths":entries,"configured_audio_dir":paths.audio_dir(config),
        "active_audio_dir":active,"runtime_error":runtime_error,
        "audio_directory_pending_restart":active.as_ref().and_then(|path| path.as_str()).map(|path| Path::new(path) != paths.audio_dir(config)),
    });
    if json {
        print_json(&value)
    } else {
        for entry in entries {
            println!(
                "{}  [{}; exists={:?}] {} — {}",
                entry.path.display(),
                entry.kind,
                entry.exists,
                entry.purpose,
                entry.lifecycle
            );
        }
        println!("Configured audio: {}", paths.audio_dir(config).display());
        println!(
            "Active audio: {}; pending restart: {}",
            value["active_audio_dir"], value["audio_directory_pending_restart"]
        );
        if let Some(error) = value["runtime_error"].as_str() {
            println!("Runtime unavailable: {error}");
        }
        Ok(())
    }
}

async fn doctor(paths: &Paths, config: &Config, _json: bool) -> Result<()> {
    let device = config.playback.audio_device.clone();
    let audio = tokio::task::spawn_blocking(move || yoruplay_core::audio::doctor(&device)).await?;
    let dir = paths.audio_dir(config);
    let filesystem =
        tokio::task::spawn_blocking(move || yoruplay_core::audio_cache::probe_filesystem(&dir))
            .await?;
    let failed = audio.is_err() || filesystem.is_err();
    let daemon = match IpcClient::connect(paths.socket_file()).await {
        Ok(client) => {
            let result = client.request(Command::GetPlayback).await;
            client.close().await;
            match result {
                Ok(result) => serde_json::to_value(result)?,
                Err(error) => serde_json::json!({"error":ipc_error(error).to_string()}),
            }
        }
        Err(error) => serde_json::json!({"running":false,"detail":error.to_string()}),
    };
    let value = serde_json::json!({
        "version":env!("CARGO_PKG_VERSION"),"config":paths.config_file(),"auth":paths.auth_file(),"socket":paths.socket_file(),
        "auth_permissions":fs::metadata(paths.auth_file()).ok().map(|meta| format!("{:o}",meta.permissions().mode() & 0o777)),
        "audio":match audio { Ok(value) => value, Err(error) => serde_json::json!({"error":error.to_string()}) },
        "cache_filesystem":match filesystem { Ok(value) => value, Err(error) => serde_json::json!({"error":error.to_string()}) },
        "daemon":daemon,
    });
    print_json(&value)?;
    if failed {
        bail!("audio or cache-filesystem diagnostics failed; see the diagnostic output");
    }
    Ok(())
}

async fn uninstall(
    paths: &Paths,
    config: &Config,
    yes: bool,
    remove_binary: bool,
    json: bool,
) -> Result<()> {
    if !yes {
        bail!("Refusing to remove cache/runtime state without --yes. Credentials and config are never removed.");
    }
    if paths.socket_file().exists() {
        let client = connect(paths).await?;
        let result = client.request(Command::Shutdown).await;
        client.close().await;
        result.map_err(ipc_error)?;
        if !wait_for_socket_removal(paths, Duration::from_secs(10)).await {
            bail!("daemon did not exit; refusing to remove its cache/runtime state");
        }
    }
    let cache = AudioCache::open(
        paths.audio_dir(config),
        audio_budget(config),
        config.cache.audio.enabled,
    )?;
    let audio = cache.clear()?;
    let metadata = FileCache::new(paths.metadata_dir(), config.cache.max_mb);
    let before = metadata.size_bytes();
    metadata.clear()?;
    let mut binary_removed = None;
    let mut binary_preserved = std::env::current_exe().ok();
    if remove_binary {
        if let Some(exe) = binary_preserved.as_ref() {
            let home = std::env::var_os("HOME").map(PathBuf::from);
            if home.as_ref().is_some_and(|home| exe.starts_with(home)) {
                fs::remove_file(exe).with_context(|| format!("remove {}", exe.display()))?;
                binary_removed = Some(exe.clone());
                binary_preserved = None;
            } else {
                bail!(
                    "refusing to remove executable outside HOME: {}",
                    exe.display()
                );
            }
        }
    }
    let value = serde_json::json!({
        "audio":audio,"metadata_removed_bytes":before.saturating_sub(metadata.size_bytes()),
        "preserved":{"auth":paths.auth_file(),"config":paths.config_file(),"logs":[paths.log_file(),paths.tui_log_file()],"state":paths.state_file()},
        "binary_removed":binary_removed,"binary_preserved":binary_preserved,
    });
    if json {
        print_json(&value)
    } else {
        println!("Removed managed audio and metadata; daemon stopped. Credentials, configuration, logs and saved playback state preserved.");
        print_json(&value)
    }
}

fn load_config(paths: &Paths) -> Result<Config> {
    paths.ensure_dirs().context("create XDG directories")?;
    Config::load_or_create(&paths.config_file()).map_err(Into::into)
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

fn print_json<T: serde::Serialize + ?Sized>(result: &T) -> Result<()> {
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
        CommandResult::Playback(playback) => print_playback(&playback),
        CommandResult::Queue(queue) => print_queue(&queue),
        CommandResult::Data(value) => print_json(&value)?,
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

fn print_state(snapshot: &StateSnapshot) {
    match &snapshot.auth.profile {
        Some(profile) if snapshot.auth.logged_in => {
            println!("Account: {} ({})", profile.nickname, profile.user_id)
        }
        _ if snapshot.auth.session_expired => println!("Account: session expired"),
        _ => println!("Account: not logged in"),
    }
    print_playback(&snapshot.playback);
    for warning in &snapshot.warnings {
        println!("Warning: {warning}");
    }
}

fn print_playback(p: &yoruplay_protocol::PlaybackState) {
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
        "Volume:  {:.0}%   audio: {}",
        p.volume * 100.0,
        if p.audio_available {
            "available"
        } else {
            "missing/stopped"
        }
    );
    if let Some(source) = &p.audio_source {
        println!(
            "Audio source: {source}; actual quality: {}",
            p.actual_quality.as_deref().unwrap_or("unknown")
        );
    }
    if let Some(error) = &p.error {
        println!("Error:   {error}");
    }
    if let Some(warning) = &p.warning {
        println!("Warning: {warning}");
    }
}

fn print_queue(queue: &yoruplay_protocol::QueueSnapshot) {
    if queue.items.is_empty() {
        println!("Queue is empty.");
        return;
    }
    for (index, item) in queue.items.iter().enumerate() {
        let marker = if Some(queue.offset + index) == queue.current {
            ">"
        } else {
            " "
        };
        println!(
            "{marker} {:>4}  {} — {} [{}]",
            queue.offset + index,
            item.song.name,
            item.song.artist_text(),
            item.song.id
        );
    }
    println!(
        "{} total tracks, page offset {}, revision {}, mode {}, source {}",
        queue.total,
        queue.offset,
        queue.revision,
        queue.mode.short(),
        queue_source(&queue.source)
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
    if tracks.has_more {
        println!("more available: --offset {}", tracks.next_offset);
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
    const MAX_TAIL_BYTES: u64 = 64 * 1024;
    if lines == 0 {
        return Ok(String::new());
    }
    let mut file = fs::File::open(path)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(MAX_TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity((length - start) as usize);
    file.take(MAX_TAIL_BYTES).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let text = if start > 0 {
        text.split_once('\n').map(|(_, rest)| rest).unwrap_or("")
    } else {
        &text
    };
    let text = text.trim_end_matches('\n');
    let begin = text
        .char_indices()
        .rev()
        .filter(|(_, ch)| *ch == '\n')
        .nth(lines - 1)
        .map(|(index, _)| index + 1)
        .unwrap_or(0);
    Ok(text[begin..].to_string())
}

#[cfg(test)]
mod tail_file_tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn tails_exactly_the_requested_number_of_lines() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("log");
        for text in ["first\nsecond\nlast", "first\nsecond\nlast\n"] {
            fs::write(&path, text).unwrap();
            assert_eq!(tail_file(&path, 1).unwrap(), "last");
            assert_eq!(tail_file(&path, 2).unwrap(), "second\nlast");
            assert_eq!(tail_file(&path, 0).unwrap(), "");
        }
    }

    #[test]
    fn large_logs_are_read_from_a_bounded_tail_window() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("large.log");
        let mut file = fs::File::create(&path).unwrap();
        file.seek(SeekFrom::Start(16 * 1024 * 1024)).unwrap();
        file.write_all(b"\nlast\n").unwrap();
        assert_eq!(tail_file(&path, 1).unwrap(), "last");
        assert!(tail_file(&path, usize::MAX).unwrap().len() <= 64 * 1024);
    }
}
