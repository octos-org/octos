//! Pure, stateless filter functions for the slash-command menu.
//! All functions are unit-testable without a TTY.

#![allow(dead_code)] // Phase 1/2 Completer will consume slash_prefix / match_commands

use super::slash_registry::SlashCommand;

/// If the cursor is inside a leading slash-command token, return the
/// prefix string (the part after `/`) and the byte offset of the `/`
/// within `line` (for constructing a reedline `Span`).
///
/// Returns `None` when:
/// - The line is empty or all whitespace.
/// - The first non-whitespace character is NOT `/` (mid-line slash, normal message).
/// - The cursor is past the command token (whitespace was typed).
pub fn slash_prefix(line: &str, cursor: usize) -> Option<(&str, usize)> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.as_bytes()[0] != b'/' {
        return None;
    }

    let leading_ws = line.len() - trimmed.len();
    let slash_byte = leading_ws;

    // The command token runs from slash_byte to the first whitespace (or EOL).
    // If the cursor sits beyond that token, the user is typing arguments.
    let rest = &line[slash_byte..];
    let token_end = rest
        .find(char::is_whitespace)
        .map_or(line.len(), |ws_offset| slash_byte + ws_offset);

    if cursor > token_end {
        return None; // cursor is in the argument region
    }

    let prefix = &line[slash_byte + 1..cursor.min(token_end)];
    Some((prefix, slash_byte))
}

/// Case-insensitive prefix match against the registry.
/// Returns the indices into `registry` of matching commands.
/// Exact matches are sorted first; remaining entries are in name-dictionary order.
pub fn match_commands(prefix: &str, registry: &[SlashCommand]) -> Vec<usize> {
    let lower = prefix.to_lowercase();
    let mut hits: Vec<usize> = (0..registry.len())
        .filter(|&i| {
            let name_lower = registry[i].name[1..].to_lowercase(); // strip leading '/'
            name_lower.starts_with(&lower)
        })
        .collect();

    hits.sort_by_key(|&i| {
        let exact = registry[i].name[1..].eq_ignore_ascii_case(prefix);
        (if exact { 0 } else { 1 }, registry[i].name)
    });

    hits
}

/// Resolve a submitted line (trimmed) to a command index in the registry.
/// Matches the first whitespace-delimited token (lowercased) against
/// `command.name` and all `command.aliases`.
/// Returns `None` for non-matching input (fall through to LLM).
pub fn resolve_dispatch(line: &str, registry: &[SlashCommand]) -> Option<usize> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    let first_token = trimmed
        .split_whitespace()
        .next()
        .unwrap_or(trimmed)
        .to_lowercase();

    // Normalize: strip leading '/' for comparison (both "/exit" and "exit" should match "exit").
    let normalized = first_token.strip_prefix('/').unwrap_or(&first_token);

    registry.iter().position(|cmd| {
        cmd.name[1..].eq_ignore_ascii_case(normalized)
            || cmd
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(first_token.as_str()))
    })
}

#[cfg(test)]
mod tests {
    use super::super::slash_registry::SLASH_COMMANDS;
    use super::*;

    // ── slash_prefix ──────────────────────────────────────────────────

    #[test]
    fn should_open_menu_when_buffer_is_single_slash() {
        let result = slash_prefix("/", 1);
        assert_eq!(result, Some(("", 0)));
    }

    #[test]
    fn should_not_trigger_when_slash_is_mid_line() {
        assert_eq!(slash_prefix("read /etc", 9), None);
    }

    #[test]
    fn should_close_menu_when_cursor_in_args() {
        assert_eq!(slash_prefix("/config ", 8), None);
    }

    #[test]
    fn should_return_prefix_when_partial_command() {
        assert_eq!(slash_prefix("/e", 2), Some(("e", 0)));
    }

    #[test]
    fn should_return_prefix_when_complete_command() {
        assert_eq!(slash_prefix("/exit", 5), Some(("exit", 0)));
    }

    #[test]
    fn should_return_none_when_line_is_empty() {
        assert_eq!(slash_prefix("", 0), None);
    }

