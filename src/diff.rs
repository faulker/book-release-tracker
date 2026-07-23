//! PURE: compares a matched series' Hardcover book list against what's owned
//! in ABS and what's already known, and classifies the result into
//! notification events plus `known_releases` rows to persist. Per PLAN.md
//! section 5 step 4. Never calls `Utc::now()`; `today` is always a parameter
//! so tests stay deterministic. Consumed by `checker.rs`, which calls
//! `diff_and_classify` per matched series, hands `KnownReleaseUpsert`s to
//! `db::queries::upsert_known_release`, and (after a notifier succeeds)
//! stamps `*_notified_at` via `stamp_notified`.

use chrono::NaiveDate;

use crate::db::models::{KnownRelease, OwnedBook, ReleaseStatus};
use crate::matching::normalize_series_name;
use crate::metadata::RemoteBook;

/// Position comparisons use float equality with this epsilon rather than
/// `==`, since positions round-trip through `f64` (e.g. novella 1.5s).
const POSITION_EPSILON: f64 = 1e-6;

/// A release worth telling the user about. Enrichment fields start `None`
/// and are filled in by the checker (milestone 6) when an ASIN is present
/// and Audnexus lookup succeeds; enrichment failure never blocks the event.
#[derive(Debug, Clone, PartialEq)]
pub enum ReleaseEvent {
    /// `release_date <= today`, not owned, not previously released-notified.
    NewRelease(ReleaseEventData),
    /// `release_date > today`, not previously upcoming-notified.
    Upcoming(ReleaseEventData),
    /// A watched release whose reminder window has arrived. Never produced by
    /// `diff_and_classify`; built by `checker::process_reminders` from the
    /// watch list.
    Reminder(ReleaseEventData),
}

/// Fields shared by both `ReleaseEvent` variants.
#[derive(Debug, Clone, PartialEq)]
pub struct ReleaseEventData {
    pub series_name: String,
    pub book_title: String,
    pub position: Option<f64>,
    pub release_date: NaiveDate,
    pub asin: Option<String>,
    pub narrators: Option<Vec<String>>,
    pub runtime_minutes: Option<i64>,
    pub summary: Option<String>,
}

impl ReleaseEventData {
    fn new(series_name: &str, book: &RemoteBook, release_date: NaiveDate) -> Self {
        ReleaseEventData {
            series_name: series_name.to_string(),
            book_title: book.title.clone(),
            position: book.position,
            release_date,
            asin: book.asin.clone(),
            narrators: None,
            runtime_minutes: None,
            summary: None,
        }
    }
}

/// The input shape `db::queries::upsert_known_release` expects, one per
/// remote book seen this run (including owned ones, so `known_releases`
/// stays in sync even for books that never fire an event).
#[derive(Debug, Clone, PartialEq)]
pub struct KnownReleaseUpsert {
    pub series_id: i64,
    pub source_book_id: String,
    pub title: String,
    pub position: Option<f64>,
    pub release_date: Option<NaiveDate>,
    pub status: ReleaseStatus,
    pub asin: Option<String>,
}

