//! reedline Completer adapter for the slash-command menu.
//! Thin wrapper over the pure functions in [`super::slash_filter`].

use reedline::{Completer, Span, Suggestion};
use super::slash_filter::{match_commands, slash_prefix};
use super::slash_registry::CommandKind;

/// Completer that provides slash-command suggestions when the cursor is
/// inside a leading command token (starts with `/`).
#[allow(dead_code)] // Phase 3 crossterm prompt does not use reedline completer directly
pub struct SlashCompleter;

impl Completer for SlashCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let (prefix, slash_start) = match slash_prefix(line, pos) {
            Some(x) => x,
            None => return vec![],
        };

        let indices = match_commands(prefix, super::slash_registry::SLASH_COMMANDS);

        indices
            .into_iter()
            .map(|i| {
                let cmd = &super::slash_registry::SLASH_COMMANDS[i];
                Suggestion {
                    value: cmd.name.to_string(),
                    description: Some(cmd.description.to_string()),
                    span: Span::new(slash_start, pos),
                    append_whitespace: matches!(
                        cmd.kind,
                        CommandKind::TakesArgs | CommandKind::HasSubcommands
                    ),
                    ..Suggestion::default()
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete_at(line: &str, pos: usize) -> Vec<Suggestion> {
        let mut c = SlashCompleter;
        c.complete(line, pos)
    }

    #[test]
    fn should_not_emit_suggestions_when_completing_mid_line_slash() {
        assert!(complete_at("read /x", 7).is_empty());
    }

    #[test]
    fn should_emit_append_whitespace_when_command_takes_args() {
        let suggestions = complete_at("/config", 7);
        assert_eq!(suggestions.len(), 1);
        assert!(suggestions[0].append_whitespace);
        assert_eq!(suggestions[0].value, "/config");
    }

    #[test]
    fn should_not_append_whitespace_when_command_is_immediate() {
        let suggestions = complete_at("/exit", 5);
        assert_eq!(suggestions.len(), 1);
        assert!(!suggestions[0].append_whitespace);
        assert_eq!(suggestions[0].value, "/exit");
    }

    #[test]
    fn should_return_suggestions_when_single_slash() {
        let suggestions = complete_at("/", 1);
        assert_eq!(suggestions.len(), 2);
    }

    #[test]
    fn should_filter_suggestions_when_partial() {
        let suggestions = complete_at("/e", 2);
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].value, "/exit");
    }

    #[test]
    fn should_return_empty_when_no_match() {
        assert!(complete_at("/a", 2).is_empty());
    }

    #[test]
    fn should_return_empty_when_cursor_in_args() {
        assert!(complete_at("/config ", 8).is_empty());
    }

    #[test]
    fn should_compute_correct_span() {
        let suggestions = complete_at("/e", 2);
        assert_eq!(suggestions[0].span.start, 0);
        assert_eq!(suggestions[0].span.end, 2);
    }

    #[test]
    fn should_compute_correct_span_with_leading_whitespace() {
        let suggestions = complete_at("  /e", 4);
        assert_eq!(suggestions[0].span.start, 2);
        assert_eq!(suggestions[0].span.end, 4);
    }
}
