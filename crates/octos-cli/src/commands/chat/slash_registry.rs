//! Slash-command registry — single source of truth for command metadata,
//! shared by menu rendering (Phase 2) and dispatch (Phase 0).

/// What happens after the user accepts / submits a slash command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
    /// No arguments — selecting and pressing Enter immediately executes the
    /// command (e.g. `/exit`).
    Immediate,
    /// Requires arguments — Tab-completion appends a trailing space so the
    /// user can continue typing (e.g. `/config <key> [value]`).
    TakesArgs,
    /// Nested sub-commands — selecting drills into a sub-menu (Phase 3).
    #[allow(dead_code)]
    HasSubcommands,
}

/// One slash-command entry in the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommand {
    /// Canonical name shown in the menu and used as the completion value.
    /// Must include the leading slash, e.g. `"/exit"`.
    pub name: &'static str,
    /// Dispatch aliases (NOT shown in the menu).
    /// May be slash-less, e.g. `"exit"`, `"quit"`, `":q"`.
    pub aliases: &'static [&'static str],
    /// One-line description shown in the menu right column.
    pub description: &'static str,
    pub kind: CommandKind,
}

/// Master registry — add new commands here and both menu + dispatch pick
/// them up automatically.
pub static SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/exit",
        aliases: &["/quit", "exit", "quit", ":q"],
        description: "退出会话",
        kind: CommandKind::Immediate,
    },
    SlashCommand {
        name: "/config",
        aliases: &[],
        description: "查看/修改工具默认配置",
        kind: CommandKind::TakesArgs,
    },
];
