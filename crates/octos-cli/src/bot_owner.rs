//! One consumer per bot account (T-0654, `docs/IM_REMOTE_CONTROL.md` §7).
//!
//! Two processes polling one Telegram bot split its messages between them,
//! and Telegram answers the second `getUpdates` with 409. So whoever consumes
//! a bot, `octos serve` for a `serve.im` entry or a gateway for a channel,
//! first takes the bot's lock. The lock is named by the bot's own id, not by
//! the config entry that reaches it: two profiles, or a profile and
//! `serve.im`, that name one bot through different settings meet at one
//! lock. The locks are in one directory per user, so two servers with
//! different data directories meet there too.
//!
//! It is an advisory file lock, so it goes with its process, however that
//! ends.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long to wait for a bot's last owner to let go: a gateway restart
/// starts the new process half a second after it stops the old one.
pub(crate) const HANDOFF: Duration = Duration::from_secs(3);

/// What every refusal says, for whoever must tell it from other errors.
pub(crate) const TAKEN: &str = "already has an owner";

/// Where this user's octos processes take bot locks.
pub(crate) fn default_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".octos").join("bot-owners"))
}

/// A Telegram bot's id: the number its token starts with (`<id>:<secret>`),
/// which is the id `getMe` reports.
pub(crate) fn telegram_bot_id(token: &str) -> Option<i64> {
    let (id, secret) = token.trim().split_once(':')?;
    (!secret.is_empty())
        .then(|| id.parse::<i64>().ok())
        .flatten()
        .filter(|id| *id > 0)
}

#[derive(Debug)]
pub(crate) enum Unclaimed {
    /// Another process consumes the bot.
    Taken(String),
    /// The lock itself could not be used.
    Unusable(String),
}

impl std::fmt::Display for Unclaimed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Taken(why) | Self::Unusable(why) => f.write_str(why),
        }
    }
}

/// A bot this process consumes, until dropped.
#[derive(Debug)]
pub(crate) struct BotOwner {
    file: File,
}

impl BotOwner {
    /// Take `platform`'s bot `bot` for `holder`, which is what whoever finds
    /// it taken is told. The note is a file of its own: Windows lets no one
    /// else read a locked file. Fully-qualified `fs2::FileExt` calls, as in
    /// serve's data-dir lock: std grew inherent methods of the same names
    /// after the workspace MSRV.
    pub(crate) fn claim(
        dir: &Path,
        platform: &str,
        bot: i64,
        holder: &str,
        wait: Duration,
    ) -> Result<Self, Unclaimed> {
        let path = dir.join(format!("{platform}-{bot}.lock"));
        let note = dir.join(format!("{platform}-{bot}.owner"));
        let unusable = |error: std::io::Error| {
            Unclaimed::Unusable(format!("bot lock {}: {error}", path.display()))
        };
        std::fs::create_dir_all(dir).map_err(unusable)?;
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(unusable)?;
        let deadline = Instant::now() + wait;
        loop {
            match fs2::FileExt::try_lock_exclusive(&file) {
                Ok(()) => break,
                Err(error)
                    if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
                {
                    let now = Instant::now();
                    if now >= deadline {
                        let other = std::fs::read_to_string(&note).unwrap_or_default();
                        return Err(Unclaimed::Taken(format!(
                            "{platform} bot {bot} {TAKEN} ({}): a bot is consumed by \
                             serve.im or by one gateway, never by two",
                            other.trim()
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(50).min(deadline - now));
                }
                Err(error) => return Err(unusable(error)),
            }
        }
        std::fs::write(&note, format!("{holder}, pid {}", std::process::id())).map_err(unusable)?;
        Ok(Self { file })
    }

    /// A gateway's Telegram channel takes its bot before it starts. A token
    /// with no bot id fails at Telegram as it always did, and a lock
    /// directory the gateway cannot use leaves it running unlocked, as
    /// before; only a bot another process consumes stops it.
    #[cfg_attr(not(feature = "telegram"), allow(dead_code))]
    pub(crate) fn claim_for_gateway(
        token: &str,
        dir: Option<&Path>,
        holder: &str,
    ) -> Result<Option<Self>, String> {
        let (Some(dir), Some(bot)) = (dir, telegram_bot_id(token)) else {
            return Ok(None);
        };
        match Self::claim(dir, "telegram", bot, holder, HANDOFF) {
            Ok(owner) => Ok(Some(owner)),
            Err(Unclaimed::Taken(why)) => Err(why),
            Err(Unclaimed::Unusable(why)) => {
                tracing::warn!(error = %why, "telegram bot not locked");
                Ok(None)
            }
        }
    }
}

impl Drop for BotOwner {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}
