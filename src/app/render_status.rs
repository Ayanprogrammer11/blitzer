use super::App;
use indicatif::HumanBytes;
use ratatui::{
    layout::{Constraint, Layout},
    prelude::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, Paragraph, Wrap},
};

impl App {
    pub(super) fn render_downloading(&self, frame: &mut ratatui::Frame<'_>) {
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
                    "Strategy: {}  Retries: {}  Timeout: {}s  No-resume: {}",
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

    pub(super) fn render_done(&self, frame: &mut ratatui::Frame<'_>) {
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

    pub(super) fn render_failed(&self, frame: &mut ratatui::Frame<'_>) {
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
