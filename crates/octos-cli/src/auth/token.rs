//! Paste-token flow for providers without OAuth (e.g. Anthropic).

use eyre::Result;

use super::store::AuthCredential;

/// Prompt the user to paste an API key from their clipboard/terminal.
pub fn paste_token_flow(provider: &str) -> Result<AuthCredential> {
    let env_hint = match provider {
        "anthropic" => "ANTHROPIC_API_KEY",
        "gemini" | "google" => "GEMINI_API_KEY",
        "deepseek" => "DEEPSEEK_API_KEY",
        _ => "API_KEY",
    };

    println!("Paste your {provider} API key (from {env_hint}):");
    print!("> ");
    use std::io::Write;
    std::io::stdout().flush()?;

    let mut token = String::new();
    std::io::stdin().read_line(&mut token)?;
    let token = token.trim().to_string();

    if token.is_empty() {
        eyre::bail!("no token provided");
    }

    Ok(AuthCredential {
        access_token: token,
        refresh_token: None,
        expires_at: None,
        provider: provider.to_string(),
        auth_method: "paste_token".to_string(),
    })
}
