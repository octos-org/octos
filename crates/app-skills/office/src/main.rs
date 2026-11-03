//! Bundled office skill binary: `./office office_generate` with JSON stdin/stdout.

use std::io::Read;

use serde_json::json;

fn main() {
    let tool = std::env::args().nth(1).unwrap_or_default();
    let mut input = String::new();
    let result = (|| {
        if tool != "office_generate" {
            return Err(eyre::eyre!(
                "unknown tool '{tool}'; expected office_generate"
            ));
        }
        std::io::stdin().read_to_string(&mut input)?;
        let input: office::OfficeInput = serde_json::from_str(&input)?;
        let root = office::workspace_root()?;
        office::generate(input, &root)
    })();
    match result {
        Ok(path) => println!(
            "{}",
            json!({
                "success": true,
                "output": format!("Created {}", path.display()),
                "file_modified": path.clone(),
                "files_to_send": [path],
            })
        ),
        Err(error) => {
            println!(
                "{}",
                json!({"success": false, "output": format!("{error:#}")})
            );
            std::process::exit(1);
        }
    }
}
