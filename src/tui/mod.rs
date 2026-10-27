mod app;
mod host;
mod input;
mod markdown;
mod render;
mod theme;

use std::io::stdout;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    EventStream,
};
use crossterm::execute;
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

pub use app::Options;
pub use host::TuiError;

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
        ratatui::restore();
    }
}

pub async fn run(
    state_directory: PathBuf,
    options: Options,
    cancellation: CancellationToken,
) -> Result<(), TuiError> {
    let host = host::Host::new(state_directory.clone(), cancellation.clone())?;
    let conversations = host.conversations(100).await.map_err(|error| match error {
        TuiError::Host(_) => TuiError::Unavailable(state_directory.clone()),
        error => error,
    })?;
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app::App::new(host, sender, state_directory, options, conversations).await;
    let mut terminal = ratatui::try_init()?;
    let _guard = TerminalGuard;
    execute!(stdout(), EnableBracketedPaste, EnableMouseCapture)?;
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    while !app.should_quit {
        if app.mouse_changed {
            app.mouse_changed = false;
            if app.mouse {
                execute!(stdout(), EnableMouseCapture)?;
            } else {
                execute!(stdout(), DisableMouseCapture)?;
            }
        }
        terminal.draw(|frame| render::draw(frame, &mut app))?;
        tokio::select! {
            () = cancellation.cancelled() => break,
            _ = tick.tick() => {}
            Some(event) = receiver.recv() => app.on_host(event).await,
            event = events.next() => match event {
                Some(Ok(event)) => app.on_terminal(event).await,
                Some(Err(error)) => return Err(error.into()),
                None => break,
            },
        }
    }
    Ok(())
}
