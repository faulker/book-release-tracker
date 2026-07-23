//! End-to-end tests for `checker::run_check`: wiremock-backed ABS, Hardcover,
//! and Audnexus servers, a temp sqlite database, and a `RecordingNotifier`
//! capturing dispatched events. Covers the happy path, second-run dedupe,
//! partial failure (one series' Hardcover call failing), full ABS failure,
//! notifier-outage retry, and Audnexus enrichment (success and failure).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::NaiveDate;
use reqwest::Url;
use serde_json::json;
use sqlx::SqlitePool;
use wiremock::matchers::{body_string_contains, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use book_release_tracker::abs::AbsClient;
use book_release_tracker::checker::{ScanMode, run_check};
use book_release_tracker::db;
use book_release_tracker::db::models::CheckRunStatus;
use book_release_tracker::diff::ReleaseEvent;
use book_release_tracker::metadata::audnexus::AudnexusClient;
use book_release_tracker::metadata::hardcover::HardcoverClient;
use book_release_tracker::metadata::{MetadataError, MetadataSource, RemoteBook, SeriesCandidate};
use book_release_tracker::notify::{Notifier, NotifyError};

/// Fixed "today" for deterministic classification: matches the dates baked
/// into the mock Hardcover responses below.
fn today() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 7, 21).unwrap()
}

/// Test notifier that records every event it is asked to deliver.
#[derive(Clone, Default)]
struct RecordingNotifier {
    events: Arc<Mutex<Vec<ReleaseEvent>>>,
}

impl RecordingNotifier {
    fn recorded(&self) -> Vec<ReleaseEvent> {
        self.events.lock().unwrap().clone()
    }

    fn clear(&self) {
        self.events.lock().unwrap().clear();
    }
}

#[async_trait]
impl Notifier for RecordingNotifier {
    fn name(&self) -> &'static str {
        "recording"
    }

    async fn notify(&self, event: &ReleaseEvent) -> Result<(), NotifyError> {
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }
}

/// Test notifier that always fails, simulating a total notification outage.
struct FailingNotifier;

#[async_trait]
impl Notifier for FailingNotifier {
    fn name(&self) -> &'static str {
        "failing"
    }

    async fn notify(&self, _event: &ReleaseEvent) -> Result<(), NotifyError> {
        Err(NotifyError::Send("simulated outage".into()))
    }
}

/// Stub metadata source used by the enrichment tests: matches any series name
/// exactly and returns a fixed book list (the real Hardcover client never
/// populates ASINs, so enrichment needs a stub to exercise end-to-end).
struct StubMetadata {
    books: Vec<RemoteBook>,
}

#[async_trait]
impl MetadataSource for StubMetadata {
    async fn find_series(&self, name: &str) -> Result<Vec<SeriesCandidate>, MetadataError> {
        Ok(vec![SeriesCandidate {
            source_id: "900".into(),
            name: name.to_string(),
            slug: Some("stub-series".into()),
            is_completed: false,
            readers_count: 0,
        }])
    }

    async fn series_books(&self, _id: &str) -> Result<Vec<RemoteBook>, MetadataError> {
        Ok(self.books.clone())
    }

    async fn series_slug(&self, _id: &str) -> Result<Option<String>, MetadataError> {
        Ok(Some("stub-series".into()))
    }
}

/// Opens a file-backed temp database; the tempdir guard must stay alive for
/// the duration of the test.
async fn temp_db(dir: &tempfile::TempDir) -> SqlitePool {
    let path = dir.path().join("tracker.db");
    db::init(path.to_str().expect("utf8 temp path"))
        .await
        .expect("init temp db")
}

fn abs_client(server: &MockServer) -> AbsClient {
    AbsClient::new(Url::parse(&server.uri()).unwrap(), "abs-key")
}

/// Hardcover client with zero throttle and zero retries so failure tests
/// don't sit in backoff sleeps.
fn hardcover_client(server: &MockServer) -> HardcoverClient {
    HardcoverClient::new(
        Url::parse(&server.uri()).unwrap(),
        "hc-token".into(),
        0,
        0,
        None,
    )
}

