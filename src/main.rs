use anyhow::{Context, Result, bail};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use indicatif::HumanBytes;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    prelude::{Alignment, Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, Paragraph, Wrap},
};
use reqwest::{
    Client, StatusCode, Url,
    header::{
        ACCEPT_ENCODING, ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, RANGE,
    },
};
use std::{
    cmp::min,
    io::{self, Stdout},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    fs::{self, OpenOptions},
    io::{self as tokio_io, AsyncWriteExt},
    sync::mpsc,
    task::JoinSet,
    time::sleep,
};

const MIN_CONNECTIONS: usize = 1;
const MAX_CONNECTIONS: usize = 64;
const MAX_RETRIES: usize = 20;
const MIN_TIMEOUT: u64 = 5;
const MAX_TIMEOUT: u64 = 300;

#[derive(Debug, Clone)]
struct DownloadConfig {
    url: Url,
    output: Option<PathBuf>,
    connections: usize,
    retries: usize,
    timeout_secs: u64,
    no_resume: bool,
}

#[derive(Debug, Clone, Copy)]
struct Chunk {
    index: usize,
    start: u64,
    end: u64,
}

#[derive(Debug, Default)]
struct RemoteInfo {
    size: Option<u64>,
    supports_ranges: bool,
    suggested_filename: Option<String>,
}

#[derive(Debug, Clone)]
struct DownloadSummary {
    output: PathBuf,
    final_size: u64,
    newly_transferred: u64,
    elapsed: Duration,
    avg_mib_per_sec: f64,
    used_parallel: bool,
}

