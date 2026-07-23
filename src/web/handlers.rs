//! One handler per dashboard page, plus the override and run-now form posts.
//! Handlers pre-format everything into plain-string view rows so the askama
//! templates stay logic-free.

use std::collections::HashMap;

use askama::Template;
use askama_web::WebTemplate;
use axum::Form;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{DateTime, NaiveDate, Utc};
use serde::Deserialize;

use crate::checker::ScanMode;
use crate::config;
use crate::db::models::{CheckRunStatus, KnownRelease, MatchStatus, Series};
use crate::db::queries::{self, QueryError};
use crate::matching::normalize_series_name;

use super::AppState;

/// How many check runs the status page shows.
const RUN_HISTORY_LIMIT: i64 = 20;

/// Flash token appended to `/` when a run-now press finds a check already
/// running or queued.
const FLASH_ALREADY_RUNNING: &str = "check-already-running";

/// Handler-side failure. Everything the dashboard can't render becomes a
/// plain 500 with the cause in the body; there is no partial page state
/// worth preserving.
#[derive(Debug, thiserror::Error)]
pub enum WebError {
    #[error("database error: {0}")]
    Db(#[from] QueryError),
    #[error("config error: {0}")]
    Config(#[from] crate::config::ConfigError),
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        tracing::error!(error = %self, "dashboard request failed");
        (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()).into_response()
    }
}

/// Status page: recent check runs, headline counts, and configured interval.
#[derive(Template, WebTemplate)]
#[template(path = "index.html")]
pub struct IndexTemplate {
    flash: String,
    /// Human-friendly interval phrase, e.g. "every week".
    interval_label: String,
    series_count: usize,
    upcoming_count: usize,
    new_count: usize,
    watched_count: usize,
    unmatched_count: usize,
    runs: Vec<RunRow>,
}

/// One `check_runs` row, pre-formatted for the status table.
struct RunRow {
    status: &'static str,
    started_at: String,
    finished_at: String,
    series_checked: i64,
    events_found: i64,
    errors: String,
}

/// Optional `?flash=` token on the status page (set by `POST /check`).
#[derive(Debug, Deserialize)]
pub struct IndexQuery {
    flash: Option<String>,
}

/// `GET /`: check-run history plus upcoming/new/unmatched counts.
pub async fn index(
    State(state): State<AppState>,
    Query(query): Query<IndexQuery>,
) -> Result<IndexTemplate, WebError> {
    let runs = queries::recent_check_runs(&state.pool, RUN_HISTORY_LIMIT).await?;
    let series_count = queries::list_series(&state.pool).await?.len();
    let upcoming_count = queries::list_upcoming_releases(&state.pool).await?.len();
    let new_count = queries::list_new_releases(&state.pool).await?.len();
    let watched_count = queries::watched_release_ids(&state.pool).await?.len();
    let unmatched_count = queries::get_series_by_match_status(&state.pool, MatchStatus::Unmatched)
        .await?
        .len();

    let flash = match query.flash.as_deref() {
        Some(FLASH_ALREADY_RUNNING) => "A check is already running or queued.".to_string(),
        _ => String::new(),
    };

    Ok(IndexTemplate {
        flash,
        interval_label: fmt_interval_days(state.interval.as_secs() / 86_400),
        series_count,
        upcoming_count,
        new_count,
        watched_count,
        unmatched_count,
        runs: runs
            .iter()
            .map(|run| RunRow {
                status: run_status_label(run.status),
                started_at: fmt_time(&run.started_at),
                finished_at: run.finished_at.as_ref().map(fmt_time).unwrap_or_default(),
                series_checked: run.series_checked,
                events_found: run.events_found,
                errors: fmt_errors(run.errors.as_deref()),
            })
            .collect(),
    })
}

/// Upcoming or new releases, pre-formatted for the shared table layout.
/// `id`/`watched` feed the upcoming page's watch buttons. The `*_url` fields
/// are empty strings when no link target is known; templates fall back to
/// plain text.
struct ReleaseRow {
    id: i64,
    watched: bool,
    series: String,
    /// Hardcover series page; empty when the series has no stored slug.
    series_url: String,
    /// Comma-joined author names, feeding the client-side text filter.
    author: String,
    /// One display link per author.
    authors: Vec<AuthorLink>,
    title: String,
    /// Audible product page for the release; empty when no ASIN is known.
    book_url: String,
    position: String,
    release_date: String,
    when: String,
}

/// One linked author name; `url` points at an Audible author search.
struct AuthorLink {
    name: String,
    url: String,
}

/// Upcoming-releases page.
#[derive(Template, WebTemplate)]
#[template(path = "upcoming.html")]
pub struct UpcomingTemplate {
    rows: Vec<ReleaseRow>,
}

/// `GET /upcoming`: releases with status `upcoming`, soonest first, unknown
/// dates last.
pub async fn upcoming(State(state): State<AppState>) -> Result<UpcomingTemplate, WebError> {
    let releases = queries::list_upcoming_releases(&state.pool).await?;
    let rows = release_rows(&state.pool, &releases).await?;
    Ok(UpcomingTemplate { rows })
}

/// New-releases page.
#[derive(Template, WebTemplate)]
#[template(path = "new.html")]
pub struct NewTemplate {
    rows: Vec<ReleaseRow>,
}

/// `GET /new`: releases with status `released`, newest first.
pub async fn new_releases(State(state): State<AppState>) -> Result<NewTemplate, WebError> {
    let releases = queries::list_new_releases(&state.pool).await?;
    let rows = release_rows(&state.pool, &releases).await?;
    Ok(NewTemplate { rows })
}

/// Builds display rows for a list of releases, resolving each `series_id` to
/// its ABS name and owned-book authors via one series scan (the table is
/// small).
async fn release_rows(
    pool: &sqlx::SqlitePool,
    releases: &[KnownRelease],
) -> Result<Vec<ReleaseRow>, WebError> {
    let series_info: HashMap<i64, (String, Option<String>)> = queries::list_series(pool)
        .await?
        .into_iter()
        .map(|series| (series.id, (series.abs_name, series.hardcover_slug)))
        .collect();
    let authors = queries::authors_by_series(pool).await?;
    let watched = queries::watched_release_ids(pool).await?;

    let today = Utc::now().date_naive();
    Ok(releases
        .iter()
        .map(|release| {
            let (series, slug) = series_info
                .get(&release.series_id)
                .cloned()
                .unwrap_or_else(|| (format!("series #{}", release.series_id), None));
            ReleaseRow {
                id: release.id,
                watched: watched.contains(&release.id),
                series,
                series_url: series_url(slug.as_deref()),
                author: authors
                    .get(&release.series_id)
                    .map(|joined| fmt_authors(joined))
                    .unwrap_or_default(),
                authors: authors
                    .get(&release.series_id)
                    .map(|joined| author_links(joined))
                    .unwrap_or_default(),
                title: release.title.clone(),
                book_url: book_url(release.asin.as_deref()),
                position: fmt_position(release.position),
                release_date: release
                    .release_date
                    .map(|date| date.to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
                when: release
                    .release_date
                    .map(|date| fmt_days_until(days_until(today, date)))
                    .unwrap_or_default(),
            }
        })
        .collect())
}

/// Hardcover series page URL, or empty when no slug is stored.
fn series_url(slug: Option<&str>) -> String {
    slug.map(|slug| format!("https://hardcover.app/series/{slug}"))
        .unwrap_or_default()
}

/// Audible product page URL for an ASIN, or empty when none is known.
fn book_url(asin: Option<&str>) -> String {
    asin.map(|asin| format!("https://www.audible.com/pd/{asin}"))
        .unwrap_or_default()
}

/// Splits SQL's comma-joined author list ("A,B") into display links, each
/// pointing at an Audible author search with the name percent-encoded.
fn author_links(joined: &str) -> Vec<AuthorLink> {
    joined
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| AuthorLink {
            url: reqwest::Url::parse_with_params(
                "https://www.audible.com/search",
                [("searchAuthor", name)],
            )
            .map(|url| url.to_string())
            .unwrap_or_default(),
            name: name.to_string(),
        })
        .collect()
}

/// Watchlist page: releases the user asked to be reminded about.
#[derive(Template, WebTemplate)]
#[template(path = "watchlist.html")]
pub struct WatchlistTemplate {
    rows: Vec<WatchRow>,
}

/// One watched release, pre-formatted for the watchlist table. The signed
/// reminder offset is split into magnitude + direction for the edit form's
/// number input and before/after select.
struct WatchRow {
    id: i64,
    series: String,
    title: String,
    position: String,
    release_date: String,
    when: String,
    remind_days: i64,
    remind_after: bool,
    reminded: bool,
}

/// `GET /watchlist`: every watched release, soonest first.
pub async fn watchlist(State(state): State<AppState>) -> Result<WatchlistTemplate, WebError> {
    let watches = queries::list_watched_releases(&state.pool).await?;

    let today = Utc::now().date_naive();
    Ok(WatchlistTemplate {
        rows: watches
            .into_iter()
            .map(|watch| WatchRow {
                id: watch.known_release_id,
                series: watch.series_name,
                title: watch.title,
                position: fmt_position(watch.position),
                release_date: watch
                    .release_date
                    .map(|date| date.to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
                when: watch
                    .release_date
                    .map(|date| fmt_days_until(days_until(today, date)))
                    .unwrap_or_default(),
                remind_days: watch.remind_offset_days.abs(),
                remind_after: watch.remind_offset_days > 0,
                reminded: watch.reminder_notified_at.is_some(),
            })
            .collect(),
    })
}

/// Form body for `POST /watch` and `POST /watch/remove`. `back` names the
/// page the button lives on so the redirect returns there.
#[derive(Debug, Deserialize)]
pub struct WatchForm {
    pub release_id: i64,
    /// Days from the release date the reminder should fire (magnitude).
    #[serde(default)]
    pub remind_days: Option<i64>,
    /// "before" (default) or "after" the release date.
    #[serde(default)]
    pub remind_when: Option<String>,
    #[serde(default)]
    pub back: String,
}

/// Only redirect to pages the watch buttons actually live on; anything else
/// lands on the watchlist.
fn watch_redirect(back: &str) -> Redirect {
    match back {
        "/upcoming" | "/watchlist" => Redirect::to(back),
        _ => Redirect::to("/watchlist"),
    }
}

/// `POST /watch`: adds a release to the watch list (or updates its reminder
/// window). Rejects unknown release ids, out-of-range windows, and bad
/// directions. The signed offset stored is negative for "before" (default)
/// and positive for "after".
pub async fn add_watch(
    State(state): State<AppState>,
    Form(form): Form<WatchForm>,
) -> Result<Response, WebError> {
    let days = form.remind_days.unwrap_or(1);
    if !(0..=365).contains(&days) {
        return Ok((
            StatusCode::BAD_REQUEST,
            "remind_days must be between 0 and 365",
        )
            .into_response());
    }
    let offset = match form.remind_when.as_deref().unwrap_or("before") {
        "before" => -days,
        "after" => days,
        _ => {
            return Ok((
                StatusCode::BAD_REQUEST,
                "remind_when must be \"before\" or \"after\"",
            )
                .into_response());
        }
    };
    if queries::get_known_release(&state.pool, form.release_id)
        .await?
        .is_none()
    {
        return Ok((StatusCode::NOT_FOUND, "no such release").into_response());
    }

    queries::add_watch(&state.pool, form.release_id, offset).await?;
    Ok(watch_redirect(&form.back).into_response())
}

/// `POST /watch/remove`: drops a release from the watch list.
pub async fn remove_watch(
    State(state): State<AppState>,
    Form(form): Form<WatchForm>,
) -> Result<Response, WebError> {
    queries::remove_watch(&state.pool, form.release_id).await?;
    Ok(watch_redirect(&form.back).into_response())
}

/// Series overview page: every tracked series with its release outlook and
/// ignore/unignore actions.
#[derive(Template, WebTemplate)]
#[template(path = "series.html")]
pub struct SeriesTemplate {
    rows: Vec<SeriesRow>,
}

/// One series on the overview page, pre-formatted. When a next release is
/// scheduled, `next_*` describe it; otherwise `last_release` carries the most
/// recent past release date (the "nothing coming" case the page must always
/// show a last date for, when one is known).
struct SeriesRow {
    abs_name: String,
    authors: String,
    status: &'static str,
    ignored: bool,
    /// "1" when a next release is scheduled, "0" otherwise; rendered as a
    /// data attribute the hide-unscheduled toggle filters on.
    scheduled: &'static str,
    next_title: String,
    next_date: String,
    next_when: String,
    last_release: String,
}

/// `GET /series`: all tracked series with authors, next scheduled release,
/// and last release date.
pub async fn series(State(state): State<AppState>) -> Result<SeriesTemplate, WebError> {
    let today = Utc::now().date_naive();
    let overview = queries::list_series_overview(&state.pool, today).await?;

    Ok(SeriesTemplate {
        rows: overview
            .into_iter()
            .map(|row| {
                let has_next = row.next_release_date.is_some();
                SeriesRow {
                    authors: row.authors.as_deref().map(fmt_authors).unwrap_or_default(),
                    status: match_status_label(row.match_status),
                    ignored: row.match_status == MatchStatus::Ignored,
                    scheduled: if has_next { "1" } else { "0" },
                    next_title: row.next_title.unwrap_or_default(),
                    next_date: row
                        .next_release_date
                        .map(|date| date.to_string())
                        .unwrap_or_default(),
                    next_when: row
                        .next_release_date
                        .map(|date| fmt_days_until(days_until(today, date)))
                        .unwrap_or_default(),
                    // Only shown when nothing is scheduled; a dash when the
                    // series has no known past release either.
                    last_release: if has_next {
                        String::new()
                    } else {
                        row.last_release_date
                            .map(|date| date.to_string())
                            .unwrap_or_else(|| "—".to_string())
                    },
                    abs_name: row.abs_name,
                }
            })
            .collect(),
    })
}

/// Form body for `POST /series/ignore`.
#[derive(Debug, Deserialize)]
pub struct IgnoreForm {
    pub abs_name: String,
    /// "ignore" or "unignore".
    pub action: String,
}

/// `POST /series/ignore`: toggles the ignore override for a series by ABS
/// name. Ignoring stores an ignore override (so it survives re-syncs) and
/// marks matching rows ignored immediately; unignoring clears the override
/// and resets the rows to `unmatched` so the next check re-matches them.
pub async fn set_ignore(
    State(state): State<AppState>,
    Form(form): Form<IgnoreForm>,
) -> Result<Response, WebError> {
    let ignore = match form.action.as_str() {
        "ignore" => true,
        "unignore" => false,
        _ => {
            return Ok(
                (StatusCode::BAD_REQUEST, "action must be ignore or unignore").into_response(),
            );
        }
    };

    let normalized = normalize_series_name(&form.abs_name);
    queries::set_series_override(&state.pool, &normalized, None, ignore).await?;

    // Reflect the decision immediately instead of waiting for the next check.
    // Normalization happens in Rust, not SQL, so scan the (small) series
    // table for rows whose normalized name matches the override key.
    let status = if ignore {
        MatchStatus::Ignored
    } else {
        MatchStatus::Unmatched
    };
    for series in queries::list_series(&state.pool).await? {
        if normalize_series_name(&series.abs_name) == normalized {
            queries::set_series_match(&state.pool, series.id, None, None, None, status).await?;
        }
    }

    Ok(Redirect::to("/series").into_response())
}

/// Unmatched-series page with one override form per row.
#[derive(Template, WebTemplate)]
#[template(path = "unmatched.html")]
pub struct UnmatchedTemplate {
    rows: Vec<UnmatchedRow>,
}

/// One series awaiting a manual match decision (or already ignored).
struct UnmatchedRow {
    abs_name: String,
    status: &'static str,
}

/// `GET /unmatched`: series the matcher could not resolve, plus ignored ones
/// so an ignore can be reversed by entering an id.
pub async fn unmatched(State(state): State<AppState>) -> Result<UnmatchedTemplate, WebError> {
    let mut series =
        queries::get_series_by_match_status(&state.pool, MatchStatus::Unmatched).await?;
    series.extend(queries::get_series_by_match_status(&state.pool, MatchStatus::Ignored).await?);

    Ok(UnmatchedTemplate {
        rows: series
            .into_iter()
            .map(|series: Series| UnmatchedRow {
                abs_name: series.abs_name,
                status: match_status_label(series.match_status),
            })
            .collect(),
    })
}

/// Form body for `POST /overrides`. `ignore` is a checkbox, so it's absent
/// entirely when unchecked.
#[derive(Debug, Deserialize)]
pub struct OverrideForm {
    pub abs_name: String,
    #[serde(default)]
    pub hardcover_series_id: String,
    #[serde(default)]
    pub ignore: Option<String>,
}

/// `POST /overrides`: stores a manual series override keyed by normalized ABS
/// name, resets any series with that name back to `unmatched` so the next
/// check re-runs matching (and picks the override up), then redirects back.
pub async fn save_override(
    State(state): State<AppState>,
    Form(form): Form<OverrideForm>,
) -> Result<Response, WebError> {
    let ignore = form.ignore.is_some();
    let hardcover_series_id = form.hardcover_series_id.trim();
    if !ignore && hardcover_series_id.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            "enter a Hardcover series id or tick ignore",
        )
            .into_response());
    }

    // ignore=1 wins in the checker, so don't store a dangling id next to it.
    let stored_id = (!ignore).then_some(hardcover_series_id);
    let normalized = normalize_series_name(&form.abs_name);
    queries::set_series_override(&state.pool, &normalized, stored_id, ignore).await?;

    // Normalization happens in Rust, not SQL, so scan the (small) series
    // table for rows whose normalized name matches the override key.
    for series in queries::list_series(&state.pool).await? {
        if normalize_series_name(&series.abs_name) == normalized {
            queries::set_series_match(
                &state.pool,
                series.id,
                None,
                None,
                None,
                MatchStatus::Unmatched,
            )
            .await?;
        }
    }

    Ok(Redirect::to("/unmatched").into_response())
}

