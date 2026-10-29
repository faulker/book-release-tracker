//! Notification dispatch: the `Notifier` trait, `dispatch_all` fan-out, the
//! always-on `LogNotifier`, and `build_notifiers(&Config)`, which assembles
//! the full channel list (log + any enabled ntfy/email) for a check run.
//! Channel-specific notifiers live in the `ntfy` and `email` submodules;
//! `format_event` here gives them a shared title/body so every channel says
//! the same thing about a release.

pub mod email;
pub mod ntfy;

use async_trait::async_trait;
use reqwest::Url;
use thiserror::Error;

use crate::config::Config;
use crate::diff::{ReleaseEvent, ReleaseEventData};
use email::EmailNotifier;
use ntfy::NtfyNotifier;

/// Errors produced by a notifier attempting to deliver an event.
#[derive(Debug, Error)]
pub enum NotifyError {
    /// The notification could not be delivered (HTTP, SMTP, or other
    /// transport failure). Carries a human-readable cause for the log.
    #[error("notification delivery failed: {0}")]
    Send(String),
}

/// A notification channel. Implementations must be infallible to construct;
/// delivery failures are reported through `notify`'s `Result` and handled by
/// `dispatch_all`, never by panicking.
#[async_trait]
pub trait Notifier: Send + Sync {
    /// Short channel name used in logs (e.g. "log", "ntfy", "email").
    fn name(&self) -> &'static str;
    /// Delivers one release event to this channel.
    async fn notify(&self, event: &ReleaseEvent) -> Result<(), NotifyError>;
}

/// Sends `event` to every notifier in turn, logging each failure at error
/// level, and returns how many notifiers succeeded. Never panics and never
/// short-circuits: one channel failing must not block the others. Callers
/// stamp `*_notified_at` only when the returned count is at least 1, so a
/// total outage retries next run (PLAN.md section 4).
pub async fn dispatch_all(notifiers: &[Box<dyn Notifier>], event: &ReleaseEvent) -> usize {
    let mut succeeded = 0;
    for notifier in notifiers {
        match notifier.notify(event).await {
            Ok(()) => succeeded += 1,
            Err(e) => tracing::error!(
                notifier = notifier.name(),
                error = %e,
                "notifier failed to deliver event"
            ),
        }
    }
    succeeded
}

/// Always-on notifier that writes release events to the log at info level.
/// Useful on its own for cron + log-watching setups, and guarantees every
/// event is visible somewhere even with all other channels disabled.
pub struct LogNotifier;

#[async_trait]
impl Notifier for LogNotifier {
    fn name(&self) -> &'static str {
        "log"
    }

    async fn notify(&self, event: &ReleaseEvent) -> Result<(), NotifyError> {
        let (kind, data) = match event {
            ReleaseEvent::NewRelease(data) => ("new release", data),
            ReleaseEvent::Upcoming(data) => ("upcoming release", data),
            ReleaseEvent::Reminder(data) => ("release reminder", data),
        };
        tracing::info!(
            kind,
            series = %data.series_name,
            book = %data.book_title,
            position = ?data.position,
            release_date = %data.release_date,
            "release event"
        );
        Ok(())
    }
}

/// Builds the notifier list for a check run: `LogNotifier` is always first,
/// followed by `NtfyNotifier`/`EmailNotifier` for each `[notify.*]` block
/// with `enabled = true`. `config::validate` already guarantees an enabled
/// block's required fields are non-empty; an ntfy server that still isn't a
/// valid URL is logged and that channel is skipped rather than panicking.
pub fn build_notifiers(config: &Config) -> Vec<Box<dyn Notifier>> {
    let mut notifiers: Vec<Box<dyn Notifier>> = vec![Box::new(LogNotifier)];

    if config.notify.ntfy.enabled {
        match Url::parse(&config.notify.ntfy.server) {
            Ok(server) => {
                let token = (!config.notify.ntfy.token.is_empty())
                    .then(|| config.notify.ntfy.token.clone());
                notifiers.push(Box::new(NtfyNotifier::new(
                    server,
                    config.notify.ntfy.topic.clone(),
                    token,
                )));
            }
            Err(e) => tracing::error!(
                error = %e,
                server = %config.notify.ntfy.server,
                "notify.ntfy.server is not a valid URL; ntfy notifications disabled"
            ),
        }
    }

    if config.notify.email.enabled {
        notifiers.push(Box::new(EmailNotifier::new(
            config.notify.email.smtp_host.clone(),
            config.notify.email.smtp_port,
            config.notify.email.username.clone(),
            config.notify.email.password.clone(),
            config.notify.email.from.clone(),
            config.notify.email.to.clone(),
        )));
    }

    notifiers
}

