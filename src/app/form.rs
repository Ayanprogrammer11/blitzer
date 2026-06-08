use crate::config::{
    DownloadConfig, MAX_CONNECTIONS, MAX_OUTPUT_CHARS, MAX_RETRIES, MAX_TIMEOUT, MAX_URL_BYTES,
    default_connection_input, default_no_range_workers, default_overlap_bytes,
    parse_connection_strategy, parse_http_url, parse_no_range_strategy, parse_output_path,
    parse_retries, parse_timeout_secs,
};
use anyhow::{Result, bail};

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
    pub(super) invalid_field: Option<FormField>,
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
            invalid_field: None,
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
        self.invalid_field = None;
    }

    pub(super) fn set_error(&mut self, field: FormField, message: impl Into<String>) {
        self.message = message.into();
        self.error = true;
        self.invalid_field = Some(field);
        self.set_focus_field(field);
    }

    pub(super) fn set_info(&mut self, message: impl Into<String>) {
        self.message = message.into();
        self.error = false;
        self.invalid_field = None;
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

    pub(super) fn type_char(&mut self, c: char) -> Result<()> {
        let field = self.focused_field();
        if matches!(field, FormField::Resume) && c == ' ' {
            self.no_resume = !self.no_resume;
            return Ok(());
        }

        if let Some((value, cursor, _digits_only)) = self.focused_input_mut() {
            let idx = char_to_byte_index(value, *cursor);
            let mut candidate = value.clone();
            candidate.insert(idx, c);
            validate_input_shape(field, &candidate)?;
            value.insert(idx, c);
            *cursor += 1;
        }
        Ok(())
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

    pub(super) fn clear_focused_input(&mut self) {
        if let Some((value, cursor, _digits_only)) = self.focused_input_mut() {
            value.clear();
            *cursor = 0;
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

    pub(super) fn validate_field(&self, field: FormField) -> Result<()> {
        match field {
            FormField::Url => parse_http_url(&self.url).map(|_| ()),
            FormField::Output => parse_output_path(&self.output).map(|_| ()),
            FormField::Connections => parse_connection_strategy(&self.connections).map(|_| ()),
            FormField::Retries => parse_retries(&self.retries).map(|_| ()),
            FormField::Timeout => parse_timeout_secs(&self.timeout_secs).map(|_| ()),
            FormField::Resume => Ok(()),
        }
    }

    pub(super) fn validate_focused(&mut self) -> bool {
        let field = self.focused_field();
        match self.validate_field(field) {
            Ok(()) => true,
            Err(err) => {
                self.set_error(field, format!("{err:#}"));
                false
            }
        }
    }

    pub(super) fn build_config(&mut self) -> Result<DownloadConfig> {
        for field in FORM_FIELDS {
            if let Err(err) = self.validate_field(field) {
                self.set_error(field, format!("{err:#}"));
                return Err(err);
            }
        }

        let url = parse_http_url(&self.url)?;
        let connections = parse_connection_strategy(&self.connections)?;
        let retries = parse_retries(&self.retries)?;
        let timeout_secs = parse_timeout_secs(&self.timeout_secs)?;
        let output = parse_output_path(&self.output)?;

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

fn validate_input_shape(field: FormField, candidate: &str) -> Result<()> {
    match field {
        FormField::Url => {
            if candidate.len() > MAX_URL_BYTES {
                bail!("URL is too long (maximum {MAX_URL_BYTES} bytes)");
            }
            if candidate.chars().any(char::is_whitespace) {
                bail!("URL cannot contain whitespace");
            }
        }
        FormField::Output => {
            if candidate.chars().count() > MAX_OUTPUT_CHARS {
                bail!("Output path is too long (maximum {MAX_OUTPUT_CHARS} characters)");
            }
            if candidate.chars().any(char::is_control) {
                bail!("Output path cannot contain control characters");
            }
        }
        FormField::Connections => {
            let lower = candidate.to_ascii_lowercase();
            let is_auto_prefix = "auto".starts_with(&lower);
            let is_number = candidate.chars().all(|c| c.is_ascii_digit())
                && candidate
                    .parse::<usize>()
                    .map(|value| value <= MAX_CONNECTIONS)
                    .unwrap_or(candidate.is_empty());
            if !is_auto_prefix && !is_number {
                bail!("Connections accepts only 'auto' or 1..={MAX_CONNECTIONS}");
            }
        }
        FormField::Retries => validate_bounded_number(candidate, "Retries", MAX_RETRIES as u64)?,
        FormField::Timeout => validate_bounded_number(candidate, "Timeout", MAX_TIMEOUT)?,
        FormField::Resume => {}
    }
    Ok(())
}

fn validate_bounded_number(candidate: &str, label: &str, max: u64) -> Result<()> {
    if !candidate.chars().all(|c| c.is_ascii_digit()) {
        bail!("{label} accepts digits only");
    }
    if !candidate.is_empty() && candidate.parse::<u64>().map_or(true, |value| value > max) {
        bail!("{label} must be <= {max}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ambiguous_connection_value_cannot_leave_field_or_start() {
        let mut form = FormState {
            url: "https://example.com/file.bin".to_string(),
            ..FormState::default()
        };
        form.set_focus_field(FormField::Connections);
        form.connections = "a".to_string();
        form.connections_cursor = 1;

        assert!(!form.validate_focused());
        assert_eq!(form.focused_field(), FormField::Connections);
        assert_eq!(form.invalid_field, Some(FormField::Connections));
        assert!(form.build_config().is_err());
    }

    #[test]
    fn field_input_rejects_invalid_characters_and_bounds() {
        let mut form = FormState::default();
        form.set_focus_field(FormField::Url);
        assert!(form.type_char(' ').is_err());

        form.set_focus_field(FormField::Output);
        assert!(form.type_char('\n').is_err());

        form.set_focus_field(FormField::Connections);
        assert!(form.type_char('z').is_err());

        form.set_focus_field(FormField::Retries);
        form.retries.clear();
        form.retries_cursor = 0;
        assert!(form.type_char('x').is_err());
        assert!(form.type_char('2').is_ok());
        assert!(form.type_char('1').is_err());

        form.set_focus_field(FormField::Timeout);
        form.timeout_secs.clear();
        form.timeout_cursor = 0;
        for c in "300".chars() {
            assert!(form.type_char(c).is_ok());
        }
        assert!(form.type_char('1').is_err());
    }
}