/// Form body for `POST /check`. `mode` defaults to a partial scan; the full
/// scan button posts `mode=full`.
#[derive(Debug, Deserialize)]
pub struct CheckForm {
    #[serde(default)]
    pub mode: String,
}

/// `POST /check`: signals the scheduler to run a check now, partial by
/// default or full when requested. A full channel means a check is already
/// running or queued; surface that as a flash message instead of queueing
/// further runs.
pub async fn run_now(State(state): State<AppState>, Form(form): Form<CheckForm>) -> Redirect {
    let mode = if form.mode == "full" {
        ScanMode::Full
    } else {
        ScanMode::Partial
    };
    match state.run_tx.try_send(mode) {
        Ok(()) => Redirect::to("/"),
        Err(_) => Redirect::to(&format!("/?flash={FLASH_ALREADY_RUNNING}")),
    }
}

/// Settings page: every `config.toml` field as a form. Secrets are never
/// echoed back; `*_set` flags only drive the "currently set" placeholder.
#[derive(Template, WebTemplate)]
#[template(path = "settings.html")]
pub struct SettingsTemplate {
    flash: String,
    /// Validation problems to show in an error banner; empty on success.
    errors: Vec<String>,
    /// Form field ids that failed validation, for inline highlighting.
    error_fields: std::collections::HashSet<String>,
    config_path: String,
    abs_base_url: String,
    abs_api_key_set: bool,
    abs_library_ids: String,
    abs_startup_timeout_secs: u64,
    hc_token_set: bool,
    hc_min_request_interval_ms: u64,
    hc_max_retries: u32,
    hc_language: String,
    audnexus_enabled: bool,
    audnexus_region: String,
    interval_options: Vec<IntervalOption>,
    check_db_path: String,
    dashboard_enabled: bool,
    dashboard_bind: String,
    ntfy_enabled: bool,
    ntfy_server: String,
    ntfy_topic: String,
    ntfy_token_set: bool,
    email_enabled: bool,
    email_smtp_host: String,
    email_smtp_port: u16,
    email_username: String,
    email_password_set: bool,
    email_from: String,
    email_to: String,
}

