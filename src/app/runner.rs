use super::{App, AppAction};
use crate::download::{CancelToken, DownloadEvent, run_download_with_cancel};
use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyEventKind};
use ratatui::{Terminal, backend::CrosstermBackend, layout::Rect};
use std::{io::Stdout, time::Duration};
use tokio::{sync::mpsc, task::JoinHandle};

struct ActiveDownload {
    handle: JoinHandle<()>,
    cancel: CancelToken,
}

pub async fn run_app(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<DownloadEvent>();
    let mut app = App::default();
    let mut active_download: Option<ActiveDownload> = None;

    loop {
        while let Ok(evt) = rx.try_recv() {
            let finished = app.on_download_event(evt);
            if finished {
                active_download = None;
            }
        }

        terminal.draw(|frame| app.render(frame))?;

        if app.should_quit {
            break;
        }

        if let Some(active) = active_download.as_ref()
            && active.handle.is_finished()
        {
            active_download = None;
        }

        if event::poll(Duration::from_millis(100)).context("failed polling terminal events")? {
            let term_size = terminal.size().context("failed reading terminal size")?;
            let term_area = Rect::new(0, 0, term_size.width, term_size.height);
            let event = event::read().context("failed reading terminal event")?;
            let action = match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key),
                Event::Mouse(mouse) => app.on_mouse(mouse, term_area),
                _ => AppAction::None,
            };

            match action {
                AppAction::None => {}
                AppAction::Start(cfg) => {
                    let sender = tx.clone();
                    let cancel = CancelToken::default();
                    let task_cancel = cancel.clone();
                    let handle = tokio::spawn(async move {
                        if let Err(err) =
                            run_download_with_cancel(cfg, sender.clone(), task_cancel).await
                        {
                            let _ = sender.send(DownloadEvent::Failed(format!("{err:#}")));
                        }
                    });
                    active_download = Some(ActiveDownload { handle, cancel });
                }
                AppAction::CancelDownload => {
                    if let Some(active) = active_download.as_ref() {
                        active.cancel.cancel();
                        app.mark_cancelling();
                    } else {
                        app.mark_cancelled(0);
                    }
                }
                AppAction::Quit => {
                    if let Some(active) = active_download.take() {
                        active.handle.abort();
                    }
                    app.should_quit = true;
                }
            }
        }

        app.tick();
    }

    if let Some(active) = active_download {
        active.handle.abort();
    }

    Ok(())
}