#[derive(Debug, Clone)]
enum DownloadEvent {
    Phase(String),
    TargetResolved {
        output: PathBuf,
        total_size: Option<u64>,
        supports_ranges: bool,
    },
    ResumeOffset(u64),
    Advanced(u64),
    Completed(DownloadSummary),
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiMode {
    Form,
    Downloading,
    Done,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormField {
    Url,
    Output,
    Connections,
    Retries,
    Timeout,
    Resume,
}

const FORM_FIELDS: [FormField; 6] = [
    FormField::Url,
    FormField::Output,
    FormField::Connections,
    FormField::Retries,
    FormField::Timeout,
    FormField::Resume,
];

#[derive(Debug)]
struct FormState {
    url: String,
    output: String,
    connections: String,
    retries: String,
    timeout_secs: String,
    url_cursor: usize,
    output_cursor: usize,
    connections_cursor: usize,
    retries_cursor: usize,
    timeout_cursor: usize,
    no_resume: bool,
    focused: usize,
    message: String,
    error: bool,
}

impl Default for FormState {
    fn default() -> Self {
        Self {
            url: String::new(),
            output: String::new(),
            connections: default_connections().to_string(),
            retries: "4".to_string(),
            timeout_secs: "30".to_string(),
            url_cursor: 0,
            output_cursor: 0,
            connections_cursor: default_connections().to_string().chars().count(),
            retries_cursor: 1,
            timeout_cursor: 2,
            no_resume: false,
            focused: 0,
            message: "Enter details, then press Enter to start.".to_string(),
            error: false,
        }
    }
}

impl FormState {
    fn focused_field(&self) -> FormField {
        FORM_FIELDS[self.focused]
    }

    fn next_field(&mut self) {
        self.focused = (self.focused + 1) % FORM_FIELDS.len();
    }

    fn prev_field(&mut self) {
        self.focused = (self.focused + FORM_FIELDS.len() - 1) % FORM_FIELDS.len();
    }

    fn clear_message(&mut self) {
        self.message.clear();
        self.error = false;
    }

    fn set_error(&mut self, message: impl Into<String>) {
        self.message = message.into();
        self.error = true;
    }

    fn set_info(&mut self, message: impl Into<String>) {
        self.message = message.into();
        self.error = false;
    }

    fn backspace(&mut self) {
        if let Some((value, cursor, _digits_only)) = self.focused_input_mut() {
            if *cursor == 0 {
                return;
            }
            let end = char_to_byte_index(value, *cursor);
            let start = char_to_byte_index(value, *cursor - 1);
            value.replace_range(start..end, "");
            *cursor -= 1;
        }
    }

    fn type_char(&mut self, c: char) {
        if let Some((value, cursor, digits_only)) = self.focused_input_mut() {
            if digits_only && !c.is_ascii_digit() {
                return;
            }
            let idx = char_to_byte_index(value, *cursor);
            value.insert(idx, c);
            *cursor += 1;
        } else if matches!(self.focused_field(), FormField::Resume) && c == ' ' {
            self.no_resume = !self.no_resume;
        }
    }

    fn delete(&mut self) {
        if let Some((value, cursor, _digits_only)) = self.focused_input_mut() {
            let len = value.chars().count();
            if *cursor >= len {
                return;
            }
            let start = char_to_byte_index(value, *cursor);
            let end = char_to_byte_index(value, *cursor + 1);
            value.replace_range(start..end, "");
        }
    }

    fn move_cursor_left(&mut self) {
        if let Some((_value, cursor, _digits_only)) = self.focused_input_mut() {
            *cursor = cursor.saturating_sub(1);
        }
    }

    fn move_cursor_right(&mut self) {
        if let Some((value, cursor, _digits_only)) = self.focused_input_mut() {
            let len = value.chars().count();
            *cursor = (*cursor + 1).min(len);
        }
    }

    fn move_cursor_home(&mut self) {
        if let Some((_value, cursor, _digits_only)) = self.focused_input_mut() {
            *cursor = 0;
        }
    }

    fn move_cursor_end(&mut self) {
        if let Some((value, cursor, _digits_only)) = self.focused_input_mut() {
            *cursor = value.chars().count();
        }
    }

    fn focused_cursor(&self) -> Option<usize> {
        match self.focused_field() {
            FormField::Url => Some(self.url_cursor),
            FormField::Output => Some(self.output_cursor),
            FormField::Connections => Some(self.connections_cursor),
            FormField::Retries => Some(self.retries_cursor),
            FormField::Timeout => Some(self.timeout_cursor),
            FormField::Resume => None,
        }
    }

    fn set_focus_field(&mut self, field: FormField) {
        if let Some((idx, _)) = FORM_FIELDS.iter().enumerate().find(|(_, f)| **f == field) {
            self.focused = idx;
        }
    }

    fn field_len(&self, field: FormField) -> Option<usize> {
        match field {
            FormField::Url => Some(self.url.chars().count()),
            FormField::Output => Some(self.output.chars().count()),
            FormField::Connections => Some(self.connections.chars().count()),
            FormField::Retries => Some(self.retries.chars().count()),
            FormField::Timeout => Some(self.timeout_secs.chars().count()),
            FormField::Resume => None,
        }
    }

    fn set_cursor_for_field(&mut self, field: FormField, pos: usize) {
        match field {
            FormField::Url => self.url_cursor = pos.min(self.url.chars().count()),
            FormField::Output => self.output_cursor = pos.min(self.output.chars().count()),
            FormField::Connections => {
                self.connections_cursor = pos.min(self.connections.chars().count())
            }
            FormField::Retries => self.retries_cursor = pos.min(self.retries.chars().count()),
            FormField::Timeout => self.timeout_cursor = pos.min(self.timeout_secs.chars().count()),
            FormField::Resume => {}
        }
    }

    fn focused_input_mut(&mut self) -> Option<(&mut String, &mut usize, bool)> {
        match self.focused_field() {
            FormField::Url => Some((&mut self.url, &mut self.url_cursor, false)),
            FormField::Output => Some((&mut self.output, &mut self.output_cursor, false)),
            FormField::Connections => {
                Some((&mut self.connections, &mut self.connections_cursor, true))
            }
            FormField::Retries => Some((&mut self.retries, &mut self.retries_cursor, true)),
            FormField::Timeout => Some((&mut self.timeout_secs, &mut self.timeout_cursor, true)),
            FormField::Resume => None,
        }
    }

    fn build_config(&self) -> Result<DownloadConfig> {
        let url = Url::parse(self.url.trim()).context("URL is invalid")?;
        let connections = parse_usize(&self.connections, "Connections")?;
        let retries = parse_usize(&self.retries, "Retries")?;
        let timeout_secs = parse_u64(&self.timeout_secs, "Timeout")?;

        if !(MIN_CONNECTIONS..=MAX_CONNECTIONS).contains(&connections) {
            bail!(
                "Connections must be in range {}..={}",
                MIN_CONNECTIONS,
                MAX_CONNECTIONS
            );
        }
        if retries > MAX_RETRIES {
            bail!("Retries must be <= {}", MAX_RETRIES);
        }
        if !(MIN_TIMEOUT..=MAX_TIMEOUT).contains(&timeout_secs) {
            bail!(
                "Timeout must be in range {}..={} seconds",
                MIN_TIMEOUT,
                MAX_TIMEOUT
            );
        }

        let output = if self.output.trim().is_empty() {
            None
        } else {
            Some(PathBuf::from(self.output.trim()))
        };

        Ok(DownloadConfig {
            url,
            output,
            connections,
            retries,
            timeout_secs,
            no_resume: self.no_resume,
        })
    }
}

#[derive(Debug)]
struct ProgressState {
    url: String,
    output: String,
    total_size: Option<u64>,
    downloaded: u64,
    newly_transferred: u64,
    phase: String,
    mode: String,
    connections: usize,
    retries: usize,
    timeout_secs: u64,
    no_resume: bool,
    started: Instant,
}

impl ProgressState {
    fn new(cfg: &DownloadConfig) -> Self {
        Self {
            url: cfg.url.to_string(),
            output: cfg
                .output
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(auto)".to_string()),
            total_size: None,
            downloaded: 0,
            newly_transferred: 0,
            phase: "Probing remote server...".to_string(),
            mode: "Detecting server capabilities...".to_string(),
            connections: cfg.connections,
            retries: cfg.retries,
            timeout_secs: cfg.timeout_secs,
            no_resume: cfg.no_resume,
            started: Instant::now(),
        }
    }
}

enum AppAction {
    None,
    Start(DownloadConfig),
    CancelDownload,
    Quit,
}

struct App {
    mode: UiMode,
    form: FormState,
    progress: Option<ProgressState>,
    summary: Option<DownloadSummary>,
    failure: Option<String>,
    should_quit: bool,
    spinner_idx: usize,
}

impl Default for App {
    fn default() -> Self {
        Self {
            mode: UiMode::Form,
            form: FormState::default(),
            progress: None,
            summary: None,
            failure: None,
            should_quit: false,
            spinner_idx: 0,
        }
    }
}

impl App {
    fn tick(&mut self) {
        self.spinner_idx = (self.spinner_idx + 1) % 4;
    }

    fn on_key(&mut self, key: KeyEvent) -> AppAction {
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

    fn on_mouse(&mut self, mouse: MouseEvent, area: Rect) -> AppAction {
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
                self.form.no_resume = !self.form.no_resume;
                self.form.set_info(if self.form.no_resume {
                    "Resume disabled."
                } else {
                    "Resume enabled."
                });
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
                    self.form.no_resume = !self.form.no_resume;
                    self.form.set_info(if self.form.no_resume {
                        "Resume disabled."
                    } else {
                        "Resume enabled."
                    });
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

    fn on_download_event(&mut self, evt: DownloadEvent) -> bool {
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
                        format!("Parallel ({} connections)", progress.connections)
                    } else {
                        "Single stream (server has no byte ranges)".to_string()
                    };
                }
            }
            DownloadEvent::ResumeOffset(bytes) => {
                if let Some(progress) = self.progress.as_mut() {
                    progress.downloaded = progress.downloaded.saturating_add(bytes);
                }
            }
            DownloadEvent::Advanced(bytes) => {
                if let Some(progress) = self.progress.as_mut() {
                    progress.downloaded = progress.downloaded.saturating_add(bytes);
                    progress.newly_transferred = progress.newly_transferred.saturating_add(bytes);
                }
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

    fn mark_cancelled(&mut self) {
        self.mode = UiMode::Failed;
        self.failure = Some("Download cancelled.".to_string());
    }

    fn render(&self, frame: &mut ratatui::Frame<'_>) {
        match self.mode {
            UiMode::Form => self.render_form(frame),
            UiMode::Downloading => self.render_downloading(frame),
            UiMode::Done => self.render_done(frame),
            UiMode::Failed => self.render_failed(frame),
        }
    }

    fn render_form(&self, frame: &mut ratatui::Frame<'_>) {
        let area = frame.area();
        let block = Block::default().borders(Borders::ALL).title(" Blitzer ");
        frame.render_widget(block, area);
        let chunks = form_chunks(area);

        let title = Paragraph::new("High-throughput downloader configuration").style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
        frame.render_widget(title, chunks[0]);

        let mut cursor = None;
        cursor = render_text_field(
            frame,
            chunks[1],
            "URL",
            &self.form.url,
            self.form.focused_field() == FormField::Url,
            self.form.focused_cursor(),
            "https://example.com/file.iso",
            true,
        )
        .or(cursor);

        cursor = render_text_field(
            frame,
            chunks[2],
            "Output Path (optional)",
            &self.form.output,
            self.form.focused_field() == FormField::Output,
            self.form.focused_cursor(),
            "./downloads/file.iso",
            true,
        )
        .or(cursor);

        cursor = render_text_field(
            frame,
            chunks[3],
            "Connections",
            &self.form.connections,
            self.form.focused_field() == FormField::Connections,
            self.form.focused_cursor(),
            "8",
            true,
        )
        .or(cursor);

        cursor = render_text_field(
            frame,
            chunks[4],
            "Retries",
            &self.form.retries,
            self.form.focused_field() == FormField::Retries,
            self.form.focused_cursor(),
            "4",
            true,
        )
        .or(cursor);

        cursor = render_text_field(
            frame,
            chunks[5],
            "Timeout (seconds)",
            &self.form.timeout_secs,
            self.form.focused_field() == FormField::Timeout,
            self.form.focused_cursor(),
            "30",
            true,
        )
        .or(cursor);

        let resume_value = if self.form.no_resume {
            "[x] Start fresh (ignore existing part files)"
        } else {
            "[ ] Resume existing part files if present"
        };
        let _ = render_text_field(
            frame,
            chunks[6],
            "Resume Mode (press Space to toggle)",
            resume_value,
            self.form.focused_field() == FormField::Resume,
            None,
            "",
            false,
        );

        let help = Paragraph::new(
            "Enter: start  Tab/Shift+Tab: move fields  Left/Right/Home/End: move cursor  Click: focus/caret  Esc: quit",
        )
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Left);
        frame.render_widget(help, chunks[7]);

        let message_style = if self.form.error {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::Green)
        };
        let message = Paragraph::new(self.form.message.as_str())
            .style(message_style)
            .wrap(Wrap { trim: true });
        frame.render_widget(message, chunks[8]);

        if let Some((x, y)) = cursor {
            frame.set_cursor_position((x, y));
        }
    }

    fn render_downloading(&self, frame: &mut ratatui::Frame<'_>) {
        let area = frame.area();
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Blitzer Download ");
        frame.render_widget(block, area);
        let inner = area.inner(ratatui::layout::Margin {
            vertical: 1,
            horizontal: 1,
        });
        let chunks = Layout::vertical([
            Constraint::Length(8),
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(inner);

        let spinner = ["|", "/", "-", "\\"];
        let spinner_char = spinner[self.spinner_idx];

        if let Some(progress) = &self.progress {
            let elapsed = progress.started.elapsed().as_secs_f64().max(0.001);
            let current_rate = progress.newly_transferred as f64 / elapsed;

            let info = vec![
                Line::from(vec![
                    Span::styled("Status: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(format!("{spinner_char} {}", progress.phase)),
                ]),
                Line::from(vec![
                    Span::styled("URL: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(progress.url.as_str()),
                ]),
                Line::from(vec![
                    Span::styled("Output: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(progress.output.as_str()),
                ]),
                Line::from(vec![
                    Span::styled("Mode: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(progress.mode.as_str()),
                ]),
                Line::from(format!(
                    "Connections: {}  Retries: {}  Timeout: {}s  No-resume: {}",
                    progress.connections,
                    progress.retries,
                    progress.timeout_secs,
                    progress.no_resume
                )),
                Line::from(format!(
                    "Downloaded: {}  Rate: {:.2} MiB/s",
                    HumanBytes(progress.downloaded),
                    current_rate / (1024.0 * 1024.0)
                )),
            ];
            frame.render_widget(Paragraph::new(info).wrap(Wrap { trim: true }), chunks[0]);

            if let Some(total) = progress.total_size {
                let ratio = if total == 0 {
                    1.0
                } else {
                    (progress.downloaded as f64 / total as f64).clamp(0.0, 1.0)
                };
                let gauge = Gauge::default()
                    .block(Block::default().borders(Borders::ALL).title("Progress"))
                    .gauge_style(Style::default().fg(Color::Cyan).bg(Color::Black))
                    .ratio(ratio)
                    .label(format!(
                        "{} / {} ({:.1}%)",
                        HumanBytes(progress.downloaded),
                        HumanBytes(total),
                        ratio * 100.0
                    ));
                frame.render_widget(gauge, chunks[1]);
            } else {
                let unknown = Paragraph::new(format!(
                    "Progress: {} downloaded (server did not provide content length)",
                    HumanBytes(progress.downloaded)
                ))
                .block(Block::default().borders(Borders::ALL).title("Progress"));
                frame.render_widget(unknown, chunks[1]);
            }
        }

        let log_text = Paragraph::new("Download is active. Press 'c' to cancel or 'q' to quit.")
            .style(Style::default().fg(Color::Yellow))
            .wrap(Wrap { trim: true });
        frame.render_widget(log_text, chunks[2]);

        let footer =
            Paragraph::new("Ctrl+C works too. Quitting while active aborts the download task.")
                .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(footer, chunks[3]);
    }

    fn render_done(&self, frame: &mut ratatui::Frame<'_>) {
        let area = frame.area();
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Download Complete ");
        frame.render_widget(block, area);
        let inner = area.inner(ratatui::layout::Margin {
            vertical: 1,
            horizontal: 1,
        });
        let chunks = Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).split(inner);

        if let Some(summary) = &self.summary {
            let lines = vec![
                Line::from(Span::styled(
                    "Download finished successfully.",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(format!("Saved to: {}", summary.output.display())),
                Line::from(format!("Final size: {}", HumanBytes(summary.final_size))),
                Line::from(format!(
                    "Newly transferred this run: {}",
                    HumanBytes(summary.newly_transferred)
                )),
                Line::from(format!(
                    "Average speed: {:.2} MiB/s",
                    summary.avg_mib_per_sec
                )),
                Line::from(format!("Elapsed: {:.2}s", summary.elapsed.as_secs_f64())),
                Line::from(format!(
                    "Mode: {}",
                    if summary.used_parallel {
                        "Parallel range download"
                    } else {
                        "Single stream fallback"
                    }
                )),
            ];
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), chunks[0]);
        }

        let footer = Paragraph::new("Enter: new download  q/Esc: quit")
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(footer, chunks[1]);
    }

    fn render_failed(&self, frame: &mut ratatui::Frame<'_>) {
        let area = frame.area();
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Download Error ");
        frame.render_widget(block, area);
        let inner = area.inner(ratatui::layout::Margin {
            vertical: 1,
            horizontal: 1,
        });
        let chunks = Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).split(inner);

        let message = self.failure.as_deref().unwrap_or("Unknown error.");
        let details = vec![
            Line::from(Span::styled(
                "Download failed.",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(message.to_string()),
        ];
        frame.render_widget(Paragraph::new(details).wrap(Wrap { trim: true }), chunks[0]);

        let footer = Paragraph::new("Enter: back to form  q/Esc: quit")
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(footer, chunks[1]);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut terminal = setup_terminal()?;
    let app_result = run_app(&mut terminal).await;
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

async fn run_app(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<DownloadEvent>();
    let mut app = App::default();
    let mut active_download: Option<tokio::task::JoinHandle<()>> = None;

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

        if let Some(handle) = active_download.as_ref()
            && handle.is_finished()
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
                    active_download = Some(tokio::spawn(async move {
                        if let Err(err) = run_download(cfg, sender.clone()).await {
                            let _ = sender.send(DownloadEvent::Failed(format!("{err:#}")));
                        }
                    }));
                }
                AppAction::CancelDownload => {
                    if let Some(handle) = active_download.take() {
                        handle.abort();
                    }
                    app.mark_cancelled();
                }
                AppAction::Quit => {
                    if let Some(handle) = active_download.take() {
                        handle.abort();
                    }
                    app.should_quit = true;
                }
            }
        }

        app.tick();
    }

    if let Some(handle) = active_download {
        handle.abort();
    }

    Ok(())
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

async fn run_download(cfg: DownloadConfig, tx: mpsc::UnboundedSender<DownloadEvent>) -> Result<()> {
    let client = Client::builder()
        .user_agent("blitzer/0.2.0")
        .timeout(Duration::from_secs(cfg.timeout_secs))
        .build()
        .context("failed to build HTTP client")?;

    let _ = tx.send(DownloadEvent::Phase("Probing remote server...".to_string()));
    let remote = probe_remote(&client, &cfg.url).await?;
    let output = resolve_output_path(&cfg.url, cfg.output.clone(), remote.suggested_filename);
    ensure_parent_dir(&output).await?;

    let _ = tx.send(DownloadEvent::TargetResolved {
        output: output.clone(),
        total_size: remote.size,
        supports_ranges: remote.supports_ranges,
    });

    let start = Instant::now();
    let transferred_new = if let (Some(total), true) = (remote.size, remote.supports_ranges) {
        let _ = tx.send(DownloadEvent::Phase(format!(
            "Downloading with {} parallel connections...",
            cfg.connections
        )));
        download_parallel(
            &client,
            &cfg.url,
            &output,
            total,
            cfg.connections,
            cfg.retries,
            cfg.no_resume,
            tx.clone(),
        )
        .await?
    } else {
        let _ = tx.send(DownloadEvent::Phase(
            "Server does not support byte ranges; switching to single stream.".to_string(),
        ));
        download_single(&client, &cfg.url, &output, tx.clone()).await?
    };

    let elapsed = start.elapsed();
    let final_size = match remote.size {
        Some(size) => size,
        None => fs::metadata(&output)
            .await
            .with_context(|| format!("failed to read output metadata: {}", output.display()))?
            .len(),
    };
    let avg_mib_per_sec =
        (transferred_new as f64 / elapsed.as_secs_f64().max(0.001)) / (1024.0 * 1024.0);

    let _ = tx.send(DownloadEvent::Completed(DownloadSummary {
        output,
        final_size,
        newly_transferred: transferred_new,
        elapsed,
        avg_mib_per_sec,
        used_parallel: remote.supports_ranges && remote.size.is_some(),
    }));
    Ok(())
}

async fn probe_remote(client: &Client, url: &Url) -> Result<RemoteInfo> {
    let mut info = RemoteInfo::default();

    if let Ok(resp) = client.head(url.clone()).send().await
        && resp.status().is_success()
    {
        info.size = parse_content_length(resp.headers().get(CONTENT_LENGTH));
        info.supports_ranges = resp
            .headers()
            .get(ACCEPT_RANGES)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_ascii_lowercase().contains("bytes"))
            .unwrap_or(false);
        info.suggested_filename = parse_filename_from_disposition(
            resp.headers()
                .get(CONTENT_DISPOSITION)
                .and_then(|v| v.to_str().ok()),
        );
    }

    if info.size.is_none() || !info.supports_ranges {
        let resp = client
            .get(url.clone())
            .header(RANGE, "bytes=0-0")
            .header(ACCEPT_ENCODING, "identity")
            .send()
            .await
            .context("failed to probe range support")?;

        if resp.status() == StatusCode::PARTIAL_CONTENT {
            info.supports_ranges = true;
            if info.size.is_none() {
                info.size = parse_total_from_content_range(resp.headers().get(CONTENT_RANGE));
            }
        }

        if info.size.is_none() {
            info.size = parse_content_length(resp.headers().get(CONTENT_LENGTH));
        }

        if info.suggested_filename.is_none() {
            info.suggested_filename = parse_filename_from_disposition(
                resp.headers()
                    .get(CONTENT_DISPOSITION)
                    .and_then(|v| v.to_str().ok()),
            );
        }
    }

    Ok(info)
}

async fn download_single(
    client: &Client,
    url: &Url,
    output: &Path,
    tx: mpsc::UnboundedSender<DownloadEvent>,
) -> Result<u64> {
    let resp = client
        .get(url.clone())
        .header(ACCEPT_ENCODING, "identity")
        .send()
        .await
        .context("failed to start download")?;
    if !resp.status().is_success() {
        bail!("download request failed with status {}", resp.status());
    }

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(output)
        .await
        .with_context(|| format!("failed to create {}", output.display()))?;

    let mut transferred = 0u64;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("failed while reading response body")?;
        file.write_all(&chunk)
            .await
            .context("failed writing output file")?;
        let bytes = chunk.len() as u64;
        transferred = transferred.saturating_add(bytes);
        let _ = tx.send(DownloadEvent::Advanced(bytes));
    }
    file.flush().await.context("failed to flush output file")?;
    Ok(transferred)
}

async fn download_parallel(
    client: &Client,
    url: &Url,
    output: &Path,
    total_size: u64,
    connections: usize,
    retries: usize,
    no_resume: bool,
    tx: mpsc::UnboundedSender<DownloadEvent>,
) -> Result<u64> {
    if total_size == 0 {
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(output)
            .await
            .with_context(|| format!("failed to create {}", output.display()))?;
        return Ok(0);
    }

    let chunks = build_chunks(total_size, connections);
    let part_dir = part_dir_for(output)?;
    if no_resume && part_dir.exists() {
        fs::remove_dir_all(&part_dir)
            .await
            .with_context(|| format!("failed to remove {}", part_dir.display()))?;
    }
    fs::create_dir_all(&part_dir)
        .await
        .with_context(|| format!("failed to create {}", part_dir.display()))?;

    let mut already_downloaded = 0u64;
    if !no_resume {
        for chunk in &chunks {
            let part_path = part_path_for(&part_dir, chunk.index);
            if let Ok(meta) = fs::metadata(&part_path).await {
                let expected = chunk_len(*chunk);
                already_downloaded = already_downloaded.saturating_add(min(meta.len(), expected));
            }
        }
    }
    if already_downloaded > 0 {
        let _ = tx.send(DownloadEvent::ResumeOffset(already_downloaded));
    }

    let transferred = Arc::new(AtomicU64::new(0));
    let mut workers = JoinSet::new();
    for chunk in chunks.iter().copied() {
        let client = client.clone();
        let url = url.clone();
        let part_path = part_path_for(&part_dir, chunk.index);
        let transferred = transferred.clone();
        let tx = tx.clone();
        workers.spawn(async move {
            download_chunk(
                client,
                url,
                chunk,
                part_path,
                retries,
                no_resume,
                transferred,
                tx,
            )
            .await
        });
    }

    while let Some(joined) = workers.join_next().await {
        joined.context("download worker panicked")??;
    }

    merge_parts(&part_dir, output, &chunks, total_size).await?;
    fs::remove_dir_all(&part_dir)
        .await
        .with_context(|| format!("failed to cleanup {}", part_dir.display()))?;

    Ok(transferred.load(Ordering::Relaxed))
}

async fn download_chunk(
    client: Client,
    url: Url,
    chunk: Chunk,
    part_path: PathBuf,
    retries: usize,
    no_resume: bool,
    transferred: Arc<AtomicU64>,
    tx: mpsc::UnboundedSender<DownloadEvent>,
) -> Result<()> {
    let expected_len = chunk_len(chunk);
    if no_resume && part_path.exists() {
        fs::remove_file(&part_path)
            .await
            .with_context(|| format!("failed removing {}", part_path.display()))?;
    }

    let mut local_len = match fs::metadata(&part_path).await {
        Ok(meta) => meta.len(),
        Err(_) => 0,
    };

    if local_len > expected_len {
        local_len = 0;
        fs::remove_file(&part_path)
            .await
            .with_context(|| format!("failed resetting {}", part_path.display()))?;
    }
    if local_len == expected_len {
        return Ok(());
    }

    for attempt in 0..=retries {
        let range_start = chunk.start + local_len;
        let range_header = format!("bytes={}-{}", range_start, chunk.end);
        let resp = client
            .get(url.clone())
            .header(RANGE, range_header)
            .header(ACCEPT_ENCODING, "identity")
            .send()
            .await;

        match resp {
            Ok(resp) => {
                if resp.status() != StatusCode::PARTIAL_CONTENT {
                    bail!(
                        "server does not honor range requests (status {})",
                        resp.status()
                    );
                }

                let mut file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&part_path)
                    .await
                    .with_context(|| format!("failed to open {}", part_path.display()))?;

                let mut stream = resp.bytes_stream();
                let mut stream_error = None;
                while let Some(next) = stream.next().await {
                    match next {
                        Ok(buf) => {
                            file.write_all(&buf).await.with_context(|| {
                                format!("failed writing {}", part_path.display())
                            })?;
                            let bytes = buf.len() as u64;
                            local_len = local_len.saturating_add(bytes);
                            transferred.fetch_add(bytes, Ordering::Relaxed);
                            let _ = tx.send(DownloadEvent::Advanced(bytes));
                            if local_len > expected_len {
                                bail!("chunk {} exceeded expected size", chunk.index);
                            }
                        }
                        Err(e) => {
                            stream_error = Some(e);
                            break;
                        }
                    }
                }

                if let Some(e) = stream_error {
                    if attempt >= retries {
                        return Err(e).context("stream failed after retries");
                    }
                    sleep(backoff(attempt)).await;
                    continue;
                }

                file.flush()
                    .await
                    .with_context(|| format!("failed flushing {}", part_path.display()))?;
                if local_len == expected_len {
                    return Ok(());
                }
                if attempt >= retries {
                    bail!(
                        "chunk {} incomplete after retries (have {}, expected {})",
                        chunk.index,
                        local_len,
                        expected_len
                    );
                }
            }
            Err(e) => {
                if attempt >= retries {
                    return Err(e).context("request failed after retries");
                }
            }
        }
        sleep(backoff(attempt)).await;
    }

    bail!("chunk {} failed unexpectedly", chunk.index)
}

async fn merge_parts(
    part_dir: &Path,
    output: &Path,
    chunks: &[Chunk],
    total_size: u64,
) -> Result<()> {
    let tmp_name = format!(
        ".{}.blitzer.tmp",
        output
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("download")
    );
    let tmp_output = output
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(tmp_name);

    if tmp_output.exists() {
        fs::remove_file(&tmp_output)
            .await
            .with_context(|| format!("failed removing stale {}", tmp_output.display()))?;
    }

    let mut out = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp_output)
        .await
        .with_context(|| format!("failed to create {}", tmp_output.display()))?;

    for chunk in chunks {
        let path = part_path_for(part_dir, chunk.index);
        let mut in_file = OpenOptions::new()
            .read(true)
            .open(&path)
            .await
            .with_context(|| format!("missing part {}", path.display()))?;
        tokio_io::copy(&mut in_file, &mut out)
            .await
            .with_context(|| format!("failed merging {}", path.display()))?;
    }
    out.flush().await.context("failed flushing merged file")?;

    let final_meta = fs::metadata(&tmp_output)
        .await
        .with_context(|| format!("failed stat {}", tmp_output.display()))?;
    if final_meta.len() != total_size {
        bail!(
            "merged file size mismatch: got {}, expected {}",
            final_meta.len(),
            total_size
        );
    }

    if output.exists() {
        fs::remove_file(output)
            .await
            .with_context(|| format!("failed replacing {}", output.display()))?;
    }
    fs::rename(&tmp_output, output)
        .await
        .with_context(|| format!("failed moving merged output to {}", output.display()))?;
    Ok(())
}

fn form_chunks(area: Rect) -> Vec<Rect> {
    let inner = area.inner(ratatui::layout::Margin {
        vertical: 1,
        horizontal: 1,
    });
    Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(2),
        Constraint::Min(2),
    ])
    .split(inner)
    .to_vec()
}