/// Mounts a healthy two-series ABS: library `lib1` with "The Expanse" (one
/// owned book) and "Dungeon Crawler Carl" (one owned book). A podcast library
/// is included to prove non-book libraries are skipped.
async fn mount_abs_two_series(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api/libraries"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "libraries": [
                {"id": "lib1", "name": "Audiobooks", "mediaType": "book"},
                {"id": "pods", "name": "Podcasts", "mediaType": "podcast"}
            ]
        })))
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/libraries/lib1/series"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [
                {"id": "ser1", "name": "The Expanse"},
                {"id": "ser2", "name": "Dungeon Crawler Carl"}
            ],
            "total": 2, "limit": 100, "page": 0
        })))
        .mount(server)
        .await;

    // ABS filters items by `series.<base64(series_id)>`.
    Mock::given(method("GET"))
        .and(path("/api/libraries/lib1/items"))
        .and(query_param("filter", "series.c2VyMQ==")) // base64("ser1")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{
                "id": "li_1",
                "media": {"metadata": {
                    "title": "Leviathan Wakes",
                    "series": [{"id": "ser1", "name": "The Expanse", "sequence": "1"}],
                    "asin": "B0064CGH1W"
                }}
            }],
            "total": 1, "limit": 100, "page": 0
        })))
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/libraries/lib1/items"))
        .and(query_param("filter", "series.c2VyMg==")) // base64("ser2")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{
                "id": "li_2",
                "media": {"metadata": {
                    "title": "Dungeon Crawler Carl",
                    "series": [{"id": "ser2", "name": "Dungeon Crawler Carl", "sequence": "1"}],
                    "asin": ""
                }}
            }],
            "total": 1, "limit": 100, "page": 0
        })))
        .mount(server)
        .await;
}

/// Mounts Hardcover series-search responses for both ABS series: the `search`
/// query per series name, plus the follow-up by-id `series` lookups.
async fn mount_hardcover_find_series(server: &MockServer) {
    Mock::given(method("POST"))
        .and(body_string_contains("FindSeries"))
        .and(body_string_contains("The Expanse"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"search": {"results": {"hits": [{"document": {"id": "101", "name": "The Expanse"}}]}}}
        })))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(body_string_contains("FindSeries"))
        .and(body_string_contains("Dungeon Crawler Carl"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"search": {"results": {"hits": [{"document": {"id": "202", "name": "Dungeon Crawler Carl"}}]}}}
        })))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(body_string_contains("SeriesByIds"))
        .and(body_string_contains("[101]"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"series": [{"id": 101, "name": "The Expanse", "is_completed": false}]}
        })))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(body_string_contains("SeriesByIds"))
        .and(body_string_contains("[202]"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"series": [{"id": 202, "name": "Dungeon Crawler Carl", "is_completed": false}]}
        })))
        .mount(server)
        .await;
}

/// Mounts Hardcover book lists: The Expanse has the owned book plus one
/// recently released and one future book (2 events); Dungeon Crawler Carl
/// has only the owned book (0 events).
async fn mount_hardcover_books_happy(server: &MockServer) {
    Mock::given(method("POST"))
        .and(body_string_contains("SeriesBooks"))
        .and(body_string_contains("\"seriesId\":101"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"book_series": [
                {"position": 1.0, "book": {"id": 1001, "title": "Leviathan Wakes", "release_date": "2011-06-15"}},
                {"position": 2.0, "book": {"id": 1002, "title": "Caliban's War", "release_date": "2026-07-01"}},
                {"position": 3.0, "book": {"id": 1003, "title": "Abaddon's Gate", "release_date": "2026-09-01"}}
            ]}
        })))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(body_string_contains("SeriesBooks"))
        .and(body_string_contains("\"seriesId\":202"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"book_series": [
                {"position": 1.0, "book": {"id": 2001, "title": "Dungeon Crawler Carl", "release_date": "2020-01-01"}}
            ]}
        })))
        .mount(server)
        .await;
}

/// Fetches (status, upcoming_notified_at present, released_notified_at
/// present) for one known_releases row by Hardcover book id.
async fn release_state(pool: &SqlitePool, source_book_id: &str) -> (String, bool, bool) {
    let (status, upcoming, released): (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT status, upcoming_notified_at, released_notified_at FROM known_releases WHERE source_book_id = ?",
    )
    .bind(source_book_id)
    .fetch_one(pool)
    .await
    .expect("fetch known_releases row");
    (status, upcoming.is_some(), released.is_some())
}

