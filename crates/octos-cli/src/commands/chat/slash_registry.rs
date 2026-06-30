//! Slash-command registry — single source of truth for command metadata,
//! shared by menu rendering and dispatch.
//!
//! # Design
//!
//! Commands are split along **two orthogonal axes**:
//!
//! | Axis      | Field    | Purpose                                         |
//! |-----------|----------|-------------------------------------------------|
//! | Behaviour | `kind`   | How the REPL loop reacts: `Immediate` commands    |
//! |           |          | cause `break`; `TakesArgs` commands `continue`    |
//! |           |          | after the handler runs.                          |
//! | Execution | `handler`| What code to run when the command is submitted.   |
//!
//! All dispatch decisions are driven by the enum fields — **no string
//! matching on `name` or `aliases` appears in the dispatch path**.  Adding
//! a new command only requires inserting one entry here and, if the
//! handler is new, adding one variant to `CommandHandler` plus its
//! implementation arm at the call site.
//!
//! # Sub-command vs handler
//!
//! A command like `/config set <key> <value>` is *not* modelled as nested
//! `SlashCommand`s today.  Instead `/config` is a single `TakesArgs`
//! command whose `ToolConfigStore`-backed handler parses `"set …"`,
//! `"reset …"`, `"<tool>"` etc.  from the free-text argument tail.
//! Structured sub-command registration (the `HasSubcommands` kind) is
//! deferred to a future phase.

/// What happens after the user accepts / submits a slash command.
///
/// Separate from [`CommandHandler`]: this controls the **REPL loop flow**,
/// not the side-effect code to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
    /// No arguments — selecting and pressing Enter immediately executes the
    /// command (e.g. `/exit`).  The REPL loop performs `break`.
    Immediate,
    /// Requires arguments — Tab-completion appends a trailing space so the
    /// user can continue typing (e.g. `/config <key> [value]`).
    /// The REPL loop calls the command's handler then `continue`s.
    TakesArgs,
    /// Nested sub-commands — selecting drills into a sub-menu (Phase 3).
    #[allow(dead_code)]
    HasSubcommands,
}

/// Which side-effect to invoke when a command is dispatched.
///
/// Each variant maps 1:1 to a block of code in the chat loop (or, for
/// future commands, to a handler function).  The dispatch site `match`es
/// exhaustively on this enum so the compiler will reject any new variant
/// that lacks a corresponding arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandHandler {
    /// Exit the REPL immediately.
    Exit,
    /// Delegate to [`ToolConfigStore::handle_config_command`], which
    /// internally parses sub-commands (`get`, `set`, `reset`, `list`).
    ToolConfig,
}

/// One slash-command entry in the registry.
///
/// Every field is `'static` so the entire table lives in read-only memory;
/// no heap allocation needed at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommand {
    /// Canonical name shown in the menu and used as the completion value.
    /// Must include the leading slash, e.g. `"/exit"`.
    pub name: &'static str,
    /// Dispatch aliases (**not** shown in the menu).
    /// May be slash-less, e.g. `"exit"`, `"quit"`, `":q"`.
    pub aliases: &'static [&'static str],
    /// One-line description shown in the menu right column.
    pub description: &'static str,
    /// Controls REPL loop flow: `break`, `continue`, or sub-menu drill-down.
    pub kind: CommandKind,
    /// Controls the side-effect to execute on submission.
    pub handler: CommandHandler,
}

/// Master registry — add new commands here and both menu + dispatch pick
/// them up automatically.
pub static SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/exit",
        aliases: &["/quit", "exit", "quit", ":q"],
        description: "退出会话",
        kind: CommandKind::Immediate,
        handler: CommandHandler::Exit,
    },
    SlashCommand {
        name: "/config",
        aliases: &[],
        description: "查看/修改工具默认配置",
        kind: CommandKind::TakesArgs,
        handler: CommandHandler::ToolConfig,
    },
];
