//! ntfy.sh push notifications: `POST {server}/{topic}` with the message body
//! as plain text and metadata carried in headers, per
//! <https://docs.ntfy.sh/publish/>.

use async_trait::async_trait;
use reqwest::header::HeaderValue;
use reqwest::{Client, Url};

use super::{Notifier, NotifyError, format_event};
use crate::diff::ReleaseEvent;

/// Pushes release events to an ntfy topic via HTTP POST.
pub struct NtfyNotifier {
    server: Url,
    topic: String,
    token: Option<String>,
    client: Client,
}

impl NtfyNotifier {
    /// Builds a notifier posting to `{server}/{topic}`, sending
    /// `Authorization: Bearer <token>` when `token` is present. `server` is
    /// explicit (rather than hardcoded to ntfy.sh) so tests can point it at a
    /// wiremock server.
    pub fn new(server: Url, topic: String, token: Option<String>) -> Self {
        Self {
            server,
            topic,
            token,
            client: Client::new(),
        }
    }
}

#[async_trait]
impl Notifier for NtfyNotifier {
    fn name(&self) -> &'static str {
        "ntfy"
    }

    async fn notify(&self, event: &ReleaseEvent) -> Result<(), NotifyError> {
        let (title, body) = format_event(event);
        let priority = match event {
            ReleaseEvent::NewRelease(_) | ReleaseEvent::Reminder(_) => "high",
            ReleaseEvent::Upcoming(_) => "default",
        };

        let url = self
            .server
            .join(&self.topic)
            .map_err(|e| NotifyError::Send(format!("invalid ntfy topic: {e}")))?;

        let mut request = self
            .client
            .post(url)
            .header("Title", sanitize_title(&title))
            .header("Tags", "books")
            .header("Priority", priority)
            .body(body);

        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }

        let response = request
            .send()
            .await
            .map_err(|e| NotifyError::Send(format!("ntfy request failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(200).collect();
            return Err(NotifyError::Send(format!(
                "ntfy returned {status}: {snippet}"
            )));
        }

        Ok(())
    }
}

/// Encodes `s` as an HTTP `Title` header value: strips line breaks (headers
/// must be single-line) and maps any codepoint outside Latin-1 to `?`, then
/// emits it as raw single-byte-per-char bytes rather than UTF-8, since ntfy
/// reads the `Title` header as ISO-8859-1 and would otherwise mangle
/// multi-byte UTF-8 sequences into mojibake.
fn sanitize_title(s: &str) -> HeaderValue {
    let bytes: Vec<u8> = s
        .chars()
        .filter(|c| !c.is_control() || *c == '\t')
        .map(|c| if (c as u32) <= 0xFF { c as u8 } else { b'?' })
        .collect();
    HeaderValue::from_bytes(&bytes).unwrap_or_else(|_| HeaderValue::from_static("(release)"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::ReleaseEventData;
    use chrono::NaiveDate;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    /// Matches requests that carry no `Authorization` header at all, since
    /// wiremock's built-in matchers only assert presence/value, not absence.
    struct NoAuthorizationHeader;

    impl wiremock::Match for NoAuthorizationHeader {
        fn matches(&self, request: &Request) -> bool {
            !request.headers.contains_key("authorization")
        }
    }

    fn event() -> ReleaseEvent {
        ReleaseEvent::NewRelease(ReleaseEventData {
            series_name: "The Expanse".into(),
            book_title: "Leviathan Wakes".into(),
            position: Some(1.0),
            release_date: NaiveDate::from_ymd_opt(2026, 7, 21).unwrap(),
            asin: None,
            narrators: None,
            runtime_minutes: None,
            summary: None,
        })
    }

    #[tokio::test]
    async fn posts_to_server_and_topic_with_expected_headers_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/my-topic"))
            .and(header(
                "Title",
                "New release: Leviathan Wakes (The Expanse #1)",
            ))
            .and(header("Tags", "books"))
            .and(header("Priority", "high"))
            .and(header("Authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let notifier = NtfyNotifier::new(
            Url::parse(&server.uri()).unwrap(),
            "my-topic".into(),
            Some("test-token".into()),
        );

        let result = notifier.notify(&event()).await;

        assert!(result.is_ok(), "expected success, got {result:?}");
    }

    #[tokio::test]
    async fn omits_authorization_header_when_no_token_configured() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/my-topic"))
            .and(NoAuthorizationHeader)
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let notifier =
            NtfyNotifier::new(Url::parse(&server.uri()).unwrap(), "my-topic".into(), None);

        let result = notifier.notify(&event()).await;

        assert!(result.is_ok(), "expected success, got {result:?}");
    }

    #[tokio::test]
    async fn upcoming_event_uses_default_priority() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/my-topic"))
            .and(header("Priority", "default"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let notifier =
            NtfyNotifier::new(Url::parse(&server.uri()).unwrap(), "my-topic".into(), None);
        let upcoming = ReleaseEvent::Upcoming(ReleaseEventData {
            series_name: "The Expanse".into(),
            book_title: "Babylon's Ashes".into(),
            position: Some(6.0),
            release_date: NaiveDate::from_ymd_opt(2026, 12, 1).unwrap(),
            asin: None,
            narrators: None,
            runtime_minutes: None,
            summary: None,
        });

        let result = notifier.notify(&upcoming).await;

        assert!(result.is_ok(), "expected success, got {result:?}");
    }

    #[tokio::test]
    async fn non_2xx_response_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/my-topic"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let notifier =
            NtfyNotifier::new(Url::parse(&server.uri()).unwrap(), "my-topic".into(), None);

        let err = notifier
            .notify(&event())
            .await
            .expect_err("expected an error for a 500 response");

        match err {
            NotifyError::Send(msg) => {
                assert!(msg.contains("500"));
                assert!(msg.contains("boom"));
            }
        }
    }
}