async fn latest_check_run(pool: &SqlitePool) -> (String, i64, i64, Option<String>) {
    sqlx::query_as(
        "SELECT status, series_checked, events_found, errors FROM check_runs ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(pool)
    .await
    .expect("fetch latest check_run")
}

#[tokio::test]
async fn happy_path_finds_two_events_and_second_run_dedupes() {
    let abs_server = MockServer::start().await;
    let hc_server = MockServer::start().await;
    mount_abs_two_series(&abs_server).await;
    mount_hardcover_find_series(&hc_server).await;
    mount_hardcover_books_happy(&hc_server).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let pool = temp_db(&dir).await;
    let abs = abs_client(&abs_server);
    let hardcover = hardcover_client(&hc_server);
    let recorder = RecordingNotifier::default();
    let notifiers: Vec<Box<dyn Notifier>> = vec![Box::new(recorder.clone())];

    // First run: one released + one upcoming event.
    let report = run_check(
        &pool,
        &abs,
        &hardcover,
        None,
        &notifiers,
        &[],
        today(),
        ScanMode::Full,
    )
    .await
    .expect("first run");

    assert_eq!(report.status, CheckRunStatus::Ok);
    assert_eq!(report.exit_code(), 0);
    assert_eq!(report.series_checked, 2);
    assert_eq!(report.events_found, 2);
    assert!(report.errors.is_empty());

    let events = recorder.recorded();
    assert_eq!(events.len(), 2);
    assert!(events.iter().any(|e| matches!(
        e,
        ReleaseEvent::NewRelease(d) if d.book_title == "Caliban's War"
            && d.series_name == "The Expanse"
            && d.release_date == NaiveDate::from_ymd_opt(2026, 7, 1).unwrap()
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        ReleaseEvent::Upcoming(d) if d.book_title == "Abaddon's Gate"
            && d.release_date == NaiveDate::from_ymd_opt(2026, 9, 1).unwrap()
    )));

    // DB state: all four remote books stored, correct statuses and stamps.
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM known_releases")
        .fetch_one(&pool)
        .await
        .expect("count releases");
    assert_eq!(count, 4);
    assert_eq!(
        release_state(&pool, "1001").await,
        ("owned".into(), false, false)
    );
    assert_eq!(
        release_state(&pool, "1002").await,
        ("released".into(), false, true)
    );
    assert_eq!(
        release_state(&pool, "1003").await,
        ("upcoming".into(), true, false)
    );
    assert_eq!(
        release_state(&pool, "2001").await,
        ("owned".into(), false, false)
    );

    let (series_status, hc_id): (String, Option<String>) = sqlx::query_as(
        "SELECT match_status, hardcover_series_id FROM series WHERE abs_series_id = 'ser1'",
    )
    .fetch_one(&pool)
    .await
    .expect("fetch series row");
    assert_eq!(series_status, "matched");
    assert_eq!(hc_id.as_deref(), Some("101"));

    let (run_status, checked, found, errors) = latest_check_run(&pool).await;
    assert_eq!(run_status, "ok");
    assert_eq!((checked, found), (2, 2));
    assert!(errors.is_none());

    // Second run against identical mocks: everything already stamped, so no
    // new events (dedupe proof).
    recorder.clear();
    let report = run_check(
        &pool,
        &abs,
        &hardcover,
        None,
        &notifiers,
        &[],
        today(),
        ScanMode::Full,
    )
    .await
    .expect("second run");

    assert_eq!(report.status, CheckRunStatus::Ok);
    assert_eq!(report.events_found, 0);
    assert!(recorder.recorded().is_empty());
}

#[tokio::test]
async fn partial_scan_skips_series_with_scheduled_future_release() {
    let abs_server = MockServer::start().await;
    let hc_server = MockServer::start().await;
    mount_abs_two_series(&abs_server).await;
    mount_hardcover_find_series(&hc_server).await;
    mount_hardcover_books_happy(&hc_server).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let pool = temp_db(&dir).await;
    let abs = abs_client(&abs_server);
    let hardcover = hardcover_client(&hc_server);
    let recorder = RecordingNotifier::default();
    let notifiers: Vec<Box<dyn Notifier>> = vec![Box::new(recorder.clone())];

    // Full first run learns The Expanse has Abaddon's Gate scheduled for
    // 2026-09-01 (future); Dungeon Crawler Carl has nothing scheduled.
    let report = run_check(
        &pool,
        &abs,
        &hardcover,
        None,
        &notifiers,
        &[],
        today(),
        ScanMode::Full,
    )
    .await
    .expect("full run");
    assert_eq!(report.series_checked, 2);

    // Partial run: The Expanse is skipped, Dungeon Crawler Carl re-checked.
    let report = run_check(
        &pool,
        &abs,
        &hardcover,
        None,
        &notifiers,
        &[],
        today(),
        ScanMode::Partial,
    )
    .await
    .expect("partial run");
    assert_eq!(report.status, CheckRunStatus::Ok);
    assert_eq!(report.series_checked, 1);

    // Once the scheduled date has passed, the partial scan checks it again.
    let after_release = NaiveDate::from_ymd_opt(2026, 9, 2).unwrap();
    let report = run_check(
        &pool,
        &abs,
        &hardcover,
        None,
        &notifiers,
        &[],
        after_release,
        ScanMode::Partial,
    )
    .await
    .expect("partial run after release date");
    assert_eq!(report.series_checked, 2);
}

#[tokio::test]
async fn ignored_series_is_never_matched_or_checked() {
    let abs_server = MockServer::start().await;
    let hc_server = MockServer::start().await;
    mount_abs_two_series(&abs_server).await;
    mount_hardcover_find_series(&hc_server).await;
    mount_hardcover_books_happy(&hc_server).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let pool = temp_db(&dir).await;

    // Ignore Dungeon Crawler Carl up front (as the dashboard's ignore
    // button would, keyed by normalized name).
    book_release_tracker::db::queries::set_series_override(
        &pool,
        "dungeon crawler carl",
        None,
        true,
    )
    .await
    .expect("store ignore override");

    let recorder = RecordingNotifier::default();
    let notifiers: Vec<Box<dyn Notifier>> = vec![Box::new(recorder.clone())];
    let report = run_check(
        &pool,
        &abs_client(&abs_server),
        &hardcover_client(&hc_server),
        None,
        &notifiers,
        &[],
        today(),
        ScanMode::Full,
    )
    .await
    .expect("run with ignored series");

    assert_eq!(report.status, CheckRunStatus::Ok);
    // The ignored series produced no known_releases rows at all.
    let (count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM known_releases kr
         JOIN series s ON s.id = kr.series_id
         WHERE s.abs_series_id = 'ser2'",
    )
    .fetch_one(&pool)
    .await
    .expect("count ignored series releases");
    assert_eq!(count, 0);

    let (status,): (String,) =
        sqlx::query_as("SELECT match_status FROM series WHERE abs_series_id = 'ser2'")
            .fetch_one(&pool)
            .await
            .expect("fetch ignored series");
    assert_eq!(status, "ignored");
}

#[tokio::test]
async fn one_series_failing_yields_partial_and_other_series_still_processed() {
    let abs_server = MockServer::start().await;
    let hc_server = MockServer::start().await;
    mount_abs_two_series(&abs_server).await;
    mount_hardcover_find_series(&hc_server).await;

    // The Expanse works; Dungeon Crawler Carl's book list 500s (client is
    // configured with zero retries, so this exhausts retries immediately).
    Mock::given(method("POST"))
        .and(body_string_contains("SeriesBooks"))
        .and(body_string_contains("\"seriesId\":101"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"book_series": [
                {"position": 2.0, "book": {"id": 1002, "title": "Caliban's War", "release_date": "2026-07-01"}}
            ]}
        })))
        .mount(&hc_server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("SeriesBooks"))
        .and(body_string_contains("\"seriesId\":202"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&hc_server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let pool = temp_db(&dir).await;
    let recorder = RecordingNotifier::default();
    let notifiers: Vec<Box<dyn Notifier>> = vec![Box::new(recorder.clone())];

    let report = run_check(
        &pool,
        &abs_client(&abs_server),
        &hardcover_client(&hc_server),
        None,
        &notifiers,
        &[],
        today(),
        ScanMode::Full,
    )
    .await
    .expect("run completes despite one series failing");

    assert_eq!(report.status, CheckRunStatus::Partial);
    assert_eq!(report.exit_code(), 1);
    assert_eq!(report.series_checked, 2);
    // The healthy series was still fully processed.
    assert_eq!(report.events_found, 1);
    assert_eq!(recorder.recorded().len(), 1);
    assert_eq!(report.errors.len(), 1);
    assert!(report.errors[0].contains("Dungeon Crawler Carl"));

    let (run_status, _, _, errors) = latest_check_run(&pool).await;
    assert_eq!(run_status, "partial");
    assert!(
        errors
            .expect("errors json")
            .contains("Dungeon Crawler Carl")
    );
}

#[tokio::test]
async fn abs_outage_fails_the_run_but_records_it() {
    let abs_server = MockServer::start().await;
    let hc_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/libraries"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&abs_server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let pool = temp_db(&dir).await;
    let notifiers: Vec<Box<dyn Notifier>> = vec![Box::new(RecordingNotifier::default())];

    let report = run_check(
        &pool,
        &abs_client(&abs_server),
        &hardcover_client(&hc_server),
        None,
        &notifiers,
        &[],
        today(),
        ScanMode::Full,
    )
    .await
    .expect("ABS outage returns a failed report, not Err");

    assert_eq!(report.status, CheckRunStatus::Failed);
    assert_eq!(report.exit_code(), 2);
    assert_eq!(report.errors.len(), 1);

    let (run_status, checked, found, errors) = latest_check_run(&pool).await;
    assert_eq!(run_status, "failed");
    assert_eq!((checked, found), (0, 0));
    assert!(errors.expect("errors json").contains("audiobookshelf"));
}

#[tokio::test]
async fn notifier_outage_stores_releases_unstamped_so_next_run_retries() {
    let abs_server = MockServer::start().await;
    let hc_server = MockServer::start().await;
    mount_abs_two_series(&abs_server).await;
    mount_hardcover_find_series(&hc_server).await;
    mount_hardcover_books_happy(&hc_server).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let pool = temp_db(&dir).await;
    let abs = abs_client(&abs_server);
    let hardcover = hardcover_client(&hc_server);

    // First run: every notifier fails.
    let failing: Vec<Box<dyn Notifier>> = vec![Box::new(FailingNotifier)];
    let report = run_check(
        &pool,
        &abs,
        &hardcover,
        None,
        &failing,
        &[],
        today(),
        ScanMode::Full,
    )
    .await
    .expect("run with failing notifier");

    // Notification failure is not a series error: the run itself is ok.
    assert_eq!(report.status, CheckRunStatus::Ok);
    assert_eq!(report.events_found, 2);

    // Releases were stored but NOT stamped.
    assert_eq!(
        release_state(&pool, "1002").await,
        ("released".into(), false, false)
    );
    assert_eq!(
        release_state(&pool, "1003").await,
        ("upcoming".into(), false, false)
    );

    // Second run with a working notifier: the same two events fire again
    // (retry proof) and get stamped this time.
    let recorder = RecordingNotifier::default();
    let working: Vec<Box<dyn Notifier>> = vec![Box::new(recorder.clone())];
    let report = run_check(
        &pool,
        &abs,
        &hardcover,
        None,
        &working,
        &[],
        today(),
        ScanMode::Full,
    )
    .await
    .expect("retry run");

    assert_eq!(report.events_found, 2);
    assert_eq!(recorder.recorded().len(), 2);
    assert_eq!(
        release_state(&pool, "1002").await,
        ("released".into(), false, true)
    );
    assert_eq!(
        release_state(&pool, "1003").await,
        ("upcoming".into(), true, false)
    );
}

/// Mounts a single-series ABS (one library, one series, one owned book) for
/// the enrichment tests, which drive metadata via `StubMetadata`.
async fn mount_abs_single_series(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api/libraries"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "libraries": [{"id": "lib1", "name": "Audiobooks", "mediaType": "book"}]
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/libraries/lib1/series"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{"id": "ser1", "name": "The Expanse"}],
            "total": 1, "limit": 100, "page": 0
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/libraries/lib1/items"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{
                "id": "li_1",
                "media": {"metadata": {
                    "title": "Leviathan Wakes",
                    "series": [{"id": "ser1", "name": "The Expanse", "sequence": "1"}]
                }}
            }],
            "total": 1, "limit": 100, "page": 0
        })))
        .mount(server)
        .await;
}

