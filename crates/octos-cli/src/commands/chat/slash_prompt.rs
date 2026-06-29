//! Custom crossterm-based line editor with slash-command menu.
//! Full control over rendering: dropdown auto-opens on `/`, hides completely
//! when no commands match.  History navigation with ↑/↓ when menu is closed.

use std::io::{self, Write};
use std::path::Path;

use crossterm::{
    cursor::{self, RestorePosition, SavePosition},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    queue,
    style::{Attribute, Print, SetAttribute},
    terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode},
};
use eyre::Result;

use super::slash_filter::{match_commands, slash_prefix};
use super::slash_registry::{CommandKind, SLASH_COMMANDS};

// ── Menu state ──────────────────────────────────────────────────────

#[derive(Default)]
struct MenuState {
    active: bool,
    matches: Vec<usize>,
    selected: usize,
}

impl MenuState {
    fn update(&mut self, prefix: &str) {
        self.matches = match_commands(prefix, SLASH_COMMANDS);
        if self.matches.is_empty() {
            self.active = false;
            self.selected = 0;
        } else if !self.active {
            self.active = true;
            self.selected = 0;
        }
        if self.selected >= self.matches.len().saturating_sub(1) {
            self.selected = self.matches.len().saturating_sub(1);
        }
    }

    fn open(&mut self, prefix: &str) {
        self.active = true;
        self.selected = 0;
        self.update(prefix);
    }

    fn close(&mut self) {
        self.active = false;
        self.matches.clear();
        self.selected = 0;
    }

    fn next(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1) % self.matches.len();
        }
    }

    fn prev(&mut self) {
        if !self.matches.is_empty() {
            self.selected = self.selected.saturating_sub(1);
            if self.selected >= self.matches.len() {
                self.selected = self.matches.len() - 1;
            }
        }
    }

    fn selected_command(&self) -> Option<&'static str> {
        self.matches
            .get(self.selected)
            .map(|&i| SLASH_COMMANDS[i].name)
    }
}

// ── History ─────────────────────────────────────────────────────────

struct History {
    entries: Vec<String>,
    path: std::path::PathBuf,
    cursor: Option<usize>,
}

impl History {
    fn load(path: &Path) -> Self {
        let entries = std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(String::from)
            .collect();
        Self { entries, path: path.to_path_buf(), cursor: None }
    }

    fn save(&self) {
        if let Ok(mut f) = std::fs::File::create(&self.path) {
            for entry in &self.entries {
                let _ = writeln!(f, "{entry}");
            }
        }
    }

    fn add(&mut self, line: &str) {
        if !line.trim().is_empty() {
            // Dedup: remove previous identical entry then push to front.
            let l = line.to_string();
            self.entries.retain(|e| e != &l);
            self.entries.push(l);
        }
        self.cursor = None;
    }

    fn prev(&mut self, _current: &str) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }
        let idx = self.cursor.map_or(self.entries.len() - 1, |c| c.saturating_sub(1));
        // Clamp to valid range.
        let idx = idx.min(self.entries.len() - 1);
        self.cursor = Some(idx);
        // If cursor is 0 and we'd go negative, return current unchanged.
        if let Some(c) = self.cursor {
            if c < self.entries.len() {
                return Some(&self.entries[c]);
            }
        }
        None
    }

    fn next(&mut self, _current: &str) -> Option<&str> {
        match self.cursor {
            Some(c) if c + 1 < self.entries.len() => {
                self.cursor = Some(c + 1);
                Some(&self.entries[c + 1])
            }
            _ => {
                self.cursor = None;
                None
            }
        }
    }
}

// ── Prompt ──────────────────────────────────────────────────────────

pub struct SlashPrompt {
    history: History,
}

impl SlashPrompt {
    pub fn new(history_path: &Path) -> Result<Self> {
        Ok(Self { history: History::load(history_path) })
    }

