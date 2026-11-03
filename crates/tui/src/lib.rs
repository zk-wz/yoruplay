//! YoruPlay terminal interface.

pub mod app;
pub mod ui;

use yoruplay_protocol::{Config, Paths};

pub async fn run(paths: Paths, config: Config) -> anyhow::Result<()> {
    crate::run_impl(paths, config).await
}

async fn run_impl(paths: Paths, config: Config) -> anyhow::Result<()> {
    use crossterm::event::{Event as CrosstermEvent, EventStream, KeyEventKind};
    use crossterm::execute;
    use crossterm::terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
    };
    use futures_util::StreamExt;
    use ratatui::backend::CrosstermBackend;
    use ratatui::Terminal;
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use yoruplay_ipc::IpcClient;

    use crate::app::{App, UiResponse};

    let client = Arc::new(
        IpcClient::connect(paths.socket_file())
            .await
            .map_err(|e| anyhow::anyhow!(e.to_error_info().message))?,
    );
    let mut daemon_events = client.subscribe();
    let (response_tx, mut response_rx) = mpsc::channel::<UiResponse>(64);
    let mut app = App::new(client.clone(), response_tx, config.ui.language);
    app.initial_load();

    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            std::io::stdout(),
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        original_hook(info);
    }));

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, crossterm::cursor::Hide)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let result = loop {
        if app.dirty {
            if let Err(e) = terminal.draw(|frame| crate::ui::draw(&mut app, frame)) {
                break Err(e.into());
            }
            app.dirty = false;
        }
        if app.quit {
            break Ok(());
        }
        tokio::select! {
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(CrosstermEvent::Key(key))) if key.kind == KeyEventKind::Press => {
                        app.handle_key(key);
                    }
                    Some(Ok(CrosstermEvent::Resize(_, _))) => {
                        app.dirty = true;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => break Err(e.into()),
                    None => break Ok(()),
                }
            }
            event = daemon_events.recv() => {
                match event {
                    Ok(event) => app.handle_event(event),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        app.set_status(yoruplay_protocol::NoticeLevel::Warning, "UI fell behind the daemon; resynchronising.");
                        let kind = crate::app::RequestKind::RefreshState;
                        app.send_external(kind, yoruplay_protocol::Command::GetState);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        app.quit = true;
                        app.set_status(yoruplay_protocol::NoticeLevel::Warning, "The daemon connection closed.");
                    }
                }
            }
            response = response_rx.recv() => {
                if let Some(response) = response {
                    app.handle_response(response);
                }
            }
            _ = ticker.tick() => app.tick(),
        }
    };
    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
    let _ = terminal.show_cursor();
    result
}
