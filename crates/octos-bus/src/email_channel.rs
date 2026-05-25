//! Email channel: IMAP polling for inbound, SMTP for outbound.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use eyre::{Result, WrapErr};
use futures::StreamExt;
use octos_core::{InboundMessage, OutboundMessage};
use sha2::{Digest, Sha256};
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
    let mut parsed_emails: Vec<ParsedEmailMessage> = Vec::new();
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
            let message_id = extract_header(&parsed, "Message-ID");
            let in_reply_to = extract_header(&parsed, "In-Reply-To");
            let references = extract_header(&parsed, "References");
            let mut text_body = extract_text_body(&parsed).unwrap_or_default();
            octos_core::truncate_utf8(&mut text_body, config.max_body_chars, "...");

            if !text_body.is_empty() {
                parsed_emails.push(ParsedEmailMessage {
                    from,
                    subject,
                    message_id,
                    in_reply_to,
                    references,
                    text_body,
                });
            }
        }
    }
    // Stream dropped — session is free again.

    // Mark all fetched as seen
    session.store(&seq_set, "+FLAGS (\\Seen)").await.ok();
    session.logout().await.ok();

    // Send parsed emails as inbound messages
    let mut count = 0;
    for parsed_email in parsed_emails {
        let inbound = build_inbound_message(parsed_email);
        if tx.send(inbound).await.is_err() {
            break;
        }
        count += 1;
    }

    Ok(count)
}

struct ParsedEmailMessage {
    from: String,
    subject: String,
    message_id: Option<String>,
    in_reply_to: Option<String>,
    references: Option<String>,
    text_body: String,
}

fn build_inbound_message(parsed: ParsedEmailMessage) -> InboundMessage {
    let ParsedEmailMessage {
        from,
        subject,
        message_id,
        in_reply_to,
        references,
        text_body,
    } = parsed;

    let sender_email = extract_email_address(&from);
    let topic = email_thread_topic(
        &subject,
        message_id.as_deref(),
        in_reply_to.as_deref(),
        references.as_deref(),
    );

    let content = if subject.is_empty() {
        text_body
    } else {
        format!("[Subject: {subject}]\n{text_body}")
    };

    InboundMessage {
        channel: "email".into(),
        sender_id: sender_email.clone(),
        chat_id: sender_email,
        content,
        timestamp: Utc::now(),
        media: vec![],
        metadata: serde_json::json!({
            "subject": subject,
            "topic": topic.clone(),
            "email_thread_key": topic,
            "email_message_id": message_id.clone(),
            "in_reply_to": in_reply_to.clone(),
            "references": references.clone(),
        }),
        message_id,
    }
}

fn email_thread_topic(
    subject: &str,
    message_id: Option<&str>,
    in_reply_to: Option<&str>,
    references: Option<&str>,
) -> String {
    let normalized_subject = normalize_subject(subject);
    let basis = if normalized_subject.is_empty() {
        references
            .and_then(first_message_id)
            .or_else(|| in_reply_to.and_then(first_message_id))
            .or_else(|| message_id.and_then(first_message_id))
            .map(|id| format!("message-id:{id}"))
            .unwrap_or_else(|| "untitled".to_string())
    } else {
        format!("subject:{normalized_subject}")
    };
    let digest = Sha256::digest(basis.as_bytes());
    let hex = format!("{digest:x}");
    format!("email-thread-{}", &hex[..12])
}