/// Diffs a matched series' remote book list against owned books and prior
/// `known_releases` state, producing notification events and the rows to
/// persist. `series_id`/`series_name` aren't derivable from `remote` alone
/// (a series can have zero owned/known books on first match), so both are
/// taken as explicit parameters rather than inferred from `owned`/`known`.
pub fn diff_and_classify(
    today: NaiveDate,
    series_id: i64,
    series_name: &str,
    remote: &[RemoteBook],
    owned: &[OwnedBook],
    known: &[KnownRelease],
) -> (Vec<ReleaseEvent>, Vec<KnownReleaseUpsert>) {
    let mut events = Vec::new();
    let mut upserts = Vec::new();

    for book in remote {
        let existing = known
            .iter()
            .find(|k| k.source_book_id == book.source_book_id);
        let is_owned = find_owned_match(book, owned).is_some();

        let status = if is_owned {
            ReleaseStatus::Owned
        } else {
            match book.release_date {
                Some(date) if date <= today => ReleaseStatus::Released,
                _ => ReleaseStatus::Upcoming,
            }
        };

        if !is_owned && let Some(date) = book.release_date {
            if date <= today {
                let already_notified = existing.is_some_and(|k| k.released_notified_at.is_some());
                if !already_notified {
                    events.push(ReleaseEvent::NewRelease(ReleaseEventData::new(
                        series_name,
                        book,
                        date,
                    )));
                }
            } else {
                let already_notified = existing.is_some_and(|k| k.upcoming_notified_at.is_some());
                if !already_notified {
                    events.push(ReleaseEvent::Upcoming(ReleaseEventData::new(
                        series_name,
                        book,
                        date,
                    )));
                }
            }
        }
        // else (owned, or no release_date): persisted below silently, no event.

        upserts.push(KnownReleaseUpsert {
            series_id,
            source_book_id: book.source_book_id.clone(),
            title: book.title.clone(),
            position: book.position,
            release_date: book.release_date,
            status,
            asin: book.asin.clone(),
        });
    }

    (events, upserts)
}