/// Flash token appended to `/settings` after a successful save.
const FLASH_SETTINGS_SAVED: &str = "saved";

/// One entry in the check-frequency dropdown.
struct IntervalOption {
    value: u64,
    label: String,
    selected: bool,
}

/// Human-friendly interval presets (in days) offered by the settings
/// dropdown and used to phrase the status page.
const INTERVAL_PRESETS: &[(u64, &str)] = &[
    (1, "Every day"),
    (2, "Every 2 days"),
    (3, "Every 3 days"),
    (7, "Every week"),
    (14, "Every 2 weeks"),
    (30, "Every month"),
];

/// Builds the dropdown options, marking `current` selected. A non-preset
/// current value is preserved as its own leading option so an out-of-band
/// edit to `config.toml` isn't silently overwritten on the next save.
fn interval_options(current: u64) -> Vec<IntervalOption> {
    let mut options: Vec<IntervalOption> = INTERVAL_PRESETS
        .iter()
        .map(|(value, label)| IntervalOption {
            value: *value,
            label: (*label).to_string(),
            selected: *value == current,
        })
        .collect();
    if !INTERVAL_PRESETS.iter().any(|(value, _)| *value == current) {
        options.insert(
            0,
            IntervalOption {
                value: current,
                label: format!("Every {current} days"),
                selected: true,
            },
        );
    }
    options
}

/// Phrases a day interval for the status page, e.g. "every week".
fn fmt_interval_days(days: u64) -> String {
    match days {
        0 | 1 => "every day".to_string(),
        7 => "every week".to_string(),
        14 => "every 2 weeks".to_string(),
        30 => "every month".to_string(),
        d => format!("every {d} days"),
    }
}

/// Maps validation problem messages (which start with the dotted config
/// path) to the form field ids they came from, so those inputs can be
/// highlighted. Unmapped problems still show in the error banner.
fn error_field_ids(problems: &[String]) -> std::collections::HashSet<String> {
    const MAP: &[(&str, &str)] = &[
        ("audiobookshelf.base_url", "abs_base_url"),
        ("audiobookshelf.api_key", "abs_api_key"),
        ("hardcover.token", "hc_token"),
        ("notify.ntfy.server", "ntfy_server"),
        ("notify.ntfy.topic", "ntfy_topic"),
        ("notify.email.smtp_host", "email_smtp_host"),
        ("notify.email.password", "email_password"),
        ("notify.email.from", "email_from"),
        ("notify.email.to", "email_to"),
    ];
    let mut fields = std::collections::HashSet::new();
    for problem in problems {
        if let Some((_, field)) = MAP.iter().find(|(prefix, _)| problem.starts_with(prefix)) {
            fields.insert((*field).to_string());
        }
    }
    fields
}

