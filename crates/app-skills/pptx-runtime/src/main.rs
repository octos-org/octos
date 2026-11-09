use std::io::{Read, Write};

fn main() {
    // Font and rendering code has the same stack needs on CLI/plugin worker threads.
    let worker = std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(run);
    let code = match worker {
        Ok(h) => h.join().unwrap_or(1),
        Err(e) => {
            eprintln!("could not start PPTX worker: {e}");
            1
        }
    };
    std::process::exit(code);
}

fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|s| s == "--help" || s == "help") {
        println!(
            "octos-pptx pptx_tool < request.json\nJSON action dispatcher; workspace from OCTOS_SESSION_WORKSPACE or current directory.\nUse {{\"action\":\"help\"}} for operations. No Python, Node, or LibreOffice is invoked."
        );
        return 0;
    }
    let result = (|| -> anyhow::Result<serde_json::Value> {
        let root = std::env::var_os("OCTOS_SESSION_WORKSPACE")
            .map(std::path::PathBuf::from)
            .unwrap_or(std::env::current_dir()?);
        let mut ctx = octos_pptx::Context::new(&root)?;
        ctx.continuation_owner = std::env::var("OCTOS_CONTINUATION_OWNER")
            .ok()
            .filter(|s| !s.is_empty());
        if args.first().is_some_and(|s| s == "mcp") {
            let roots = deckcraft_engine::file_access::AutomationRoots::new(
                std::slice::from_ref(&ctx.root),
                std::slice::from_ref(&ctx.root),
            )
            .map_err(anyhow::Error::msg)?;
            let backend = deckcraft_mcp::Headless::new().with_automation_roots(roots);
            deckcraft_mcp::Server::new(Box::new(backend))
                .serve(std::io::stdin().lock(), std::io::stdout().lock())?;
            return Ok(serde_json::json!({"mcp_closed":true}));
        }
        let mut input = String::new();
        std::io::stdin()
            .take(16 * 1024 * 1024 + 1)
            .read_to_string(&mut input)?;
        anyhow::ensure!(input.len() <= 16 * 1024 * 1024, "request exceeds 16 MiB");
        let request = if input.trim().is_empty() {
            serde_json::json!({"action":"help"})
        } else {
            serde_json::from_str(&input)?
        };
        ctx.execute(request)
    })();
    let (code, value) = match result {
        Ok(value) if value["mcp_closed"] == true => return 0,
        Ok(value) if value["qa_command"] == true && value["status"] != "pass" => (
            2,
            serde_json::json!({"success":true,"output":serde_json::to_string(&value).unwrap_or_default(),"result":value}),
        ),
        Ok(value) => (
            0,
            serde_json::json!({"success":true,"output":serde_json::to_string(&value).unwrap_or_default(),"result":value}),
        ),
        Err(e) => (
            1,
            serde_json::json!({"success":false,"output":format!("{e:#}")}),
        ),
    };
    if writeln!(std::io::stdout(), "{value}").is_err() {
        return 1;
    }
    code
}
