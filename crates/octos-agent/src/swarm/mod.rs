//! Swarm coordination primitives.

pub mod mailbox;

pub use mailbox::{
    FileMailbox, InProcessMailbox, MAILBOX_SCHEMA_VERSION, MailboxBackend, MailboxEnvelope,
    MailboxMessage, MailboxRecovery,
};
