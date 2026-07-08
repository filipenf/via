use std::path::Path;

use crate::nvim::FileTarget;
use crate::pty::TerminalSize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReferenceTarget {
    File(FileTarget),
    Symbol(String),
    Url(String),
}

#[derive(Debug)]
pub struct ReferenceSpan {
    pub start: usize,
    pub end: usize,
    pub target: ReferenceTarget,
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

#[derive(Debug)]
struct FileReferenceSpan {
    start: usize,
    end: usize,
    target: FileTarget,
}

#[derive(Debug)]
struct SymbolReferenceSpan {
    start: usize,
    end: usize,
    symbol: String,
}

#[derive(Debug)]
struct UrlReferenceSpan {
    start: usize,
    end: usize,
    url: String,
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
pub fn reference_target_from_row(
    row: &str,
    column: usize,
    working_directory: &Path,
) -> Option<ReferenceTarget> {
    reference_spans_from_row(row, working_directory)
        .into_iter()
        .find(|span| column >= span.start && column < span.end)
        .map(|span| span.target)
}

pub fn reference_spans_from_row(row: &str, working_directory: &Path) -> Vec<ReferenceSpan> {
    let mut spans: Vec<ReferenceSpan> = url_reference_spans(row)
        .into_iter()
        .map(|span| ReferenceSpan {
            start: span.start,
            end: span.end,
            target: ReferenceTarget::Url(span.url),
        })
        .collect();

    spans.extend(
        file_reference_spans(row, working_directory)
            .into_iter()
            .map(|span| ReferenceSpan {
                start: span.start,
                end: span.end,
                target: ReferenceTarget::File(span.target),
            }),
    );

    spans.extend(
        symbol_reference_spans(row)
            .into_iter()
            .map(|span| ReferenceSpan {
                start: span.start,
                end: span.end,
                target: ReferenceTarget::Symbol(span.symbol),
            }),
    );

    spans
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

        let raw_slice = &chars[start..index];
        let raw_collected: String = raw_slice.iter().collect();

        let (token_start, token_end, token) = if let Some((n_start, n_end, narrowed)) =
            narrow_call_wrapped_file_path(&raw_collected)
        {
            if looks_like_file_reference(&narrowed) {
                (n_start, n_end, narrowed)
            } else if let Some((ts, te, t)) = trim_file_reference(raw_slice) {
                (ts, te, t)
            } else {
                continue;
            }
        } else if let Some((ts, te, t)) = trim_file_reference(raw_slice) {
            (ts, te, t)
        } else {
            continue;
        };

        if !looks_like_file_reference(&token) {
            continue;
        }

        if is_http_url(&token) {
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

fn symbol_reference_spans(row: &str) -> Vec<SymbolReferenceSpan> {
    let chars: Vec<char> = row.chars().collect();
    let mut spans = Vec::new();
    let mut index = 0;

    while index < chars.len() {
        while index < chars.len() && !is_symbol_char(chars[index]) {
            index += 1;
        }

        let start = index;

        while index < chars.len() && is_symbol_char(chars[index]) {
            index += 1;
        }

        if start == index {
            continue;
        }

        let raw = &chars[start..index];
        let (rel_start, rel_end) = symbol_trim_offsets(raw);
        if rel_start >= rel_end {
            continue;
        }
        let token: String = raw[rel_start..rel_end].iter().collect();

        if !looks_like_scanned_symbol(&token) {
            continue;
        }

        spans.push(SymbolReferenceSpan {
            start: start + rel_start,
            end: start + rel_end,
            symbol: token,
        });
    }

    spans
}

fn url_reference_spans(row: &str) -> Vec<UrlReferenceSpan> {
    let chars: Vec<char> = row.chars().collect();
    let mut spans = Vec::new();
    let mut index = 0usize;

    while index < chars.len() {
        let Some(start) = http_scheme_char_index(&chars, index) else {
            break;
        };

        let token_end = url_token_char_end(&chars, start);
        let raw: String = chars[start..token_end].iter().collect();
        let url = trim_url_trailing(&raw);
        let end = start + url.chars().count();

        if !url.is_empty() {
            spans.push(UrlReferenceSpan {
                start,
                end,
                url: url.to_string(),
            });
            index = end;
        } else {
            index = start.saturating_add(1);
        }
    }

    spans
}

fn http_scheme_char_index(chars: &[char], from: usize) -> Option<usize> {
    (from..chars.len()).find(|&i| {
        starts_with_chars(chars, i, "https://") || starts_with_chars(chars, i, "http://")
    })
}

fn starts_with_chars(chars: &[char], start: usize, prefix: &str) -> bool {
    chars[start..]
        .iter()
        .copied()
        .zip(prefix.chars())
        .all(|(actual, expected)| actual == expected)
        && chars.len().saturating_sub(start) >= prefix.chars().count()
}

fn url_token_char_end(chars: &[char], start: usize) -> usize {
    let mut index = start;
    while index < chars.len() {
        let ch = chars[index];
        if ch.is_whitespace() || is_url_delimiter(ch) {
            break;
        }
        index += 1;
    }
    index
}

fn is_url_delimiter(ch: char) -> bool {
    matches!(
        ch,
        '"' | '\'' | '`' | '<' | '>' | '(' | ')' | '[' | ']' | '{' | '}'
    )
}

fn trim_url_trailing(url: &str) -> &str {
    let url = url.trim_end_matches([',', ';', '!', ')', ']', '}', '"', '\'']);

    if url.ends_with('.')
        && !url.ends_with(".html")
        && !url.ends_with(".htm")
        && !url.ends_with(".json")
        && !url.ends_with(".md")
        && !url.ends_with(".rs")
    {
        &url[..url.len() - '.'.len_utf8()]
    } else {
        url
    }
}

#[cfg(test)]
pub(super) fn parse_vt_hyperlinks(bytes: &[u8], size: TerminalSize) -> Vec<Vec<LinkSpan>> {
    let mut parser = Osc8Tracker::new(size);

    parser.process(bytes);
    parser.rows
}

pub fn reference_target_from_uri(uri: &str, working_directory: &Path) -> Option<ReferenceTarget> {
    if is_http_url(uri) {
        return Some(ReferenceTarget::Url(uri.to_string()));
    }

    if let Some(target) = file_target_from_uri(uri, working_directory) {
        return Some(ReferenceTarget::File(target));
    }

    symbol_target_from_uri(uri).map(ReferenceTarget::Symbol)
}

fn is_http_url(uri: &str) -> bool {
    uri.starts_with("http://") || uri.starts_with("https://")
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

fn symbol_target_from_uri(uri: &str) -> Option<String> {
    uri.strip_prefix("symbol://")
        .or_else(|| uri.strip_prefix("symbol:"))
        .map(percent_decode)
        .and_then(|symbol| {
            if looks_like_symbol(&symbol) {
                Some(symbol)
            } else {
                None
            }
        })
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

/// If the token looks like `Something(path-like)`, return character offsets and the inner path
/// string so clicks resolve to the file (e.g. agent output `● Update(src/lsp_bridge.rs)`).
fn narrow_call_wrapped_file_path(token: &str) -> Option<(usize, usize, String)> {
    let chars: Vec<char> = token.chars().collect();
    let open_idx = chars.iter().position(|&c| c == '(')?;
    let mut depth = 0usize;
    let mut close_idx: Option<usize> = None;

    for (i, c) in chars.iter().copied().enumerate().skip(open_idx) {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close_idx = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }

    let close_idx = close_idx?;
    let inner_start = open_idx + 1;
    if inner_start >= close_idx {
        return None;
    }

    let (rel_start, rel_end, path) = trim_file_reference(&chars[inner_start..close_idx])?;
    Some((inner_start + rel_start, inner_start + rel_end, path))
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

/// Trim offsets for scanned symbol tokens: wrappers first, then `.`.
/// Span coordinates and the returned symbol string must use the same offsets.
fn symbol_trim_offsets(chars: &[char]) -> (usize, usize) {
    let mut start = 0;
    let mut end = chars.len();

    while start < end && is_symbol_wrapper(chars[start]) {
        start += 1;
    }
    while end > start && is_symbol_wrapper(chars[end - 1]) {
        end -= 1;
    }
    while start < end && chars[start] == '.' {
        start += 1;
    }
    while end > start && chars[end - 1] == '.' {
        end -= 1;
    }

    (start, end)
}

fn looks_like_symbol(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }

    let starts_like_symbol = token
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_');

    starts_like_symbol
        && token
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | ':' | '.' | '-' | '#'))
}

fn looks_like_scanned_symbol(token: &str) -> bool {
    looks_like_symbol(token) && (token.contains("::") || token.contains('#'))
}

fn is_symbol_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric()
        || matches!(
            ch,
            '_' | ':' | '.' | '-' | '#' | '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}'
        )
}

fn is_symbol_wrapper(ch: char) -> bool {
    matches!(ch, '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}')
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn char_column(row: &str, needle: &str) -> usize {
        let byte = row.find(needle).expect("needle in row");
        row[..byte].chars().count()
    }

    #[test]
    fn resolves_http_url_from_row() {
        let row = "see https://example.com/docs/page.html for details";
        let col = char_column(row, "https://") + 4;

        assert_eq!(
            reference_target_from_row(row, col, Path::new("/repo")),
            Some(ReferenceTarget::Url(
                "https://example.com/docs/page.html".to_string()
            ))
        );
    }

    #[test]
    fn does_not_treat_http_url_as_file_reference() {
        let row = "see https://example.com/page.html for details";
        let col = char_column(row, "page");

        assert!(matches!(
            reference_target_from_row(row, col, Path::new("/repo")),
            Some(ReferenceTarget::Url(url)) if url == "https://example.com/page.html"
        ));
    }

    #[test]
    fn resolves_http_url_after_multibyte_utf8_prefix() {
        let row = "日本語 https://example.com/path 後";
        let col = char_column(row, "example");

        assert_eq!(
            reference_target_from_row(row, col, Path::new("/repo")),
            Some(ReferenceTarget::Url("https://example.com/path".to_string()))
        );
    }

    #[test]
    fn scanned_reference_spans_include_http_urls() {
        let spans = reference_spans_from_row(
            "docs at https://example.com/a.rs and src/main.rs",
            Path::new("/repo"),
        );

        assert!(spans.iter().any(|span| matches!(
            &span.target,
            ReferenceTarget::Url(url) if url == "https://example.com/a.rs"
        )));
        assert!(spans.iter().any(|span| matches!(
            &span.target,
            ReferenceTarget::File(target) if target.path.ends_with("src/main.rs")
        )));
    }

    #[test]
    fn resolves_file_reference_from_row() {
        let target =
            reference_target_from_row("open src/main.rs:42", 8, Path::new("/repo")).unwrap();

        assert_eq!(
            target,
            ReferenceTarget::File(FileTarget {
                path: PathBuf::from("/repo/src/main.rs"),
                line: Some(42),
            })
        );
    }

    #[test]
    fn resolves_symbol_reference_from_row() {
        let target =
            reference_target_from_row("symbol Foo::bar_baz here", 9, Path::new("/repo")).unwrap();

        assert_eq!(target, ReferenceTarget::Symbol("Foo::bar_baz".to_string()));
    }

    #[test]
    fn resolves_symbol_reference_from_uri() {
        let target =
            reference_target_from_uri("symbol://Foo%3A%3Abar", Path::new("/repo")).unwrap();

        assert_eq!(target, ReferenceTarget::Symbol("Foo::bar".to_string()));
    }

    #[test]
    fn ignores_non_file_and_non_symbol_uri() {
        assert!(reference_target_from_uri("ftp://example.com/file", Path::new("/repo")).is_none());
    }

    #[test]
    fn resolves_http_uri_as_url_target() {
        assert_eq!(
            reference_target_from_uri("https://example.com/Foo::bar", Path::new("/repo")),
            Some(ReferenceTarget::Url(
                "https://example.com/Foo::bar".to_string()
            ))
        );
    }

    #[test]
    fn rejects_invalid_symbol_uri() {
        assert!(reference_target_from_uri("symbol://123bad", Path::new("/repo")).is_none());
    }

    #[test]
    fn trims_wrapped_symbol_reference_from_row() {
        let target = reference_target_from_row("call (`Foo::bar`)", 8, Path::new("/repo")).unwrap();

        assert_eq!(target, ReferenceTarget::Symbol("Foo::bar".to_string()));
    }

    #[test]
    fn symbol_span_excludes_trailing_and_leading_dots() {
        let row = "see Foo::bar. and .Baz::qux here";
        let spans = reference_spans_from_row(row, Path::new("/repo"));
        let symbols: Vec<_> = spans
            .iter()
            .filter_map(|span| match &span.target {
                ReferenceTarget::Symbol(symbol) => Some((span.start, span.end, symbol.as_str())),
                _ => None,
            })
            .collect();

        assert_eq!(
            symbols,
            vec![(4, 12, "Foo::bar"), (19, 27, "Baz::qux")]
        );
        let chars: Vec<char> = row.chars().collect();
        assert_eq!(&chars[4..12], &['F', 'o', 'o', ':', ':', 'b', 'a', 'r'][..]);
        assert_eq!(&chars[19..27], &['B', 'a', 'z', ':', ':', 'q', 'u', 'x'][..]);
    }

    #[test]
    fn ignores_invalid_symbol_like_token_in_row() {
        assert!(reference_target_from_row("see 123Foo::bar", 6, Path::new("/repo")).is_none());
    }

    #[test]
    fn ignores_plain_words_as_symbol_references_from_row() {
        assert!(
            reference_target_from_row("these are plain words", 1, Path::new("/repo")).is_none()
        );
        assert!(
            reference_target_from_row("these are plain words", 10, Path::new("/repo")).is_none()
        );
    }

    #[test]
    fn scanned_reference_spans_skip_plain_words() {
        let spans = reference_spans_from_row("open Foo::bar in src/main.rs", Path::new("/repo"));

        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].start, 17);
        assert_eq!(spans[0].end, 28);
        assert_eq!(spans[1].start, 5);
        assert_eq!(spans[1].end, 13);
    }

