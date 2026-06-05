use super::{
    App, AppAction, ProgressState, UiMode,
    form::FormField,
    render::{form_chunks, point_in_rect},
};
use crate::download::DownloadEvent;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use indicatif::HumanBytes;
use ratatui::layout::Rect;

impl App {
    pub(super) fn tick(&mut self) {
        self.spinner_idx = (self.spinner_idx + 1) % 4;
    }

    pub(super) fn on_key(&mut self, key: KeyEvent) -> AppAction {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return AppAction::Quit;
        }

        match self.mode {
            UiMode::Form => self.on_key_form(key),
            UiMode::Downloading => self.on_key_downloading(key),
            UiMode::Done => self.on_key_done(key),
            UiMode::Failed => self.on_key_failed(key),
        }
    }

    pub(super) fn on_mouse(&mut self, mouse: MouseEvent, area: Rect) -> AppAction {
        match self.mode {
            UiMode::Form => self.on_mouse_form(mouse, area),
            UiMode::Downloading | UiMode::Done | UiMode::Failed => AppAction::None,
        }
    }

    fn on_mouse_form(&mut self, mouse: MouseEvent, area: Rect) -> AppAction {
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return AppAction::None;
        }
        if !point_in_rect(area, mouse.column, mouse.row) {
            return AppAction::None;
        }

        let chunks = form_chunks(area);
        if chunks.len() < 7 {
            return AppAction::None;
        }

        let fields = [
            (FormField::Url, 1usize),
            (FormField::Output, 2usize),
            (FormField::Connections, 3usize),
            (FormField::Retries, 4usize),
            (FormField::Timeout, 5usize),
            (FormField::Resume, 6usize),
        ];

        for (field, chunk_idx) in fields {
            let chunk = chunks[chunk_idx];
            if !point_in_rect(chunk, mouse.column, mouse.row) {
                continue;
            }

            self.form.set_focus_field(field);
            if field == FormField::Resume {
                self.toggle_resume();
                return AppAction::None;
            }

            let input_inner = chunk.inner(ratatui::layout::Margin {
                vertical: 1,
                horizontal: 1,
            });
            let click_offset = mouse.column.saturating_sub(input_inner.x) as usize;
            let text_len = self.form.field_len(field).unwrap_or(0);
            let cursor = click_offset.min(text_len);
            self.form.set_cursor_for_field(field, cursor);
            self.form.clear_message();
            return AppAction::None;
        }

        AppAction::None
    }

    fn on_key_form(&mut self, key: KeyEvent) -> AppAction {
        match key.code {
            KeyCode::Esc => {
                self.should_quit = true;
                AppAction::Quit
            }
            KeyCode::Tab | KeyCode::Down => {
                self.form.next_field();
                self.form.clear_message();
                AppAction::None
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.form.prev_field();
                self.form.clear_message();
                AppAction::None
            }
            KeyCode::Backspace => {
                self.form.backspace();
                self.form.clear_message();
                AppAction::None
            }
            KeyCode::Delete => {
                self.form.delete();
                self.form.clear_message();
                AppAction::None
            }
            KeyCode::Left => {
                self.form.move_cursor_left();
                self.form.clear_message();
                AppAction::None
            }
            KeyCode::Right => {
                self.form.move_cursor_right();
                self.form.clear_message();
                AppAction::None
            }
            KeyCode::Home => {
                self.form.move_cursor_home();
                self.form.clear_message();
                AppAction::None
            }
            KeyCode::End => {
                self.form.move_cursor_end();
                self.form.clear_message();
                AppAction::None
            }
            KeyCode::Char(' ') => {
                if matches!(self.form.focused_field(), FormField::Resume) {
                    self.toggle_resume();
                } else {
                    self.form.type_char(' ');
                    self.form.clear_message();
                }
                AppAction::None
            }
            KeyCode::Enter => match self.form.build_config() {
                Ok(cfg) => {
                    self.mode = UiMode::Downloading;
                    self.progress = Some(ProgressState::new(&cfg));
                    self.summary = None;
                    self.failure = None;
                    self.form.clear_message();
                    AppAction::Start(cfg)
                }
                Err(e) => {
                    self.form.set_error(format!("{e:#}"));
                    AppAction::None
                }
            },
            KeyCode::Char(c) => {
                self.form.type_char(c);
                self.form.clear_message();
                AppAction::None
            }
            _ => AppAction::None,
        }
    }

    fn on_key_downloading(&mut self, key: KeyEvent) -> AppAction {
        match key.code {
            KeyCode::Char('c') | KeyCode::Esc => AppAction::CancelDownload,
            KeyCode::Char('q') => {
                self.should_quit = true;
                AppAction::Quit
            }
            _ => AppAction::None,
        }
    }

    fn on_key_done(&mut self, key: KeyEvent) -> AppAction {
        match key.code {
            KeyCode::Enter => {
                self.mode = UiMode::Form;
                self.progress = None;
                self.summary = None;
                self.failure = None;
                self.form
                    .set_info("Ready for another download. Update fields and press Enter.");
                AppAction::None
            }
            KeyCode::Char('q') | KeyCode::Esc => {
                self.should_quit = true;
                AppAction::Quit
            }
            _ => AppAction::None,
        }
    }

    fn on_key_failed(&mut self, key: KeyEvent) -> AppAction {
        match key.code {
            KeyCode::Enter => {
                self.mode = UiMode::Form;
                self.progress = None;
                self.summary = None;
                self.failure = None;
                self.form
                    .set_info("Correct settings and press Enter to try again.");
                AppAction::None
            }
            KeyCode::Char('q') | KeyCode::Esc => {
                self.should_quit = true;
                AppAction::Quit
            }
            _ => AppAction::None,
        }
    }

    pub(super) fn on_download_event(&mut self, evt: DownloadEvent) -> bool {
        match evt {
            DownloadEvent::Phase(msg) => {
                if let Some(progress) = self.progress.as_mut() {
                    progress.phase = msg;
                }
            }
            DownloadEvent::TargetResolved {
                output,
                total_size,
                supports_ranges,
            } => {
                if let Some(progress) = self.progress.as_mut() {
                    progress.output = output.display().to_string();
                    progress.total_size = total_size;
                    progress.mode = if supports_ranges {
                        format!("Parallel ({})", progress.connections)
                    } else {
                        "Single stream (server has no byte ranges)".to_string()
                    };
                }
            }
            DownloadEvent::ResumeOffset(bytes) => {
                if let Some(progress) = self.progress.as_mut() {
                    progress.downloaded = bytes;
                }
            }
            DownloadEvent::PlanSelected {
                strategy,
                workers,
                segments,
                segment_size,
            } => {
                if let Some(progress) = self.progress.as_mut() {
                    progress.connections = strategy.clone();
                    progress.mode = format!(
                        "{} tuned ({} workers, {} segments, ~{:.1} MiB/segment)",
                        strategy,
                        workers,
                        segments,
                        (segment_size as f64 / (1024.0 * 1024.0)).max(0.1)
                    );
                }
            }
            DownloadEvent::Advanced(bytes) => {
                if let Some(progress) = self.progress.as_mut() {
                    progress.downloaded = progress.downloaded.saturating_add(bytes);
                    progress.newly_transferred = progress.newly_transferred.saturating_add(bytes);
                }
            }
            DownloadEvent::Cancelled { saved_bytes } => {
                if let Some(progress) = self.progress.as_mut() {
                    progress.downloaded = saved_bytes;
                    progress.phase = format!(
                        "Cancelled after saving {} verified bytes.",
                        HumanBytes(saved_bytes)
                    );
                }
                self.mark_cancelled(saved_bytes);
                return true;
            }
            DownloadEvent::Completed(summary) => {
                self.mode = UiMode::Done;
                self.summary = Some(summary);
                return true;
            }
            DownloadEvent::Failed(err) => {
                self.mode = UiMode::Failed;
                self.failure = Some(err);
                return true;
            }
        }
        false
    }

    pub(super) fn mark_cancelling(&mut self) {
        if let Some(progress) = self.progress.as_mut() {
            progress.phase = "Cancelling and flushing verified part files...".to_string();
        }
    }

    pub(super) fn mark_cancelled(&mut self, saved_bytes: u64) {
        self.mode = UiMode::Failed;
        self.failure = Some(format!(
            "Download cancelled. Saved {} verified bytes for resume.",
            HumanBytes(saved_bytes)
        ));
    }

    fn toggle_resume(&mut self) {
        self.form.no_resume = !self.form.no_resume;
        self.form.set_info(if self.form.no_resume {
            "Resume disabled."
        } else {
            "Resume enabled."
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConnectionStrategy, DownloadConfig, NoRangeStrategy};
    use url::Url;

    #[test]
    fn resume_offset_is_an_absolute_snapshot() {
        let cfg = DownloadConfig {
            url: Url::parse("http://example.test/file.bin").unwrap(),
            output: None,
            connections: ConnectionStrategy::Fixed(4),
            retries: 1,
            timeout_secs: 30,
            no_resume: false,
            no_range_strategy: NoRangeStrategy::Single,
        };
        let mut app = App {
            mode: UiMode::Downloading,
            progress: Some(ProgressState::new(&cfg)),
            ..App::default()
        };

        app.on_download_event(DownloadEvent::ResumeOffset(44));
        app.on_download_event(DownloadEvent::Advanced(1));
        app.on_download_event(DownloadEvent::ResumeOffset(45));

        let progress = app.progress.as_ref().unwrap();
        assert_eq!(progress.downloaded, 45);
        assert_eq!(progress.newly_transferred, 1);
    }
}
