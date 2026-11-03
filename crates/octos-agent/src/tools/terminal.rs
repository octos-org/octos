//! A real OS PTY, using the already-sandboxed and sanitized command.
use super::command_output::CommandOutput;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub(crate) type TerminalWriter = Arc<Mutex<Box<dyn Write + Send>>>;

pub(crate) struct Terminal {
    pub writer: TerminalWriter,
    pub pid: Option<u32>,
    pub done: tokio::task::JoinHandle<i32>,
}

pub(crate) fn launch(
    command: &tokio::process::Command,
    output: CommandOutput,
    timeout_secs: u64,
) -> eyre::Result<Terminal> {
    let source = command.as_std();
    let mut builder = CommandBuilder::new(source.get_program());
    builder.args(source.get_args());
    if let Some(cwd) = source.get_current_dir() {
        builder.cwd(cwd);
    }
    builder.env_clear();
    for (key, value) in std::env::vars_os() {
        builder.env(key, value);
    }
    // Preserve removals as well as overrides: inheriting the harness environment
    // directly here would undo secret filtering on the normal command path.
    for (key, value) in source.get_envs() {
        match value {
            Some(value) => builder.env(key, value),
            None => builder.env_remove(key),
        }
    }
    let pair = native_pty_system()
        .openpty(PtySize::default())
        .map_err(|e| eyre::eyre!("open PTY: {e}"))?;
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| eyre::eyre!("PTY reader: {e}"))?;
    let writer = Arc::new(Mutex::new(
        pair.master
            .take_writer()
            .map_err(|e| eyre::eyre!("PTY writer: {e}"))?,
    ));
    let mut child = pair
        .slave
        .spawn_command(builder)
        .map_err(|e| eyre::eyre!("PTY spawn: {e}"))?;
    drop(pair.slave);
    let pid = child.process_id();
    let mut killer = child.clone_killer();
    let captured = output.clone();
    let mut read_task = tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => captured.append(&buf[..n]),
                Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
                Err(e) => {
                    captured.record_error(e);
                    break;
                }
            }
        }
    });
    let mut wait_task =
        tokio::task::spawn_blocking(move || child.wait().map(|s| s.exit_code() as i32));
    let done = tokio::spawn(async move {
        let _master = pair.master;
        let status =
            match tokio::time::timeout(Duration::from_secs(timeout_secs), &mut wait_task).await {
                Ok(Ok(Ok(status))) => status,
                Ok(_) => {
                    output.record_error("PTY wait failed");
                    -1
                }
                Err(_) => {
                    output.append(
                        format!("\nCommand timed out after {timeout_secs} seconds\n").as_bytes(),
                    );
                    let _ = killer.kill();
                    super::coding_tools::kill_timed_out_child(pid).await;
                    let _ = tokio::time::timeout(Duration::from_secs(2), &mut wait_task).await;
                    -1
                }
            };
        if tokio::time::timeout(Duration::from_millis(200), &mut read_task)
            .await
            .is_err()
        {
            output.record_error("PTY descendant kept output open after exit");
            let _ = killer.kill();
            super::coding_tools::kill_timed_out_child(pid).await;
        }
        output.finish();
        status
    });
    Ok(Terminal { writer, pid, done })
}