/// Renders `event` as a notification title and body shared by every channel.
/// Title identifies the book and its place in the series (e.g. "New
/// release: Leviathan Wakes (The Expanse #1)"); body adds the release date
/// plus any narrator/runtime/summary enrichment that's present.
pub fn format_event(event: &ReleaseEvent) -> (String, String) {
    let (label, data) = match event {
        ReleaseEvent::NewRelease(data) => ("New release", data),
        ReleaseEvent::Upcoming(data) => ("Upcoming", data),
        ReleaseEvent::Reminder(data) => ("Release reminder", data),
    };

    let title = format!("{label}: {}{}", data.book_title, format_series_suffix(data));

    let mut lines = vec![format!("Release date: {}", data.release_date)];
    if let Some(narrators) = &data.narrators
        && !narrators.is_empty()
    {
        lines.push(format!("Narrated by: {}", narrators.join(", ")));
    }
    if let Some(minutes) = data.runtime_minutes {
        lines.push(format!("Runtime: {}", format_runtime(minutes)));
    }
    if let Some(summary) = &data.summary {
        lines.push(String::new());
        lines.push(summary.clone());
    }

    (title, lines.join("\n"))
}

/// " (Series #position)" or " (Series)" when there's no position to show.
fn format_series_suffix(data: &ReleaseEventData) -> String {
    match data.position {
        Some(position) => format!(" ({} #{})", data.series_name, format_position(position)),
        None => format!(" ({})", data.series_name),
    }
}

/// Formats a book position without a spurious trailing `.0` (e.g. `1` not
/// `1.0`), while keeping fractional novella positions like `1.5` as-is.
fn format_position(position: f64) -> String {
    if position.fract() == 0.0 {
        format!("{position:.0}")
    } else {
        position.to_string()
    }
}

