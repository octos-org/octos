//! A host-confined patch worker. This command does not load a profile,
//! credentials, plugins, or a provider. The embedding host must run it in a
//! workspace-only OS sandbox and supervise its entire process tree.
use super::Executable;
use clap::Args;
use eyre::{Result, bail};
use octos_agent::tools::{Tool, apply_patch::ApplyPatchTool};
use std::io::{BufRead, BufReader, Read, Write};

const MAX_INPUT: u64 = 64 * 1024;

#[derive(Debug, Args)]
pub struct HostPatchCommand {}

impl Executable for HostPatchCommand {
    fn execute(self) -> Result<()> {
        let mut input = Vec::new();
        let stdin = std::io::stdin();
        BufReader::new(stdin.lock().take(MAX_INPUT + 1)).read_until(b'\n', &mut input)?;
        if input.len() as u64 > MAX_INPUT || !input.ends_with(b"\n") {
            bail!("invalid or oversized host patch input");
        }
        let args: serde_json::Value = serde_json::from_slice(&input)?;
        let workspace = std::env::current_dir()?.canonicalize()?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let result = runtime.block_on(ApplyPatchTool::new(workspace).execute(&args))?;
        let mut stdout = std::io::stdout().lock();
        serde_json::to_writer(
            &mut stdout,
            &serde_json::json!({"success":result.success,"output":result.output}),
        )?;
        stdout.write_all(b"\n")?;
        Ok(())
    }
}
