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
    /// Number of lines the menu occupied on the last render.
    prev_lines: usize,
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
            if self.selected == 0 {
                self.selected = self.matches.len() - 1;
            } else {
                self.selected -= 1;
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
            let l = line.to_string();
            self.entries.retain(|e| e != &l);
            self.entries.push(l);
        }
        self.cursor = None;
    }

    fn prev(&mut self) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }
        let idx = self.cursor.map_or(self.entries.len().saturating_sub(1), |c| c.saturating_sub(1));
        let idx = idx.min(self.entries.len().saturating_sub(1));
        self.cursor = Some(idx);
        self.entries.get(idx).map(|s| s.as_str())
    }

    fn next(&mut self) -> Option<&str> {
        match self.cursor {
            Some(c) if c + 1 < self.entries.len() => {
                self.cursor = Some(c + 1);
                self.entries.get(c + 1).map(|s| s.as_str())
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
        queue!(stdout, Print(prompt))?;
        stdout.flush()?;

        let mut buffer = String::new();
        let mut cursor: usize = 0;
        let mut menu = MenuState::default();
        let mut history_nav_buffer: Option<String> = None;

        let result: io::Result<Option<String>> = (|| {
            loop {
                let has_menu = menu.active && !menu.matches.is_empty();
                let menu_count = if has_menu { menu.matches.len() } else { 0 };

                // --- clear old menu area ---
                if menu.prev_lines > 0 && menu_count < menu.prev_lines {
                    // Move cursor down to where old menu started, clear from there.
                    for _ in 0..menu.prev_lines {
                        queue!(stdout, Print("\r\n"), Clear(ClearType::CurrentLine))?;
                    }
                    // Move back up.
                    queue!(stdout, cursor::MoveUp(menu.prev_lines as u16))?;
                }
                menu.prev_lines = menu_count;

                // --- re-render prompt + buffer ---
                queue!(
                    stdout,
                    SavePosition,
                    cursor::MoveToColumn(0),
                    Clear(ClearType::CurrentLine),
                    Print(prompt),
                    SetAttribute(Attribute::Reset),
                )?;

                if cursor == buffer.len() {
                    queue!(stdout, Print(&buffer))?;
                } else {
                    queue!(stdout, Print(&buffer[..cursor]))?;
                    queue!(stdout, SavePosition)?;
                    queue!(stdout, Print(&buffer[cursor..]))?;
                    queue!(stdout, RestorePosition)?;
                }

                // --- render menu ---
                if has_menu {
                    queue!(stdout, Print("\r\n"))?;
                    for (i, &idx) in menu.matches.iter().enumerate() {
                        let cmd = &SLASH_COMMANDS[idx];
                        let sel = i == menu.selected;
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
                        queue!(stdout, Print(format!("  {}  {}", cmd.name, cmd.description)))?;
                        queue!(stdout, SetAttribute(Attribute::Reset), Print("\r\n"))?;
                    }
                }

                // --- restore cursor ---
                queue!(stdout, RestorePosition)?;
                let target_col = prompt.len() + if cursor < buffer.len() { cursor } else { buffer.len() };
                queue!(stdout, cursor::MoveToColumn(target_col as u16))?;
                stdout.flush()?;

                // --- read keystroke ---
                let ev = event::read()?;
                match ev {
                    Event::Key(KeyEvent { code, modifiers: KeyModifiers::NONE, kind: KeyEventKind::Press, .. })
                    | Event::Key(KeyEvent { code, modifiers: KeyModifiers::SHIFT, kind: KeyEventKind::Press, .. }) =>
                    {
                        match code {
                            KeyCode::Char(c) => {
                                history_nav_buffer = None;
                                buffer.insert(cursor, c);
                                cursor += 1;
                                if buffer.trim_start().starts_with('/') {
                                    if let Some((prefix, _)) = slash_prefix(&buffer, cursor) {
                                        if menu.active {
                                            menu.update(prefix);
                                        } else {
                                            menu.open(prefix);
                                        }
                                    } else {
                                        menu.close();
                                    }
                                } else {
                                    menu.close();
                                }
                            }
                            KeyCode::Backspace => {
                                history_nav_buffer = None;
                                if cursor > 0 {
                                    buffer.remove(cursor - 1);
                                    cursor -= 1;
                                }
                                if buffer.trim_start().starts_with('/') {
                                    if let Some((prefix, _)) = slash_prefix(&buffer, cursor) {
                                        menu.update(prefix);
                                    } else {
                                        menu.close();
                                    }
                                } else {
                                    menu.close();
                                }
                            }
                            KeyCode::Delete => {
                                history_nav_buffer = None;
                                if cursor < buffer.len() {
                                    buffer.remove(cursor);
                                }
                                if buffer.trim_start().starts_with('/') {
                                    if let Some((prefix, _)) = slash_prefix(&buffer, cursor) {
                                        menu.update(prefix);
                                    } else {
                                        menu.close();
                                    }
                                } else {
                                    menu.close();
                                }
                            }
                            KeyCode::Enter => {
                                history_nav_buffer = None;
                                // Accept selected command if menu is open.
                                if menu.active && !menu.matches.is_empty() {
                                    if let Some(selected) = menu.selected_command() {
                                        let cmd = SLASH_COMMANDS
                                            .iter()
                                            .find(|c| c.name == selected)
                                            .unwrap();
                                        if cmd.kind == CommandKind::Immediate {
                                            buffer = selected.to_string();
                                            cursor = buffer.len();
                                            menu.close();
                                        } else if cmd.kind == CommandKind::TakesArgs {
                                            buffer = selected.to_string();
                                            buffer.push(' ');
                                            cursor = buffer.len();
                                            menu.close();
                                        }
                                    }
                                }
                                // Clear menu area below.
                                for _ in 0..menu.prev_lines {
                                    queue!(stdout, Print("\r\n"), Clear(ClearType::CurrentLine))?;
                                }
                                // Move back to the prompt line and finalize.
                                if menu.prev_lines > 0 {
                                    queue!(stdout, cursor::MoveToColumn(0))?;
                                    queue!(stdout, Clear(ClearType::CurrentLine))?;
                                    queue!(stdout, Print(prompt), Print(&buffer))?;
                                }
                                queue!(stdout, Print("\r\n"))?;
                                stdout.flush()?;
                                disable_raw_mode()?;
                                return Ok(Some(buffer));
                            }
                            KeyCode::Esc => {
                                history_nav_buffer = None;
                                menu.close();
                            }
                            KeyCode::Tab => {
                                history_nav_buffer = None;
                                if menu.active && !menu.matches.is_empty() {
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
                                        // Keep menu open for further typing/navigation if TakesArgs.
                                        if cmd.kind == CommandKind::Immediate {
                                            menu.close();
                                        }
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
                                    if let Some(entry) = self.history.prev() {
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
                                    if let Some(entry) = self.history.next() {
                                        buffer = entry.to_string();
                                        cursor = buffer.len();
                                        menu.close();
                                    } else if let Some(saved) = history_nav_buffer.take() {
                                        buffer.clone_from(&saved);
                                        cursor = buffer.len();
                                    }
                                }
                            }
                            KeyCode::Left => { if cursor > 0 { cursor -= 1; } }
                            KeyCode::Right => { if cursor < buffer.len() { cursor += 1; } }
                            KeyCode::Home => cursor = 0,
                            KeyCode::End => cursor = buffer.len(),
                            _ => {}
                        }
                    }
                    Event::Key(KeyEvent { code: KeyCode::Char('c'), modifiers: KeyModifiers::CONTROL, .. }) => {
                        if menu.active {
                            menu.close();
                        } else {
                            // Clear menu area before exiting.
                            for _ in 0..menu.prev_lines {
                                queue!(stdout, Print("\r\n"), Clear(ClearType::CurrentLine))?;
                            }
                            queue!(stdout, Print("^C\r\n"))?;
                            stdout.flush()?;
                            disable_raw_mode()?;
                            return Ok(None);
                        }
                    }
                    Event::Key(KeyEvent { code: KeyCode::Char('d'), modifiers: KeyModifiers::CONTROL, .. }) => {
                        if buffer.is_empty() {
                            for _ in 0..menu.prev_lines {
                                queue!(stdout, Print("\r\n"), Clear(ClearType::CurrentLine))?;
                            }
                            stdout.flush()?;
                            disable_raw_mode()?;
                            return Ok(None);
                        }
                    }
                    Event::Resize(..) => {}
                    _ => {}
                }
            }
        })();

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
