//! Convert standard markdown to Telegram-compatible HTML.
//!
//! Telegram supports a limited subset of HTML: `<b>`, `<i>`, `<s>`, `<u>`,
//! `<code>`, `<pre>`, `<a href>`, `<blockquote>`, `<tg-spoiler>`.
//! This module converts common markdown patterns to that subset.

/// Convert markdown text to Telegram-compatible HTML.
///
/// Handles:
/// - `**bold**` / `__bold__` → `<b>bold</b>`
/// - `*italic*` / `_italic_` → `<i>italic</i>`
/// - `` `code` `` → `<code>code</code>`
/// - ```` ```lang\ncode\n``` ```` → `<pre><code class="language-lang">code</code></pre>`
/// - `~~strike~~` → `<s>strike</s>`
/// - `[text](url)` → `<a href="url">text</a>`
/// - `# Heading` → `<b>Heading</b>`
/// - `> quote` → `<blockquote>quote</blockquote>`
/// - List items (`- `, `* `, `1. `) → preserved with bullet/number prefix
///
/// All other text is HTML-escaped (`<`, `>`, `&`).
pub fn markdown_to_telegram_html(input: &str) -> String {
    let mut result = String::with_capacity(input.len() + input.len() / 4);
    let lines: Vec<&str> = input.lines().collect();
    let len = lines.len();
    let mut i = 0;

    while i < len {
        let line = lines[i];

        // Fenced code block: ```lang ... ```
        if line.trim_start().starts_with("```") {
            let indent = line.len() - line.trim_start().len();
            let after_fence = line.trim_start().trim_start_matches('`');
            let lang = after_fence.trim();

            i += 1;
            let mut code = String::new();
            let mut closed = false;

            while i < len {
                if lines[i].trim_start().starts_with("```") {
                    closed = true;
                    i += 1;
                    break;
                }
                if !code.is_empty() {
                    code.push('\n');
                }
                // Remove common indent from code lines
                let code_line = if lines[i].len() > indent {
                    let (prefix, rest) = lines[i].split_at(indent.min(lines[i].len()));
                    if prefix.chars().all(|c| c == ' ' || c == '\t') {
                        rest
                    } else {
                        lines[i]
                    }
                } else {
                    lines[i]
                };
                code.push_str(code_line);
                i += 1;
            }

            if !closed {
                // Unterminated code block — treat as-is
                result.push_str(&html_escape(line));
                result.push('\n');
                continue;
            }

            if lang.is_empty() {
                result.push_str("<pre>");
                result.push_str(&html_escape(&code));
                result.push_str("</pre>");
            } else {
                result.push_str("<pre><code class=\"language-");
                result.push_str(&html_escape(lang));
                result.push_str("\">");
                result.push_str(&html_escape(&code));
                result.push_str("</code></pre>");
            }
            result.push('\n');
            continue;
        }

        // Horizontal rule: ---, ***, ___ (3+ chars, possibly with spaces)
        if is_horizontal_rule(line) {
            result.push_str("———");
            result.push('\n');
            i += 1;
            continue;
        }

        // Markdown table: consecutive lines starting with |
        if line.trim_start().starts_with('|') {
            let mut table_lines = Vec::new();
            while i < len && lines[i].trim_start().starts_with('|') {
                table_lines.push(lines[i]);
                i += 1;
            }
            render_table(&table_lines, &mut result);
            continue;
        }

        // Heading: # ... → bold
        if let Some(rest) = strip_heading(line) {
            result.push_str("<b>");
            result.push_str(&convert_inline(&html_escape(rest)));
            result.push_str("</b>");
            result.push('\n');
            i += 1;
            continue;
        }

        // Blockquote: > text
        if line.trim_start().starts_with("> ") || line.trim_start() == ">" {
            let mut quote_lines = Vec::new();
            while i < len
                && (lines[i].trim_start().starts_with("> ") || lines[i].trim_start() == ">")
            {
                let content = lines[i]
                    .trim_start()
                    .strip_prefix("> ")
                    .or_else(|| lines[i].trim_start().strip_prefix(">"))
                    .unwrap_or("");
                quote_lines.push(content);
                i += 1;
            }
            result.push_str("<blockquote>");
            let quote_text = quote_lines.join("\n");
            result.push_str(&convert_inline(&html_escape(&quote_text)));
            result.push_str("</blockquote>");
            result.push('\n');
            continue;
        }

        // Unordered list item: - text or * text (but not ** or *italic*)
        if (line.trim_start().starts_with("- ") || line.trim_start().starts_with("* "))
            && !line.trim_start().starts_with("**")
        {
            let content = if line.trim_start().starts_with("- ") {
                line.trim_start().strip_prefix("- ").unwrap_or("")
            } else {
                line.trim_start().strip_prefix("* ").unwrap_or("")
            };
            result.push_str("• ");
            result.push_str(&convert_inline(&html_escape(content)));
            result.push('\n');
            i += 1;
            continue;
        }

        // Ordered list item: 1. text
        if is_ordered_list_item(line.trim_start()) {
            let trimmed = line.trim_start();
            let dot_pos = trimmed.find(". ").unwrap();
            let number = &trimmed[..dot_pos];
            let content = &trimmed[dot_pos + 2..];
            result.push_str(number);
            result.push_str(". ");
            result.push_str(&convert_inline(&html_escape(content)));
            result.push('\n');
            i += 1;
            continue;
        }

        // Regular line: apply inline formatting
        let escaped = html_escape(line);
        result.push_str(&convert_inline(&escaped));
        result.push('\n');
        i += 1;
    }

    // Trim trailing newline
    if result.ends_with('\n') {
        result.truncate(result.len() - 1);
    }

    result
}

