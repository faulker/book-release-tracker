//! All SQL lives here, behind typed functions. Uses the non-macro sqlx API
//! (`sqlx::query`/`sqlx::query_as::<_, T>`) per PLAN.md section 1.

use chrono::{NaiveDate, Utc};
use sqlx::SqlitePool;
use thiserror::Error;

use super::models::{
    CheckRun, CheckRunStatus, KnownRelease, MatchStatus, OwnedBook, Series, SeriesOverride, Watch,
    WatchedRelease,
};

/// Errors produced while running a query. Wraps `sqlx::Error` so callers keep
/// the underlying cause without every call site matching on `sqlx` directly.
#[derive(Debug, Error)]
pub enum QueryError {
    #[error("database query failed: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("could not serialize check_run errors: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// Which notification a `known_releases` row is being stamped for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifiedKind {
    Upcoming,
    Released,
}

/// Inserts a series, or updates its ABS-sourced fields (name, completed flag)
/// if `abs_series_id` already exists. Never touches match/Hardcover fields,
/// so a re-sync doesn't undo prior matching work. Seeing a series in ABS
/// again un-archives it.
pub async fn upsert_series(
    pool: &SqlitePool,
    abs_series_id: &str,
    abs_name: &str,
    is_completed: bool,
) -> Result<Series, QueryError> {
    let series = sqlx::query_as::<_, Series>(
        r#"
        INSERT INTO series (abs_series_id, abs_name, is_completed)
        VALUES (?, ?, ?)
        ON CONFLICT(abs_series_id) DO UPDATE SET
            abs_name = excluded.abs_name,
            is_completed = excluded.is_completed,
            archived_at = NULL
        RETURNING *
        "#,
    )
    .bind(abs_series_id)
    .bind(abs_name)
    .bind(is_completed)
    .fetch_one(pool)
    .await?;

    Ok(series)
}