fn point_in_rect(rect: Rect, x: u16, y: u16) -> bool {
    let right = rect.x.saturating_add(rect.width);
    let bottom = rect.y.saturating_add(rect.height);
    x >= rect.x && x < right && y >= rect.y && y < bottom
}

fn render_text_field(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    label: &str,
    value: &str,
    focused: bool,
    cursor_char_pos: Option<usize>,
    placeholder: &str,
    editable: bool,
) -> Option<(u16, u16)> {
    let border_style = if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };

    let show_value = if value.is_empty() {
        Span::styled(
            placeholder.to_string(),
            Style::default().fg(Color::DarkGray),
        )
    } else {
        Span::raw(value.to_string())
    };

    let paragraph = Paragraph::new(Line::from(show_value))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(label)
                .border_style(border_style),
        )
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);

    if focused && editable {
        let inner = area.inner(ratatui::layout::Margin {
            vertical: 1,
            horizontal: 1,
        });
        let max_width = inner.width.saturating_sub(1) as usize;
        let cursor_pos = cursor_char_pos.unwrap_or_else(|| value.chars().count());
        let caret_offset = cursor_pos.min(max_width) as u16;
        Some((inner.x.saturating_add(caret_offset), inner.y))
    } else {
        None
    }
}

fn char_to_byte_index(s: &str, char_index: usize) -> usize {
    s.char_indices()
        .nth(char_index)
        .map(|(idx, _)| idx)
        .unwrap_or(s.len())
}