fn normalize_subject(subject: &str) -> String {
    let mut value = subject.trim();
    loop {
        let lower = value.to_ascii_lowercase();
        let Some(rest) = lower
            .strip_prefix("re:")
            .or_else(|| lower.strip_prefix("fw:"))
            .or_else(|| lower.strip_prefix("fwd:"))
        else {
            break;
        };
        let prefix_len = value.len() - rest.len();
        value = value[prefix_len..].trim_start();
    }
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn first_message_id(value: &str) -> Option<String> {
    value
        .split_whitespace()
        .find_map(clean_message_id)
        .or_else(|| clean_message_id(value))
}

fn clean_message_id(value: &str) -> Option<String> {
    let trimmed = value.trim().trim_matches(',');
    if trimmed.is_empty() {
        return None;
    }
    if let Some(start) = trimmed.find('<') {
        if let Some(end) = trimmed[start + 1..].find('>') {
            let id = trimmed[start + 1..start + 1 + end].trim();
            if !id.is_empty() {
                return Some(id.to_ascii_lowercase());
            }
        }
    }
    Some(
        trimmed
            .trim_start_matches('<')
            .trim_end_matches('>')
            .to_ascii_lowercase(),
    )
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

    #[test]
    fn test_email_thread_topic_separates_subjects() {
        let invoice = build_test_inbound("Quarterly invoice", Some("<a@example.test>"));
        let support = build_test_inbound("Support follow-up", Some("<b@example.test>"));

        assert_eq!(invoice.chat_id, "sender@example.com");
        assert_eq!(support.chat_id, "sender@example.com");
        assert_ne!(
            invoice.metadata["topic"], support.metadata["topic"],
            "different email subjects from the same sender must not share one session"
        );
        assert_ne!(
            routed_email_session_key(&invoice),
            routed_email_session_key(&support)
        );
    }

    #[test]
    fn test_email_thread_topic_normalizes_reply_subjects() {
        let original = build_test_inbound("Incident 42", Some("<orig@example.test>"));
        let reply = build_test_inbound("Re: Incident   42", Some("<reply@example.test>"));

        assert_eq!(original.metadata["topic"], reply.metadata["topic"]);
        assert_eq!(
            routed_email_session_key(&original),
            routed_email_session_key(&reply)
        );
    }

    #[test]
    fn test_email_thread_topic_uses_message_id_without_subject() {
        let first = build_test_inbound("", Some("<first@example.test>"));
        let second = build_test_inbound("", Some("<second@example.test>"));

        assert_ne!(first.metadata["topic"], second.metadata["topic"]);
        assert_ne!(
            routed_email_session_key(&first),
            routed_email_session_key(&second)
        );
    }

    #[test]
    fn test_email_thread_topic_uses_references_for_subjectless_replies() {
        let original = build_test_inbound("", Some("<root@example.test>"));
        let reply = build_inbound_message(ParsedEmailMessage {
            from: "Sender <sender@example.com>".into(),
            subject: String::new(),
            message_id: Some("<reply@example.test>".into()),
            in_reply_to: Some("<root@example.test>".into()),
            references: Some("<root@example.test> <middle@example.test>".into()),
            text_body: "reply body".into(),
        });

        assert_eq!(original.metadata["topic"], reply.metadata["topic"]);
        assert_eq!(
            routed_email_session_key(&original),
            routed_email_session_key(&reply)
        );
    }

    #[test]
    fn test_email_thread_topic_is_gateway_safe() {
        let msg = build_test_inbound(
            "default: / # control\n subject with enough words to overflow any readable topic label",
            Some("<safe@example.test>"),
        );
        let topic = msg.metadata["topic"].as_str().unwrap();

        assert!(topic.starts_with("email-thread-"));
        assert!(topic.len() <= 50);
        assert!(!topic.chars().any(|c| matches!(c, '#' | ':' | '/')));
        assert!(!topic.chars().any(char::is_control));
    }

    fn build_test_inbound(subject: &str, message_id: Option<&str>) -> InboundMessage {
        build_inbound_message(ParsedEmailMessage {
            from: "Sender <sender@example.com>".into(),
            subject: subject.into(),
            message_id: message_id.map(String::from),
            in_reply_to: None,
            references: None,
            text_body: "body".into(),
        })
    }

    fn routed_email_session_key(msg: &InboundMessage) -> octos_core::SessionKey {
        let topic = msg.metadata["topic"].as_str().unwrap();
        octos_core::SessionKey::with_topic(&msg.channel, &msg.chat_id, topic)
    }
}