/// Builds the settings view from the config file's contents (not the running
/// daemon's config, which may differ until restart). `errors`/`error_fields`
/// carry validation feedback when re-rendering after a rejected save.
fn settings_view(
    config: &config::Config,
    config_path: &str,
    flash: String,
    errors: Vec<String>,
    error_fields: std::collections::HashSet<String>,
) -> SettingsTemplate {
    SettingsTemplate {
        flash,
        errors,
        error_fields,
        config_path: config_path.to_string(),
        abs_base_url: config.audiobookshelf.base_url.clone(),
        abs_api_key_set: !config.audiobookshelf.api_key.trim().is_empty(),
        abs_library_ids: config.audiobookshelf.library_ids.join(", "),
        abs_startup_timeout_secs: config.audiobookshelf.startup_timeout_secs,
        hc_token_set: !config.hardcover.token.trim().is_empty(),
        hc_min_request_interval_ms: config.hardcover.min_request_interval_ms,
        hc_max_retries: config.hardcover.max_retries,
        hc_language: config.hardcover.language.clone(),
        audnexus_enabled: config.audnexus.enabled,
        audnexus_region: config.audnexus.region.clone(),
        interval_options: interval_options(config.check.interval_days),
        check_db_path: config.check.db_path.clone(),
        dashboard_enabled: config.dashboard.enabled,
        dashboard_bind: config.dashboard.bind.clone(),
        ntfy_enabled: config.notify.ntfy.enabled,
        ntfy_server: config.notify.ntfy.server.clone(),
        ntfy_topic: config.notify.ntfy.topic.clone(),
        ntfy_token_set: !config.notify.ntfy.token.trim().is_empty(),
        email_enabled: config.notify.email.enabled,
        email_smtp_host: config.notify.email.smtp_host.clone(),
        email_smtp_port: config.notify.email.smtp_port,
        email_username: config.notify.email.username.clone(),
        email_password_set: !config.notify.email.password.trim().is_empty(),
        email_from: config.notify.email.from.clone(),
        email_to: config.notify.email.to.clone(),
    }
}

/// Optional `?flash=` token on the settings page (set by `POST /settings`).
#[derive(Debug, Deserialize)]
pub struct SettingsQuery {
    flash: Option<String>,
}

/// `GET /settings`: renders the config file as an editable form.
pub async fn settings(
    State(state): State<AppState>,
    Query(query): Query<SettingsQuery>,
) -> Result<SettingsTemplate, WebError> {
    let config = config::load_file(&state.config_path)?;
    let flash = match query.flash.as_deref() {
        Some(FLASH_SETTINGS_SAVED) => {
            "Settings saved. Restart the daemon to apply them.".to_string()
        }
        _ => String::new(),
    };
    Ok(settings_view(
        &config,
        &state.config_path.display().to_string(),
        flash,
        Vec::new(),
        std::collections::HashSet::new(),
    ))
}

/// Form body for `POST /settings`. Checkboxes are absent when unchecked;
/// secret fields left blank mean "keep the current value".
#[derive(Debug, Deserialize)]
pub struct SettingsForm {
    pub abs_base_url: String,
    #[serde(default)]
    pub abs_api_key: String,
    #[serde(default)]
    pub abs_library_ids: String,
    pub abs_startup_timeout_secs: u64,
    #[serde(default)]
    pub hc_token: String,
    pub hc_min_request_interval_ms: u64,
    pub hc_max_retries: u32,
    #[serde(default)]
    pub hc_language: String,
    #[serde(default)]
    pub audnexus_enabled: Option<String>,
    pub audnexus_region: String,
    pub check_interval_days: u64,
    pub check_db_path: String,
    #[serde(default)]
    pub dashboard_enabled: Option<String>,
    pub dashboard_bind: String,
    #[serde(default)]
    pub ntfy_enabled: Option<String>,
    #[serde(default)]
    pub ntfy_server: String,
    #[serde(default)]
    pub ntfy_topic: String,
    #[serde(default)]
    pub ntfy_token: String,
    #[serde(default)]
    pub email_enabled: Option<String>,
    #[serde(default)]
    pub email_smtp_host: String,
    pub email_smtp_port: u16,
    #[serde(default)]
    pub email_username: String,
    #[serde(default)]
    pub email_password: String,
    #[serde(default)]
    pub email_from: String,
    #[serde(default)]
    pub email_to: String,
}

/// Overlays a submitted settings form onto `config`. Blank secret fields keep
/// their stored values; everything else is replaced. Shared by the save and
/// test-email handlers so both interpret the form identically.
fn apply_settings_form(config: &mut config::Config, form: &SettingsForm) {
    config.audiobookshelf.base_url = form.abs_base_url.trim().to_string();
    if !form.abs_api_key.trim().is_empty() {
        config.audiobookshelf.api_key = form.abs_api_key.trim().to_string();
    }
    config.audiobookshelf.library_ids = form
        .abs_library_ids
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect();
    config.audiobookshelf.startup_timeout_secs = form.abs_startup_timeout_secs;

    if !form.hc_token.trim().is_empty() {
        config.hardcover.token = form.hc_token.trim().to_string();
    }
    config.hardcover.min_request_interval_ms = form.hc_min_request_interval_ms;
    config.hardcover.max_retries = form.hc_max_retries;
    config.hardcover.language = form.hc_language.trim().to_string();

    config.audnexus.enabled = form.audnexus_enabled.is_some();
    config.audnexus.region = form.audnexus_region.trim().to_string();

    config.check.interval_days = form.check_interval_days.max(1);
    config.check.db_path = form.check_db_path.trim().to_string();

    config.dashboard.enabled = form.dashboard_enabled.is_some();
    config.dashboard.bind = form.dashboard_bind.trim().to_string();

    config.notify.ntfy.enabled = form.ntfy_enabled.is_some();
    config.notify.ntfy.server = form.ntfy_server.trim().to_string();
    config.notify.ntfy.topic = form.ntfy_topic.trim().to_string();
    if !form.ntfy_token.trim().is_empty() {
        config.notify.ntfy.token = form.ntfy_token.trim().to_string();
    }

    config.notify.email.enabled = form.email_enabled.is_some();
    config.notify.email.smtp_host = form.email_smtp_host.trim().to_string();
    config.notify.email.smtp_port = form.email_smtp_port;
    config.notify.email.username = form.email_username.trim().to_string();
    if !form.email_password.trim().is_empty() {
        config.notify.email.password = form.email_password.trim().to_string();
    }
    config.notify.email.from = form.email_from.trim().to_string();
    config.notify.email.to = form.email_to.trim().to_string();
}

/// `POST /settings`: overlays the form onto the current config file (blank
/// secrets keep their stored values), validates the effective result (env
/// overrides still count), and rewrites `config.toml` atomically. On
/// validation failure the settings page is re-rendered in place with an
/// error banner and the offending fields highlighted, preserving the user's
/// edits rather than sending them to a bare error page. Changes apply on the
/// next daemon start.
pub async fn save_settings(
    State(state): State<AppState>,
    Form(form): Form<SettingsForm>,
) -> Result<Response, WebError> {
    let mut config = config::load_file(&state.config_path)?;
    apply_settings_form(&mut config, &form);

    if let Err(crate::config::ConfigError::Validation(problems)) =
        config::validate_effective(&config)
    {
        let error_fields = error_field_ids(&problems);
        let view = settings_view(
            &config,
            &state.config_path.display().to_string(),
            String::new(),
            problems,
            error_fields,
        );
        return Ok((StatusCode::BAD_REQUEST, view).into_response());
    }

    config::save(&config, &state.config_path)?;
    Ok(Redirect::to(&format!("/settings?flash={FLASH_SETTINGS_SAVED}")).into_response())
}