/// Remote books for the enrichment tests: the owned book plus one unowned
/// released book that carries an ASIN (the trigger for Audnexus lookup).
fn stub_books_with_asin() -> Vec<RemoteBook> {
    vec![
        RemoteBook {
            source_book_id: "1001".into(),
            title: "Leviathan Wakes".into(),
            position: Some(1.0),
            release_date: NaiveDate::from_ymd_opt(2011, 6, 15),
            asin: None,
        },
        RemoteBook {
            source_book_id: "1002".into(),
            title: "Caliban's War".into(),
            position: Some(2.0),
            release_date: NaiveDate::from_ymd_opt(2026, 7, 1),
            asin: Some("B005SCRR2A".into()),
        },
    ]
}

#[tokio::test]
async fn enrichment_merges_audnexus_data_into_the_event() {
    let abs_server = MockServer::start().await;
    let audnexus_server = MockServer::start().await;
    mount_abs_single_series(&abs_server).await;
    Mock::given(method("GET"))
        .and(path("/books/B005SCRR2A"))
        .and(query_param("region", "us"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "narrators": [{"name": "Jefferson Mays"}],
            "runtimeLengthMin": 1261,
            "summary": "The war for the solar system heats up."
        })))
        .expect(1)
        .mount(&audnexus_server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let pool = temp_db(&dir).await;
    let metadata = StubMetadata {
        books: stub_books_with_asin(),
    };
    let enricher = AudnexusClient::new(Url::parse(&audnexus_server.uri()).unwrap(), "us".into());
    let recorder = RecordingNotifier::default();
    let notifiers: Vec<Box<dyn Notifier>> = vec![Box::new(recorder.clone())];

    let report = run_check(
        &pool,
        &abs_client(&abs_server),
        &metadata,
        Some(&enricher),
        &notifiers,
        &[],
        today(),
        ScanMode::Full,
    )
    .await
    .expect("enriched run");

    assert_eq!(report.status, CheckRunStatus::Ok);
    let events = recorder.recorded();
    assert_eq!(events.len(), 1);
    let ReleaseEvent::NewRelease(data) = &events[0] else {
        panic!("expected NewRelease, got {:?}", events[0]);
    };
    assert_eq!(data.book_title, "Caliban's War");
    assert_eq!(data.narrators, Some(vec!["Jefferson Mays".to_string()]));
    assert_eq!(data.runtime_minutes, Some(1261));
    assert_eq!(
        data.summary.as_deref(),
        Some("The war for the solar system heats up.")
    );
}