    pub fn read_line(&mut self, prompt: &str) -> io::Result<Option<String>> {
        let mut stdout = io::stdout();
        enable_raw_mode()?;

        // Queue the initial prompt.
        queue!(stdout, Print(prompt))?;
        stdout.flush()?;

        let mut buffer = String::new();
        let mut cursor: usize = 0;
        let mut menu = MenuState::default();
        let mut history_nav_buffer: Option<String> = None;
        let result: io::Result<Option<String>> = (|| {
            loop {
                // Re-render.
                queue!(
                    stdout,
                    SavePosition,
                    cursor::MoveToColumn(0),
                    Clear(ClearType::CurrentLine),
                    Print(prompt),
                    SetAttribute(Attribute::Reset),
                )?;

                // Print buffer with cursor.
                if cursor == buffer.len() {
                    queue!(stdout, Print(&buffer))?;
                } else {
                    queue!(stdout, Print(&buffer[..cursor]))?;
                    queue!(stdout, SavePosition)?;
                    queue!(stdout, Print(&buffer[cursor..]))?;
                    queue!(stdout, RestorePosition)?;
                }

                // Print menu if active.
                let menu_lines = if menu.active && !menu.matches.is_empty() {
                    let mut lines = Vec::new();
                    for (i, &idx) in menu.matches.iter().enumerate() {
                        let cmd = &SLASH_COMMANDS[idx];
                        let sel = i == menu.selected;
                        lines.push((cmd.name, cmd.description, sel));
                    }
                    Some(lines)
                } else {
                    None
                };

                if let Some(ref lines) = menu_lines {
                    queue!(stdout, Print("\r\n"))?;
                    for &(name, desc, sel) in lines {
                        queue!(
                            stdout,
                            Clear(ClearType::CurrentLine),
                            SetAttribute(Attribute::Reset),
                        )?;
                        if sel {
                            queue!(stdout, SetAttribute(Attribute::Reverse))?;
                        } else {
                            queue!(stdout, SetAttribute(Attribute::Dim))?;
                        }
                        queue!(stdout, Print(format!("  {name}  {desc}")))?;
                        queue!(stdout, SetAttribute(Attribute::Reset))?;
                        queue!(stdout, Print("\r\n"))?;
                    }
                }

                // Restore cursor to after the buffer.
                queue!(stdout, RestorePosition)?;
                if cursor < buffer.len() {
                    // We saved position mid-buffer; move cursor back there.
                    queue!(stdout, cursor::MoveToColumn(
                        (prompt.len() + cursor) as u16
                    ))?;
                } else {
                    queue!(stdout, cursor::MoveToColumn(
                        (prompt.len() + buffer.len()) as u16
                    ))?;
                }
                stdout.flush()?;

                // Read keystroke.
                let ev = event::read()?;
                match ev {
                    Event::Key(KeyEvent { code, modifiers: KeyModifiers::NONE, kind: KeyEventKind::Press, .. })
                    | Event::Key(KeyEvent { code, modifiers: KeyModifiers::SHIFT, kind: KeyEventKind::Press, .. }) =>
                    {
                        match code {
                            KeyCode::Char(c) => {
                                history_nav_buffer = None;
                                if menu.active {
                                    // In menu mode, typing filters the command.
                                    buffer.insert(cursor, c);
                                    cursor += 1;
                                    let prefix = slash_prefix(&buffer, cursor)
                                        .map(|(p, _)| p)
                                        .unwrap_or("");
                                    menu.update(prefix);
                                } else {
                                    buffer.insert(cursor, c);
                                    cursor += 1;
                                    // Auto-open menu if first char is '/'.
                                    if buffer.trim_start().starts_with('/') {
                                        let prefix = slash_prefix(&buffer, cursor)
                                            .map(|(p, _)| p)
                                            .unwrap_or("");
                                        menu.open(prefix);
                                    }
                                }
                            }
                            KeyCode::Backspace => {
                                history_nav_buffer = None;
                                if cursor > 0 {
                                    buffer.remove(cursor - 1);
                                    cursor -= 1;
                                    if menu.active {
                                        let prefix = slash_prefix(&buffer, cursor)
                                            .map(|(p, _)| p)
                                            .unwrap_or("");
                                        menu.update(prefix);
                                    }
                                    // Close menu if slash removed from start.
                                    if buffer.trim_start().is_empty()
                                        || !buffer.trim_start().starts_with('/')
                                    {
                                        menu.close();
                                    }
                                }
                            }
                            KeyCode::Delete => {
                                history_nav_buffer = None;
                                if cursor < buffer.len() {
                                    buffer.remove(cursor);
                                    if menu.active {
                                        let prefix = slash_prefix(&buffer, cursor)
                                            .map(|(p, _)| p)
                                            .unwrap_or("");
                                        menu.update(prefix);
                                    }
                                }
                            }
                            KeyCode::Enter => {
                                history_nav_buffer = None;
                                if menu.active && !menu.matches.is_empty() {
                                    // Accept selected command.
                                    if let Some(selected) = menu.selected_command() {
                                        let cmd = SLASH_COMMANDS
                                            .iter()
                                            .find(|c| c.name == selected)
                                            .unwrap();
                                        buffer = selected.to_string();
                                        cursor = buffer.len();
                                        menu.close();
                                        if cmd.kind == CommandKind::TakesArgs {
                                            buffer.push(' ');
                                            cursor += 1;
                                        }
                                        // Re-render before potentially submitting.
                                        queue!(stdout, SavePosition)?;
                                        queue!(stdout, cursor::MoveToColumn(0))?;
                                        queue!(stdout, Clear(ClearType::CurrentLine))?;
                                        queue!(stdout, Print(prompt))?;
                                        queue!(stdout, Print(&buffer))?;
                                        queue!(stdout, RestorePosition)?;
                                        stdout.flush()?;
                                        if cmd.kind == CommandKind::Immediate {
                                            // For exit, submit immediately.
                                        }
                                    }
                                }
                                // Clear menu area.
                                queue!(stdout, Print("\r\n"))?;
                                // Submit (break loop).
                                disable_raw_mode()?;
                                return Ok(Some(buffer));
                            }
                            KeyCode::Esc => {
                                if menu.active {
                                    menu.close();
                                }
                            }
                            KeyCode::Tab => {
                                history_nav_buffer = None;
                                if menu.active && !menu.matches.is_empty() {
                                    // Complete to first match.
                                    if let Some(selected) = menu.selected_command() {
                                        let cmd = SLASH_COMMANDS
                                            .iter()
                                            .find(|c| c.name == selected)
                                            .unwrap();
                                        buffer = selected.to_string();
                                        cursor = buffer.len();
                                        if cmd.kind == CommandKind::TakesArgs {
                                            buffer.push(' ');
                                            cursor += 1;
                                        }
                                        menu.close();
                                    }
                                }
                            }
                            KeyCode::Up => {
                                if menu.active {
                                    menu.prev();
                                } else {
                                    if history_nav_buffer.is_none() {
                                        history_nav_buffer = Some(buffer.clone());
                                    }
                                    if let Some(entry) = self.history.prev(&buffer) {
                                        buffer = entry.to_string();
                                        cursor = buffer.len();
                                        menu.close();
                                    }
                                }
                            }
                            KeyCode::Down => {
                                if menu.active {
                                    menu.next();
                                } else {
                                    if let Some(entry) = self.history.next(&buffer) {
                                        buffer = entry.to_string();
                                        cursor = buffer.len();
                                        menu.close();
                                    } else if let Some(saved) = history_nav_buffer.take() {
                                        buffer.clone_from(&saved);
                                        cursor = buffer.len();
                                    }
                                }
                            }
                            KeyCode::Left => {
                                if cursor > 0 {
                                    cursor -= 1;
                                }
                            }
                            KeyCode::Right => {
                                if cursor < buffer.len() {
                                    cursor += 1;
                                }
                            }
                            KeyCode::Home => cursor = 0,
                            KeyCode::End => cursor = buffer.len(),
                            _ => {}
                        }
                    }
                    Event::Key(KeyEvent { code: KeyCode::Char('c'), modifiers: KeyModifiers::CONTROL, .. }) => {
                        if menu.active {
                            menu.close();
                        } else {
                            disable_raw_mode()?;
                            queue!(stdout, Print("^C\r\n"))?;
                            stdout.flush()?;
                            return Ok(None);
                        }
                    }
                    Event::Key(KeyEvent { code: KeyCode::Char('d'), modifiers: KeyModifiers::CONTROL, .. }) => {
                        if buffer.is_empty() {
                            disable_raw_mode()?;
                            return Ok(None);
                        }
                    }
                    Event::Resize(..) => {}
                    _ => {}
                }
            }
        })();

        // Ensure raw mode is disabled on all exit paths.
        let _ = disable_raw_mode();
        result
    }

    pub fn save_history(&mut self) -> Result<()> {
        self.history.save();
        Ok(())
    }

    pub fn add_history(&mut self, line: &str) {
        self.history.add(line);
    }
}
