mod app;
mod host;
mod input;
mod markdown;
mod onboarding;
mod render;
mod theme;

use std::io::{IsTerminal, stdout};
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
    mut options: Options,
    cancellation: CancellationToken,
) -> Result<(), TuiError> {
    if !std::io::stdin().is_terminal() || !stdout().is_terminal() {
        return Err(TuiError::NoTerminal);
    }
    let cancellation = cancellation.child_token();
    #[cfg(unix)]
    {
        let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
        let cancellation = cancellation.clone();
        tokio::spawn(async move {
            hangup.recv().await;
            cancellation.cancel();
        });
    }
    let host = host::Host::new(state_directory.clone(), cancellation.clone())?;
    let mut terminal = ratatui::try_init()?;
    let _guard = TerminalGuard;
    execute!(stdout(), EnableBracketedPaste)?;
    let mut events = EventStream::new();
    let mut screen = onboarding::Screen::new(&mut terminal, &mut events, cancellation.clone());
    let session = match onboarding::ensure_host(&mut screen, &host, &state_directory).await {
        Err(TuiError::Interrupted) => return Ok(()),
        result => result?,
    };
    let conversations = host.conversations(100).await.map_err(|error| match error {
        TuiError::Host(_) => TuiError::Unavailable(state_directory.clone()),
        error => error,
    })?;
    if let Some(found) = onboarding::scan(&host, conversations.is_empty()).await? {
        let route = match onboarding::run(
            &mut screen,
            &host,
            &state_directory,
            found,
            options.default_instructions,
            session.is_some(),
        )
        .await
        {
            Err(TuiError::Interrupted) => return Ok(()),
            result => result?,
        };
        if let Some(route) = route
            && options.provider.is_none()
            && options.model.is_none()
        {
            options.provider = Some(route.provider);
            options.model = Some(route.model);
            options.endpoint = Some(route.endpoint);
        }
    }
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app::App::new(host, sender, state_directory, options, conversations).await?;
    execute!(stdout(), EnableMouseCapture)?;
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
    drop(session);
    Ok(())
}