fn resolve_output_path(
    url: &Url,
    manual_output: Option<PathBuf>,
    suggested: Option<String>,
) -> PathBuf {
    if let Some(output) = manual_output {
        return output;
    }
    if let Some(name) = suggested.filter(|n| !n.trim().is_empty()) {
        return PathBuf::from(name);
    }

    let inferred = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|segment| !segment.is_empty())
        .unwrap_or("download.bin");
    PathBuf::from(inferred)
}

async fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    Ok(())
}

fn part_dir_for(output: &Path) -> Result<PathBuf> {
    let file = output
        .file_name()
        .and_then(|v| v.to_str())
        .context("invalid output filename")?;
    Ok(output
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".{}.parts", file)))
}

fn part_path_for(dir: &Path, index: usize) -> PathBuf {
    dir.join(format!("part-{index:04}.bin"))
}

fn build_chunks(total_size: u64, requested_connections: usize) -> Vec<Chunk> {
    let chunk_count = min(requested_connections.max(1), total_size as usize);
    let base = total_size / chunk_count as u64;
    let remainder = total_size % chunk_count as u64;

    let mut chunks = Vec::with_capacity(chunk_count);
    let mut start = 0u64;
    for idx in 0..chunk_count {
        let extra = if idx < remainder as usize { 1 } else { 0 };
        let len = base + extra;
        let end = start + len - 1;
        chunks.push(Chunk {
            index: idx,
            start,
            end,
        });
        start = end + 1;
    }
    chunks
}