    #[test]
    fn parses_file_reference_with_trailing_punctuation() {
        let target =
            reference_target_from_row("open src/main.rs:42,", 8, Path::new("/repo")).unwrap();

        assert_eq!(
            target,
            ReferenceTarget::File(FileTarget {
                path: PathBuf::from("/repo/src/main.rs"),
                line: Some(42),
            })
        );
    }

    #[test]
    fn resolves_file_inside_agent_style_call_wrapped_path() {
        let row = "● Update(src/lsp_bridge.rs)";
        let s_col = row
            .chars()
            .enumerate()
            .find(|(_, c)| *c == 's')
            .map(|(i, _)| i)
            .expect("path segment");

        let target = reference_target_from_row(row, s_col, Path::new("/repo")).unwrap();

        assert_eq!(
            target,
            ReferenceTarget::File(FileTarget {
                path: PathBuf::from("/repo/src/lsp_bridge.rs"),
                line: None,
            })
        );
    }

    #[test]
    fn resolves_file_inside_call_wrapped_path_with_line() {
        let row = "Update(src/lsp_bridge.rs:10)";
        let s_col = row
            .chars()
            .enumerate()
            .find(|(_, c)| *c == 's')
            .map(|(i, _)| i)
            .expect("path segment");

        let target = reference_target_from_row(row, s_col, Path::new("/repo")).unwrap();

        assert_eq!(
            target,
            ReferenceTarget::File(FileTarget {
                path: PathBuf::from("/repo/src/lsp_bridge.rs"),
                line: Some(10),
            })
        );
    }
}
