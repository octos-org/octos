//! Email channel: IMAP polling for inbound, SMTP for outbound.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use eyre::{Result, WrapErr};
use futures::StreamExt;
use octos_core::{InboundMessage, OutboundMessage};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::channel::Channel;

const MAX_EMAIL_TOPIC_CHARS: usize = 120;

/// Email channel configuration.
pub struct EmailConfig {
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub username: String,
    pub password: String,
    pub from_address: String,
    pub poll_interval_secs: u64,
    pub allowed_senders: Vec<String>,
    pub max_body_chars: usize,
}

pub struct EmailChannel {
    config: Arc<EmailConfig>,
    shutdown: Arc<AtomicBool>,
}

struct ParsedEmail {
    from: String,
    subject: String,
    text_body: String,
    message_id: Option<String>,
    in_reply_to: Option<String>,
    references: Option<String>,
    thread_topic: String,
}

impl EmailChannel {
    pub fn new(config: EmailConfig, shutdown: Arc<AtomicBool>) -> Self {
        Self {
            config: Arc::new(config),
            shutdown,
        }
    }
}

#[async_trait]
impl Channel for EmailChannel {
    fn name(&self) -> &str {
        "email"
    }

    async fn start(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        let interval = Duration::from_secs(self.config.poll_interval_secs);

        loop {
            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }

            match imap_poll(&self.config, &inbound_tx).await {
                Ok(0) => {}
                Ok(n) => info!(count = n, "processed emails"),
                Err(e) => warn!("IMAP poll failed: {e}"),
            }

            tokio::time::sleep(interval).await;
        }

        Ok(())
    }

    async fn send(&self, msg: &OutboundMessage) -> Result<()> {
        // chat_id is the recipient email address
        let subject = msg
            .metadata
            .get("subject")
            .and_then(|v| v.as_str())
            .unwrap_or("Re: Message");

        info!(
            recipient = %msg.chat_id,
            subject = %subject,
            "smtp_send outbound email"
        );
        smtp_send(&self.config, &msg.chat_id, subject, &msg.content).await
    }

    fn is_allowed(&self, sender_id: &str) -> bool {
        self.config.allowed_senders.is_empty()
            || self.config.allowed_senders.iter().any(|s| s == sender_id)
    }
}

/// Poll IMAP for unseen messages, send them as InboundMessages. Returns count.
async fn imap_poll(config: &EmailConfig, tx: &mpsc::Sender<InboundMessage>) -> Result<usize> {
    // Build TLS connector
    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_config));
    let server_name = rustls::pki_types::ServerName::try_from(config.imap_host.clone())
        .wrap_err("invalid IMAP hostname")?;

    // Connect
    let tcp = tokio::net::TcpStream::connect((&*config.imap_host, config.imap_port))
        .await
        .wrap_err("IMAP connection failed")?;
    let tls_stream = connector
        .connect(server_name, tcp)
        .await
        .wrap_err("IMAP TLS handshake failed")?;

    let client = async_imap::Client::new(tls_stream);

    // Login
    let mut session = client
        .login(&config.username, &config.password)
        .await
        .map_err(|(e, _)| e)
        .wrap_err("IMAP login failed")?;

    // Select INBOX
    session
        .select("INBOX")
        .await
        .wrap_err("IMAP SELECT INBOX failed")?;

    // Search unseen
    let unseen = session
        .search("UNSEEN")
        .await
        .wrap_err("IMAP SEARCH failed")?;

    if unseen.is_empty() {
        session.logout().await.ok();
        return Ok(0);
    }

    // Fetch each unseen message
    let seq_set = unseen
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join(",");

    // Collect parsed emails first, then drop the stream to release session borrow.
    let mut parsed_emails = Vec::new();
    {
        let mut messages = session
            .fetch(&seq_set, "RFC822")
            .await
            .wrap_err("IMAP FETCH failed")?;

        while let Some(result) = messages.next().await {
            let msg = match result {
                Ok(m) => m,
                Err(e) => {
                    warn!("IMAP fetch error: {e}");
                    continue;
                }
            };

            let body_bytes = match msg.body() {
                Some(b) => b,
                None => continue,
            };

            let parsed = match mailparse::parse_mail(body_bytes) {
                Ok(p) => p,
                Err(e) => {
                    warn!("failed to parse email: {e}");
                    continue;
                }
            };

            let from = extract_header(&parsed, "From").unwrap_or_default();
            let subject = extract_header(&parsed, "Subject").unwrap_or_default();
            let message_id = extract_header(&parsed, "Message-ID").and_then(non_empty_header);
            let in_reply_to = extract_header(&parsed, "In-Reply-To").and_then(non_empty_header);
            let references = extract_header(&parsed, "References").and_then(non_empty_header);
            let thread_topic = email_thread_topic(
                message_id.as_deref(),
                in_reply_to.as_deref(),
                references.as_deref(),
                &subject,
            );
            let mut text_body = extract_text_body(&parsed).unwrap_or_default();
            octos_core::truncate_utf8(&mut text_body, config.max_body_chars, "...");

            if !text_body.is_empty() {
                parsed_emails.push(ParsedEmail {
                    from,
                    subject,
                    text_body,
                    message_id,
                    in_reply_to,
                    references,
                    thread_topic,
                });
            }
        }
    }
    // Stream dropped — session is free again.

    // Mark all fetched as seen.
    if let Err(e) = session.store(&seq_set, "+FLAGS (\\Seen)").await {
        warn!(error = %e, "IMAP STORE failed while marking messages seen");
    }
    session.logout().await.ok();

    // Send parsed emails as inbound messages
    let mut count = 0;
    for email in parsed_emails {
        let sender_email = extract_email_address(&email.from);
        let message_id = email.message_id.clone();

        if should_skip_self_reply(&sender_email, &email.subject, config) {
            info!(
                sender = %sender_email,
                subject = %email.subject,
                "skipping self-sent email reply"
            );
            continue;
        }

        let content = if email.subject.is_empty() {
            email.text_body
        } else {
            format!("[Subject: {}]\n{}", email.subject, email.text_body)
        };

        let inbound = InboundMessage {
            channel: "email".into(),
            sender_id: sender_email.clone(),
            chat_id: sender_email,
            content,
            timestamp: Utc::now(),
            media: vec![],
            metadata: serde_json::json!({
                "subject": email.subject,
                "topic": email.thread_topic,
                "message_id": email.message_id,
                "in_reply_to": email.in_reply_to,
                "references": email.references,
            }),
            message_id,
            origin: octos_core::MessageOrigin::ExternalUser,
        };

        if tx.send(inbound).await.is_err() {
            break;
        }
        count += 1;
    }

    Ok(count)
}