fn chunk_len(chunk: Chunk) -> u64 {
    chunk.end - chunk.start + 1
}

fn parse_content_length(value: Option<&reqwest::header::HeaderValue>) -> Option<u64> {
    value
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
}

fn parse_total_from_content_range(value: Option<&reqwest::header::HeaderValue>) -> Option<u64> {
    let raw = value.and_then(|v| v.to_str().ok())?;
    let (_, total) = raw.split_once('/')?;
    total.trim().parse::<u64>().ok()
}

fn parse_filename_from_disposition(content_disposition: Option<&str>) -> Option<String> {
    let raw = content_disposition?;
    for segment in raw.split(';') {
        let segment = segment.trim();
        if let Some(name) = segment.strip_prefix("filename=") {
            return Some(name.trim_matches('"').to_string());
        }
    }
    None
}

fn backoff(attempt: usize) -> Duration {
    let capped = min(attempt as u32, 6);
    Duration::from_millis(250 * (1u64 << capped))
}

fn parse_usize(raw: &str, label: &str) -> Result<usize> {
    raw.trim()
        .parse::<usize>()
        .with_context(|| format!("{label} must be a valid integer"))
}

fn parse_u64(raw: &str, label: &str) -> Result<u64> {
    raw.trim()
        .parse::<u64>()
        .with_context(|| format!("{label} must be a valid integer"))
}

fn default_connections() -> usize {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    (cpus * 2).clamp(4, 16)
}
