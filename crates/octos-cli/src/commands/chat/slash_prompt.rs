//! Reedline assembly for the `octos chat` REPL with slash-command menu.
//! Custom Menu: vertical dropdown with ↑/↓ navigation; completely hidden
//! when there are no completions (no "NO RECORDS FOUND" placeholder).
//! Plain prompt removes status-indicator symbols.

use std::borrow::Cow;
use std::cell::Cell;
use std::path::Path;

use eyre::{Result, WrapErr};
use reedline::{
    default_emacs_keybindings, Completer, EditCommand, Editor, Emacs, FileBackedHistory, KeyCode,
    KeyModifiers, Menu, MenuEvent, Painter, Prompt, PromptEditMode, PromptHistorySearch, Reedline,
    ReedlineEvent, ReedlineMenu, Signal, Suggestion, UndoBehavior,
};

use super::slash_completer::SlashCompleter;
use super::slash_filter::slash_prefix;

// ── Plain prompt: no ">" or mode-status symbols ─────────────────────

struct PlainPrompt {
    text: String,
}

impl Prompt for PlainPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> { Cow::Borrowed(&self.text) }
    fn render_prompt_right(&self) -> Cow<'_, str> { Cow::Borrowed("") }
    fn render_prompt_indicator(&self, _: PromptEditMode) -> Cow<'_, str> { Cow::Borrowed("") }
    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> { Cow::Borrowed("") }
    fn render_prompt_history_search_indicator(&self, _: PromptHistorySearch) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
}

// ── Custom Menu: auto-hides when empty ──────────────────────────────

struct SlashMenu {
    active: bool,
    values: Vec<Suggestion>,
    selected: usize,
}

impl SlashMenu {
    fn new() -> Self {
        Self { active: false, values: Vec::new(), selected: 0 }
    }
}

impl Menu for SlashMenu {
    // Override `name` / `indicator` so `settings()` (which panics by
    // default) is never invoked — we don't implement `MenuBuilder` and
    // reedline's runtime never calls `settings()` either.
    fn name(&self) -> &str { "slash_menu" }
    fn indicator(&self) -> &str { "" }

    fn is_active(&self) -> bool {
        self.active
    }

    fn menu_event(&mut self, event: MenuEvent) {
        match event {
            MenuEvent::Activate(..) => self.active = true,
            MenuEvent::Deactivate => {
                self.active = false;
                self.values.clear();
                self.selected = 0;
            }
            MenuEvent::NextElement => {
                if !self.values.is_empty() {
                    self.selected = (self.selected + 1).min(self.values.len() - 1);
                }
            }
            MenuEvent::PreviousElement => {
                self.selected = self.selected.saturating_sub(1);
            }
            _ => {}
        }
    }

    fn can_quick_complete(&self) -> bool { true }

    fn can_partially_complete(
        &mut self, _values_updated: bool, _editor: &mut Editor, _completer: &mut dyn Completer,
    ) -> bool { true }

    fn update_values(&mut self, editor: &mut Editor, completer: &mut dyn Completer) {
        let line = editor.get_buffer().to_string();
        let cursor = Cell::new(line.len());
        editor.edit_buffer(|buf| cursor.set(buf.insertion_point()), UndoBehavior::MoveCursor);
        let pos = cursor.get();

        self.values = completer.complete(&line, pos);

        // Auto-close when cursor leaves the slash-command token.
        if slash_prefix(&line, pos).is_none() {
            self.active = false;
        }

        if self.selected >= self.values.len().saturating_sub(1) {
            self.selected = self.values.len().saturating_sub(1);
        }
    }

    fn update_working_details(
        &mut self, _editor: &mut Editor, _completer: &mut dyn Completer, _painter: &Painter,
    ) {}

    fn replace_in_buffer(&self, editor: &mut Editor) {
        if let Some(val) = self.values.get(self.selected) {
            let replacement = val.value.clone();
            editor.edit_buffer(
                |buf| {
                    let end = buf.get_buffer().len();
                    buf.replace_range(..end, &replacement);
                },
                UndoBehavior::CreateUndoPoint,
            );
        }
    }

    fn menu_required_lines(&self, _terminal_columns: u16) -> u16 {
        if !self.active || self.values.is_empty() { 0 } else { self.values.len() as u16 }
    }

    fn menu_string(&self, _available_lines: u16, _use_ansi_coloring: bool) -> String {
        if !self.active || self.values.is_empty() {
            return String::new();
        }
        let mut out = String::new();
        for (i, val) in self.values.iter().enumerate() {
            if i == self.selected {
                out.push_str(&format!(" \x1b[7m {}  {}\x1b[0m\n", val.value, val.description.as_deref().unwrap_or("")));
            } else {
                out.push_str(&format!("   \x1b[2m{}\x1b[0m  {}\n", val.value, val.description.as_deref().unwrap_or("")));
            }
        }
        out
    }

    fn min_rows(&self) -> u16 {
        if !self.active || self.values.is_empty() { 0 } else { self.values.len() as u16 }
    }

    fn get_values(&self) -> &[Suggestion] { &self.values }
}

// ── Public builder ──────────────────────────────────────────────────

pub struct SlashPrompt {
    editor: Reedline,
}

impl SlashPrompt {
    pub fn new(history_path: &Path) -> Result<Self> {
        let completer = Box::new(SlashCompleter);
        let menu = ReedlineMenu::EngineCompleter(Box::new(SlashMenu::new()));

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
            .with_history(history);

        Ok(Self { editor })
    }

    pub fn read_line(&mut self, prompt: &str) -> std::io::Result<Option<String>> {
        let prompt = PlainPrompt { text: prompt.to_string() };
        let signal = self.editor.read_line(&prompt)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        match signal {
            Signal::Success(line) => Ok(Some(line)),
            Signal::CtrlC | Signal::CtrlD => Ok(None),
        }
    }

    pub fn save_history(&mut self) -> Result<()> {
        self.editor.sync_history().wrap_err("failed to sync reedline history")?;
        Ok(())
    }
}