/// Send an email via SMTP with lettre.
async fn smtp_send(config: &EmailConfig, to: &str, subject: &str, body: &str) -> Result<()> {
    use lettre::message::header::ContentType;
    use lettre::transport::smtp::authentication::Credentials;
    use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

    let email = Message::builder()
        .from(
            config
                .from_address
                .parse()
                .wrap_err("invalid from address")?,
        )
        .to(to.parse().wrap_err("invalid recipient address")?)
        .subject(subject)
        .header(ContentType::TEXT_PLAIN)
        .body(body.to_string())
        .wrap_err("failed to build email")?;

    let creds = Credentials::new(config.username.clone(), config.password.clone());

    let mailer = if config.smtp_port == 465 {
        // Implicit TLS (SMTPS)
        AsyncSmtpTransport::<Tokio1Executor>::relay(&config.smtp_host)
            .wrap_err("SMTP relay setup failed")?
            .credentials(creds)
            .port(config.smtp_port)
            .build()
    } else {
        // STARTTLS (port 587 or other)
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.smtp_host)
            .wrap_err("SMTP STARTTLS relay setup failed")?
            .credentials(creds)
            .port(config.smtp_port)
            .build()
    };

    mailer.send(email).await.wrap_err("failed to send email")?;

    Ok(())
}

/// Extract a header value from a parsed email.
fn extract_header(mail: &mailparse::ParsedMail, name: &str) -> Option<String> {
    mail.headers
        .iter()
        .find(|h| h.get_key().eq_ignore_ascii_case(name))
        .map(|h| h.get_value())
}

