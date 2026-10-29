//! `FromRow` structs mirroring the tables in `migrations/0001_init.sql`.

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::FromRow;

/// `series.match_status`: how (or whether) an ABS series has been matched to
/// a Hardcover series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(rename_all = "lowercase")]
pub enum MatchStatus {
    Unmatched,
    Matched,
    Overridden,
    Ignored,
}

/// `known_releases.status`: where a remote book stands relative to what the
/// user owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(rename_all = "lowercase")]
pub enum ReleaseStatus {
    Upcoming,
    Released,
    Owned,
}

/// `check_runs.status`: outcome of a single `run_check` invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(rename_all = "lowercase")]
pub enum CheckRunStatus {
    Running,
    Ok,
    Partial,
    Failed,
    /// The run exceeded `check.run_timeout_mins` and was cancelled.
    #[sqlx(rename = "timed_out")]
    TimedOut,
}

/// One ABS series being tracked, and its (possibly still-unmatched) Hardcover
/// counterpart.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct Series {
    pub id: i64,
    pub abs_series_id: String,
    pub abs_name: String,
    pub hardcover_series_id: Option<String>,
    pub hardcover_name: Option<String>,
    /// Hardcover URL slug for the matched series; feeds dashboard links.
    pub hardcover_slug: Option<String>,
    pub is_completed: bool,
    pub match_status: MatchStatus,
    pub last_checked_at: Option<DateTime<Utc>>,
    /// Set when the series disappeared from ABS; NULL while active.
    pub archived_at: Option<DateTime<Utc>>,
}

/// A book the user owns in ABS, belonging to a tracked series.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct OwnedBook {
    pub id: i64,
    pub series_id: i64,
    pub abs_item_id: String,
    pub title: String,
    pub sequence: Option<f64>,
    pub asin: Option<String>,
    pub isbn: Option<String>,
    pub author: Option<String>,
}

/// One series joined with its release outlook, for the dashboard series page:
/// author(s) from owned books, the next scheduled release (if any), and the
/// most recent past release date.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct SeriesOverview {
    pub id: i64,
    pub abs_name: String,
    pub match_status: MatchStatus,
    pub archived_at: Option<DateTime<Utc>>,
    /// Distinct authors of owned books in this series, comma-joined by SQL.
    pub authors: Option<String>,
    /// Title of the soonest upcoming release with a future date.
    pub next_title: Option<String>,
    /// Date of the soonest upcoming release with a future date.
    pub next_release_date: Option<NaiveDate>,
    /// Most recent known release date that is today or earlier.
    pub last_release_date: Option<NaiveDate>,
}

/// A remote book seen from Hardcover, with its notification/dedupe state.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct KnownRelease {
    pub id: i64,
    pub series_id: i64,
    pub source_book_id: String,
    pub title: String,
    pub position: Option<f64>,
    pub release_date: Option<NaiveDate>,
    pub status: ReleaseStatus,
    pub asin: Option<String>,
    pub first_seen_at: DateTime<Utc>,
    pub upcoming_notified_at: Option<DateTime<Utc>>,
    pub released_notified_at: Option<DateTime<Utc>>,
}

/// A manual ABS-name -> Hardcover-series mapping, edited via the dashboard.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct SeriesOverride {
    pub abs_name_normalized: String,
    pub hardcover_series_id: Option<String>,
    pub ignore: bool,
}

/// One watch/reminder entry for a known release, edited via the dashboard.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct Watch {
    pub id: i64,
    pub known_release_id: i64,
    /// Signed day offset from `release_date` when the reminder fires:
    /// negative = days before, positive = days after, 0 = on release day.
    pub remind_offset_days: i64,
    /// Set once the reminder has been delivered; NULL means still pending.
    pub reminder_notified_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// A watch joined with its release and series, for the watchlist page and
/// the reminder dispatcher.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct WatchedRelease {
    pub known_release_id: i64,
    pub remind_offset_days: i64,
    pub reminder_notified_at: Option<DateTime<Utc>>,
    pub series_name: String,
    pub title: String,
    pub position: Option<f64>,
    pub release_date: Option<NaiveDate>,
    pub asin: Option<String>,
}

/// One row of run history, shown on the dashboard status page.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct CheckRun {
    pub id: i64,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub status: CheckRunStatus,
    pub series_checked: i64,
    pub events_found: i64,
    pub errors: Option<String>,
}
