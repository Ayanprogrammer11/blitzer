use super::{App, UiMode};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    prelude::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

impl App {
    pub(super) fn render(&self, frame: &mut ratatui::Frame<'_>) {
        match self.mode {
            UiMode::Form => self.render_form(frame),
            UiMode::Downloading => self.render_downloading(frame),
            UiMode::Done => self.render_done(frame),
            UiMode::Failed => self.render_failed(frame),
        }
    }
}

pub(super) fn form_chunks(area: Rect) -> Vec<Rect> {
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

pub(super) fn point_in_rect(rect: Rect, x: u16, y: u16) -> bool {
    let right = rect.x.saturating_add(rect.width);
    let bottom = rect.y.saturating_add(rect.height);
    x >= rect.x && x < right && y >= rect.y && y < bottom
}

pub(super) struct TextField<'a> {
    pub(super) label: &'a str,
    pub(super) value: &'a str,
    pub(super) focused: bool,
    pub(super) cursor_char_pos: Option<usize>,
    pub(super) placeholder: &'a str,
    pub(super) editable: bool,
}

pub(super) fn render_text_field(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    field: TextField<'_>,
) -> Option<(u16, u16)> {
    let border_style = if field.focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };

    let show_value = if field.value.is_empty() {
        Span::styled(
            field.placeholder.to_string(),
            Style::default().fg(Color::DarkGray),
        )
    } else {
        Span::raw(field.value.to_string())
    };

    let paragraph = Paragraph::new(Line::from(show_value))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(field.label)
                .border_style(border_style),
        )
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);

    if field.focused && field.editable {
        let inner = area.inner(ratatui::layout::Margin {
            vertical: 1,
            horizontal: 1,
        });
        let max_width = inner.width.saturating_sub(1) as usize;
        let cursor_pos = field
            .cursor_char_pos
            .unwrap_or_else(|| field.value.chars().count());
        let caret_offset = cursor_pos.min(max_width) as u16;
        Some((inner.x.saturating_add(caret_offset), inner.y))
    } else {
        None
    }
}
