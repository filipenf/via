use std::path::Path;

use crate::nvim::FileTarget;
use crate::pty::TerminalSize;

pub(super) fn file_reference_at(
    row: &str,
    column: usize,
    working_directory: &Path,
) -> Option<FileTarget> {
    let spans = file_reference_spans(row, working_directory);

    spans
        .into_iter()
        .find(|span| column >= span.start && column < span.end)
        .map(|span| span.target)
}

fn file_reference_spans(row: &str, working_directory: &Path) -> Vec<FileReferenceSpan> {
    let chars: Vec<char> = row.chars().collect();
    let mut spans = Vec::new();
    let mut index = 0;

    while index < chars.len() {
        while index < chars.len() && !is_file_reference_char(chars[index]) {
            index += 1;
        }

        let start = index;

        while index < chars.len() && is_file_reference_char(chars[index]) {
            index += 1;
        }

        if start == index {
            continue;
        }

        let Some((token_start, token_end, token)) = trim_file_reference(&chars[start..index])
        else {
            continue;
        };

        if !looks_like_file_reference(&token) {
            continue;
        }

        let target = FileTarget::parse(&token, working_directory);

        spans.push(FileReferenceSpan {
            start: start + token_start,
            end: start + token_end,
            target,
        });
    }

    spans
}

#[derive(Debug)]
struct FileReferenceSpan {
    pub(super) start: usize,
    pub(super) end: usize,
    target: FileTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LinkSpan {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) uri: String,
}

#[derive(Debug)]
pub(super) struct Osc8Tracker {
    rows: Vec<Vec<LinkSpan>>,
    active_uri: Option<String>,
    size: TerminalSize,
    row: usize,
    column: usize,
    index: usize,
}

impl Osc8Tracker {
    pub(super) fn new(size: TerminalSize) -> Self {
        Self {
            rows: vec![Vec::new(); size.rows as usize],
            active_uri: None,
            size,
            row: 0,
            column: 0,
            index: 0,
        }
    }

    pub(super) fn resize(&mut self, size: TerminalSize) {
        self.size = size;
        self.rows.resize(size.rows as usize, Vec::new());
        self.row = self.row.min(size.rows.saturating_sub(1) as usize);
        self.column = self.column.min(size.cols.saturating_sub(1) as usize);
    }

    pub(super) fn links(&self) -> &[Vec<LinkSpan>] {
        &self.rows
    }

    pub(super) fn process(&mut self, bytes: &[u8]) {
        self.index = 0;

        while self.index < bytes.len() {
            match bytes[self.index] {
                b'\x1b' => self.parse_escape(bytes),
                b'\n' => self.newline(),
                b'\r' => {
                    self.column = 0;
                    self.index += 1;
                }
                byte if byte.is_ascii_control() => {
                    self.index += 1;
                }
                _ => self.print_char(bytes),
            }
        }
    }

    fn parse_escape(&mut self, bytes: &[u8]) {
        if bytes.get(self.index + 1) == Some(&b']') {
            self.parse_osc(bytes);
            return;
        }

        if bytes.get(self.index + 1) == Some(&b'[') {
            self.parse_csi(bytes);
            return;
        }

        self.index += 1;
        while self.index < bytes.len() {
            let byte = bytes[self.index];
            self.index += 1;

            if (0x40..=0x7e).contains(&byte) {
                break;
            }
        }
    }

    fn parse_csi(&mut self, bytes: &[u8]) {
        let start = self.index + 2;
        let mut cursor = start;

        while cursor < bytes.len() {
            let byte = bytes[cursor];

            if (0x40..=0x7e).contains(&byte) {
                self.apply_csi(&bytes[start..cursor], byte);
                self.index = cursor + 1;
                return;
            }

            cursor += 1;
        }

        self.index = bytes.len();
    }

    fn parse_osc(&mut self, bytes: &[u8]) {
        let payload_start = self.index + 2;
        let mut cursor = payload_start;

        while cursor < bytes.len() {
            if bytes[cursor] == b'\x07' {
                self.apply_osc(&bytes[payload_start..cursor]);
                self.index = cursor + 1;
                return;
            }

            if bytes[cursor] == b'\x1b' && bytes.get(cursor + 1) == Some(&b'\\') {
                self.apply_osc(&bytes[payload_start..cursor]);
                self.index = cursor + 2;
                return;
            }

            cursor += 1;
        }

        self.index = bytes.len();
    }

    fn apply_osc(&mut self, payload: &[u8]) {
        let Ok(payload) = std::str::from_utf8(payload) else {
            return;
        };

        let Some(rest) = payload.strip_prefix("8;") else {
            return;
        };
        let Some((_, uri)) = rest.split_once(';') else {
            return;
        };

        if uri.is_empty() {
            self.active_uri = None;
        } else {
            self.active_uri = Some(uri.to_string());
        }
    }

    fn print_char(&mut self, bytes: &[u8]) {
        let byte = bytes[self.index];
        let len = if byte < 0x80 {
            1
        } else {
            match std::str::from_utf8(&bytes[self.index..]) {
                Ok(text) => text.chars().next().map(char::len_utf8).unwrap_or(1),
                Err(error) => error.valid_up_to().max(1),
            }
        };

        self.extend_active_link();
        self.advance_column();
        self.index += len;
    }

    fn newline(&mut self) {
        self.row += 1;
        if self.row >= self.size.rows as usize {
            self.scroll_rows_up();
            self.row = self.size.rows.saturating_sub(1) as usize;
        }
        self.column = 0;
        self.index += 1;
    }

