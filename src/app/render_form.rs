use super::{
    App,
    form::{FormField, FormState},
    render::{TextField, form_chunks, render_text_field},
};
use ratatui::{
    layout::Rect,
    prelude::{Alignment, Color, Modifier, Style},
    widgets::{Block, Borders, Paragraph, Wrap},
};

impl App {
    pub(super) fn render_form(&self, frame: &mut ratatui::Frame<'_>) {
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
            TextField {
                label: "URL",
                value: &self.form.url,
                focused: self.form.focused_field() == FormField::Url,
                invalid: self.form.invalid_field == Some(FormField::Url),
                cursor_char_pos: self.form.focused_cursor(),
                placeholder: "https://example.com/file.iso",
                editable: true,
            },
        )
        .or(cursor);

        cursor = render_text_field(
            frame,
            chunks[2],
            TextField {
                label: "Output",
                value: &self.form.output,
                focused: self.form.focused_field() == FormField::Output,
                invalid: self.form.invalid_field == Some(FormField::Output),
                cursor_char_pos: self.form.focused_cursor(),
                placeholder: "./downloads/file.iso",
                editable: true,
            },
        )
        .or(cursor);

        cursor = render_text_field(
            frame,
            chunks[3],
            TextField {
                label: "Connections (auto or 1-64)",
                value: &self.form.connections,
                focused: self.form.focused_field() == FormField::Connections,
                invalid: self.form.invalid_field == Some(FormField::Connections),
                cursor_char_pos: self.form.focused_cursor(),
                placeholder: "auto",
                editable: true,
            },
        )
        .or(cursor);

        cursor = render_text_field(
            frame,
            chunks[4],
            TextField {
                label: "Retries (0-20)",
                value: &self.form.retries,
                focused: self.form.focused_field() == FormField::Retries,
                invalid: self.form.invalid_field == Some(FormField::Retries),
                cursor_char_pos: self.form.focused_cursor(),
                placeholder: "4",
                editable: true,
            },
        )
        .or(cursor);

        cursor = render_text_field(
            frame,
            chunks[5],
            TextField {
                label: "Timeout (5-300s)",
                value: &self.form.timeout_secs,
                focused: self.form.focused_field() == FormField::Timeout,
                invalid: self.form.invalid_field == Some(FormField::Timeout),
                cursor_char_pos: self.form.focused_cursor(),
                placeholder: "30",
                editable: true,
            },
        )
        .or(cursor);

        render_resume_field(frame, chunks[6], &self.form);
        render_form_help(frame, chunks[7]);
        render_form_message(frame, chunks[8], &self.form);

        if let Some((x, y)) = cursor {
            frame.set_cursor_position((x, y));
        }
    }
}

fn render_resume_field(frame: &mut ratatui::Frame<'_>, area: Rect, form: &FormState) {
    let resume_value = if form.no_resume {
        "[x] Start fresh (ignore existing part files)"
    } else {
        "[ ] Resume existing part files if present"
    };
    let _ = render_text_field(
        frame,
        area,
        TextField {
            label: "Resume",
            value: resume_value,
            focused: form.focused_field() == FormField::Resume,
            invalid: false,
            cursor_char_pos: None,
            placeholder: "",
            editable: false,
        },
    );
}

fn render_form_help(frame: &mut ratatui::Frame<'_>, area: Rect) {
    let help = Paragraph::new(vec![
        ratatui::text::Line::from("Enter start | Tab move | Arrows/Home/End edit | Ctrl+U clear"),
        ratatui::text::Line::from("Esc quit | Click focuses fields | Space toggles resume"),
    ])
    .style(Style::default().fg(Color::DarkGray))
    .alignment(Alignment::Left);
    frame.render_widget(help, area);
}

fn render_form_message(frame: &mut ratatui::Frame<'_>, area: Rect, form: &FormState) {
    let message_style = if form.error {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::Green)
    };
    let message = Paragraph::new(form.message.as_str())
        .style(message_style)
        .wrap(Wrap { trim: true });
    frame.render_widget(message, area);
}
