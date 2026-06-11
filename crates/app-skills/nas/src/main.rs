//! NAS skill (`nas_bridge`): browse, read and search files on a Synology NAS
//! via the FileStation Web API.
//!
//! Protocol: `./nas_bridge <tool_name>` with a JSON object on stdin, one JSON
//! object on stdout: {"success":bool,"output":"...","files_to_send":[...]}.
//!
//! Credentials are read from environment variables (never hardcoded):
//!   NAS_URL   e.g. https://nas.example.com:5001  (base, no /webapi)
//!   NAS_USER  DSM account name
//!   NAS_PASS  DSM account password (2FA accounts are NOT supported)
//! Optional:
//!   NAS_VERIFY_TLS=false  accept self-signed HTTPS certs (common on home NAS)

use std::io::Read as _;
use std::thread::sleep;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

use nas::{
    download_body_is_error, download_url, format_listing, list_folder_url, list_share_url,
    login_url, logout_url, parse_file_list, parse_login, parse_search_list, parse_search_start,
    parse_share_list, search_cleanup_url, search_list_url, search_start_url, text_from_bytes,
    Entry,
};

#[derive(Deserialize)]
struct ListInput {
    #[serde(default)]
    path: Option<String>,
}

#[derive(Deserialize)]
struct ReadInput {
    path: String,
}

#[derive(Deserialize)]
struct SearchInput {
    folder: String,
    pattern: String,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let tool_name = args.get(1).map(|s| s.as_str()).unwrap_or("unknown");

    let mut buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
        fail(&format!("Failed to read stdin: {e}"));
    }

    let result = match tool_name {
        "nas_list_folder" => handle_list(&buf),
        "nas_read_file" => handle_read(&buf),
        "nas_search" => handle_search(&buf),
        other => Err(format!(
            "Unknown tool '{other}'. Expected: nas_list_folder, nas_read_file, nas_search"
        )),
    };

    match result {
        Ok(output) => println!("{}", json!({"output": output, "success": true})),
        Err(msg) => fail(&msg),
    }
}

fn fail(msg: &str) -> ! {
    println!("{}", json!({"output": msg, "success": false}));
    std::process::exit(1);
}

// ---------------------------------------------------------------------------
// Environment / HTTP setup
// ---------------------------------------------------------------------------

struct Config {
    base: String,
    user: String,
    pass: String,
}

fn load_config() -> Result<Config, String> {
    let base = env_required("NAS_URL")?;
    let user = env_required("NAS_USER")?;
    let pass = env_required("NAS_PASS")?;
    Ok(Config { base, user, pass })
}

fn env_required(name: &str) -> Result<String, String> {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => Ok(v),
        _ => Err(format!("environment variable '{name}' is not set")),
    }
}

fn http_client() -> Result<reqwest::blocking::Client, String> {
    let accept_invalid = std::env::var("NAS_VERIFY_TLS")
        .map(|v| v.eq_ignore_ascii_case("false") || v == "0")
        .unwrap_or(false);
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(8))
        .danger_accept_invalid_certs(accept_invalid)
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))
}

fn get_text(client: &reqwest::blocking::Client, url: &str, label: &str) -> Result<String, String> {
    let resp = client
        .get(url)
        .send()
        .map_err(|e| format!("{label} request failed: {e}"))?;
    let status = resp.status();
    let body = resp
        .text()
        .map_err(|e| format!("failed to read {label} response: {e}"))?;
    if !status.is_success() {
        return Err(format!("{label} HTTP error {status}"));
    }
    Ok(body)
}

/// Log in, run `op` with the session id, then best-effort log out.
fn with_session<T>(
    cfg: &Config,
    client: &reqwest::blocking::Client,
    op: impl FnOnce(&str) -> Result<T, String>,
) -> Result<T, String> {
    let body = get_text(client, &login_url(&cfg.base, &cfg.user, &cfg.pass), "login")?;
    let sid = parse_login(&body)?;
    let result = op(&sid);
    // Best-effort logout — ignore any failure.
    let _ = client.get(logout_url(&cfg.base, &sid)).send();
    result
}

// ---------------------------------------------------------------------------
// Tool handlers
// ---------------------------------------------------------------------------