    fn apply_csi(&mut self, params: &[u8], command: u8) {
        let (first, second) = first_two_csi_numbers(params);

        match command {
            b'A' => self.row = self.row.saturating_sub(first),
            b'B' => {
                self.row = (self.row + first).min(self.size.rows.saturating_sub(1) as usize);
            }
            b'C' => {
                self.column = (self.column + first).min(self.size.cols.saturating_sub(1) as usize);
            }
            b'D' => self.column = self.column.saturating_sub(first),
            b'H' | b'f' => {
                self.row = first
                    .saturating_sub(1)
                    .min(self.size.rows.saturating_sub(1) as usize);
                self.column = second
                    .saturating_sub(1)
                    .min(self.size.cols.saturating_sub(1) as usize);
            }
            b'J' => self.clear_screen(),
            b'K' => self.clear_current_row(),
            _ => {}
        }
    }

    fn extend_active_link(&mut self) {
        let Some(uri) = &self.active_uri else {
            return;
        };
        let Some(row) = self.rows.get_mut(self.row) else {
            return;
        };

        match row.last_mut() {
            Some(span) if span.uri == *uri && span.end == self.column => {
                span.end += 1;
            }
            _ => row.push(LinkSpan {
                start: self.column,
                end: self.column + 1,
                uri: uri.clone(),
            }),
        }
    }

    fn advance_column(&mut self) {
        self.column += 1;

        if self.column >= self.size.cols as usize {
            self.newline_without_index();
        }
    }

    fn newline_without_index(&mut self) {
        self.row += 1;
        if self.row >= self.size.rows as usize {
            self.scroll_rows_up();
            self.row = self.size.rows.saturating_sub(1) as usize;
        }
        self.column = 0;
    }

    fn scroll_rows_up(&mut self) {
        if self.rows.is_empty() {
            return;
        }

        self.rows.rotate_left(1);
        if let Some(row) = self.rows.last_mut() {
            row.clear();
        }
    }

    fn clear_screen(&mut self) {
        self.rows.iter_mut().for_each(Vec::clear);
        self.row = 0;
        self.column = 0;
    }

    fn clear_current_row(&mut self) {
        if let Some(row) = self.rows.get_mut(self.row) {
            row.clear();
        }
    }
}

#[cfg(test)]
pub(super) fn parse_vt_hyperlinks(bytes: &[u8], size: TerminalSize) -> Vec<Vec<LinkSpan>> {
    let mut parser = Osc8Tracker::new(size);

    parser.process(bytes);
    parser.rows
}

fn first_two_csi_numbers(params: &[u8]) -> (usize, usize) {
    let mut numbers = [1usize; 2];
    let mut index = 0usize;
    let mut value = 0usize;
    let mut has_value = false;

    for byte in params.iter().copied() {
        match byte {
            b'?' => {}
            b'0'..=b'9' => {
                has_value = true;
                value = value
                    .saturating_mul(10)
                    .saturating_add((byte - b'0') as usize);
            }
            b';' => {
                if index < numbers.len() {
                    numbers[index] = if has_value { value } else { 1 };
                }
                index += 1;
                value = 0;
                has_value = false;
                if index >= numbers.len() {
                    break;
                }
            }
            _ => {}
        }
    }

    if index < numbers.len() {
        numbers[index] = if has_value { value } else { 1 };
    }

    (numbers[0], numbers[1])
}

pub(super) fn file_target_from_uri(uri: &str, working_directory: &Path) -> Option<FileTarget> {
    let path = uri
        .strip_prefix("file://")
        .map(percent_decode)
        .or_else(|| uri.strip_prefix("file:").map(percent_decode))
        .or_else(|| {
            if uri.contains("://") {
                None
            } else {
                Some(uri.to_string())
            }
        })?;

    Some(FileTarget::parse(&path, working_directory))
}

fn percent_decode(input: &str) -> String {
    let mut bytes = Vec::with_capacity(input.len());
    let input = input.as_bytes();
    let mut index = 0;

    while index < input.len() {
        if input[index] == b'%' && index + 2 < input.len() {
            if let Ok(hex) = std::str::from_utf8(&input[index + 1..index + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    bytes.push(byte);
                    index += 3;
                    continue;
                }
            }
        }

        bytes.push(input[index]);
        index += 1;
    }

    String::from_utf8_lossy(&bytes).into_owned()
}

fn trim_file_reference(chars: &[char]) -> Option<(usize, usize, String)> {
    let mut start = 0;
    let mut end = chars.len();

    while start < end && matches!(chars[start], '"' | '\'' | '`' | '(' | '[' | '{' | '<') {
        start += 1;
    }

    while end > start
        && matches!(
            chars[end - 1],
            '"' | '\'' | '`' | ')' | ']' | '}' | '>' | ',' | ';' | '.' | ':'
        )
    {
        end -= 1;
    }

    if start == end {
        return None;
    }

    Some((start, end, chars[start..end].iter().collect()))
}

fn looks_like_file_reference(token: &str) -> bool {
    token.contains('/')
        || token.contains('\\')
        || token.contains('.')
        || token
            .rsplit_once(':')
            .is_some_and(|(_, line)| line.parse::<u32>().is_ok())
}

fn is_file_reference_char(ch: char) -> bool {
    ch.is_alphanumeric()
        || matches!(
            ch,
            '/' | '\\'
                | '.'
                | '_'
                | '-'
                | ':'
                | '~'
                | '@'
                | '+'
                | '"'
                | '\''
                | '`'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '<'
                | '>'
                | ','
                | ';'
        )
}