    #[test]
    fn should_return_none_when_line_is_whitespace() {
        assert_eq!(slash_prefix("   ", 3), None);
    }

    #[test]
    fn should_handle_leading_whitespace_before_slash() {
        assert_eq!(slash_prefix("  /ex", 5), Some(("ex", 2)));
    }

    #[test]
    fn should_return_none_when_cursor_after_end() {
        assert_eq!(slash_prefix("/", 2), None);
    }

    // ── match_commands ────────────────────────────────────────────────

    #[test]
    fn should_filter_to_exit_when_prefix_is_e() {
        let hits = match_commands("e", SLASH_COMMANDS);
        assert_eq!(hits.len(), 1);
        assert_eq!(SLASH_COMMANDS[hits[0]].name, "/exit");
    }

    #[test]
    fn should_filter_to_config_when_prefix_is_c() {
        let hits = match_commands("c", SLASH_COMMANDS);
        assert_eq!(hits.len(), 1);
        assert_eq!(SLASH_COMMANDS[hits[0]].name, "/config");
    }

    #[test]
    fn should_match_case_insensitively_when_prefix_uppercase() {
        let hits = match_commands("E", SLASH_COMMANDS);
        assert_eq!(hits.len(), 1);
        assert_eq!(SLASH_COMMANDS[hits[0]].name, "/exit");
    }

    #[test]
    fn should_return_empty_when_no_command_matches() {
        assert!(match_commands("a", SLASH_COMMANDS).is_empty());
        assert!(match_commands("zzz", SLASH_COMMANDS).is_empty());
    }

    #[test]
    fn should_return_all_when_prefix_is_empty() {
        let hits = match_commands("", SLASH_COMMANDS);
        assert_eq!(hits.len(), 2);
    }

    // ── resolve_dispatch ──────────────────────────────────────────────

    #[test]
    fn should_resolve_exit_when_line_is_bare_exit() {
        let idx = resolve_dispatch("exit", SLASH_COMMANDS).unwrap();
        assert_eq!(SLASH_COMMANDS[idx].name, "/exit");
    }

    #[test]
    fn should_resolve_exit_when_line_is_bare_quit() {
        let idx = resolve_dispatch("quit", SLASH_COMMANDS).unwrap();
        assert_eq!(SLASH_COMMANDS[idx].name, "/exit");
    }

    #[test]
    fn should_resolve_exit_when_line_is_colon_q() {
        let idx = resolve_dispatch(":q", SLASH_COMMANDS).unwrap();
        assert_eq!(SLASH_COMMANDS[idx].name, "/exit");
    }

    #[test]
    fn should_resolve_exit_when_line_is_uppercase_QUIT() {
        let idx = resolve_dispatch("QUIT", SLASH_COMMANDS).unwrap();
        assert_eq!(SLASH_COMMANDS[idx].name, "/exit");
    }

    #[test]
    fn should_resolve_exit_when_line_is_slash_exit() {
        let idx = resolve_dispatch("/exit", SLASH_COMMANDS).unwrap();
        assert_eq!(SLASH_COMMANDS[idx].name, "/exit");
    }

    #[test]
    fn should_resolve_config_when_line_is_slash_config() {
        let idx = resolve_dispatch("/config", SLASH_COMMANDS).unwrap();
        assert_eq!(SLASH_COMMANDS[idx].name, "/config");
    }

    #[test]
    fn should_ignore_trailing_text_for_dispatch() {
        let idx = resolve_dispatch("exit extra stuff", SLASH_COMMANDS).unwrap();
        assert_eq!(SLASH_COMMANDS[idx].name, "/exit");
    }

    #[test]
    fn should_return_none_when_no_match() {
        assert!(resolve_dispatch("hello", SLASH_COMMANDS).is_none());
        assert!(resolve_dispatch("/unknown", SLASH_COMMANDS).is_none());
    }

    #[test]
    fn should_return_none_when_line_is_blank() {
        assert!(resolve_dispatch("", SLASH_COMMANDS).is_none());
        assert!(resolve_dispatch("  ", SLASH_COMMANDS).is_none());
    }
}