/// Inserts an owned book, or updates it if `abs_item_id` already exists.
#[allow(clippy::too_many_arguments)] // one arg per column; a params struct would just move the noise
pub async fn upsert_owned_book(
    pool: &SqlitePool,
    series_id: i64,
    abs_item_id: &str,
    title: &str,
    sequence: Option<f64>,
    asin: Option<&str>,
    isbn: Option<&str>,
    author: Option<&str>,
) -> Result<OwnedBook, QueryError> {
    let book = sqlx::query_as::<_, OwnedBook>(
        r#"
        INSERT INTO owned_books (series_id, abs_item_id, title, sequence, asin, isbn, author)
        VALUES (?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(abs_item_id) DO UPDATE SET
            series_id = excluded.series_id,
            title = excluded.title,
            sequence = excluded.sequence,
            asin = excluded.asin,
            isbn = excluded.isbn,
            author = excluded.author
        RETURNING *
        "#,
    )
    .bind(series_id)
    .bind(abs_item_id)
    .bind(title)
    .bind(sequence)
    .bind(asin)
    .bind(isbn)
    .bind(author)
    .fetch_one(pool)
    .await?;

    Ok(book)
}

/// All active (non-archived) series with the given `match_status`, e.g.
/// `Unmatched` for the dashboard's review queue.
pub async fn get_series_by_match_status(
    pool: &SqlitePool,
    status: MatchStatus,
) -> Result<Vec<Series>, QueryError> {
    let rows = sqlx::query_as::<_, Series>(
        "SELECT * FROM series WHERE match_status = ? AND archived_at IS NULL",
    )
    .bind(status)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// One tracked series by id, if it exists (`GET /series/{id}`).
pub async fn get_series(pool: &SqlitePool, id: i64) -> Result<Option<Series>, QueryError> {
    let row = sqlx::query_as::<_, Series>("SELECT * FROM series WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;

    Ok(row)
}

/// Every known release of one series in reading order (position, then
/// date; unpositioned books last), for the series detail page.
pub async fn list_series_releases(
    pool: &SqlitePool,
    series_id: i64,
) -> Result<Vec<KnownRelease>, QueryError> {
    let rows = sqlx::query_as::<_, KnownRelease>(
        r#"
        SELECT * FROM known_releases
        WHERE series_id = ?
        ORDER BY position IS NULL, position ASC,
                 release_date IS NULL, release_date ASC
        "#,
    )
    .bind(series_id)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// All tracked series, sorted by ABS name. The dashboard uses this to map
/// `series_id`s to names and to find rows affected by a new override.
pub async fn list_series(pool: &SqlitePool) -> Result<Vec<Series>, QueryError> {
    let rows = sqlx::query_as::<_, Series>("SELECT * FROM series ORDER BY abs_name")
        .fetch_all(pool)
        .await?;

    Ok(rows)
}

/// Records the outcome of matching a series against Hardcover (or an
/// override/ignore decision) and stamps `last_checked_at`.
pub async fn set_series_match(
    pool: &SqlitePool,
    series_id: i64,
    hardcover_series_id: Option<&str>,
    hardcover_name: Option<&str>,
    hardcover_slug: Option<&str>,
    match_status: MatchStatus,
) -> Result<(), QueryError> {
    sqlx::query(
        r#"
        UPDATE series SET
            hardcover_series_id = ?,
            hardcover_name = ?,
            hardcover_slug = ?,
            match_status = ?,
            last_checked_at = ?
        WHERE id = ?
        "#,
    )
    .bind(hardcover_series_id)
    .bind(hardcover_name)
    .bind(hardcover_slug)
    .bind(match_status)
    .bind(Utc::now())
    .bind(series_id)
    .execute(pool)
    .await?;

    Ok(())
}

/// Inserts a newly-seen remote book, or updates its title/position/date/
/// status/asin if `(series_id, source_book_id)` already exists (the `UNIQUE`
/// constraint dedupes re-fetches). `first_seen_at` and the notified-at
/// columns are set only on first insert and never overwritten.
#[allow(clippy::too_many_arguments)] // one arg per column; a params struct would just move the noise
pub async fn upsert_known_release(
    pool: &SqlitePool,
    series_id: i64,
    source_book_id: &str,
    title: &str,
    position: Option<f64>,
    release_date: Option<NaiveDate>,
    status: super::models::ReleaseStatus,
    asin: Option<&str>,
) -> Result<KnownRelease, QueryError> {
    let release = sqlx::query_as::<_, KnownRelease>(
        r#"
        INSERT INTO known_releases
            (series_id, source_book_id, title, position, release_date, status, asin, first_seen_at)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(series_id, source_book_id) DO UPDATE SET
            title = excluded.title,
            position = excluded.position,
            release_date = excluded.release_date,
            status = excluded.status,
            asin = excluded.asin
        RETURNING *
        "#,
    )
    .bind(series_id)
    .bind(source_book_id)
    .bind(title)
    .bind(position)
    .bind(release_date)
    .bind(status)
    .bind(asin)
    .bind(Utc::now())
    .fetch_one(pool)
    .await?;

    Ok(release)
}

/// All `known_releases` rows for one series, as prior state for the
/// checker's diff phase.
pub async fn known_releases_for_series(
    pool: &SqlitePool,
    series_id: i64,
) -> Result<Vec<KnownRelease>, QueryError> {
    let rows =
        sqlx::query_as::<_, KnownRelease>("SELECT * FROM known_releases WHERE series_id = ?")
            .bind(series_id)
            .fetch_all(pool)
            .await?;

    Ok(rows)
}

/// All `owned_books` rows for one series, for re-diffing a series without
/// re-reading ABS.
pub async fn owned_books_for_series(
    pool: &SqlitePool,
    series_id: i64,
) -> Result<Vec<OwnedBook>, QueryError> {
    let rows = sqlx::query_as::<_, OwnedBook>("SELECT * FROM owned_books WHERE series_id = ?")
        .bind(series_id)
        .fetch_all(pool)
        .await?;

    Ok(rows)
}

/// Archives every active series whose ABS id is not in `seen_abs_ids` and
/// deletes its owned books and known releases (watches cascade), all in one
/// transaction. The series row and its match survive so a later sync that
/// sees it again can restore it. Returns the number of series archived.
pub async fn archive_missing_series(
    pool: &SqlitePool,
    seen_abs_ids: &[String],
) -> Result<u64, QueryError> {
    let mut tx = pool.begin().await?;
    let archived = sqlx::query(
        r#"
        UPDATE series SET archived_at = ?
        WHERE archived_at IS NULL
          AND abs_series_id NOT IN (SELECT value FROM json_each(?))
        "#,
    )
    .bind(Utc::now())
    // One JSON array param instead of an IN list, so large libraries can't
    // hit sqlite's bound-parameter limit.
    .bind(serde_json::to_string(seen_abs_ids)?)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    // Also sweeps rows left on already-archived series, keeping this idempotent.
    for table in ["owned_books", "known_releases"] {
        sqlx::query(&format!(
            "DELETE FROM {table} WHERE series_id IN \
             (SELECT id FROM series WHERE archived_at IS NOT NULL)"
        ))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    Ok(archived)
}

/// Deletes every `known_releases` row for one series (watches cascade).
/// Used when a series is re-pointed at a different Hardcover series, so
/// books from the old match don't linger.
pub async fn delete_known_releases_for_series(
    pool: &SqlitePool,
    series_id: i64,
) -> Result<(), QueryError> {
    sqlx::query("DELETE FROM known_releases WHERE series_id = ?")
        .bind(series_id)
        .execute(pool)
        .await?;

    Ok(())
}

/// Releases due an "upcoming" notification: a future `release_date`, not yet
/// stamped, not already owned.
pub async fn releases_needing_upcoming_notification(
    pool: &SqlitePool,
    today: NaiveDate,
) -> Result<Vec<KnownRelease>, QueryError> {
    let rows = sqlx::query_as::<_, KnownRelease>(
        r#"
        SELECT * FROM known_releases
        WHERE release_date IS NOT NULL
          AND release_date > ?
          AND upcoming_notified_at IS NULL
          AND status != 'owned'
        "#,
    )
    .bind(today)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Releases due a "released" notification: `release_date` today or in the
/// past, not yet stamped, not already owned.
pub async fn releases_needing_released_notification(
    pool: &SqlitePool,
    today: NaiveDate,
) -> Result<Vec<KnownRelease>, QueryError> {
    let rows = sqlx::query_as::<_, KnownRelease>(
        r#"
        SELECT * FROM known_releases
        WHERE release_date IS NOT NULL
          AND release_date <= ?
          AND released_notified_at IS NULL
          AND status != 'owned'
        "#,
    )
    .bind(today)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Stamps a `known_releases` row as notified for `kind`. Called only after at
/// least one notifier has succeeded, so a total outage lets the next run
/// retry (see PLAN.md section 4).
pub async fn stamp_notified(
    pool: &SqlitePool,
    known_release_id: i64,
    kind: NotifiedKind,
) -> Result<(), QueryError> {
    let column = match kind {
        NotifiedKind::Upcoming => "upcoming_notified_at",
        NotifiedKind::Released => "released_notified_at",
    };
    let sql = format!("UPDATE known_releases SET {column} = ? WHERE id = ?");

    sqlx::query(&sql)
        .bind(Utc::now())
        .bind(known_release_id)
        .execute(pool)
        .await?;

    Ok(())
}

/// Looks up a manual series override by normalized ABS name, if one exists.
pub async fn get_series_override(
    pool: &SqlitePool,
    abs_name_normalized: &str,
) -> Result<Option<SeriesOverride>, QueryError> {
    let row = sqlx::query_as::<_, SeriesOverride>(
        "SELECT * FROM series_overrides WHERE abs_name_normalized = ?",
    )
    .bind(abs_name_normalized)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

/// Creates or replaces a manual series override (dashboard `POST /overrides`).
pub async fn set_series_override(
    pool: &SqlitePool,
    abs_name_normalized: &str,
    hardcover_series_id: Option<&str>,
    ignore: bool,
) -> Result<SeriesOverride, QueryError> {
    let row = sqlx::query_as::<_, SeriesOverride>(
        r#"
        INSERT INTO series_overrides (abs_name_normalized, hardcover_series_id, ignore)
        VALUES (?, ?, ?)
        ON CONFLICT(abs_name_normalized) DO UPDATE SET
            hardcover_series_id = excluded.hardcover_series_id,
            ignore = excluded.ignore
        RETURNING *
        "#,
    )
    .bind(abs_name_normalized)
    .bind(hardcover_series_id)
    .bind(ignore)
    .fetch_one(pool)
    .await?;

    Ok(row)
}

/// Starts a new check run and returns its id. Status begins `running`;
/// finalize with [`finalize_check_run`].
pub async fn insert_check_run(pool: &SqlitePool) -> Result<i64, QueryError> {
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO check_runs (started_at, status) VALUES (?, 'running') RETURNING id",
    )
    .bind(Utc::now())
    .fetch_one(pool)
    .await?;

    Ok(id)
}

/// Marks a check run finished with its final status and tallies. `errors` is
/// serialized as a JSON array of per-series error strings.
pub async fn finalize_check_run(
    pool: &SqlitePool,
    id: i64,
    status: CheckRunStatus,
    series_checked: i64,
    events_found: i64,
    errors: &[String],
) -> Result<(), QueryError> {
    let errors_json = if errors.is_empty() {
        None
    } else {
        Some(serde_json::to_string(errors)?)
    };

    sqlx::query(
        r#"
        UPDATE check_runs SET
            finished_at = ?,
            status = ?,
            series_checked = ?,
            events_found = ?,
            errors = ?
        WHERE id = ?
        "#,
    )
    .bind(Utc::now())
    .bind(status)
    .bind(series_checked)
    .bind(events_found)
    .bind(errors_json)
    .bind(id)
    .execute(pool)
    .await?;

    Ok(())
}

/// Marks every check run still `running` as `timed_out`, stamping
/// `finished_at` and recording `message` as its only error. Called after a
/// run is cancelled for exceeding the configured timeout.
pub async fn time_out_running_check_runs(
    pool: &SqlitePool,
    message: &str,
) -> Result<u64, QueryError> {
    let errors_json = serde_json::to_string(&[message])?;
    let result = sqlx::query(
        "UPDATE check_runs SET finished_at = ?, status = 'timed_out', errors = ? WHERE status = 'running'",
    )
    .bind(Utc::now())
    .bind(errors_json)
    .execute(pool)
    .await?;

    Ok(result.rows_affected())
}

/// Most recent check runs, newest first, for the dashboard status page.
pub async fn recent_check_runs(pool: &SqlitePool, limit: i64) -> Result<Vec<CheckRun>, QueryError> {
    let rows =
        sqlx::query_as::<_, CheckRun>("SELECT * FROM check_runs ORDER BY started_at DESC LIMIT ?")
            .bind(limit)
            .fetch_all(pool)
            .await?;

    Ok(rows)
}

/// Releases with `status = 'upcoming'`, sorted by `release_date` ascending
/// with `NULL` dates last (`GET /upcoming`). Releases of ignored series are
/// left out.
pub async fn list_upcoming_releases(pool: &SqlitePool) -> Result<Vec<KnownRelease>, QueryError> {
    let rows = sqlx::query_as::<_, KnownRelease>(
        r#"
        SELECT kr.* FROM known_releases kr
        JOIN series s ON s.id = kr.series_id
        WHERE kr.status = 'upcoming' AND s.match_status != 'ignored'
        ORDER BY kr.release_date IS NULL, kr.release_date ASC
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Releases with `status = 'released'`, newest first (`GET /new`).
/// Releases of ignored series are left out.
pub async fn list_new_releases(pool: &SqlitePool) -> Result<Vec<KnownRelease>, QueryError> {
    let rows = sqlx::query_as::<_, KnownRelease>(
        r#"
        SELECT kr.* FROM known_releases kr
        JOIN series s ON s.id = kr.series_id
        WHERE kr.status = 'released' AND s.match_status != 'ignored'
        ORDER BY kr.release_date DESC
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// One known release by id, if it exists. The dashboard uses this to
/// validate watch form posts before touching the watch list.
pub async fn get_known_release(
    pool: &SqlitePool,
    id: i64,
) -> Result<Option<KnownRelease>, QueryError> {
    let row = sqlx::query_as::<_, KnownRelease>("SELECT * FROM known_releases WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;

    Ok(row)
}

/// Adds a release to the watch list, or updates its reminder window if
/// already watched. `remind_offset_days` is signed: negative = days before
/// the release, positive = days after. Changing the window resets the
/// delivered stamp so the reminder fires again at the new time.
pub async fn add_watch(
    pool: &SqlitePool,
    known_release_id: i64,
    remind_offset_days: i64,
) -> Result<Watch, QueryError> {
    let watch = sqlx::query_as::<_, Watch>(
        r#"
        INSERT INTO watches (known_release_id, remind_offset_days, created_at)
        VALUES (?, ?, ?)
        ON CONFLICT(known_release_id) DO UPDATE SET
            remind_offset_days = excluded.remind_offset_days,
            reminder_notified_at = NULL
        RETURNING *
        "#,
    )
    .bind(known_release_id)
    .bind(remind_offset_days)
    .bind(Utc::now())
    .fetch_one(pool)
    .await?;

    Ok(watch)
}

/// Removes a release from the watch list. Removing an unwatched release is a
/// no-op.
pub async fn remove_watch(pool: &SqlitePool, known_release_id: i64) -> Result<(), QueryError> {
    sqlx::query("DELETE FROM watches WHERE known_release_id = ?")
        .bind(known_release_id)
        .execute(pool)
        .await?;

    Ok(())
}

/// Every watched release with its series name, soonest release date first,
/// unknown dates last (`GET /watchlist`).
pub async fn list_watched_releases(pool: &SqlitePool) -> Result<Vec<WatchedRelease>, QueryError> {
    let rows = sqlx::query_as::<_, WatchedRelease>(
        r#"
        SELECT
            kr.id AS known_release_id,
            w.remind_offset_days,
            w.reminder_notified_at,
            s.abs_name AS series_name,
            kr.title,
            kr.position,
            kr.release_date,
            kr.asin
        FROM watches w
        JOIN known_releases kr ON kr.id = w.known_release_id
        JOIN series s ON s.id = kr.series_id
        ORDER BY kr.release_date IS NULL, kr.release_date ASC
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Watched releases whose reminder window has arrived: a known release date,
/// `release_date + remind_offset_days` on or before `today`, and no reminder
/// delivered yet.
pub async fn watches_due_reminder(
    pool: &SqlitePool,
    today: NaiveDate,
) -> Result<Vec<WatchedRelease>, QueryError> {
    // SQLite date modifiers accept both "-3 days" and "7 days" (unsigned means
    // forward), so concatenating the signed offset directly works for both
    // before- and after-release windows.
    let rows = sqlx::query_as::<_, WatchedRelease>(
        r#"
        SELECT
            kr.id AS known_release_id,
            w.remind_offset_days,
            w.reminder_notified_at,
            s.abs_name AS series_name,
            kr.title,
            kr.position,
            kr.release_date,
            kr.asin
        FROM watches w
        JOIN known_releases kr ON kr.id = w.known_release_id
        JOIN series s ON s.id = kr.series_id
        WHERE kr.release_date IS NOT NULL
          AND w.reminder_notified_at IS NULL
          AND DATE(kr.release_date, w.remind_offset_days || ' days') <= DATE(?)
        ORDER BY kr.release_date ASC
        "#,
    )
    .bind(today)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Stamps a watch as reminded. Called only after at least one notifier
/// succeeded, so a total outage retries next time (same rule as
/// [`stamp_notified`]).
pub async fn stamp_watch_reminded(
    pool: &SqlitePool,
    known_release_id: i64,
) -> Result<(), QueryError> {
    sqlx::query("UPDATE watches SET reminder_notified_at = ? WHERE known_release_id = ?")
        .bind(Utc::now())
        .bind(known_release_id)
        .execute(pool)
        .await?;

    Ok(())
}

/// Ids of releases currently on the watch list, for the upcoming page's
/// watch/unwatch buttons.
pub async fn watched_release_ids(
    pool: &SqlitePool,
) -> Result<std::collections::HashSet<i64>, QueryError> {
    let rows: Vec<(i64,)> = sqlx::query_as("SELECT known_release_id FROM watches")
        .fetch_all(pool)
        .await?;

    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// When the most recent completed check run finished (`ok` or `partial`;
/// failed runs don't count), or `None` if no run has ever completed. Drives
/// the scheduler's scan cooldown.
pub async fn latest_completed_check_run(
    pool: &SqlitePool,
) -> Result<Option<chrono::DateTime<Utc>>, QueryError> {
    let (finished_at,): (Option<chrono::DateTime<Utc>>,) =
        sqlx::query_as("SELECT MAX(finished_at) FROM check_runs WHERE status IN ('ok', 'partial')")
            .fetch_one(pool)
            .await?;

    Ok(finished_at)
}

/// Every tracked series with its release outlook, sorted by next release date
/// (soonest first, series with nothing scheduled after, then by name):
/// distinct owned-book authors, the soonest scheduled (future) upcoming
/// release, and the most recent release date on or before `today`
/// (`GET /series`).
pub async fn list_series_overview(
    pool: &SqlitePool,
    today: NaiveDate,
) -> Result<Vec<super::models::SeriesOverview>, QueryError> {
    let rows = sqlx::query_as::<_, super::models::SeriesOverview>(
        r#"
        SELECT
            s.id,
            s.abs_name,
            s.match_status,
            s.archived_at,
            (SELECT GROUP_CONCAT(DISTINCT ob.author)
               FROM owned_books ob
              WHERE ob.series_id = s.id AND ob.author IS NOT NULL) AS authors,
            (SELECT kr.title FROM known_releases kr
              WHERE kr.series_id = s.id AND kr.status = 'upcoming'
                AND kr.release_date IS NOT NULL AND kr.release_date > ?1
              ORDER BY kr.release_date ASC LIMIT 1) AS next_title,
            (SELECT kr.release_date FROM known_releases kr
              WHERE kr.series_id = s.id AND kr.status = 'upcoming'
                AND kr.release_date IS NOT NULL AND kr.release_date > ?1
              ORDER BY kr.release_date ASC LIMIT 1) AS next_release_date,
            (SELECT MAX(kr.release_date) FROM known_releases kr
              WHERE kr.series_id = s.id AND kr.release_date <= ?1) AS last_release_date
        FROM series s
        ORDER BY next_release_date IS NULL, next_release_date ASC,
                 s.abs_name COLLATE NOCASE
        "#,
    )
    .bind(today)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Ids of series that have an upcoming release scheduled strictly after
/// `today`. A partial scan skips these: the next book is already known and
/// its date hasn't arrived, so there is nothing new to learn yet.
pub async fn series_ids_with_scheduled_release(
    pool: &SqlitePool,
    today: NaiveDate,
) -> Result<Vec<i64>, QueryError> {
    let rows: Vec<(i64,)> = sqlx::query_as(
        r#"
        SELECT DISTINCT series_id FROM known_releases
        WHERE status = 'upcoming'
          AND release_date IS NOT NULL
          AND release_date > ?
        "#,
    )
    .bind(today)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Distinct owned-book authors per series, comma-joined, for the release
/// pages' author column and filter.
pub async fn authors_by_series(
    pool: &SqlitePool,
) -> Result<std::collections::HashMap<i64, String>, QueryError> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        r#"
        SELECT series_id, GROUP_CONCAT(DISTINCT author)
        FROM owned_books
        WHERE author IS NOT NULL
        GROUP BY series_id
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::ReleaseStatus;

    async fn test_pool() -> SqlitePool {
        crate::db::init(":memory:").await.expect("init test db")
    }

    #[tokio::test]
    async fn upsert_series_is_idempotent() {
        let pool = test_pool().await;

        let first = upsert_series(&pool, "abs-1", "Original Name", false)
            .await
            .expect("first upsert");
        let second = upsert_series(&pool, "abs-1", "Renamed", true)
            .await
            .expect("second upsert");

        assert_eq!(first.id, second.id);
        assert_eq!(second.abs_name, "Renamed");
        assert!(second.is_completed);

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM series")
            .fetch_one(&pool)
            .await
            .expect("count series");
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn upsert_owned_book_is_idempotent() {
        let pool = test_pool().await;
        let series = upsert_series(&pool, "abs-1", "Some Series", false)
            .await
            .expect("upsert series");

        let first = upsert_owned_book(
            &pool,
            series.id,
            "item-1",
            "Book One",
            Some(1.0),
            None,
            None,
            None,
        )
        .await
        .expect("first upsert");
        let second = upsert_owned_book(
            &pool,
            series.id,
            "item-1",
            "Book One (revised title)",
            Some(1.0),
            Some("ASIN123"),
            None,
            Some("Jane Author"),
        )
        .await
        .expect("second upsert");

        assert_eq!(first.id, second.id);
        assert_eq!(second.title, "Book One (revised title)");
        assert_eq!(second.author.as_deref(), Some("Jane Author"));

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM owned_books")
            .fetch_one(&pool)
            .await
            .expect("count owned_books");
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn known_releases_dedupe_on_series_and_source_book_id() {
        let pool = test_pool().await;
        let series = upsert_series(&pool, "abs-1", "Some Series", false)
            .await
            .expect("upsert series");

        let first = upsert_known_release(
            &pool,
            series.id,
            "hc-book-1",
            "Book One",
            Some(1.0),
            None,
            ReleaseStatus::Upcoming,
            None,
        )
        .await
        .expect("first upsert");
        let second = upsert_known_release(
            &pool,
            series.id,
            "hc-book-1",
            "Book One (renamed)",
            Some(1.0),
            None,
            ReleaseStatus::Upcoming,
            None,
        )
        .await
        .expect("second upsert");

        assert_eq!(first.id, second.id);
        assert_eq!(second.title, "Book One (renamed)");
        // first_seen_at must not move on re-upsert.
        assert_eq!(first.first_seen_at, second.first_seen_at);

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM known_releases")
            .fetch_one(&pool)
            .await
            .expect("count known_releases");
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn known_releases_for_series_returns_only_that_series() {
        let pool = test_pool().await;
        let series_a = upsert_series(&pool, "abs-a", "Series A", false)
            .await
            .expect("upsert series a");
        let series_b = upsert_series(&pool, "abs-b", "Series B", false)
            .await
            .expect("upsert series b");

        upsert_known_release(
            &pool,
            series_a.id,
            "hc-a1",
            "Book A1",
            Some(1.0),
            None,
            ReleaseStatus::Upcoming,
            None,
        )
        .await
        .expect("upsert a1");
        upsert_known_release(
            &pool,
            series_b.id,
            "hc-b1",
            "Book B1",
            Some(1.0),
            None,
            ReleaseStatus::Upcoming,
            None,
        )
        .await
        .expect("upsert b1");

        let rows = known_releases_for_series(&pool, series_a.id)
            .await
            .expect("query series a releases");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source_book_id, "hc-a1");
    }

    #[tokio::test]
    async fn notification_queries_exclude_already_stamped_rows() {
        let pool = test_pool().await;
        let series = upsert_series(&pool, "abs-1", "Some Series", false)
            .await
            .expect("upsert series");

        let today = chrono::Utc::now().date_naive();
        let past = today - chrono::Duration::days(1);
        let future = today + chrono::Duration::days(1);

        let upcoming = upsert_known_release(
            &pool,
            series.id,
            "hc-upcoming",
            "Upcoming Book",
            None,
            Some(future),
            ReleaseStatus::Upcoming,
            None,
        )
        .await
        .expect("upsert upcoming");
        let released = upsert_known_release(
            &pool,
            series.id,
            "hc-released",
            "Released Book",
            None,
            Some(past),
            ReleaseStatus::Released,
            None,
        )
        .await
        .expect("upsert released");

        let needing_upcoming = releases_needing_upcoming_notification(&pool, today)
            .await
            .expect("query upcoming");
        assert_eq!(needing_upcoming.len(), 1);
        assert_eq!(needing_upcoming[0].id, upcoming.id);

        let needing_released = releases_needing_released_notification(&pool, today)
            .await
            .expect("query released");
        assert_eq!(needing_released.len(), 1);
        assert_eq!(needing_released[0].id, released.id);

        // Stamp both, then confirm they no longer show up.
        stamp_notified(&pool, upcoming.id, NotifiedKind::Upcoming)
            .await
            .expect("stamp upcoming");
        stamp_notified(&pool, released.id, NotifiedKind::Released)
            .await
            .expect("stamp released");

        let needing_upcoming = releases_needing_upcoming_notification(&pool, today)
            .await
            .expect("query upcoming after stamp");
        let needing_released = releases_needing_released_notification(&pool, today)
            .await
            .expect("query released after stamp");
        assert!(needing_upcoming.is_empty());
        assert!(needing_released.is_empty());
    }

    #[tokio::test]
    async fn list_series_returns_all_sorted_by_name() {
        let pool = test_pool().await;
        upsert_series(&pool, "abs-b", "Zeta Series", false)
            .await
            .expect("upsert zeta");
        upsert_series(&pool, "abs-a", "Alpha Series", false)
            .await
            .expect("upsert alpha");

        let rows = list_series(&pool).await.expect("list series");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].abs_name, "Alpha Series");
        assert_eq!(rows[1].abs_name, "Zeta Series");
    }

    #[tokio::test]
    async fn series_override_get_and_set() {
        let pool = test_pool().await;

        assert!(
            get_series_override(&pool, "some series")
                .await
                .expect("lookup missing override")
                .is_none()
        );

        let created = set_series_override(&pool, "some series", Some("hc-42"), false)
            .await
            .expect("create override");
        assert_eq!(created.hardcover_series_id.as_deref(), Some("hc-42"));
        assert!(!created.ignore);

        let updated = set_series_override(&pool, "some series", None, true)
            .await
            .expect("update override");
        assert_eq!(updated.hardcover_series_id, None);
        assert!(updated.ignore);

        let fetched = get_series_override(&pool, "some series")
            .await
            .expect("lookup override")
            .expect("override present");
        assert_eq!(fetched, updated);
    }

    #[tokio::test]
    async fn series_overview_reports_next_and_last_releases() {
        let pool = test_pool().await;
        let today = chrono::Utc::now().date_naive();

        // Series A: one past release, one scheduled future release.
        let a = upsert_series(&pool, "abs-a", "Alpha Series", false)
            .await
            .expect("upsert a");
        upsert_owned_book(
            &pool,
            a.id,
            "item-a1",
            "Alpha One",
            Some(1.0),
            None,
            None,
            Some("Alice Writer"),
        )
        .await
        .expect("owned a1");
        upsert_known_release(
            &pool,
            a.id,
            "hc-a1",
            "Alpha One",
            Some(1.0),
            Some(today - chrono::Duration::days(30)),
            ReleaseStatus::Owned,
            None,
        )
        .await
        .expect("release a1");
        upsert_known_release(
            &pool,
            a.id,
            "hc-a2",
            "Alpha Two",
            Some(2.0),
            Some(today + chrono::Duration::days(10)),
            ReleaseStatus::Upcoming,
            None,
        )
        .await
        .expect("release a2");

        // Series B: only a past release, nothing scheduled.
        let b = upsert_series(&pool, "abs-b", "Beta Series", false)
            .await
            .expect("upsert b");
        upsert_known_release(
            &pool,
            b.id,
            "hc-b1",
            "Beta One",
            Some(1.0),
            Some(today - chrono::Duration::days(5)),
            ReleaseStatus::Released,
            None,
        )
        .await
        .expect("release b1");

        let rows = list_series_overview(&pool, today).await.expect("overview");
        assert_eq!(rows.len(), 2);

        let alpha = &rows[0];
        assert_eq!(alpha.abs_name, "Alpha Series");
        assert_eq!(alpha.authors.as_deref(), Some("Alice Writer"));
        assert_eq!(alpha.next_title.as_deref(), Some("Alpha Two"));
        assert_eq!(
            alpha.next_release_date,
            Some(today + chrono::Duration::days(10))
        );
        assert_eq!(
            alpha.last_release_date,
            Some(today - chrono::Duration::days(30))
        );

        let beta = &rows[1];
        assert_eq!(beta.next_title, None);
        assert_eq!(beta.next_release_date, None);
        assert_eq!(
            beta.last_release_date,
            Some(today - chrono::Duration::days(5))
        );
    }

    #[tokio::test]
    async fn scheduled_release_ids_only_include_future_upcoming() {
        let pool = test_pool().await;
        let today = chrono::Utc::now().date_naive();

        let scheduled = upsert_series(&pool, "abs-a", "Scheduled", false)
            .await
            .expect("upsert scheduled");
        let overdue = upsert_series(&pool, "abs-b", "Overdue", false)
            .await
            .expect("upsert overdue");
        let idle = upsert_series(&pool, "abs-c", "Idle", false)
            .await
            .expect("upsert idle");

        upsert_known_release(
            &pool,
            scheduled.id,
            "hc-s",
            "Future Book",
            None,
            Some(today + chrono::Duration::days(7)),
            ReleaseStatus::Upcoming,
            None,
        )
        .await
        .expect("future release");
        // Upcoming status but the date already passed: must be re-scanned.
        upsert_known_release(
            &pool,
            overdue.id,
            "hc-o",
            "Overdue Book",
            None,
            Some(today - chrono::Duration::days(1)),
            ReleaseStatus::Upcoming,
            None,
        )
        .await
        .expect("overdue release");
        let _ = idle;

        let ids = series_ids_with_scheduled_release(&pool, today)
            .await
            .expect("scheduled ids");
        assert_eq!(ids, vec![scheduled.id]);
    }

    #[tokio::test]
    async fn authors_by_series_joins_distinct_authors() {
        let pool = test_pool().await;
        let series = upsert_series(&pool, "abs-a", "Some Series", false)
            .await
            .expect("upsert series");

        for (item, author) in [
            ("item-1", Some("Alice Writer")),
            ("item-2", Some("Alice Writer")),
            ("item-3", None),
        ] {
            upsert_owned_book(&pool, series.id, item, item, None, None, None, author)
                .await
                .expect("upsert book");
        }

        let map = authors_by_series(&pool).await.expect("authors map");
        assert_eq!(
            map.get(&series.id).map(String::as_str),
            Some("Alice Writer")
        );
    }

    #[tokio::test]
    async fn watch_lifecycle_add_update_remove() {
        let pool = test_pool().await;
        let series = upsert_series(&pool, "abs-1", "Some Series", false)
            .await
            .expect("upsert series");
        let release = upsert_known_release(
            &pool,
            series.id,
            "hc-1",
            "Book One",
            Some(1.0),
            Some(chrono::Utc::now().date_naive() + chrono::Duration::days(30)),
            ReleaseStatus::Upcoming,
            None,
        )
        .await
        .expect("upsert release");

        let watch = add_watch(&pool, release.id, -3).await.expect("add watch");
        assert_eq!(watch.remind_offset_days, -3);
        assert!(watch.reminder_notified_at.is_none());

        // Stamp, then change the window: the stamp must reset so the
        // reminder fires again at the new time.
        stamp_watch_reminded(&pool, release.id)
            .await
            .expect("stamp watch");
        let updated = add_watch(&pool, release.id, 7).await.expect("update watch");
        assert_eq!(updated.id, watch.id);
        assert_eq!(updated.remind_offset_days, 7);
        assert!(updated.reminder_notified_at.is_none());

        let ids = watched_release_ids(&pool).await.expect("watched ids");
        assert!(ids.contains(&release.id));

        remove_watch(&pool, release.id).await.expect("remove watch");
        assert!(
            watched_release_ids(&pool)
                .await
                .expect("watched ids")
                .is_empty()
        );
        // Removing again is a no-op, not an error.
        remove_watch(&pool, release.id).await.expect("remove again");
    }

    #[tokio::test]
    async fn archive_missing_series_drops_books_and_upsert_restores() {
        let pool = test_pool().await;
        let kept = upsert_series(&pool, "abs-keep", "Kept", false)
            .await
            .expect("upsert kept");
        let gone = upsert_series(&pool, "abs-gone", "Gone", false)
            .await
            .expect("upsert gone");
        for (series_id, item) in [(kept.id, "li-keep"), (gone.id, "li-gone")] {
            upsert_owned_book(&pool, series_id, item, "Book", Some(1.0), None, None, None)
                .await
                .expect("upsert book");
        }
        let release = upsert_known_release(
            &pool,
            gone.id,
            "hc-gone",
            "Gone 2",
            Some(2.0),
            None,
            ReleaseStatus::Upcoming,
            None,
        )
        .await
        .expect("upsert release");
        add_watch(&pool, release.id, 0).await.expect("add watch");

        let archived = archive_missing_series(&pool, &["abs-keep".to_string()])
            .await
            .expect("archive");
        assert_eq!(archived, 1);

        let gone_row = get_series(&pool, gone.id).await.expect("get").expect("row");
        assert!(gone_row.archived_at.is_some());
        assert!(
            owned_books_for_series(&pool, gone.id)
                .await
                .expect("books")
                .is_empty()
        );
        assert!(
            known_releases_for_series(&pool, gone.id)
                .await
                .expect("kr")
                .is_empty()
        );
        assert!(
            watched_release_ids(&pool)
                .await
                .expect("watches")
                .is_empty()
        );
        // The kept series is untouched.
        let kept_row = get_series(&pool, kept.id).await.expect("get").expect("row");
        assert!(kept_row.archived_at.is_none());
        assert_eq!(
            owned_books_for_series(&pool, kept.id)
                .await
                .expect("books")
                .len(),
            1
        );

        // Re-running is a no-op for an already-archived series.
        let again = archive_missing_series(&pool, &["abs-keep".to_string()])
            .await
            .expect("archive again");
        assert_eq!(again, 0);

        // Seen in ABS again: same row comes back active.
        let restored = upsert_series(&pool, "abs-gone", "Gone", false)
            .await
            .expect("restore");
        assert_eq!(restored.id, gone.id);
        assert!(restored.archived_at.is_none());
    }

    #[tokio::test]
    async fn watches_due_reminder_honors_window_and_stamp() {
        let pool = test_pool().await;
        let today = chrono::Utc::now().date_naive();
        let series = upsert_series(&pool, "abs-1", "Some Series", false)
            .await
            .expect("upsert series");

        // Due: releases in 3 days, window is 3 days before.
        let due = upsert_known_release(
            &pool,
            series.id,
            "hc-due",
            "Due Book",
            None,
            Some(today + chrono::Duration::days(3)),
            ReleaseStatus::Upcoming,
            None,
        )
        .await
        .expect("upsert due");
        add_watch(&pool, due.id, -3).await.expect("watch due");

        // Not due yet: releases in 10 days, window is 2 days before.
        let later = upsert_known_release(
            &pool,
            series.id,
            "hc-later",
            "Later Book",
            None,
            Some(today + chrono::Duration::days(10)),
            ReleaseStatus::Upcoming,
            None,
        )
        .await
        .expect("upsert later");
        add_watch(&pool, later.id, -2).await.expect("watch later");

        // No date: never due.
        let undated = upsert_known_release(
            &pool,
            series.id,
            "hc-undated",
            "Undated Book",
            None,
            None,
            ReleaseStatus::Upcoming,
            None,
        )
        .await
        .expect("upsert undated");
        add_watch(&pool, undated.id, -1)
            .await
            .expect("watch undated");

        let pending = watches_due_reminder(&pool, today).await.expect("due list");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].known_release_id, due.id);
        assert_eq!(pending[0].series_name, "Some Series");

        // Once stamped it drops out.
        stamp_watch_reminded(&pool, due.id).await.expect("stamp");
        assert!(
            watches_due_reminder(&pool, today)
                .await
                .expect("due list after stamp")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn watches_due_reminder_supports_after_release_offsets() {
        let pool = test_pool().await;
        let today = chrono::Utc::now().date_naive();
        let series = upsert_series(&pool, "abs-1", "Some Series", false)
            .await
            .expect("upsert series");

        // Due: released 7 days ago, reminder set to 7 days after.
        let due = upsert_known_release(
            &pool,
            series.id,
            "hc-after-due",
            "Week-Old Book",
            None,
            Some(today - chrono::Duration::days(7)),
            ReleaseStatus::Released,
            None,
        )
        .await
        .expect("upsert due");
        add_watch(&pool, due.id, 7).await.expect("watch due");

        // Not due yet: released yesterday, reminder set to 7 days after.
        let later = upsert_known_release(
            &pool,
            series.id,
            "hc-after-later",
            "Fresh Book",
            None,
            Some(today - chrono::Duration::days(1)),
            ReleaseStatus::Released,
            None,
        )
        .await
        .expect("upsert later");
        add_watch(&pool, later.id, 7).await.expect("watch later");

        let pending = watches_due_reminder(&pool, today).await.expect("due list");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].known_release_id, due.id);
        assert_eq!(pending[0].remind_offset_days, 7);
    }

    #[tokio::test]
    async fn list_watched_releases_sorts_by_date_with_unknown_last() {
        let pool = test_pool().await;
        let today = chrono::Utc::now().date_naive();
        let series = upsert_series(&pool, "abs-1", "Some Series", false)
            .await
            .expect("upsert series");

        let far = upsert_known_release(
            &pool,
            series.id,
            "hc-far",
            "Far Book",
            None,
            Some(today + chrono::Duration::days(60)),
            ReleaseStatus::Upcoming,
            None,
        )
        .await
        .expect("upsert far");
        let soon = upsert_known_release(
            &pool,
            series.id,
            "hc-soon",
            "Soon Book",
            None,
            Some(today + chrono::Duration::days(5)),
            ReleaseStatus::Upcoming,
            None,
        )
        .await
        .expect("upsert soon");
        let undated = upsert_known_release(
            &pool,
            series.id,
            "hc-undated",
            "Undated Book",
            None,
            None,
            ReleaseStatus::Upcoming,
            None,
        )
        .await
        .expect("upsert undated");
        for id in [far.id, soon.id, undated.id] {
            add_watch(&pool, id, -1).await.expect("watch");
        }

        let rows = list_watched_releases(&pool).await.expect("list watches");
        let titles: Vec<_> = rows.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(titles, vec!["Soon Book", "Far Book", "Undated Book"]);
    }

    #[tokio::test]
    async fn latest_completed_check_run_ignores_failed_and_running() {
        let pool = test_pool().await;

        assert!(
            latest_completed_check_run(&pool)
                .await
                .expect("empty history")
                .is_none()
        );

        // A failed run and a still-running run don't count.
        let failed = insert_check_run(&pool).await.expect("insert failed run");
        finalize_check_run(&pool, failed, CheckRunStatus::Failed, 0, 0, &[])
            .await
            .expect("finalize failed");
        insert_check_run(&pool).await.expect("insert running run");
        assert!(
            latest_completed_check_run(&pool)
                .await
                .expect("history")
                .is_none()
        );

        let ok = insert_check_run(&pool).await.expect("insert ok run");
        finalize_check_run(&pool, ok, CheckRunStatus::Ok, 1, 0, &[])
            .await
            .expect("finalize ok");
        assert!(
            latest_completed_check_run(&pool)
                .await
                .expect("history")
                .is_some()
        );
    }

    #[tokio::test]
    async fn series_overview_sorts_by_next_release_date() {
        let pool = test_pool().await;
        let today = chrono::Utc::now().date_naive();

        // Named so a name sort would give the wrong order.
        let no_next = upsert_series(&pool, "abs-a", "Aardvark Series", false)
            .await
            .expect("upsert a");
        let later = upsert_series(&pool, "abs-b", "Later Series", false)
            .await
            .expect("upsert b");
        let soon = upsert_series(&pool, "abs-c", "Zebra Series", false)
            .await
            .expect("upsert c");

        upsert_known_release(
            &pool,
            later.id,
            "hc-l",
            "Later Book",
            None,
            Some(today + chrono::Duration::days(30)),
            ReleaseStatus::Upcoming,
            None,
        )
        .await
        .expect("later release");
        upsert_known_release(
            &pool,
            soon.id,
            "hc-s",
            "Soon Book",
            None,
            Some(today + chrono::Duration::days(3)),
            ReleaseStatus::Upcoming,
            None,
        )
        .await
        .expect("soon release");
        let _ = no_next;

        let rows = list_series_overview(&pool, today).await.expect("overview");
        let names: Vec<_> = rows.iter().map(|r| r.abs_name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Zebra Series", "Later Series", "Aardvark Series"]
        );
    }

    #[tokio::test]
    async fn get_known_release_returns_row_or_none() {
        let pool = test_pool().await;
        let series = upsert_series(&pool, "abs-1", "Some Series", false)
            .await
            .expect("upsert series");
        let release = upsert_known_release(
            &pool,
            series.id,
            "hc-1",
            "Book One",
            None,
            None,
            ReleaseStatus::Upcoming,
            None,
        )
        .await
        .expect("upsert release");

        let found = get_known_release(&pool, release.id)
            .await
            .expect("lookup existing");
        assert_eq!(found.map(|r| r.id), Some(release.id));
        assert!(
            get_known_release(&pool, 9999)
                .await
                .expect("lookup missing")
                .is_none()
        );
    }

    #[tokio::test]
    async fn check_run_lifecycle() {
        let pool = test_pool().await;

        let id = insert_check_run(&pool).await.expect("insert check_run");

        let running = recent_check_runs(&pool, 10).await.expect("recent runs");
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].status, CheckRunStatus::Running);
        assert!(running[0].finished_at.is_none());

        finalize_check_run(
            &pool,
            id,
            CheckRunStatus::Partial,
            3,
            2,
            &["series X failed".to_string()],
        )
        .await
        .expect("finalize check_run");

        let finished = recent_check_runs(&pool, 10).await.expect("recent runs");
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0].status, CheckRunStatus::Partial);
        assert!(finished[0].finished_at.is_some());
        assert_eq!(finished[0].series_checked, 3);
        assert_eq!(finished[0].events_found, 2);
        assert_eq!(
            finished[0].errors.as_deref(),
            Some(r#"["series X failed"]"#)
        );
    }
}
