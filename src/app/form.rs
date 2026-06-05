use crate::config::{
    DownloadConfig, MAX_RETRIES, MAX_TIMEOUT, MIN_TIMEOUT, default_connection_input,
    default_no_range_workers, default_overlap_bytes, parse_connection_strategy,
    parse_no_range_strategy,
};
use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FormField {
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
pub(super) struct FormState {
    pub(super) url: String,
    pub(super) output: String,
    pub(super) connections: String,
    pub(super) retries: String,
    pub(super) timeout_secs: String,
    pub(super) no_resume: bool,
    pub(super) message: String,
    pub(super) error: bool,
    url_cursor: usize,
    output_cursor: usize,
    connections_cursor: usize,
    retries_cursor: usize,
    timeout_cursor: usize,
    focused: usize,
}

impl Default for FormState {
    fn default() -> Self {
        let connections = default_connection_input();
        Self {
            url: String::new(),
            output: String::new(),
            connections_cursor: connections.chars().count(),
            connections,
            retries: "4".to_string(),
            timeout_secs: "30".to_string(),
            url_cursor: 0,
            output_cursor: 0,
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
    pub(super) fn focused_field(&self) -> FormField {
        FORM_FIELDS[self.focused]
    }

    pub(super) fn next_field(&mut self) {
        self.focused = (self.focused + 1) % FORM_FIELDS.len();
    }

    pub(super) fn prev_field(&mut self) {
        self.focused = (self.focused + FORM_FIELDS.len() - 1) % FORM_FIELDS.len();
    }

    pub(super) fn clear_message(&mut self) {
        self.message.clear();
        self.error = false;
    }

    pub(super) fn set_error(&mut self, message: impl Into<String>) {
        self.message = message.into();
        self.error = true;
    }

    pub(super) fn set_info(&mut self, message: impl Into<String>) {
        self.message = message.into();
        self.error = false;
    }

    pub(super) fn backspace(&mut self) {
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

    pub(super) fn type_char(&mut self, c: char) {
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

    pub(super) fn delete(&mut self) {
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

    pub(super) fn move_cursor_left(&mut self) {
        if let Some((_value, cursor, _digits_only)) = self.focused_input_mut() {
            *cursor = cursor.saturating_sub(1);
        }
    }

    pub(super) fn move_cursor_right(&mut self) {
        if let Some((value, cursor, _digits_only)) = self.focused_input_mut() {
            let len = value.chars().count();
            *cursor = (*cursor + 1).min(len);
        }
    }

    pub(super) fn move_cursor_home(&mut self) {
        if let Some((_value, cursor, _digits_only)) = self.focused_input_mut() {
            *cursor = 0;
        }
    }

    pub(super) fn move_cursor_end(&mut self) {
        if let Some((value, cursor, _digits_only)) = self.focused_input_mut() {
            *cursor = value.chars().count();
        }
    }

    pub(super) fn focused_cursor(&self) -> Option<usize> {
        match self.focused_field() {
            FormField::Url => Some(self.url_cursor),
            FormField::Output => Some(self.output_cursor),
            FormField::Connections => Some(self.connections_cursor),
            FormField::Retries => Some(self.retries_cursor),
            FormField::Timeout => Some(self.timeout_cursor),
            FormField::Resume => None,
        }
    }

    pub(super) fn set_focus_field(&mut self, field: FormField) {
        if let Some((idx, _)) = FORM_FIELDS.iter().enumerate().find(|(_, f)| **f == field) {
            self.focused = idx;
        }
    }

    pub(super) fn field_len(&self, field: FormField) -> Option<usize> {
        match field {
            FormField::Url => Some(self.url.chars().count()),
            FormField::Output => Some(self.output.chars().count()),
            FormField::Connections => Some(self.connections.chars().count()),
            FormField::Retries => Some(self.retries.chars().count()),
            FormField::Timeout => Some(self.timeout_secs.chars().count()),
            FormField::Resume => None,
        }
    }

    pub(super) fn set_cursor_for_field(&mut self, field: FormField, pos: usize) {
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

    pub(super) fn build_config(&self) -> Result<DownloadConfig> {
        let url = Url::parse(self.url.trim()).context("URL is invalid")?;
        let connections = parse_connection_strategy(&self.connections)?;
        let retries = parse_usize(&self.retries, "Retries")?;
        let timeout_secs = parse_u64(&self.timeout_secs, "Timeout")?;

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
            no_range_strategy: parse_no_range_strategy(
                "overlap",
                default_no_range_workers(),
                default_overlap_bytes(),
            )?,
        })
    }

    fn focused_input_mut(&mut self) -> Option<(&mut String, &mut usize, bool)> {
        match self.focused_field() {
            FormField::Url => Some((&mut self.url, &mut self.url_cursor, false)),
            FormField::Output => Some((&mut self.output, &mut self.output_cursor, false)),
            FormField::Connections => {
                Some((&mut self.connections, &mut self.connections_cursor, false))
            }
            FormField::Retries => Some((&mut self.retries, &mut self.retries_cursor, true)),
            FormField::Timeout => Some((&mut self.timeout_secs, &mut self.timeout_cursor, true)),
            FormField::Resume => None,
        }
    }
}

fn char_to_byte_index(s: &str, char_index: usize) -> usize {
    s.char_indices()
        .nth(char_index)
        .map(|(idx, _)| idx)
        .unwrap_or(s.len())
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
