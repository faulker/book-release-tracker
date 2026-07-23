//! Orchestrates one full check run (PLAN.md section 5): ABS sync, per-series
//! matching, diff/classify, Audnexus enrichment, notification, and
//! `check_runs` bookkeeping. Each phase lives in its own helper; per-series
//! failures are isolated in the report so one bad series never sinks the run.
//! Only ABS fetch and database access are run-fatal, and an ABS failure is
//! still recorded in `check_runs` before returning.

use std::collections::HashMap;

use chrono::NaiveDate;
use sqlx::SqlitePool;
use thiserror::Error;
use tracing::{error, info, warn};

use crate::abs::{AbsClient, AbsError, ItemMetadata};
use crate::db::models::{CheckRunStatus, MatchStatus, OwnedBook, Series};
use crate::db::queries::{self, NotifiedKind, QueryError};
use crate::diff::{ReleaseEvent, ReleaseEventData, diff_and_classify};
use crate::matching::{MatchOutcome, normalize_series_name, pick_series_match};
use crate::metadata::audnexus::AudnexusClient;
use crate::metadata::{MetadataError, MetadataSource};
use crate::notify::{Notifier, dispatch_all};

/// Errors that abort a check run entirely. Everything else (ABS outage,
/// per-series metadata failures) is captured in the [`CheckReport`] instead.
#[derive(Debug, Error)]
pub enum CheckError {
    /// A database query failed; without the DB there is nothing to record
    /// against, so the run cannot proceed or even be finalized.
    #[error("database error during check run: {0}")]
    Db(#[from] QueryError),
}

/// How much of the library one check run covers.
///
/// `Partial` skips series that already have an upcoming release scheduled for
/// a future date: the next book is known and hasn't arrived, so re-querying
/// Hardcover teaches us nothing until that date passes. Series with no
/// scheduled book, or whose scheduled date has passed, are still checked.
/// On an empty database nothing is scheduled yet, so the first partial scan
/// naturally covers everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanMode {
    Full,
    Partial,
}

/// Outcome of one `run_check` invocation, mirroring the finalized
/// `check_runs` row. `status` drives the `check` subcommand's exit code.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckReport {
    pub status: CheckRunStatus,
    pub series_checked: i64,
    pub events_found: i64,
    pub errors: Vec<String>,
}

impl CheckReport {
    /// Cron-friendly exit code: 0 ok, 1 partial, 2 failed.
    pub fn exit_code(&self) -> i32 {
        match self.status {
            CheckRunStatus::Ok => 0,
            CheckRunStatus::Partial => 1,
            CheckRunStatus::Failed | CheckRunStatus::Running => 2,
        }
    }
}