/// Check if a line is a horizontal rule (---, ***, ___ with 3+ chars).
fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.len() < 3 {
        return false;
    }
    let c = trimmed.as_bytes()[0];
    if c != b'-' && c != b'*' && c != b'_' {
        return false;
    }
    trimmed.bytes().all(|b| b == c || b == b' ')
}

/// Check if a table row is a separator row like |---|---|.
fn is_table_separator(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.starts_with('|') {
        return false;
    }
    // A separator row contains only |, -, :, and spaces
    trimmed
        .bytes()
        .all(|b| b == b'|' || b == b'-' || b == b':' || b == b' ')
        && trimmed.contains('-')
}

/// Parse table cells from a row like "| A | B | C |".
fn parse_table_cells(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    // Strip leading and trailing |
    let inner = trimmed
        .strip_prefix('|')
        .unwrap_or(trimmed)
        .strip_suffix('|')
        .unwrap_or(trimmed);
    inner
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect()
}

/// Render a markdown table into Telegram-compatible HTML.
///
/// Strategy: bold the header row, format data rows as label: value pairs
/// for 2-column tables, or as pipe-separated lines for wider tables.
fn render_table(table_lines: &[&str], result: &mut String) {
    // Separate header, separator, and data rows
    let mut header: Option<Vec<String>> = None;
    let mut data_rows: Vec<Vec<String>> = Vec::new();

    for &line in table_lines {
        if is_table_separator(line) {
            continue; // Skip separator rows
        }
        let cells = parse_table_cells(line);
        if header.is_none() {
            header = Some(cells);
        } else {
            data_rows.push(cells);
        }
    }

    let header = match header {
        Some(h) => h,
        None => return, // Empty table
    };

    if data_rows.is_empty() {
        // Header-only table → just bold it
        result.push_str("<b>");
        result.push_str(&html_escape(&header.join(" | ")));
        result.push_str("</b>\n");
        return;
    }

    let col_count = header.len();

    if col_count == 2 {
        // 2-column table → key: value format with bold keys
        for row in &data_rows {
            let key = row.first().map(|s| s.as_str()).unwrap_or("");
            let val = row.get(1).map(|s| s.as_str()).unwrap_or("");
            result.push_str("<b>");
            result.push_str(&convert_inline(&html_escape(key)));
            result.push_str("</b>: ");
            result.push_str(&convert_inline(&html_escape(val)));
            result.push('\n');
        }
    } else {
        // Multi-column table → header as bold, data as pipe-separated lines
        result.push_str("<b>");
        result.push_str(&html_escape(&header.join(" | ")));
        result.push_str("</b>\n");
        for row in &data_rows {
            result.push_str(&convert_inline(&html_escape(&row.join(" | "))));
            result.push('\n');
        }
    }
}

