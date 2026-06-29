//! Reedline assembly for the `octos chat` REPL with slash-command menu.
//! Wraps reedline setup (editor, keybindings, history, completer, menu)
//! into a single read-line call that mirrors the current rustyline loop.

use std::path::Path;

use eyre::{Result, WrapErr};
use reedline::{
    default_emacs_keybindings, ColumnarMenu, DefaultPrompt, DefaultPromptSegment, EditCommand,
    Emacs, FileBackedHistory, KeyCode, KeyModifiers, MenuBuilder, Reedline, ReedlineEvent,
    ReedlineMenu, Signal,
};

use super::slash_completer::SlashCompleter;

/// Builder for a reedline editor configured with slash-command menu support.
pub struct SlashPrompt {
    editor: Reedline,
}

impl SlashPrompt {
    /// Build a reedline editor wired with the slash-command completer,
    /// a columnar menu, and custom keybinding for auto-open on `/`.
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

    /// Read one line from the user. Returns the line or `None` on exit/interrupt.
    pub fn read_line(&mut self, prompt: &str) -> std::io::Result<Option<String>> {
        let prompt = DefaultPrompt::new(
            DefaultPromptSegment::Basic(prompt.to_string()),
            DefaultPromptSegment::Basic("".to_string()),
        );

        let signal = self
            .editor
            .read_line(&prompt)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        match signal {
            Signal::Success(line) => Ok(Some(line)),
            Signal::CtrlC | Signal::CtrlD => Ok(None),
        }
    }

    /// Persist history to disk.
    pub fn save_history(&mut self) -> Result<()> {
        self.editor
            .sync_history()
            .wrap_err("failed to sync reedline history")?;
        Ok(())
    }
}
