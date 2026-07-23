//! Email notifications via SMTP with STARTTLS. `build_message` is split out
//! from `EmailNotifier::notify` so subject/body/address content is testable
//! without an actual SMTP connection.

use async_trait::async_trait;
use lettre::message::Mailbox;
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use thiserror::Error;

use super::{Notifier, NotifyError, format_event};
use crate::diff::ReleaseEvent;

/// Errors building an email `Message` from a release event.
#[derive(Debug, Error)]
pub enum EmailError {
    /// `from`/`to` did not parse as an RFC 5322 mailbox.
    #[error("invalid email address: {0}")]
    Address(#[from] lettre::address::AddressError),
    /// `lettre`'s message builder rejected the assembled message.
    #[error("could not build email message: {0}")]
    Message(#[from] lettre::error::Error),
}

/// Sends release events as plain-text email over SMTP with STARTTLS.
pub struct EmailNotifier {
    host: String,
    port: u16,
    username: String,
    password: String,
    from: String,
    to: String,
}

impl EmailNotifier {
    /// Builds a notifier for the given SMTP server and credentials.
    /// Connection details are only used lazily inside `notify`, so
    /// construction itself never fails.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        host: String,
        port: u16,
        username: String,
        password: String,
        from: String,
        to: String,
    ) -> Self {
        Self {
            host,
            port,
            username,
            password,
            from,
            to,
        }
    }
}

#[async_trait]
impl Notifier for EmailNotifier {
    fn name(&self) -> &'static str {
        "email"
    }

    async fn notify(&self, event: &ReleaseEvent) -> Result<(), NotifyError> {
        let message = build_message(event, &self.from, &self.to)
            .map_err(|e| NotifyError::Send(e.to_string()))?;
        self.send(message).await
    }
}

impl EmailNotifier {
    /// Builds the STARTTLS transport, attaching credentials only when a
    /// username is set. Servers that accept unauthenticated mail (localhost,
    /// internal relays) work with the username and password left blank.
    fn transport(&self) -> Result<AsyncSmtpTransport<Tokio1Executor>, NotifyError> {
        let mut builder = AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&self.host)
            .map_err(|e| NotifyError::Send(format!("could not configure SMTP transport: {e}")))?
            .port(self.port);
        if !self.username.trim().is_empty() {
            builder = builder.credentials(Credentials::new(
                self.username.clone(),
                self.password.clone(),
            ));
        }
        Ok(builder.build())
    }

    /// Sends an already-built message over the configured transport.
    async fn send(&self, message: Message) -> Result<(), NotifyError> {
        self.transport()?
            .send(message)
            .await
            .map_err(|e| NotifyError::Send(format!("SMTP send failed: {e}")))?;
        Ok(())
    }

    /// Sends a fixed test message so the dashboard can verify SMTP settings
    /// without waiting for a real release. Surfaces any transport, auth, or
    /// address error as `NotifyError::Send`.
    pub async fn send_test(&self) -> Result<(), NotifyError> {
        let message = Message::builder()
            .from(
                self.from
                    .parse::<Mailbox>()
                    .map_err(|e| NotifyError::Send(format!("invalid from address: {e}")))?,
            )
            .to(self
                .to
                .parse::<Mailbox>()
                .map_err(|e| NotifyError::Send(format!("invalid to address: {e}")))?)
            .subject("book-release-tracker test email")
            .header(ContentType::TEXT_PLAIN)
            .body(
                "This is a test email from book-release-tracker. \
                 If you received it, your SMTP settings work."
                    .to_string(),
            )
            .map_err(|e| NotifyError::Send(format!("could not build test email: {e}")))?;
        self.send(message).await
    }
}

/// Builds the email for `event`, addressed `from` -> `to`, using the shared
/// `format_event` title as the subject and its body as plain text. Kept free
/// of any transport so tests can assert on content directly.
pub fn build_message(event: &ReleaseEvent, from: &str, to: &str) -> Result<Message, EmailError> {
    let (subject, body) = format_event(event);
    let message = Message::builder()
        .from(from.parse::<Mailbox>()?)
        .to(to.parse::<Mailbox>()?)
        .subject(subject)
        .header(ContentType::TEXT_PLAIN)
        .body(body)?;
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::ReleaseEventData;
    use chrono::NaiveDate;

    fn event() -> ReleaseEvent {
        ReleaseEvent::NewRelease(ReleaseEventData {
            series_name: "The Expanse".into(),
            book_title: "Leviathan Wakes".into(),
            position: Some(1.0),
            release_date: NaiveDate::from_ymd_opt(2026, 7, 21).unwrap(),
            asin: None,
            narrators: Some(vec!["Jefferson Mays".into()]),
            runtime_minutes: Some(1177),
            summary: Some("A detective and a washed-up ship's officer.".into()),
        })
    }

    #[test]
    fn build_message_sets_subject_and_addresses() {
        let message = build_message(&event(), "tracker@example.com", "me@example.com")
            .expect("message should build");

        let headers = message.headers();
        assert_eq!(
            headers.get_raw("Subject"),
            Some("New release: Leviathan Wakes (The Expanse #1)")
        );
        assert_eq!(headers.get_raw("From"), Some("tracker@example.com"));
        assert_eq!(headers.get_raw("To"), Some("me@example.com"));
    }

    #[test]
    fn build_message_body_includes_date_and_enrichment() {
        let message = build_message(&event(), "tracker@example.com", "me@example.com")
            .expect("message should build");

        let formatted = String::from_utf8(message.formatted()).unwrap();
        assert!(formatted.contains("2026-07-21"));
        assert!(formatted.contains("Jefferson Mays"));
        assert!(formatted.contains("19h 37m"));
        assert!(formatted.contains("A detective and a washed-up ship's officer."));
    }

    #[test]
    fn build_message_body_omits_enrichment_when_absent() {
        let bare = ReleaseEvent::NewRelease(ReleaseEventData {
            series_name: "The Expanse".into(),
            book_title: "Leviathan Wakes".into(),
            position: Some(1.0),
            release_date: NaiveDate::from_ymd_opt(2026, 7, 21).unwrap(),
            asin: None,
            narrators: None,
            runtime_minutes: None,
            summary: None,
        });

        let message = build_message(&bare, "tracker@example.com", "me@example.com")
            .expect("message should build");

        let formatted = String::from_utf8(message.formatted()).unwrap();
        assert!(formatted.contains("2026-07-21"));
        assert!(!formatted.contains("Narrated by"));
        assert!(!formatted.contains("Runtime"));
    }

    #[tokio::test]
    async fn send_test_fails_fast_on_invalid_address() {
        // A blank "from" is rejected while building the message, before any
        // network connection is attempted, so this stays offline and quick.
        let notifier = EmailNotifier::new(
            "smtp.example.com".into(),
            587,
            String::new(),
            String::new(),
            String::new(),
            "me@example.com".into(),
        );

        let err = notifier
            .send_test()
            .await
            .expect_err("expected send failure");
        assert!(matches!(err, NotifyError::Send(_)));
    }

    #[test]
    fn build_message_rejects_invalid_address() {
        let err = build_message(&event(), "not-an-email", "me@example.com")
            .expect_err("expected address error");

        assert!(matches!(err, EmailError::Address(_)));
    }
}