/// `POST /settings/test-email`: overlays the submitted form (so the user can
/// test what they typed without saving first) and sends a fixed test message
/// via the resulting SMTP settings. Nothing is written to disk; the outcome
/// is reported by re-rendering the settings page with a success flash or an
/// error banner.
pub async fn test_email(
    State(state): State<AppState>,
    Form(form): Form<SettingsForm>,
) -> Result<Response, WebError> {
    let mut config = config::load_file(&state.config_path)?;
    apply_settings_form(&mut config, &form);

    let email = &config.notify.email;
    let notifier = crate::notify::email::EmailNotifier::new(
        email.smtp_host.clone(),
        email.smtp_port,
        email.username.clone(),
        email.password.clone(),
        email.from.clone(),
        email.to.clone(),
    );

    let (flash, errors) = match notifier.send_test().await {
        Ok(()) => (
            "Test email sent. Check the destination inbox.".to_string(),
            Vec::new(),
        ),
        Err(e) => (String::new(), vec![format!("Test email failed: {e}")]),
    };

    let view = settings_view(
        &config,
        &state.config_path.display().to_string(),
        flash,
        errors,
        std::collections::HashSet::new(),
    );
    Ok(view.into_response())
}

/// Whole days from `today` until `release` (negative once past).
fn days_until(today: NaiveDate, release: NaiveDate) -> i64 {
    (release - today).num_days()
}

/// Human phrasing for a day delta: "today", "in 3 days", "2 days ago".
fn fmt_days_until(days: i64) -> String {
    match days {
        0 => "today".to_string(),
        1 => "in 1 day".to_string(),
        -1 => "1 day ago".to_string(),
        d if d > 1 => format!("in {d} days"),
        d => format!("{} days ago", -d),
    }
}

/// Reformats SQL's comma-joined author list ("A,B") for display ("A, B").
fn fmt_authors(joined: &str) -> String {
    joined.split(',').collect::<Vec<_>>().join(", ")
}

/// Renders a series position without a spurious ".0" for whole numbers.
fn fmt_position(position: Option<f64>) -> String {
    match position {
        None => "-".to_string(),
        Some(p) if p.fract() == 0.0 => format!("{}", p as i64),
        Some(p) => p.to_string(),
    }
}

/// Formats a timestamp for the dashboard tables.
fn fmt_time(time: &DateTime<Utc>) -> String {
    time.format("%Y-%m-%d %H:%M UTC").to_string()
}

/// Decodes the `check_runs.errors` JSON array into one readable line;
/// falls back to the raw string if it isn't valid JSON.
fn fmt_errors(errors: Option<&str>) -> String {
    match errors {
        None => String::new(),
        Some(raw) => serde_json::from_str::<Vec<String>>(raw)
            .map(|list| list.join("; "))
            .unwrap_or_else(|_| raw.to_string()),
    }
}

/// `check_runs.status` display label (doubles as a CSS class suffix).
fn run_status_label(status: CheckRunStatus) -> &'static str {
    match status {
        CheckRunStatus::Running => "running",
        CheckRunStatus::Ok => "ok",
        CheckRunStatus::Partial => "partial",
        CheckRunStatus::Failed => "failed",
    }
}

/// `series.match_status` display label for the unmatched page.
fn match_status_label(status: MatchStatus) -> &'static str {
    match status {
        MatchStatus::Unmatched => "unmatched",
        MatchStatus::Matched => "matched",
        MatchStatus::Overridden => "overridden",
        MatchStatus::Ignored => "ignored",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::ReleaseStatus;
    use crate::db::queries::{
        finalize_check_run, get_series_override, insert_check_run, set_series_match,
        upsert_known_release, upsert_owned_book, upsert_series,
    };
    use crate::web::{AppState, router};
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, header};
    use chrono::Duration as ChronoDuration;
    use sqlx::SqlitePool;
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tower::ServiceExt;

    /// In-memory app with a seeded DB: one matched series (with an owned,
    /// authored book) with one upcoming (today + 3) and one released
    /// (today - 2) book, one unmatched series, and one finished ok check run.
    async fn seeded_app() -> (Router, SqlitePool, mpsc::Receiver<ScanMode>) {
        let pool = crate::db::init(":memory:").await.expect("init db");
        let today = Utc::now().date_naive();

        let matched = upsert_series(&pool, "abs-1", "The Wandering Inn", false)
            .await
            .expect("seed matched series");
        set_series_match(
            &pool,
            matched.id,
            Some("hc-1"),
            Some("The Wandering Inn"),
            Some("the-wandering-inn"),
            MatchStatus::Matched,
        )
        .await
        .expect("mark matched");
        upsert_owned_book(
            &pool,
            matched.id,
            "item-1",
            "Book One",
            Some(1.0),
            None,
            None,
            Some("pirateaba"),
        )
        .await
        .expect("seed owned book");
        upsert_series(&pool, "abs-2", "Mother of Learning", false)
            .await
            .expect("seed unmatched series");

        upsert_known_release(
            &pool,
            matched.id,
            "hc-b2",
            "Book Two",
            Some(2.0),
            Some(today + ChronoDuration::days(3)),
            ReleaseStatus::Upcoming,
            Some("B0UPCOMING1"),
        )
        .await
        .expect("seed upcoming release");
        upsert_known_release(
            &pool,
            matched.id,
            "hc-b1",
            "Book One",
            Some(1.0),
            Some(today - ChronoDuration::days(2)),
            ReleaseStatus::Released,
            None,
        )
        .await
        .expect("seed released release");

        let run_id = insert_check_run(&pool).await.expect("insert run");
        finalize_check_run(&pool, run_id, CheckRunStatus::Ok, 2, 1, &[])
            .await
            .expect("finalize run");

        let (run_tx, run_rx) = mpsc::channel(1);
        let state = AppState {
            pool: pool.clone(),
            run_tx,
            interval: Duration::from_secs(7 * 86_400),
            config_path: write_settings_config(),
        };
        (router(state), pool, run_rx)
    }

    /// A complete, valid config file body for settings-page tests. Secrets are
    /// set so validation passes without env vars.
    const TEST_CONFIG: &str = r#"
[audiobookshelf]
base_url = "https://abs.example.com"
api_key = "abs-secret-key"
library_ids = ["lib-1"]

[hardcover]
token = "hc-secret-token"
min_request_interval_ms = 1100
max_retries = 3
language = "en"

[audnexus]
enabled = true
region = "us"

[check]
interval_days = 7
db_path = "test.db"

[dashboard]
enabled = true
bind = "127.0.0.1:8091"

[notify.ntfy]
enabled = false
server = ""
topic = ""
token = ""