/// Check if a line is a heading (# ... through ######) and return the content.
fn strip_heading(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('#') {
        return None;
    }
    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
    if hashes > 6 {
        return None;
    }
    let rest = &trimmed[hashes..];
    if let Some(after_space) = rest.strip_prefix(' ') {
        Some(after_space.trim_end())
    } else if rest.is_empty() {
        Some("")
    } else {
        None // Not a heading (e.g., #hashtag)
    }
}

/// Check if a line starts with an ordered list pattern like "1. ", "2. ", etc.
fn is_ordered_list_item(s: &str) -> bool {
    let digit_end = s.bytes().take_while(|b| b.is_ascii_digit()).count();
    digit_end > 0 && s[digit_end..].starts_with(". ")
}

/// Apply inline markdown conversions to already-HTML-escaped text.
///
/// Because the text is already escaped, we look for markdown patterns
/// in the escaped text. The patterns we convert don't contain `<`, `>`, `&`
/// so they survive HTML escaping unchanged.
fn convert_inline(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let len = chars.len();
    let mut result = String::with_capacity(input.len());
    let mut i = 0;

    while i < len {
        // Inline code: `...`
        if chars[i] == '`' {
            if let Some((code, end)) = extract_delimited(&chars, i, '`', '`') {
                result.push_str("<code>");
                result.push_str(&code);
                result.push_str("</code>");
                i = end;
                continue;
            }
        }

        // Bold: **...** (must check before single *)
        if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            if let Some((text, end)) = extract_double_delimited(&chars, i, '*') {
                result.push_str("<b>");
                result.push_str(&text);
                result.push_str("</b>");
                i = end;
                continue;
            }
        }

        // Bold: __...__
        if i + 1 < len && chars[i] == '_' && chars[i + 1] == '_' {
            if let Some((text, end)) = extract_double_delimited(&chars, i, '_') {
                result.push_str("<b>");
                result.push_str(&text);
                result.push_str("</b>");
                i = end;
                continue;
            }
        }

        // Strikethrough: ~~...~~
        if i + 1 < len && chars[i] == '~' && chars[i + 1] == '~' {
            if let Some((text, end)) = extract_double_delimited(&chars, i, '~') {
                result.push_str("<s>");
                result.push_str(&text);
                result.push_str("</s>");
                i = end;
                continue;
            }
        }

        // Italic: *...* (single, not **)
        if chars[i] == '*' && (i + 1 >= len || chars[i + 1] != '*') {
            if let Some((text, end)) = extract_delimited(&chars, i, '*', '*') {
                if !text.is_empty() {
                    result.push_str("<i>");
                    result.push_str(&text);
                    result.push_str("</i>");
                    i = end;
                    continue;
                }
            }
        }

        // Italic: _..._ (single, not __)
        if chars[i] == '_' && (i + 1 >= len || chars[i + 1] != '_') {
            if let Some((text, end)) = extract_delimited(&chars, i, '_', '_') {
                if !text.is_empty() {
                    result.push_str("<i>");
                    result.push_str(&text);
                    result.push_str("</i>");
                    i = end;
                    continue;
                }
            }
        }

        // Link: [text](url)
        if chars[i] == '[' {
            if let Some((link_text, url, end)) = extract_link(&chars, i) {
                result.push_str("<a href=\"");
                result.push_str(&url);
                result.push_str("\">");
                result.push_str(&link_text);
                result.push_str("</a>");
                i = end;
                continue;
            }
        }

        result.push(chars[i]);
        i += 1;
    }

    result
}