/// Matches a remote book against owned books by position first (float
/// equality within `POSITION_EPSILON`), falling back to normalized-title
/// equality when either side has no position.
fn find_owned_match<'a>(book: &RemoteBook, owned: &'a [OwnedBook]) -> Option<&'a OwnedBook> {
    if let Some(remote_pos) = book.position
        && let Some(hit) = owned.iter().find(|o| {
            o.sequence
                .is_some_and(|seq| (seq - remote_pos).abs() < POSITION_EPSILON)
        })
    {
        return Some(hit);
    }

    let remote_title = normalize_series_name(&book.title);
    owned
        .iter()
        .find(|o| normalize_series_name(&o.title) == remote_title)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn remote(
        id: &str,
        title: &str,
        position: Option<f64>,
        release_date: Option<NaiveDate>,
    ) -> RemoteBook {
        RemoteBook {
            source_book_id: id.to_string(),
            title: title.to_string(),
            position,
            release_date,
            asin: None,
        }
    }

    fn owned(series_id: i64, item_id: &str, title: &str, sequence: Option<f64>) -> OwnedBook {
        OwnedBook {
            id: 1,
            series_id,
            abs_item_id: item_id.to_string(),
            title: title.to_string(),
            sequence,
            asin: None,
            isbn: None,
            author: None,
        }
    }

    fn known(
        series_id: i64,
        source_book_id: &str,
        status: ReleaseStatus,
        release_date: Option<NaiveDate>,
        upcoming_notified: bool,
        released_notified: bool,
    ) -> KnownRelease {
        let now = chrono::Utc::now();
        KnownRelease {
            id: 1,
            series_id,
            source_book_id: source_book_id.to_string(),
            title: "whatever".to_string(),
            position: None,
            release_date,
            status,
            asin: None,
            first_seen_at: now,
            upcoming_notified_at: upcoming_notified.then_some(now),
            released_notified_at: released_notified.then_some(now),
        }
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 7, 21).unwrap()
    }

    #[test]
    fn owned_by_position_produces_no_event_and_owned_status() {
        let remote_books = vec![remote("hc-1", "Book One", Some(1.0), Some(today()))];
        let owned_books = vec![owned(1, "abs-item-1", "Totally Different Title", Some(1.0))];

        let (events, upserts) =
            diff_and_classify(today(), 1, "Series", &remote_books, &owned_books, &[]);

        assert!(events.is_empty());
        assert_eq!(upserts.len(), 1);
        assert_eq!(upserts[0].status, ReleaseStatus::Owned);
    }

    #[test]
    fn owned_by_title_fallback_when_positions_disagree() {
        let remote_books = vec![remote("hc-1", "Book One", None, Some(today()))];
        let owned_books = vec![owned(1, "abs-item-1", "Book One", None)];

        let (events, upserts) =
            diff_and_classify(today(), 1, "Series", &remote_books, &owned_books, &[]);

        assert!(events.is_empty());
        assert_eq!(upserts[0].status, ReleaseStatus::Owned);
    }

    #[test]
    fn new_release_fires_once_then_suppressed_once_notified() {
        let remote_books = vec![remote("hc-1", "Book One", Some(1.0), Some(today()))];

        let (events, _) = diff_and_classify(today(), 1, "Series", &remote_books, &[], &[]);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], ReleaseEvent::NewRelease(_)));

        let already_notified = vec![known(
            1,
            "hc-1",
            ReleaseStatus::Released,
            Some(today()),
            false,
            true,
        )];
        let (events, upserts) =
            diff_and_classify(today(), 1, "Series", &remote_books, &[], &already_notified);
        assert!(events.is_empty());
        assert_eq!(upserts[0].status, ReleaseStatus::Released);
    }

    #[test]
    fn upcoming_fires_once_then_suppressed_once_notified() {
        let future = today() + Duration::days(30);
        let remote_books = vec![remote("hc-1", "Book Two", Some(2.0), Some(future))];

        let (events, _) = diff_and_classify(today(), 1, "Series", &remote_books, &[], &[]);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], ReleaseEvent::Upcoming(_)));

        let already_notified = vec![known(
            1,
            "hc-1",
            ReleaseStatus::Upcoming,
            Some(future),
            true,
            false,
        )];
        let (events, _) =
            diff_and_classify(today(), 1, "Series", &remote_books, &[], &already_notified);
        assert!(events.is_empty());
    }

    #[test]
    fn upcoming_to_released_transition_fires_released_event() {
        // Was upcoming and already notified as such; release_date has now arrived.
        let remote_books = vec![remote("hc-1", "Book Two", Some(2.0), Some(today()))];
        let prior = vec![known(
            1,
            "hc-1",
            ReleaseStatus::Upcoming,
            Some(today()),
            true,
            false,
        )];

        let (events, upserts) = diff_and_classify(today(), 1, "Series", &remote_books, &[], &prior);

        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], ReleaseEvent::NewRelease(_)));
        assert_eq!(upserts[0].status, ReleaseStatus::Released);
    }

    #[test]
    fn no_release_date_is_stored_silently() {
        let remote_books = vec![remote("hc-1", "Unannounced Book", Some(3.0), None)];

        let (events, upserts) = diff_and_classify(today(), 1, "Series", &remote_books, &[], &[]);

        assert!(events.is_empty());
        assert_eq!(upserts.len(), 1);
        assert_eq!(upserts[0].status, ReleaseStatus::Upcoming);
        assert_eq!(upserts[0].release_date, None);
    }

    #[test]
    fn novella_position_1_5_is_handled_like_any_other_position() {
        let remote_books = vec![remote("hc-novella", "Novella", Some(1.5), Some(today()))];
        let owned_books = vec![owned(
            1,
            "abs-item-1",
            "Different Title Entirely",
            Some(1.5),
        )];

        let (events, upserts) =
            diff_and_classify(today(), 1, "Series", &remote_books, &owned_books, &[]);

        assert!(events.is_empty());
        assert_eq!(upserts[0].status, ReleaseStatus::Owned);
    }

    #[test]
    fn position_epsilon_treats_near_equal_floats_as_a_match() {
        let remote_books = vec![remote("hc-1", "Book One", Some(1.0), Some(today()))];
        // Simulates float round-trip noise (e.g. 1.0 stored/read as 0.999999999).
        let owned_books = vec![owned(
            1,
            "abs-item-1",
            "Different Title",
            Some(0.999_999_999_9),
        )];

        let (events, upserts) =
            diff_and_classify(today(), 1, "Series", &remote_books, &owned_books, &[]);

        assert!(events.is_empty());
        assert_eq!(upserts[0].status, ReleaseStatus::Owned);
    }
}