[notify.email]
enabled = false
smtp_host = ""
smtp_port = 587
username = ""
password = ""
from = ""
to = ""
"#;

    /// Writes [`TEST_CONFIG`] to a persisted temp file and returns its path.
    /// `keep()` detaches auto-deletion so the save handler can rename over it.
    fn write_settings_config() -> std::path::PathBuf {
        let file = tempfile::Builder::new()
            .suffix(".toml")
            .tempfile()
            .expect("create temp config");
        std::fs::write(file.path(), TEST_CONFIG).expect("write temp config");
        let (_file, path) = file.keep().expect("persist temp config");
        path
    }

    /// App plus the config path its settings page edits (DB is empty; settings
    /// tests don't touch it).
    async fn settings_app() -> (Router, std::path::PathBuf) {
        let pool = crate::db::init(":memory:").await.expect("init db");
        let (run_tx, _run_rx) = mpsc::channel(1);
        let path = write_settings_config();
        let state = AppState {
            pool,
            run_tx,
            interval: Duration::from_secs(7 * 86_400),
            config_path: path.clone(),
        };
        (router(state), path)
    }

    /// A valid urlencoded settings form matching [`TEST_CONFIG`], with the
    /// check interval changed to 14 days and secrets left blank (= keep).
    const SETTINGS_FORM: &str = "abs_base_url=https%3A%2F%2Fabs.example.com&abs_api_key=&\
        abs_library_ids=lib-1%2C+lib-2&abs_startup_timeout_secs=120&\
        hc_token=&hc_min_request_interval_ms=1100&\
        hc_max_retries=3&hc_language=en&audnexus_enabled=1&audnexus_region=us&\
        check_interval_days=14&check_db_path=test.db&\
        dashboard_enabled=1&dashboard_bind=127.0.0.1%3A8091&ntfy_server=&ntfy_topic=&\
        ntfy_token=&email_smtp_host=&email_smtp_port=587&email_username=&\
        email_password=&email_from=&email_to=";

    /// Runs one request through the router and returns (status, body).
    async fn get_page(app: &Router, uri: &str) -> (StatusCode, String) {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .expect("request");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        (
            status,
            String::from_utf8(bytes.to_vec()).expect("utf8 body"),
        )
    }

    /// Posts a urlencoded form to the router and returns the raw response.
    async fn post_form(app: &Router, uri: &str, body: &str) -> Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .expect("request")
    }

    #[tokio::test]
    async fn index_shows_runs_counts_and_interval() {
        let (app, _pool, _rx) = seeded_app().await;

        let (status, body) = get_page(&app, "/").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Checking every week"), "{body}");
        assert!(
            body.contains(r#"<a href="/series"><div class="figure">2</div>"#),
            "{body}"
        );
        assert!(
            body.contains(r#"<a href="/upcoming"><div class="figure">1</div>"#),
            "{body}"
        );
        assert!(
            body.contains(r#"<a href="/new"><div class="figure">1</div>"#),
            "{body}"
        );
        assert!(
            body.contains(r#"<a href="/unmatched"><div class="figure">1</div>"#),
            "{body}"
        );
        assert!(body.contains("status-ok"), "{body}");
    }

    #[tokio::test]
    async fn index_renders_flash_message() {
        let (app, _pool, _rx) = seeded_app().await;

        let (status, body) = get_page(&app, "/?flash=check-already-running").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("already running"), "{body}");
    }

    #[tokio::test]
    async fn upcoming_page_lists_release_with_days_until() {
        let (app, _pool, _rx) = seeded_app().await;

        let (status, body) = get_page(&app, "/upcoming").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Book Two"), "{body}");
        assert!(body.contains("The Wandering Inn"), "{body}");
        assert!(body.contains("in 3 days"), "{body}");
    }

    #[tokio::test]
    async fn new_page_lists_released_book() {
        let (app, _pool, _rx) = seeded_app().await;

        let (status, body) = get_page(&app, "/new").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Book One"), "{body}");
        assert!(body.contains("2 days ago"), "{body}");
    }

    #[tokio::test]
    async fn unmatched_page_lists_series_with_override_form() {
        let (app, _pool, _rx) = seeded_app().await;

        let (status, body) = get_page(&app, "/unmatched").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Mother of Learning"), "{body}");
        assert!(body.contains(r#"action="/overrides""#), "{body}");
        // The matched series must not appear in the review queue.
        assert!(!body.contains("The Wandering Inn"), "{body}");
    }

    #[tokio::test]
    async fn override_post_persists_and_resets_series() {
        let (app, pool, _rx) = seeded_app().await;

        let response = post_form(
            &app,
            "/overrides",
            "abs_name=The+Wandering+Inn&hardcover_series_id=hc-99",
        )
        .await;

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers().get(header::LOCATION).unwrap(),
            "/unmatched"
        );

        // Stored under the normalized name (leading article stripped).
        let row = get_series_override(&pool, "wandering inn")
            .await
            .expect("lookup override")
            .expect("override present");
        assert_eq!(row.hardcover_series_id.as_deref(), Some("hc-99"));
        assert!(!row.ignore);

        // The previously matched series is reset for the next check.
        let series = sqlx::query_as::<_, Series>("SELECT * FROM series WHERE abs_series_id = ?")
            .bind("abs-1")
            .fetch_one(&pool)
            .await
            .expect("fetch series");
        assert_eq!(series.match_status, MatchStatus::Unmatched);
        assert_eq!(series.hardcover_series_id, None);
    }

    #[tokio::test]
    async fn override_post_with_ignore_stores_ignore_flag() {
        let (app, pool, _rx) = seeded_app().await;

        let response = post_form(
            &app,
            "/overrides",
            "abs_name=Mother+of+Learning&hardcover_series_id=&ignore=1",
        )
        .await;

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let row = get_series_override(&pool, "mother of learning")
            .await
            .expect("lookup override")
            .expect("override present");
        assert!(row.ignore);
        assert_eq!(row.hardcover_series_id, None);
    }

    #[tokio::test]
    async fn override_post_without_id_or_ignore_is_rejected() {
        let (app, pool, _rx) = seeded_app().await;

        let response = post_form(
            &app,
            "/overrides",
            "abs_name=Mother+of+Learning&hardcover_series_id=",
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(
            get_series_override(&pool, "mother of learning")
                .await
                .expect("lookup override")
                .is_none()
        );
    }

    #[tokio::test]
    async fn series_page_shows_next_release_authors_and_last_release() {
        let (app, _pool, _rx) = seeded_app().await;

        let (status, body) = get_page(&app, "/series").await;

        assert_eq!(status, StatusCode::OK);
        // Matched series: author, and the scheduled next book with its date.
        assert!(body.contains("The Wandering Inn"), "{body}");
        assert!(body.contains("pirateaba"), "{body}");
        assert!(body.contains("Book Two"), "{body}");
        assert!(body.contains("in 3 days"), "{body}");
        // Unmatched series with no known releases: a dash for last release.
        assert!(body.contains("Mother of Learning"), "{body}");
        assert!(body.contains("—"), "{body}");
        // Every row offers an ignore action.
        assert!(body.contains(r#"action="/series/ignore""#), "{body}");
    }

    #[tokio::test]
    async fn ignore_post_marks_series_ignored_and_persists_override() {
        let (app, pool, _rx) = seeded_app().await;

        let response = post_form(
            &app,
            "/series/ignore",
            "abs_name=The+Wandering+Inn&action=ignore",
        )
        .await;

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers().get(header::LOCATION).unwrap(), "/series");

        // Override persisted under the normalized name, ignore set.
        let row = get_series_override(&pool, "wandering inn")
            .await
            .expect("lookup override")
            .expect("override present");
        assert!(row.ignore);

        // The series row reflects it immediately.
        let series = sqlx::query_as::<_, Series>("SELECT * FROM series WHERE abs_series_id = ?")
            .bind("abs-1")
            .fetch_one(&pool)
            .await
            .expect("fetch series");
        assert_eq!(series.match_status, MatchStatus::Ignored);

        // The series page now shows an unignore action for it.
        let (_, body) = get_page(&app, "/series").await;
        assert!(body.contains("Unignore"), "{body}");
    }

    #[tokio::test]
    async fn unignore_post_resets_series_to_unmatched() {
        let (app, pool, _rx) = seeded_app().await;

        post_form(
            &app,
            "/series/ignore",
            "abs_name=The+Wandering+Inn&action=ignore",
        )
        .await;
        let response = post_form(
            &app,
            "/series/ignore",
            "abs_name=The+Wandering+Inn&action=unignore",
        )
        .await;

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let row = get_series_override(&pool, "wandering inn")
            .await
            .expect("lookup override")
            .expect("override present");
        assert!(!row.ignore);

        let series = sqlx::query_as::<_, Series>("SELECT * FROM series WHERE abs_series_id = ?")
            .bind("abs-1")
            .fetch_one(&pool)
            .await
            .expect("fetch series");
        assert_eq!(series.match_status, MatchStatus::Unmatched);
    }

    #[tokio::test]
    async fn ignore_post_rejects_unknown_action() {
        let (app, _pool, _rx) = seeded_app().await;

        let response = post_form(
            &app,
            "/series/ignore",
            "abs_name=The+Wandering+Inn&action=explode",
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn upcoming_page_includes_author_column() {
        let (app, _pool, _rx) = seeded_app().await;

        let (status, body) = get_page(&app, "/upcoming").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("pirateaba"), "{body}");
        // Filter input targets the releases table.
        assert!(body.contains(r#"data-filter="upcoming-table""#), "{body}");
    }

    /// Id of the seeded release with the given Hardcover book id.
    async fn release_id(pool: &SqlitePool, source_book_id: &str) -> i64 {
        let (id,): (i64,) =
            sqlx::query_as("SELECT id FROM known_releases WHERE source_book_id = ?")
                .bind(source_book_id)
                .fetch_one(pool)
                .await
                .expect("seeded release present");
        id
    }

    #[tokio::test]
    async fn upcoming_page_offers_watch_form() {
        let (app, _pool, _rx) = seeded_app().await;

        let (status, body) = get_page(&app, "/upcoming").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#"action="/watch""#), "{body}");
        assert!(body.contains(r#"name="remind_days""#), "{body}");
        assert!(body.contains(r#"name="remind_when""#), "{body}");
    }

    #[tokio::test]
    async fn upcoming_page_links_series_author_and_book() {
        let (app, _pool, _rx) = seeded_app().await;

        let (status, body) = get_page(&app, "/upcoming").await;

        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains(r#"href="https://hardcover.app/series/the-wandering-inn""#),
            "{body}"
        );
        assert!(body.contains("searchAuthor=pirateaba"), "{body}");
        assert!(
            body.contains(r#"href="https://www.audible.com/pd/B0UPCOMING1""#),
            "{body}"
        );
    }

    #[tokio::test]
    async fn upcoming_page_uses_watch_dialog_button() {
        let (app, _pool, _rx) = seeded_app().await;

        let (status, body) = get_page(&app, "/upcoming").await;

        assert_eq!(status, StatusCode::OK);
        // A single Watch button per row, plus one shared dialog it opens.
        assert!(body.contains(r#"<dialog id="watch-dialog">"#), "{body}");
        assert!(body.contains("watch-open"), "{body}");
        // Dates render in a no-wrap cell so they stay on one line.
        assert!(body.contains(r#"<td class="date">"#), "{body}");
    }

    #[tokio::test]
    async fn watch_post_adds_watch_and_redirects_back() {
        let (app, pool, _rx) = seeded_app().await;
        let id = release_id(&pool, "hc-b2").await;

        let response = post_form(
            &app,
            "/watch",
            &format!("release_id={id}&remind_days=3&remind_when=before&back=%2Fupcoming"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers().get(header::LOCATION).unwrap(),
            "/upcoming"
        );

        let watches = crate::db::queries::list_watched_releases(&pool)
            .await
            .expect("list watches");
        assert_eq!(watches.len(), 1);
        assert_eq!(watches[0].known_release_id, id);
        assert_eq!(watches[0].remind_offset_days, -3);

        // The upcoming page now shows Unwatch for that row.
        let (_, body) = get_page(&app, "/upcoming").await;
        assert!(body.contains("Unwatch"), "{body}");
    }

    #[tokio::test]
    async fn watch_post_rejects_unknown_release_and_bad_window() {
        let (app, pool, _rx) = seeded_app().await;
        let id = release_id(&pool, "hc-b2").await;

        let missing = post_form(&app, "/watch", "release_id=9999&remind_days=1").await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);

        let bad_window =
            post_form(&app, "/watch", &format!("release_id={id}&remind_days=-1")).await;
        assert_eq!(bad_window.status(), StatusCode::BAD_REQUEST);

        let bad_direction = post_form(
            &app,
            "/watch",
            &format!("release_id={id}&remind_days=1&remind_when=during"),
        )
        .await;
        assert_eq!(bad_direction.status(), StatusCode::BAD_REQUEST);

        assert!(
            crate::db::queries::list_watched_releases(&pool)
                .await
                .expect("list watches")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn watch_post_supports_after_release_reminders() {
        let (app, pool, _rx) = seeded_app().await;
        let id = release_id(&pool, "hc-b2").await;

        let response = post_form(
            &app,
            "/watch",
            &format!("release_id={id}&remind_days=7&remind_when=after&back=%2Fupcoming"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);

        let watches = crate::db::queries::list_watched_releases(&pool)
            .await
            .expect("list watches");
        assert_eq!(watches[0].remind_offset_days, 7);

        // The watchlist edit form pre-selects "after".
        let (_, body) = get_page(&app, "/watchlist").await;
        assert!(
            body.contains(r#"<option value="after" selected>"#),
            "{body}"
        );
    }

    #[tokio::test]
    async fn watch_remove_post_drops_watch() {
        let (app, pool, _rx) = seeded_app().await;
        let id = release_id(&pool, "hc-b2").await;
        crate::db::queries::add_watch(&pool, id, -1)
            .await
            .expect("seed watch");

        let response = post_form(&app, "/watch/remove", &format!("release_id={id}")).await;

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers().get(header::LOCATION).unwrap(),
            "/watchlist"
        );
        assert!(
            crate::db::queries::list_watched_releases(&pool)
                .await
                .expect("list watches")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn watchlist_page_lists_watched_release() {
        let (app, pool, _rx) = seeded_app().await;
        let id = release_id(&pool, "hc-b2").await;
        crate::db::queries::add_watch(&pool, id, -2)
            .await
            .expect("seed watch");

        let (status, body) = get_page(&app, "/watchlist").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Book Two"), "{body}");
        assert!(body.contains("The Wandering Inn"), "{body}");
        assert!(body.contains(r#"value="2""#), "{body}");
        assert!(body.contains(r#"action="/watch/remove""#), "{body}");
    }

    #[tokio::test]
    async fn watchlist_page_empty_state() {
        let (app, _pool, _rx) = seeded_app().await;

        let (status, body) = get_page(&app, "/watchlist").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Nothing watched yet"), "{body}");
    }

    #[tokio::test]
    async fn index_shows_watched_count() {
        let (app, pool, _rx) = seeded_app().await;
        let id = release_id(&pool, "hc-b2").await;
        crate::db::queries::add_watch(&pool, id, -1)
            .await
            .expect("seed watch");

        let (status, body) = get_page(&app, "/").await;

        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains(r#"<a href="/watchlist"><div class="figure">1</div>"#),
            "{body}"
        );
    }

    #[tokio::test]
    async fn series_page_marks_scheduled_rows_and_offers_toggle() {
        let (app, _pool, _rx) = seeded_app().await;

        let (status, body) = get_page(&app, "/series").await;

        assert_eq!(status, StatusCode::OK);
        // Matched series has a scheduled next book; unmatched one doesn't.
        assert!(body.contains(r#"data-scheduled="1""#), "{body}");
        assert!(body.contains(r#"data-scheduled="0""#), "{body}");
        assert!(
            body.contains(r#"data-toggle-unscheduled="series-table""#),
            "{body}"
        );
    }

    #[tokio::test]
    async fn check_post_signals_scheduler_and_redirects() {
        let (app, _pool, mut rx) = seeded_app().await;

        let response = post_form(&app, "/check", "").await;

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers().get(header::LOCATION).unwrap(), "/");
        // No explicit mode means a partial scan.
        assert_eq!(
            rx.try_recv().expect("run-now signal delivered"),
            ScanMode::Partial
        );
    }

    #[tokio::test]
    async fn check_post_full_mode_signals_full_scan() {
        let (app, _pool, mut rx) = seeded_app().await;

        let response = post_form(&app, "/check", "mode=full").await;

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            rx.try_recv().expect("run-now signal delivered"),
            ScanMode::Full
        );
    }

    #[tokio::test]
    async fn check_post_flashes_when_run_already_queued() {
        let (app, _pool, _rx) = seeded_app().await;

        // First press fills the capacity-1 channel (receiver never drains it).
        let first = post_form(&app, "/check", "").await;
        assert_eq!(first.status(), StatusCode::SEE_OTHER);

        let second = post_form(&app, "/check", "").await;
        assert_eq!(second.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            second.headers().get(header::LOCATION).unwrap(),
            "/?flash=check-already-running"
        );
    }

    #[tokio::test]
    async fn settings_page_shows_values_and_masks_secrets() {
        let (app, _path) = settings_app().await;

        let (status, body) = get_page(&app, "/settings").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("https://abs.example.com"), "{body}");
        // Weekly interval preset is selected in the frequency dropdown.
        assert!(body.contains(r#"<option value="7" selected>"#), "{body}");
        assert!(body.contains("lib-1"), "{body}");
        // Secrets never appear in the page; masked placeholders do.
        assert!(!body.contains("abs-secret-key"), "{body}");
        assert!(!body.contains("hc-secret-token"), "{body}");
        assert!(
            body.contains("currently set; leave blank to keep"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn settings_post_rewrites_file_and_keeps_blank_secrets() {
        let (app, path) = settings_app().await;

        let response = post_form(&app, "/settings", SETTINGS_FORM).await;

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers().get(header::LOCATION).unwrap(),
            "/settings?flash=saved"
        );

        let saved = crate::config::load_file(&path).expect("reload saved config");
        assert_eq!(saved.check.interval_days, 14);
        assert_eq!(saved.audiobookshelf.library_ids, vec!["lib-1", "lib-2"]);
        assert_eq!(saved.audiobookshelf.startup_timeout_secs, 120);
        // Blank secret fields keep the stored values.
        assert_eq!(saved.audiobookshelf.api_key, "abs-secret-key");
        assert_eq!(saved.hardcover.token, "hc-secret-token");
        // Absent checkboxes mean disabled.
        assert!(!saved.notify.ntfy.enabled);
        assert!(saved.audnexus.enabled);
    }

    #[tokio::test]
    async fn settings_post_updates_secret_when_provided() {
        let (app, path) = settings_app().await;

        let body = SETTINGS_FORM.replace("abs_api_key=&", "abs_api_key=new-key&");
        let response = post_form(&app, "/settings", &body).await;

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let saved = crate::config::load_file(&path).expect("reload saved config");
        assert_eq!(saved.audiobookshelf.api_key, "new-key");
    }

    #[tokio::test]
    async fn settings_post_rejects_invalid_config_and_leaves_file_alone() {
        let (app, path) = settings_app().await;

        let body = SETTINGS_FORM.replace(
            "abs_base_url=https%3A%2F%2Fabs.example.com&",
            "abs_base_url=not+a+url&",
        );
        let response = post_form(&app, "/settings", &body).await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let text = String::from_utf8(bytes.to_vec()).expect("utf8 body");
        // Re-renders the settings form in place (not a bare error page) with an
        // error banner, the offending field highlighted, and the bad value kept.
        assert!(text.contains("Settings not saved"), "{text}");
        assert!(text.contains("base_url"), "{text}");
        assert!(text.contains(r#"name="abs_base_url""#), "{text}");
        assert!(text.contains(r#"aria-invalid="true""#), "{text}");
        assert!(text.contains(r#"value="not a url""#), "{text}");

        // The file still holds the original values.
        let saved = crate::config::load_file(&path).expect("reload config");
        assert_eq!(saved.check.interval_days, 7);
    }

    #[tokio::test]
    async fn test_email_reports_failure_without_saving() {
        let (app, path) = settings_app().await;

        // Blank from/to (email disabled in TEST_CONFIG) makes the send fail;
        // the handler re-renders the page with an error banner, not a 500.
        let response = post_form(&app, "/settings/test-email", SETTINGS_FORM).await;

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let text = String::from_utf8(bytes.to_vec()).expect("utf8 body");
        assert!(text.contains("Test email failed"), "{text}");

        // The config file is untouched by a test send.
        let saved = crate::config::load_file(&path).expect("reload config");
        assert_eq!(saved.check.interval_days, 7);
    }

    #[test]
    fn days_until_counts_whole_days() {
        let today = NaiveDate::from_ymd_opt(2026, 7, 21).unwrap();
        assert_eq!(days_until(today, today), 0);
        assert_eq!(
            days_until(today, NaiveDate::from_ymd_opt(2026, 7, 24).unwrap()),
            3
        );
        assert_eq!(
            days_until(today, NaiveDate::from_ymd_opt(2026, 7, 19).unwrap()),
            -2
        );
    }

    #[test]
    fn fmt_days_until_phrases_each_direction() {
        assert_eq!(fmt_days_until(0), "today");
        assert_eq!(fmt_days_until(1), "in 1 day");
        assert_eq!(fmt_days_until(5), "in 5 days");
        assert_eq!(fmt_days_until(-1), "1 day ago");
        assert_eq!(fmt_days_until(-4), "4 days ago");
    }

    #[test]
    fn link_builders_format_and_encode() {
        assert_eq!(
            series_url(Some("the-expanse")),
            "https://hardcover.app/series/the-expanse"
        );
        assert_eq!(series_url(None), "");
        assert_eq!(book_url(Some("B0ABC")), "https://www.audible.com/pd/B0ABC");
        assert_eq!(book_url(None), "");

        let links = author_links("James S. A. Corey,Ann Leckie");
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].name, "James S. A. Corey");
        assert_eq!(
            links[0].url,
            "https://www.audible.com/search?searchAuthor=James+S.+A.+Corey"
        );
        assert_eq!(links[1].name, "Ann Leckie");
        assert!(author_links("").is_empty());
    }

    #[test]
    fn fmt_position_drops_trailing_zero() {
        assert_eq!(fmt_position(Some(2.0)), "2");
        assert_eq!(fmt_position(Some(1.5)), "1.5");
        assert_eq!(fmt_position(None), "-");
    }

    #[test]
    fn fmt_errors_decodes_json_list() {
        assert_eq!(fmt_errors(None), "");
        assert_eq!(
            fmt_errors(Some(r#"["a: boom","b: bust"]"#)),
            "a: boom; b: bust"
        );
        assert_eq!(fmt_errors(Some("not json")), "not json");
    }
}