#[tokio::test]
async fn enrichment_failure_is_non_fatal_and_event_proceeds_unenriched() {
    let abs_server = MockServer::start().await;
    let audnexus_server = MockServer::start().await;
    mount_abs_single_series(&abs_server).await;
    Mock::given(method("GET"))
        .and(path("/books/B005SCRR2A"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&audnexus_server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let pool = temp_db(&dir).await;
    let metadata = StubMetadata {
        books: stub_books_with_asin(),
    };
    let enricher = AudnexusClient::new(Url::parse(&audnexus_server.uri()).unwrap(), "us".into());
    let recorder = RecordingNotifier::default();
    let notifiers: Vec<Box<dyn Notifier>> = vec![Box::new(recorder.clone())];

    let report = run_check(
        &pool,
        &abs_client(&abs_server),
        &metadata,
        Some(&enricher),
        &notifiers,
        &[],
        today(),
        ScanMode::Full,
    )
    .await
    .expect("run with failing audnexus");

    // Enrichment failure never fails a check or blocks the event.
    assert_eq!(report.status, CheckRunStatus::Ok);
    let events = recorder.recorded();
    assert_eq!(events.len(), 1);
    let ReleaseEvent::NewRelease(data) = &events[0] else {
        panic!("expected NewRelease, got {:?}", events[0]);
    };
    assert_eq!(data.narrators, None);
    assert_eq!(data.runtime_minutes, None);
    assert_eq!(
        release_state(&pool, "1002").await,
        ("released".into(), false, true)
    );
}