fn handle_list(input_json: &str) -> Result<String, String> {
    let input: ListInput =
        serde_json::from_str(input_json).map_err(|e| format!("Invalid input: {e}"))?;
    let cfg = load_config()?;
    let client = http_client()?;

    with_session(&cfg, &client, |sid| {
        let path = input.path.as_deref().map(str::trim).unwrap_or("");
        if path.is_empty() || path == "/" {
            let body = get_text(&client, &list_share_url(&cfg.base, sid), "list_share")?;
            let shares = parse_share_list(&body)?;
            Ok(format_listing(&shares, "Shared folders:"))
        } else {
            let body = get_text(&client, &list_folder_url(&cfg.base, sid, path), "list")?;
            let files = parse_file_list(&body)?;
            Ok(format_listing(&files, &format!("Listing of {path}:")))
        }
    })
}

fn handle_read(input_json: &str) -> Result<String, String> {
    let input: ReadInput =
        serde_json::from_str(input_json).map_err(|e| format!("Invalid input: {e}"))?;
    if input.path.trim().is_empty() {
        return Err("'path' must not be empty".to_string());
    }
    let cfg = load_config()?;
    let client = http_client()?;

    with_session(&cfg, &client, |sid| {
        let resp = client
            .get(download_url(&cfg.base, sid, &input.path))
            .send()
            .map_err(|e| format!("download request failed: {e}"))?;
        let status = resp.status();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let bytes = resp
            .bytes()
            .map_err(|e| format!("failed to read download body: {e}"))?;

        if !status.is_success() {
            // 404 with mode=open/download means the path was not found.
            return Err(format!(
                "could not download '{}': HTTP {status} (path may not exist)",
                input.path
            ));
        }
        if let Some(err) = download_body_is_error(content_type.as_deref(), &bytes) {
            return Err(err);
        }
        let text = text_from_bytes(&bytes)?;
        Ok(format!("Contents of {}:\n\n{text}", input.path))
    })
}

fn handle_search(input_json: &str) -> Result<String, String> {
    let input: SearchInput =
        serde_json::from_str(input_json).map_err(|e| format!("Invalid input: {e}"))?;
    if input.folder.trim().is_empty() {
        return Err("'folder' must not be empty".to_string());
    }
    if input.pattern.trim().is_empty() {
        return Err("'pattern' must not be empty".to_string());
    }
    let cfg = load_config()?;
    let client = http_client()?;

    with_session(&cfg, &client, |sid| {
        let body = get_text(
            &client,
            &search_start_url(&cfg.base, sid, &input.folder, &input.pattern),
            "search start",
        )?;
        let taskid = parse_search_start(&body)?;

        // Poll until finished, capped to avoid hanging on a runaway search.
        const MAX_POLLS: usize = 100;
        let mut files: Vec<Entry> = Vec::new();
        let mut finished = false;
        for _ in 0..MAX_POLLS {
            let body = get_text(
                &client,
                &search_list_url(&cfg.base, sid, &taskid),
                "search list",
            )?;
            let (f, done) = parse_search_list(&body)?;
            files = f;
            finished = done;
            if finished {
                break;
            }
            sleep(Duration::from_millis(200));
        }

        // Best-effort cleanup of the temp search task.
        let _ = client
            .get(search_cleanup_url(&cfg.base, sid, "stop", &taskid))
            .send();
        let _ = client
            .get(search_cleanup_url(&cfg.base, sid, "clean", &taskid))
            .send();

        let header = if finished {
            format!(
                "Search for '{}' in {} — {} match(es):",
                input.pattern,
                input.folder,
                files.len()
            )
        } else {
            format!(
                "Search for '{}' in {} — partial results ({} so far, search still running):",
                input.pattern,
                input.folder,
                files.len()
            )
        };
        if files.is_empty() {
            Ok(format!("{header}\n(no matches)"))
        } else {
            let mut lines = vec![header];
            for e in &files {
                lines.push(if e.isdir {
                    format!("[DIR]  {}", e.path)
                } else {
                    format!("       {}", e.path)
                });
            }
            Ok(lines.join("\n"))
        }
    })
}