/// Extract content between single-character delimiters.
/// Returns (content, position after closing delimiter).
fn extract_delimited(
    chars: &[char],
    start: usize,
    open: char,
    close: char,
) -> Option<(String, usize)> {
    if chars[start] != open {
        return None;
    }
    let content_start = start + 1;
    let mut j = content_start;
    while j < chars.len() {
        if chars[j] == close && j > content_start {
            let content: String = chars[content_start..j].iter().collect();
            return Some((content, j + 1));
        }
        // Don't span across newlines for inline elements
        if chars[j] == '\n' {
            return None;
        }
        j += 1;
    }
    None
}

/// Extract content between double-character delimiters (**, __, ~~).
/// Returns (content, position after closing delimiter).
fn extract_double_delimited(chars: &[char], start: usize, delim: char) -> Option<(String, usize)> {
    if start + 1 >= chars.len() || chars[start] != delim || chars[start + 1] != delim {
        return None;
    }
    let content_start = start + 2;
    let mut j = content_start;
    while j + 1 < chars.len() {
        if chars[j] == delim && chars[j + 1] == delim {
            if j > content_start {
                let content: String = chars[content_start..j].iter().collect();
                return Some((content, j + 2));
            }
            return None;
        }
        if chars[j] == '\n' {
            return None;
        }
        j += 1;
    }
    None
}

/// Extract a markdown link [text](url).
/// Returns (text, url, position after closing paren).
fn extract_link(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    if chars[start] != '[' {
        return None;
    }
    let text_start = start + 1;
    let mut j = text_start;

    // Find closing ]
    while j < chars.len() && chars[j] != ']' && chars[j] != '\n' {
        j += 1;
    }
    if j >= chars.len() || chars[j] != ']' {
        return None;
    }
    let text: String = chars[text_start..j].iter().collect();
    j += 1; // skip ]

    // Expect (
    if j >= chars.len() || chars[j] != '(' {
        return None;
    }
    j += 1; // skip (
    let url_start = j;

    // Find closing )
    while j < chars.len() && chars[j] != ')' && chars[j] != '\n' {
        j += 1;
    }
    if j >= chars.len() || chars[j] != ')' {
        return None;
    }
    let url: String = chars[url_start..j].iter().collect();
    j += 1; // skip )

    Some((text, url, j))
}