fn non_empty_header(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn email_thread_topic(
    message_id: Option<&str>,
    in_reply_to: Option<&str>,
    references: Option<&str>,
    subject: &str,
) -> String {
    let topic_source = references
        .and_then(first_message_id)
        .or_else(|| in_reply_to.and_then(first_message_id))
        .or_else(|| message_id.and_then(first_message_id))
        .or_else(|| normalized_subject(subject))
        .unwrap_or_else(|| "untitled".to_string());

    format!("email-{}", sanitize_topic_component(&topic_source))
}

fn first_message_id(value: &str) -> Option<String> {
    value
        .split_whitespace()
        .map(|part| part.trim().trim_matches('<').trim_matches('>'))
        .find(|part| !part.is_empty())
        .map(ToOwned::to_owned)
}

fn normalized_subject(subject: &str) -> Option<String> {
    let mut normalized = subject.trim();
    loop {
        let lower = normalized.to_ascii_lowercase();
        let stripped = ["re:", "fw:", "fwd:"]
            .iter()
            .find(|prefix| lower.starts_with(**prefix))
            .map(|prefix| &normalized[prefix.len()..]);

        let Some(stripped) = stripped else {
            break;
        };
        normalized = stripped.trim();
    }

    if normalized.is_empty() {
        None
    } else {
        Some(normalized.to_string())
    }
}

fn sanitize_topic_component(value: &str) -> String {
    let mut out = String::new();
    let mut previous_separator = false;

    for ch in value.trim().chars() {
        if out.len() >= MAX_EMAIL_TOPIC_CHARS {
            break;
        }

        if ch.is_alphanumeric() || ch == '-' {
            previous_separator = false;
            out.push(ch);
        } else if !previous_separator && !out.is_empty() {
            previous_separator = true;
            out.push('_');
        }
    }

    while out.ends_with('_') {
        out.pop();
    }

    if out.is_empty() {
        "untitled".to_string()
    } else {
        out
    }
}

/// Extract the first text/plain body from a parsed email.
fn extract_text_body(mail: &mailparse::ParsedMail) -> Option<String> {
    if mail.ctype.mimetype == "text/plain" {
        return mail.get_body().ok();
    }
    for part in &mail.subparts {
        if let Some(text) = extract_text_body(part) {
            return Some(text);
        }
    }
    None
}

/// Extract email address from "Display Name <email@example.com>" format.
fn extract_email_address(from: &str) -> String {
    if let Some(start) = from.rfind('<') {
        if let Some(end) = from[start..].find('>') {
            return from[start + 1..start + end].trim().to_string();
        }
    }
    from.trim().to_string()
}

fn canonical_email_address(value: &str) -> String {
    extract_email_address(value).to_ascii_lowercase()
}

fn subject_is_reply(subject: &str) -> bool {
    subject.trim_start().to_ascii_lowercase().starts_with("re:")
}

fn should_skip_self_reply(sender_email: &str, subject: &str, config: &EmailConfig) -> bool {
    if !subject_is_reply(subject) {
        return false;
    }

    let sender = canonical_email_address(sender_email);
    if sender.is_empty() {
        return false;
    }

    [config.from_address.as_str(), config.username.as_str()]
        .into_iter()
        .filter(|addr| !addr.trim().is_empty())
        .any(|addr| canonical_email_address(addr) == sender)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(from_address: &str, username: &str) -> EmailConfig {
        EmailConfig {
            imap_host: "imap.example.com".into(),
            imap_port: 993,
            smtp_host: "smtp.example.com".into(),
            smtp_port: 465,
            username: username.into(),
            password: "secret".into(),
            from_address: from_address.into(),
            poll_interval_secs: 30,
            allowed_senders: vec![],
            max_body_chars: 10_000,
        }
    }

    #[test]
    fn test_extract_email_address() {
        assert_eq!(
            extract_email_address("John Doe <john@example.com>"),
            "john@example.com"
        );
        assert_eq!(
            extract_email_address("jane@example.com"),
            "jane@example.com"
        );
        assert_eq!(
            extract_email_address("<bob@example.com>"),
            "bob@example.com"
        );
        assert_eq!(
            extract_email_address("Bot < bot@example.com >"),
            "bot@example.com"
        );
    }

    #[test]
    fn self_sent_reply_is_skipped_to_prevent_mail_loop() {
        let config = test_config("Bot <bot@example.com>", "bot-login@example.com");

        assert!(should_skip_self_reply(
            "bot@example.com",
            "Re: incoming question",
            &config
        ));
        assert!(should_skip_self_reply(
            "BOT@example.com",
            "  re: incoming question",
            &config
        ));
        assert!(should_skip_self_reply(
            "bot-login@example.com",
            "Re: via login address",
            &config
        ));
    }

    #[test]
    fn non_self_or_non_reply_email_is_not_skipped() {
        let config = test_config("bot@example.com", "bot-login@example.com");

        assert!(!should_skip_self_reply(
            "user@example.com",
            "Re: incoming question",
            &config
        ));
        assert!(!should_skip_self_reply(
            "bot@example.com",
            "New incoming question",
            &config
        ));
    }

    #[test]
    fn email_thread_topic_prefers_root_reference() {
        assert_eq!(
            email_thread_topic(
                Some("<reply@example.com>"),
                Some("<parent@example.com>"),
                Some("<root@example.com> <parent@example.com>"),
                "Re: Project update",
            ),
            "email-root_example_com"
        );
    }

    #[test]
    fn email_thread_topic_falls_back_to_message_id() {
        assert_eq!(
            email_thread_topic(Some("<first@example.com>"), None, None, "Project update"),
            "email-first_example_com"
        );
    }

    #[test]
    fn email_thread_topic_falls_back_to_normalized_subject() {
        assert_eq!(
            email_thread_topic(None, None, None, "Re: Fwd: Quarterly Plan"),
            "email-Quarterly_Plan"
        );
    }

    #[test]
    fn email_thread_topic_preserves_unicode_subject_words() {
        assert_eq!(
            email_thread_topic(None, None, None, "Re: 你好 世界"),
            "email-你好_世界"
        );
    }
}
