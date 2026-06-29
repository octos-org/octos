//! Reedline assembly for the `octos chat` REPL with slash-command menu.
//! Uses Hinter (inline hint) for zero-clutter suggestion display;
//! ColumnarMenu only activates on explicit Tab completion.

use std::borrow::Cow;
use std::path::Path;

use eyre::{Result, WrapErr};
use reedline::{
    default_emacs_keybindings, ColumnarMenu, Emacs, FileBackedHistory, Hinter, History,
    KeyCode, KeyModifiers, MenuBuilder, Prompt, PromptEditMode, PromptHistorySearch, Reedline,
    ReedlineEvent, ReedlineMenu, Signal,
};

use super::slash_completer::SlashCompleter;
use super::slash_filter::{match_commands, slash_prefix};
use super::slash_registry::SLASH_COMMANDS;

// ── Plain prompt: no ">" or mode-status symbols ─────────────────────

struct PlainPrompt {
    text: String,
}

impl Prompt for PlainPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.text)
    }
    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_indicator(&self, _mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_history_search_indicator(
        &self,
        _history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
}

// ── Hinter: inline suggestions, disappears on no-match ──────────────

struct SlashHinter {
    last_matches: Vec<usize>,
}

impl Hinter for SlashHinter {
    fn handle(
        &mut self,
        line: &str,
        pos: usize,
        _history: &dyn History,
        _use_ansi_coloring: bool,
        _cwd: &str,
    ) -> String {
        let prefix = match slash_prefix(line, pos) {
            Some((p, _)) => p,
            None => {
                self.last_matches.clear();
                return String::new();
            }
        };

        let indices = match_commands(prefix, SLASH_COMMANDS);
        if indices.is_empty() {
            self.last_matches.clear();
            return String::new();
        }

        self.last_matches = indices;
        let names: Vec<&str> = self
            .last_matches
            .iter()
            .map(|&i| SLASH_COMMANDS[i].name)
            .collect();
        format!("  {}", names.join("  "))
    }

    fn complete_hint(&self) -> String {
        self.last_matches
            .first()
            .map(|&i| SLASH_COMMANDS[i].name.to_string())
            .unwrap_or_default()
    }

    fn next_hint_token(&self) -> String {
        self.complete_hint()
    }
}

// ── Public builder ──────────────────────────────────────────────────

pub struct SlashPrompt {
    editor: Reedline,
}

impl SlashPrompt {
    pub fn new(history_path: &Path) -> Result<Self> {
        let completer = Box::new(SlashCompleter);
        let hinter = Box::new(SlashHinter {
            last_matches: Vec::new(),
        });

        let menu = ReedlineMenu::EngineCompleter(Box::new(
            ColumnarMenu::default().with_name("slash_menu"),
        ));

        // Tab activates the menu for arrow-key navigation; no auto-open on `/`.
        let mut keybindings = default_emacs_keybindings();
        keybindings.add_binding(
            KeyModifiers::NONE,
            KeyCode::Tab,
            ReedlineEvent::Multiple(vec![
                ReedlineEvent::Menu("slash_menu".to_string()),
            ]),
        );

        let edit_mode = Box::new(Emacs::new(keybindings));
        let history = Box::new(
            FileBackedHistory::with_file(256, history_path.to_path_buf())
                .wrap_err("failed to open reedline history")?,
        );

        let editor = Reedline::create()
            .with_completer(completer)
            .with_hinter(hinter)
            .with_menu(menu)
            .with_edit_mode(edit_mode)
            .with_history(history);

        Ok(Self { editor })
    }

    pub fn read_line(&mut self, prompt: &str) -> std::io::Result<Option<String>> {
        let prompt = PlainPrompt {
            text: prompt.to_string(),
        };
        let signal = self
            .editor
            .read_line(&prompt)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        match signal {
            Signal::Success(line) => Ok(Some(line)),
            Signal::CtrlC | Signal::CtrlD => Ok(None),
        }
    }

    pub fn save_history(&mut self) -> Result<()> {
        self.editor
            .sync_history()
            .wrap_err("failed to sync reedline history")?;
        Ok(())
    }
}
