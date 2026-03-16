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
    let mut parsed_emails: Vec<(String, String, String)> = Vec::new();
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
            let mut text_body = extract_text_body(&parsed).unwrap_or_default();
            octos_core::truncate_utf8(&mut text_body, config.max_body_chars, "...");

            if !text_body.is_empty() {
                parsed_emails.push((from, subject, text_body));
            }
        }
    }
    // Stream dropped — session is free again.

    // Mark all fetched as seen
    session.store(&seq_set, "+FLAGS (\\Seen)").await.ok();
    session.logout().await.ok();

    // Send parsed emails as inbound messages
    let mut count = 0;
    for (from, subject, text_body) in parsed_emails {
        let sender_email = extract_email_address(&from);

        let content = if subject.is_empty() {
            text_body
        } else {
            format!("[Subject: {subject}]\n{text_body}")
        };

        let inbound = InboundMessage {
            channel: "email".into(),
            sender_id: sender_email.clone(),
            chat_id: sender_email,
            content,
            timestamp: Utc::now(),
            media: vec![],
            metadata: serde_json::json!({ "subject": subject }),
            message_id: None,
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
            return from[start + 1..start + end].to_string();
        }
    }
    from.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