/// Escape HTML special characters, converting `<br>` / `<br/>` to newlines.
fn html_escape(s: &str) -> String {
    // First replace <br> variants with newlines, then escape remaining HTML
    let s = s
        .replace("<br>", "\n")
        .replace("<br/>", "\n")
        .replace("<br />", "\n");
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => result.push_str("&amp;"),
            '<' => result.push_str("&lt;"),
            '>' => result.push_str("&gt;"),
            '"' => result.push_str("&quot;"),
            _ => result.push(c),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_text() {
        assert_eq!(markdown_to_telegram_html("Hello world"), "Hello world");
    }

    #[test]
    fn test_html_escape() {
        assert_eq!(
            markdown_to_telegram_html("a < b & c > d"),
            "a &lt; b &amp; c &gt; d"
        );
    }

    #[test]
    fn test_bold() {
        assert_eq!(
            markdown_to_telegram_html("This is **bold** text"),
            "This is <b>bold</b> text"
        );
    }

    #[test]
    fn test_italic() {
        assert_eq!(
            markdown_to_telegram_html("This is *italic* text"),
            "This is <i>italic</i> text"
        );
    }

    #[test]
    fn test_inline_code() {
        assert_eq!(
            markdown_to_telegram_html("Use `println!` macro"),
            "Use <code>println!</code> macro"
        );
    }

    #[test]
    fn test_strikethrough() {
        assert_eq!(
            markdown_to_telegram_html("This is ~~deleted~~ text"),
            "This is <s>deleted</s> text"
        );
    }

    #[test]
    fn test_link() {
        assert_eq!(
            markdown_to_telegram_html("Click [here](https://example.com) now"),
            "Click <a href=\"https://example.com\">here</a> now"
        );
    }

    #[test]
    fn test_code_block() {
        let input = "```rust\nfn main() {\n    println!(\"hello\");\n}\n```";
        let expected = "<pre><code class=\"language-rust\">fn main() {\n    println!(&quot;hello&quot;);\n}</code></pre>";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn test_code_block_no_lang() {
        let input = "```\nhello\nworld\n```";
        let expected = "<pre>hello\nworld</pre>";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn test_heading() {
        assert_eq!(markdown_to_telegram_html("# Title"), "<b>Title</b>");
        assert_eq!(markdown_to_telegram_html("## Section"), "<b>Section</b>");
        assert_eq!(
            markdown_to_telegram_html("### Sub-section"),
            "<b>Sub-section</b>"
        );
    }

    #[test]
    fn test_blockquote() {
        assert_eq!(
            markdown_to_telegram_html("> This is a quote"),
            "<blockquote>This is a quote</blockquote>"
        );
    }

    #[test]
    fn test_unordered_list() {
        let input = "- Item one\n- Item two\n- Item three";
        let expected = "• Item one\n• Item two\n• Item three";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn test_ordered_list() {
        let input = "1. First\n2. Second\n3. Third";
        let expected = "1. First\n2. Second\n3. Third";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn test_mixed_formatting() {
        let input = "# Hello\n\nThis is **bold** and *italic* with `code`.\n\n- Item 1\n- Item 2";
        let expected = "<b>Hello</b>\n\nThis is <b>bold</b> and <i>italic</i> with <code>code</code>.\n\n• Item 1\n• Item 2";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn test_not_a_heading() {
        // #hashtag should not be treated as heading
        assert_eq!(markdown_to_telegram_html("#hashtag"), "#hashtag");
    }

    #[test]
    fn test_bold_with_underscore() {
        assert_eq!(
            markdown_to_telegram_html("This is __bold__ text"),
            "This is <b>bold</b> text"
        );
    }

    #[test]
    fn test_horizontal_rule() {
        assert_eq!(markdown_to_telegram_html("---"), "———");
        assert_eq!(markdown_to_telegram_html("***"), "———");
        assert_eq!(markdown_to_telegram_html("___"), "———");
        assert_eq!(markdown_to_telegram_html("------"), "———");
    }

    #[test]
    fn test_table_two_columns() {
        let input = "| 项目 | 状况 |\n|------|------|\n| 温度 | 4°C |\n| 天气 | 多云 |";
        let expected = "<b>温度</b>: 4°C\n<b>天气</b>: 多云";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn test_table_multi_columns() {
        let input = "| 日期 | 温度 | 天气 |\n|------|------|------|\n| 周一 | 4°C | 雨 |\n| 周二 | 6°C | 晴 |";
        let expected = "<b>日期 | 温度 | 天气</b>\n周一 | 4°C | 雨\n周二 | 6°C | 晴";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn test_table_with_inline_formatting() {
        let input = "| Name | Value |\n|------|-------|\n| **bold** | *italic* |";
        let expected = "<b><b>bold</b></b>: <i>italic</i>";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn test_full_weather_message() {
        let input = "🌤️ 当前天气\n\n---\n\n| 项目 | 状况 |\n|------|------|\n| 温度 | 4°C |\n| 天气 | 多云 |\n\n---\n\n出门记得带伞。";
        let result = markdown_to_telegram_html(input);
        assert!(result.contains("———"));
        assert!(result.contains("<b>温度</b>: 4°C"));
        assert!(result.contains("<b>天气</b>: 多云"));
        assert!(result.contains("出门记得带伞。"));
    }
}