/// Formats audiobook runtime minutes as `"XhYm"`/`"Ym"` for readability.
fn format_runtime(total_minutes: i64) -> String {
    let hours = total_minutes / 60;
    let minutes = total_minutes % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::ReleaseEventData;
    use chrono::NaiveDate;

    /// Test notifier with a fixed success/failure behavior.
    struct FixedNotifier {
        ok: bool,
    }

    #[async_trait]
    impl Notifier for FixedNotifier {
        fn name(&self) -> &'static str {
            "fixed"
        }

        async fn notify(&self, _event: &ReleaseEvent) -> Result<(), NotifyError> {
            if self.ok {
                Ok(())
            } else {
                Err(NotifyError::Send("simulated outage".into()))
            }
        }
    }

    fn sample_event() -> ReleaseEvent {
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
    async fn dispatch_all_counts_only_successes() {
        let notifiers: Vec<Box<dyn Notifier>> = vec![
            Box::new(FixedNotifier { ok: true }),
            Box::new(FixedNotifier { ok: false }),
            Box::new(FixedNotifier { ok: true }),
        ];

        let succeeded = dispatch_all(&notifiers, &sample_event()).await;

        assert_eq!(succeeded, 2);
    }

    #[tokio::test]
    async fn dispatch_all_returns_zero_when_every_notifier_fails() {
        let notifiers: Vec<Box<dyn Notifier>> = vec![
            Box::new(FixedNotifier { ok: false }),
            Box::new(FixedNotifier { ok: false }),
        ];

        let succeeded = dispatch_all(&notifiers, &sample_event()).await;

        assert_eq!(succeeded, 0);
    }

    #[tokio::test]
    async fn log_notifier_always_succeeds() {
        let result = LogNotifier.notify(&sample_event()).await;
        assert!(result.is_ok());
    }

    fn enriched_event(event: ReleaseEvent) -> ReleaseEventData {
        match event {
            ReleaseEvent::NewRelease(d) | ReleaseEvent::Upcoming(d) | ReleaseEvent::Reminder(d) => {
                d
            }
        }
    }

    #[test]
    fn format_event_new_release_includes_series_and_position() {
        let (title, body) = format_event(&sample_event());

        assert_eq!(title, "New release: Leviathan Wakes (The Expanse #1)");
        assert!(body.contains("Release date: 2026-07-21"));
    }

    #[test]
    fn format_event_upcoming_uses_upcoming_label() {
        let mut data = enriched_event(sample_event());
        data.release_date = NaiveDate::from_ymd_opt(2026, 12, 1).unwrap();
        let event = ReleaseEvent::Upcoming(data);

        let (title, body) = format_event(&event);

        assert_eq!(title, "Upcoming: Leviathan Wakes (The Expanse #1)");
        assert!(body.contains("Release date: 2026-12-01"));
    }

    #[test]
    fn format_event_reminder_uses_reminder_label() {
        let data = enriched_event(sample_event());
        let event = ReleaseEvent::Reminder(data);

        let (title, body) = format_event(&event);

        assert_eq!(title, "Release reminder: Leviathan Wakes (The Expanse #1)");
        assert!(body.contains("Release date: 2026-07-21"));
    }

    #[test]
    fn format_event_omits_position_suffix_when_no_position() {
        let mut data = enriched_event(sample_event());
        data.position = None;
        let event = ReleaseEvent::NewRelease(data);

        let (title, _) = format_event(&event);

        assert_eq!(title, "New release: Leviathan Wakes (The Expanse)");
    }

    #[test]
    fn format_event_body_includes_enrichment_when_present() {
        let mut data = enriched_event(sample_event());
        data.narrators = Some(vec!["Jefferson Mays".into()]);
        data.runtime_minutes = Some(1177);
        data.summary = Some("A detective story.".into());
        let event = ReleaseEvent::NewRelease(data);

        let (_, body) = format_event(&event);

        assert!(body.contains("Narrated by: Jefferson Mays"));
        assert!(body.contains("Runtime: 19h 37m"));
        assert!(body.contains("A detective story."));
    }

    #[test]
    fn format_event_body_omits_enrichment_when_absent() {
        let (_, body) = format_event(&sample_event());

        assert!(!body.contains("Narrated by"));
        assert!(!body.contains("Runtime"));
    }

    /// Config with every notifier disabled and dummy-but-valid required
    /// fields, so individual tests only need to flip what they're testing.
    fn base_config() -> crate::config::Config {
        use crate::config::{
            AudiobookshelfConfig, AudnexusConfig, CheckConfig, Config, DashboardConfig,
            EmailConfig, HardcoverConfig, NotifyConfig, NtfyConfig,
        };
        Config {
            audiobookshelf: AudiobookshelfConfig {
                base_url: "https://example.com".into(),
                api_key: "key".into(),
                library_ids: vec![],
                startup_timeout_secs: 300,
            },
            hardcover: HardcoverConfig {
                token: "tok".into(),
                min_request_interval_ms: 1100,
                max_retries: 3,
                language: String::new(),
            },
            audnexus: AudnexusConfig {
                enabled: false,
                region: "us".into(),
            },
            check: CheckConfig {
                interval_days: 7,
                db_path: "test.db".into(),
                run_timeout_mins: 120,
            },
            dashboard: DashboardConfig {
                enabled: false,
                bind: "127.0.0.1:8091".into(),
            },
            notify: NotifyConfig {
                ntfy: NtfyConfig {
                    enabled: false,
                    server: String::new(),
                    topic: String::new(),
                    token: String::new(),
                },
                email: EmailConfig {
                    enabled: false,
                    smtp_host: String::new(),
                    smtp_port: 587,
                    username: String::new(),
                    password: String::new(),
                    from: String::new(),
                    to: String::new(),
                },
            },
        }
    }

    fn notifier_names(notifiers: &[Box<dyn Notifier>]) -> Vec<&'static str> {
        notifiers.iter().map(|n| n.name()).collect()
    }

    #[test]
    fn build_notifiers_is_log_only_when_all_channels_disabled() {
        let notifiers = build_notifiers(&base_config());

        assert_eq!(notifier_names(&notifiers), vec!["log"]);
    }

    #[test]
    fn build_notifiers_adds_ntfy_when_enabled() {
        let mut config = base_config();
        config.notify.ntfy.enabled = true;
        config.notify.ntfy.server = "https://ntfy.sh".into();
        config.notify.ntfy.topic = "my-topic".into();

        let notifiers = build_notifiers(&config);

        assert_eq!(notifier_names(&notifiers), vec!["log", "ntfy"]);
    }

    #[test]
    fn build_notifiers_adds_email_when_enabled() {
        let mut config = base_config();
        config.notify.email.enabled = true;
        config.notify.email.smtp_host = "smtp.example.com".into();
        config.notify.email.username = "user".into();
        config.notify.email.password = "pass".into();
        config.notify.email.from = "tracker@example.com".into();
        config.notify.email.to = "me@example.com".into();

        let notifiers = build_notifiers(&config);

        assert_eq!(notifier_names(&notifiers), vec!["log", "email"]);
    }

    #[test]
    fn build_notifiers_adds_both_when_both_enabled() {
        let mut config = base_config();
        config.notify.ntfy.enabled = true;
        config.notify.ntfy.server = "https://ntfy.sh".into();
        config.notify.ntfy.topic = "my-topic".into();
        config.notify.email.enabled = true;
        config.notify.email.smtp_host = "smtp.example.com".into();
        config.notify.email.username = "user".into();
        config.notify.email.password = "pass".into();
        config.notify.email.from = "tracker@example.com".into();
        config.notify.email.to = "me@example.com".into();

        let notifiers = build_notifiers(&config);

        assert_eq!(notifier_names(&notifiers), vec!["log", "ntfy", "email"]);
    }

    #[test]
    fn build_notifiers_skips_ntfy_with_an_invalid_server_url() {
        let mut config = base_config();
        config.notify.ntfy.enabled = true;
        config.notify.ntfy.server = "not a url".into();
        config.notify.ntfy.topic = "my-topic".into();

        let notifiers = build_notifiers(&config);

        assert_eq!(notifier_names(&notifiers), vec!["log"]);
    }
}
