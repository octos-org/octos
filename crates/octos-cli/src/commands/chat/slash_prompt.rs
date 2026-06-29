//! Reedline assembly for the `octos chat` REPL with slash-command menu.
//! Uses a plain prompt (no status-indicator symbols) and reedline's
//! built-in ColumnarMenu.  Empty-menu placeholder is a known reedline
//! restriction (ColumnarMenu hardcodes "NO RECORDS FOUND" internally).

use std::borrow::Cow;
use std::path::Path;

use eyre::{Result, WrapErr};
use reedline::{
    default_emacs_keybindings, ColumnarMenu, EditCommand, Emacs, FileBackedHistory, KeyCode,
    KeyModifiers, MenuBuilder, Prompt, PromptEditMode, PromptHistorySearch, Reedline,
    ReedlineEvent, ReedlineMenu, Signal,
};

use super::slash_completer::SlashCompleter;

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

// ── Public builder ──────────────────────────────────────────────────

pub struct SlashPrompt {
    editor: Reedline,
}

impl SlashPrompt {
    pub fn new(history_path: &Path) -> Result<Self> {
        let completer = Box::new(SlashCompleter);
        let menu = ReedlineMenu::EngineCompleter(Box::new(
            ColumnarMenu::default().with_name("slash_menu"),
        ));

        let mut keybindings = default_emacs_keybindings();
        keybindings.add_binding(
            KeyModifiers::NONE,
            KeyCode::Char('/'),
            ReedlineEvent::Multiple(vec![
                ReedlineEvent::Edit(vec![EditCommand::InsertChar('/')]),
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
            .with_menu(menu)
            .with_edit_mode(edit_mode)
            .with_history(history)
            .with_quick_completions(true)
            .with_partial_completions(true);

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
