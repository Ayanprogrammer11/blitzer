use super::{App, UiMode};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    prelude::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

impl App {
    pub(super) fn render(&mut self, frame: &mut ratatui::Frame<'_>) {
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
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(2),
        Constraint::Min(1),
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
    pub(super) invalid: bool,
    pub(super) cursor_char_pos: Option<usize>,
    pub(super) placeholder: &'a str,
    pub(super) editable: bool,
}

pub(super) fn render_text_field(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    field: TextField<'_>,
) -> Option<(u16, u16)> {
    let border_style = if field.invalid {
        Style::default().fg(Color::Red)
    } else if field.focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };

    let inner = area.inner(ratatui::layout::Margin {
        vertical: 1,
        horizontal: 1,
    });
    let visible_width = inner.width as usize;
    let cursor_pos = field
        .cursor_char_pos
        .unwrap_or_else(|| field.value.chars().count());
    let (visible_value, caret_offset) = visible_text(field.value, cursor_pos, visible_width);

    let show_value = if field.value.is_empty() {
        Span::styled(
            trim_to_width(field.placeholder, visible_width),
            Style::default().fg(Color::DarkGray),
        )
    } else {
        Span::raw(visible_value)
    };

    let paragraph = Paragraph::new(Line::from(show_value)).block(
        Block::default()
            .borders(Borders::ALL)
            .title(field.label)
            .border_style(border_style),
    );
    frame.render_widget(paragraph, area);

    if field.focused && field.editable {
        let max_offset = inner.width.saturating_sub(1) as usize;
        Some((
            inner.x.saturating_add(caret_offset.min(max_offset) as u16),
            inner.y,
        ))
    } else {
        None
    }
}

fn visible_text(value: &str, cursor_pos: usize, width: usize) -> (String, usize) {
    if width == 0 {
        return (String::new(), 0);
    }

    let len = value.chars().count();
    if len <= width {
        return (value.to_string(), cursor_pos.min(len));
    }

    let cursor_pos = cursor_pos.min(len);
    let mut start = cursor_pos.saturating_sub(width.saturating_sub(1));
    if start + width > len {
        start = len - width;
    }
    let visible = value.chars().skip(start).take(width).collect::<String>();
    (visible, cursor_pos.saturating_sub(start).min(width))
}

fn trim_to_width(value: &str, width: usize) -> String {
    value.chars().take(width).collect()
}
