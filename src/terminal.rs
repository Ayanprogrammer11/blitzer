use crate::app;
use anyhow::{Context, Result};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::io::{self, Stdout};

pub async fn run() -> Result<()> {
    let mut terminal = setup_terminal()?;
    let app_result = app::run_app(&mut terminal).await;
    let cleanup_result = restore_terminal(&mut terminal);

    match (app_result, cleanup_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(app_err), Ok(())) => Err(app_err),
        (Ok(()), Err(clean_err)) => Err(clean_err),
        (Err(app_err), Err(clean_err)) => Err(anyhow::anyhow!(
            "application failed: {app_err:#}; cleanup failed: {clean_err:#}"
        )),
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .context("failed entering alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed initializing terminal")?;
    terminal.clear().context("failed clearing terminal")?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode().context("failed to disable raw mode")?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )
    .context("failed leaving alternate screen")?;
    terminal.show_cursor().context("failed to show cursor")?;
    Ok(())
}