/// Failures during the ABS sync phase, split so `run_check` can tell a
/// run-fatal DB error from an ABS outage (recorded as a failed run).
#[derive(Debug, Error)]
enum SyncError {
    #[error("audiobookshelf fetch failed: {0}")]
    Abs(#[from] AbsError),
    #[error(transparent)]
    Db(#[from] QueryError),
}

/// Failures while processing a single series. These are isolated: the error
/// string goes into the report and the loop moves on to the next series.
#[derive(Debug, Error)]
enum SeriesError {
    #[error("database error: {0}")]
    Db(#[from] QueryError),
    #[error("metadata error: {0}")]
    Metadata(#[from] MetadataError),
}

/// One ABS series after sync: its DB row plus the owned books just upserted
/// from ABS, ready for the matching and diff phases.
struct SyncedSeries {
    row: Series,
    owned: Vec<OwnedBook>,
}

/// Runs one full check. `today` is a parameter (not `Utc::now()` internally)
/// so tests are deterministic; the CLI passes `Utc::now().date_naive()`.
/// Returns `Err` only for database failures; an ABS outage or per-series
/// errors come back as a report with status `Failed` / `Partial`.
#[allow(clippy::too_many_arguments)] // one arg per collaborator; a context struct would just move the noise
pub async fn run_check(
    pool: &SqlitePool,
    abs: &AbsClient,
    metadata: &dyn MetadataSource,
    enricher: Option<&AudnexusClient>,
    notifiers: &[Box<dyn Notifier>],
    library_ids: &[String],
    today: NaiveDate,
    mode: ScanMode,
) -> Result<CheckReport, CheckError> {
    // Watch-list reminders fire first so they go out even if ABS is down.
    process_reminders(pool, notifiers, today).await?;

    let run_id = queries::insert_check_run(pool).await?;
    info!(
        ?mode,
        "check run started; syncing series from Audiobookshelf"
    );

    // Step 2: ABS sync. ABS being down is fatal for this run, but the run is
    // still finalized as failed so the dashboard's history shows it.
    let synced = match sync_abs(pool, abs, library_ids).await {
        Ok(synced) => synced,
        Err(SyncError::Db(e)) => return Err(e.into()),
        Err(e @ SyncError::Abs(_)) => {
            error!(error = %e, "ABS fetch failed; aborting check run");
            let report = CheckReport {
                status: CheckRunStatus::Failed,
                series_checked: 0,
                events_found: 0,
                errors: vec![e.to_string()],
            };
            finalize(pool, run_id, &report).await?;
            return Ok(report);
        }
    };

    // A partial scan skips series whose next book is already scheduled for a
    // future date; everything else (nothing scheduled, or date passed) is
    // checked as usual.
    let skip_scheduled: std::collections::HashSet<i64> = match mode {
        ScanMode::Full => Default::default(),
        ScanMode::Partial => queries::series_ids_with_scheduled_release(pool, today)
            .await?
            .into_iter()
            .collect(),
    };

    // Steps 3-6, one series at a time, each isolated in its own Result.
    let mut events_found = 0i64;
    let mut checked = 0i64;
    let mut errors = Vec::new();
    for series in &synced {
        if skip_scheduled.contains(&series.row.id) {
            info!(
                series = %series.row.abs_name,
                "partial scan: next release already scheduled; skipping"
            );
            continue;
        }
        checked += 1;
        match process_series(pool, metadata, enricher, notifiers, today, series).await {
            Ok(events) => events_found += events as i64,
            Err(e) => {
                error!(series = %series.row.abs_name, error = %e, "series check failed; continuing with remaining series");
                errors.push(format!("{}: {e}", series.row.abs_name));
            }
        }
    }

    // Step 7: finalize.
    let status = if errors.is_empty() {
        CheckRunStatus::Ok
    } else {
        CheckRunStatus::Partial
    };
    let report = CheckReport {
        status,
        series_checked: checked,
        events_found,
        errors,
    };
    finalize(pool, run_id, &report).await?;

    info!(
        status = ?report.status,
        series_checked = report.series_checked,
        events_found = report.events_found,
        "check run finished"
    );
    Ok(report)
}

/// Sends reminder notifications for watched releases whose reminder window
/// (`release_date + remind_offset_days`) has arrived, stamping each watch
/// once at least one notifier delivered it. Returns how many reminders were
/// sent. Called at the start of every check run so reminders go out even
/// when ABS is unreachable.
pub async fn process_reminders(
    pool: &SqlitePool,
    notifiers: &[Box<dyn Notifier>],
    today: NaiveDate,
) -> Result<usize, CheckError> {
    let due = queries::watches_due_reminder(pool, today).await?;
    let mut sent = 0;

    for watch in due {
        // The query guarantees a release date; skip defensively if absent.
        let Some(release_date) = watch.release_date else {
            continue;
        };
        let event = ReleaseEvent::Reminder(ReleaseEventData {
            series_name: watch.series_name.clone(),
            book_title: watch.title.clone(),
            position: watch.position,
            release_date,
            asin: watch.asin.clone(),
            narrators: None,
            runtime_minutes: None,
            summary: None,
        });

        if dispatch_all(notifiers, &event).await == 0 {
            warn!(
                book = %watch.title,
                "no notifier delivered this reminder; leaving it unstamped to retry"
            );
            continue;
        }
        queries::stamp_watch_reminded(pool, watch.known_release_id).await?;
        sent += 1;
    }

    if sent > 0 {
        info!(sent, "watch-list reminders delivered");
    }
    Ok(sent)
}

/// Writes the report back onto the `check_runs` row opened at the start.
async fn finalize(pool: &SqlitePool, run_id: i64, report: &CheckReport) -> Result<(), QueryError> {
    queries::finalize_check_run(
        pool,
        run_id,
        report.status,
        report.series_checked,
        report.events_found,
        &report.errors,
    )
    .await
}

/// Step 2: fetches libraries (filtered by `library_ids` when non-empty),
/// their series, and each series' items from ABS, upserting `series` and
/// `owned_books` rows as it goes. `is_completed` is passed as `false` since
/// ABS has no completion concept; Hardcover's flag is a matching-time detail.
async fn sync_abs(
    pool: &SqlitePool,
    abs: &AbsClient,
    library_ids: &[String],
) -> Result<Vec<SyncedSeries>, SyncError> {
    let mut libraries = abs.libraries().await?;
    if !library_ids.is_empty() {
        libraries.retain(|library| library_ids.contains(&library.id));
    }

    let mut synced = Vec::new();
    for library in &libraries {
        for abs_series in abs.series(&library.id).await? {
            let row = queries::upsert_series(pool, &abs_series.id, &abs_series.name, false).await?;

            let items = abs.series_items(&library.id, &abs_series.id).await?;
            let mut owned = Vec::with_capacity(items.len());
            for item in items {
                let meta = &item.media.metadata;
                let book = queries::upsert_owned_book(
                    pool,
                    row.id,
                    &item.id,
                    &meta.title,
                    sequence_for_series(meta, &abs_series.id),
                    non_empty(meta.asin.as_deref()),
                    non_empty(meta.isbn.as_deref()),
                    non_empty(Some(meta.author_name.as_str())),
                )
                .await?;
                owned.push(book);
            }
            synced.push(SyncedSeries { row, owned });
        }
    }
    Ok(synced)
}

/// Extracts an item's sequence within a specific ABS series. ABS stores
/// sequences as strings (e.g. "1", "2.5") and an item can belong to several
/// series, so pick the entry for this series and parse it as f64.
fn sequence_for_series(metadata: &ItemMetadata, abs_series_id: &str) -> Option<f64> {
    metadata
        .series
        .iter()
        .find(|s| s.id == abs_series_id)
        .and_then(|s| s.sequence.as_deref())
        .and_then(|seq| seq.trim().parse::<f64>().ok())
}

/// Maps empty/whitespace-only strings to `None`. ABS frequently sends
/// `"asin": ""` rather than omitting the field.
fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.trim().is_empty())
}

/// Steps 3-6 for one series: resolve its Hardcover match, diff remote books
/// against owned/known state, persist `known_releases`, then enrich, notify,
/// and stamp each event. Returns the number of events found.
async fn process_series(
    pool: &SqlitePool,
    metadata: &dyn MetadataSource,
    enricher: Option<&AudnexusClient>,
    notifiers: &[Box<dyn Notifier>],
    today: NaiveDate,
    series: &SyncedSeries,
) -> Result<usize, SeriesError> {
    // Step 3: matching. `None` means ignored or unmatched; nothing to diff.
    let Some(hardcover_series_id) = resolve_series_match(pool, metadata, &series.row).await? else {
        return Ok(0);
    };

    // Step 4: diff + classify (pure).
    let remote = metadata.series_books(&hardcover_series_id).await?;
    let known = queries::known_releases_for_series(pool, series.row.id).await?;
    let (events, upserts) = diff_and_classify(
        today,
        series.row.id,
        &series.row.abs_name,
        &remote,
        &series.owned,
        &known,
    );

    // Persist every remote book's state up front, regardless of notification
    // outcome, and remember row ids keyed by book identity so events (which
    // don't carry the source_book_id) can be stamped after notifiers run.
    let mut row_ids: HashMap<EventKey, i64> = HashMap::new();
    for upsert in &upserts {
        let row = queries::upsert_known_release(
            pool,
            upsert.series_id,
            &upsert.source_book_id,
            &upsert.title,
            upsert.position,
            upsert.release_date,
            upsert.status,
            upsert.asin.as_deref(),
        )
        .await?;
        row_ids.insert(
            EventKey::from_book(&upsert.title, upsert.release_date, upsert.position),
            row.id,
        );
    }

    // Steps 5-6: enrich, notify, and stamp only when >=1 notifier succeeded.
    let event_count = events.len();
    for mut event in events {
        enrich_event(&mut event, enricher).await;

        let succeeded = dispatch_all(notifiers, &event).await;
        let (kind, data) = match &event {
            ReleaseEvent::NewRelease(data) => (NotifiedKind::Released, data),
            ReleaseEvent::Upcoming(data) => (NotifiedKind::Upcoming, data),
            // diff_and_classify never emits reminders; they come from the
            // watch list in process_reminders.
            ReleaseEvent::Reminder(_) => continue,
        };
        if succeeded == 0 {
            warn!(
                series = %data.series_name,
                book = %data.book_title,
                "no notifier delivered this event; leaving it unstamped to retry next run"
            );
            continue;
        }
        match row_ids.get(&EventKey::from_event(data)) {
            Some(&row_id) => queries::stamp_notified(pool, row_id, kind).await?,
            // Every event comes from a remote book that was just upserted, so
            // a miss here is a bookkeeping bug; log it rather than panicking.
            None => error!(
                series = %data.series_name,
                book = %data.book_title,
                "event did not map back to a known_releases row; notification will repeat next run"
            ),
        }
    }
    Ok(event_count)
}

/// Step 3 for one series: manual override first (it wins over everything),
/// then reuse of a prior confident match, then a fresh Hardcover search
/// scored by `pick_series_match`. Ambiguous/NoMatch outcomes are stored as
/// `unmatched` for dashboard review; never guessed.
async fn resolve_series_match(
    pool: &SqlitePool,
    metadata: &dyn MetadataSource,
    series: &Series,
) -> Result<Option<String>, SeriesError> {
    let normalized = normalize_series_name(&series.abs_name);

    if let Some(override_row) = queries::get_series_override(pool, &normalized).await? {
        if override_row.ignore {
            queries::set_series_match(pool, series.id, None, None, None, MatchStatus::Ignored)
                .await?;
            return Ok(None);
        }
        if let Some(hardcover_series_id) = override_row.hardcover_series_id {
            let slug = ensure_slug(
                metadata,
                series.hardcover_slug.as_deref(),
                &hardcover_series_id,
            )
            .await;
            queries::set_series_match(
                pool,
                series.id,
                Some(&hardcover_series_id),
                series.hardcover_name.as_deref(),
                slug.as_deref(),
                MatchStatus::Overridden,
            )
            .await?;
            return Ok(Some(hardcover_series_id));
        }
        // An override row with neither an id nor ignore=1 carries no
        // decision; fall through to normal matching.
    }

    if series.match_status == MatchStatus::Ignored {
        return Ok(None);
    }

    // Already matched: refresh last_checked_at and reuse the stored id
    // rather than re-searching Hardcover every run.
    if let Some(hardcover_series_id) = &series.hardcover_series_id
        && matches!(
            series.match_status,
            MatchStatus::Matched | MatchStatus::Overridden
        )
    {
        let slug = ensure_slug(
            metadata,
            series.hardcover_slug.as_deref(),
            hardcover_series_id,
        )
        .await;
        queries::set_series_match(
            pool,
            series.id,
            Some(hardcover_series_id),
            series.hardcover_name.as_deref(),
            slug.as_deref(),
            series.match_status,
        )
        .await?;
        return Ok(Some(hardcover_series_id.clone()));
    }

    let candidates = metadata.find_series(&series.abs_name).await?;
    match pick_series_match(&series.abs_name, &candidates) {
        MatchOutcome::Matched(candidate) => {
            queries::set_series_match(
                pool,
                series.id,
                Some(&candidate.source_id),
                Some(&candidate.name),
                candidate.slug.as_deref(),
                MatchStatus::Matched,
            )
            .await?;
            Ok(Some(candidate.source_id.clone()))
        }
        outcome @ (MatchOutcome::Ambiguous | MatchOutcome::NoMatch) => {
            info!(
                series = %series.abs_name,
                ?outcome,
                "no confident Hardcover match; left unmatched for dashboard review"
            );
            queries::set_series_match(pool, series.id, None, None, None, MatchStatus::Unmatched)
                .await?;
            Ok(None)
        }
    }
}

/// The stored series slug, fetched from the metadata source when missing
/// (rows matched before slugs were stored, or overrides where only the id is
/// known). A fetch failure is logged and yields `None`: a missing slug only
/// costs the dashboard a link, never the check.
async fn ensure_slug(
    metadata: &dyn MetadataSource,
    stored: Option<&str>,
    source_series_id: &str,
) -> Option<String> {
    if let Some(slug) = stored {
        return Some(slug.to_string());
    }
    match metadata.series_slug(source_series_id).await {
        Ok(slug) => slug,
        Err(error) => {
            warn!(%source_series_id, %error, "could not fetch series slug; leaving unset");
            None
        }
    }
}

/// Step 5: merges Audnexus narrator/runtime/summary into the event when it
/// has an ASIN and enrichment is enabled. Failure (or a 404) is logged and
/// the event proceeds unenriched; enrichment never fails a check.
async fn enrich_event(event: &mut ReleaseEvent, enricher: Option<&AudnexusClient>) {
    let data = match event {
        ReleaseEvent::NewRelease(data)
        | ReleaseEvent::Upcoming(data)
        | ReleaseEvent::Reminder(data) => data,
    };
    let (Some(enricher), Some(asin)) = (enricher, data.asin.clone()) else {
        return;
    };

    match enricher.book(&asin).await {
        Ok(Some(enrichment)) => {
            if !enrichment.narrators.is_empty() {
                data.narrators = Some(enrichment.narrators);
            }
            data.runtime_minutes = enrichment.runtime_minutes.map(i64::from);
            data.summary = enrichment.summary;
        }
        Ok(None) => info!(
            asin,
            "audnexus has no record for this ASIN; proceeding unenriched"
        ),
        Err(e) => warn!(asin, error = %e, "audnexus enrichment failed; proceeding unenriched"),
    }
}

/// Identity of a remote book, used to map a `ReleaseEvent` (which carries no
/// `source_book_id`) back to the `known_releases` row upserted for the same
/// book. Position is compared by exact f64 bits: both sides come from the
/// very same `RemoteBook` value within one run, so no epsilon is needed.
#[derive(Debug, PartialEq, Eq, Hash)]
struct EventKey {
    title: String,
    release_date: Option<NaiveDate>,
    position_bits: Option<u64>,
}

impl EventKey {
    fn from_book(title: &str, release_date: Option<NaiveDate>, position: Option<f64>) -> Self {
        Self {
            title: title.to_string(),
            release_date,
            position_bits: position.map(f64::to_bits),
        }
    }

    fn from_event(data: &ReleaseEventData) -> Self {
        // Events always carry a concrete release_date (no-date books never
        // fire events), hence the `Some` wrapping.
        Self::from_book(&data.book_title, Some(data.release_date), data.position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abs::ItemSeries;
    use crate::db::models::ReleaseStatus;
    use crate::notify::LogNotifier;
    use chrono::{Duration, Utc};

    #[test]
    fn exit_code_maps_status_per_plan() {
        let report = |status| CheckReport {
            status,
            series_checked: 0,
            events_found: 0,
            errors: vec![],
        };
        assert_eq!(report(CheckRunStatus::Ok).exit_code(), 0);
        assert_eq!(report(CheckRunStatus::Partial).exit_code(), 1);
        assert_eq!(report(CheckRunStatus::Failed).exit_code(), 2);
    }

    #[test]
    fn sequence_for_series_picks_matching_series_and_parses_decimals() {
        let metadata = ItemMetadata {
            title: "Some Book".into(),
            series: vec![
                ItemSeries {
                    id: "other".into(),
                    name: "Other Series".into(),
                    sequence: Some("7".into()),
                },
                ItemSeries {
                    id: "ser_1".into(),
                    name: "Target Series".into(),
                    sequence: Some("2.5".into()),
                },
            ],
            ..Default::default()
        };

        assert_eq!(sequence_for_series(&metadata, "ser_1"), Some(2.5));
        assert_eq!(sequence_for_series(&metadata, "other"), Some(7.0));
        assert_eq!(sequence_for_series(&metadata, "missing"), None);
    }

    #[test]
    fn sequence_for_series_handles_missing_or_garbage_sequences() {
        let metadata = ItemMetadata {
            series: vec![ItemSeries {
                id: "ser_1".into(),
                name: "Target".into(),
                sequence: Some("not-a-number".into()),
            }],
            ..Default::default()
        };
        assert_eq!(sequence_for_series(&metadata, "ser_1"), None);

        let metadata = ItemMetadata {
            series: vec![ItemSeries {
                id: "ser_1".into(),
                name: "Target".into(),
                sequence: None,
            }],
            ..Default::default()
        };
        assert_eq!(sequence_for_series(&metadata, "ser_1"), None);
    }

    #[test]
    fn non_empty_filters_blank_strings() {
        assert_eq!(non_empty(Some("B0064CGH1W")), Some("B0064CGH1W"));
        assert_eq!(non_empty(Some("")), None);
        assert_eq!(non_empty(Some("   ")), None);
        assert_eq!(non_empty(None), None);
    }

    #[tokio::test]
    async fn process_reminders_sends_once_and_stamps() {
        let pool = crate::db::init(":memory:").await.expect("init db");
        let today = Utc::now().date_naive();
        let series = queries::upsert_series(&pool, "abs-1", "The Expanse", false)
            .await
            .expect("upsert series");
        let release = queries::upsert_known_release(
            &pool,
            series.id,
            "hc-1",
            "Leviathan Falls",
            Some(9.0),
            Some(today + Duration::days(2)),
            ReleaseStatus::Upcoming,
            None,
        )
        .await
        .expect("upsert release");
        queries::add_watch(&pool, release.id, -3)
            .await
            .expect("add watch");

        // LogNotifier always succeeds, so the reminder is stamped.
        let notifiers: Vec<Box<dyn Notifier>> = vec![Box::new(LogNotifier)];
        let sent = process_reminders(&pool, &notifiers, today)
            .await
            .expect("process reminders");
        assert_eq!(sent, 1);

        // Second pass finds nothing pending.
        let sent = process_reminders(&pool, &notifiers, today)
            .await
            .expect("process reminders again");
        assert_eq!(sent, 0);
    }

    #[tokio::test]
    async fn process_reminders_leaves_out_of_window_watches_alone() {
        let pool = crate::db::init(":memory:").await.expect("init db");
        let today = Utc::now().date_naive();
        let series = queries::upsert_series(&pool, "abs-1", "The Expanse", false)
            .await
            .expect("upsert series");
        let release = queries::upsert_known_release(
            &pool,
            series.id,
            "hc-1",
            "Leviathan Falls",
            None,
            Some(today + Duration::days(30)),
            ReleaseStatus::Upcoming,
            None,
        )
        .await
        .expect("upsert release");
        queries::add_watch(&pool, release.id, -1)
            .await
            .expect("add watch");

        let notifiers: Vec<Box<dyn Notifier>> = vec![Box::new(LogNotifier)];
        let sent = process_reminders(&pool, &notifiers, today)
            .await
            .expect("process reminders");
        assert_eq!(sent, 0);
    }

    #[test]
    fn event_key_round_trips_between_book_and_event() {
        let date = NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();
        let data = ReleaseEventData {
            series_name: "Series".into(),
            book_title: "Book Two".into(),
            position: Some(2.0),
            release_date: date,
            asin: None,
            narrators: None,
            runtime_minutes: None,
            summary: None,
        };

        assert_eq!(
            EventKey::from_event(&data),
            EventKey::from_book("Book Two", Some(date), Some(2.0))
        );
    }
}
