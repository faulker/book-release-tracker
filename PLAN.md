# Implementation Plan: book-release-tracker

Rust app at /Users/sane/Dev/book-release-tracker that watches the user's Audiobookshelf series and alerts on new or upcoming releases via dashboard, ntfy, and email. Plan finalized 2026-07-21.

## 1. Crate stack (final)

- tokio (full): async runtime
- reqwest (json, rustls-tls): HTTP for ABS, Hardcover, Audnexus, ntfy
- serde, serde_json
- sqlx (runtime-tokio, sqlite, chrono, migrate): use the NON-MACRO API (sqlx::query_as::<_, T> + FromRow), not query! macros. Avoids DATABASE_URL/offline-prepare friction; cargo build stays self-contained.
- axum: dashboard
- askama + askama_web (axum feature): compile-time checked HTML templates
- lettre (tokio1, rustls, smtp-transport, builder): email
- clap (derive): CLI
- toml + serde: config. Plain TOML file + explicit env overrides for the 3 secrets.
- chrono (serde): NaiveDate for release dates, DateTime<Utc> for timestamps
- tracing, tracing-subscriber (env-filter)
- thiserror (per-module error enums), anyhow (binary-level context in main)
- async-trait: MetadataSource / Notifier traits
- dev: wiremock, tempfile

No tokio-cron-scheduler: daemon mode is a single fixed-interval job; tokio::time::interval is sufficient.

## 2. Project / module layout

```
book-release-tracker/
├── Cargo.toml
├── config.example.toml
├── README.md
├── migrations/
│   └── 0001_init.sql
└── src/
    ├── main.rs            # entry: parse CLI, load config, init tracing+db, dispatch check/daemon
    ├── cli.rs             # clap derive: Check {}, Daemon {}, common --config flag
    ├── config.rs          # Config structs, load(path) -> Result, env overrides, validation
    ├── error.rs           # AppError (top-level); module errors live in their modules
    ├── db/
    │   ├── mod.rs         # pool init (sqlite file or :memory:), run migrations
    │   ├── models.rs      # FromRow structs mirroring tables
    │   └── queries.rs     # all SQL behind typed fns (upsert_series, unnotified_releases, ...)
    ├── abs/
    │   ├── mod.rs         # AbsClient: libraries(), series(lib), series_items(lib, series)
    │   └── types.rs       # ABS response DTOs (only fields we use)
    ├── metadata/
    │   ├── mod.rs         # MetadataSource trait + normalized domain types
    │   ├── hardcover.rs   # HardcoverClient: GraphQL POST, rate limiting, retry/backoff
    │   └── audnexus.rs    # AudnexusClient: GET /books/{asin}?region=..  (enrichment)
    ├── matching.rs        # PURE: normalize_series_name(), pick_series_match()
    ├── diff.rs            # PURE: diff_and_classify(remote, owned, known) -> Vec<ReleaseEvent>
    ├── checker.rs         # orchestrates one full check run; per-series error isolation
    ├── notify/
    │   ├── mod.rs         # Notifier trait, build_notifiers(config), dispatch_all()
    │   ├── ntfy.rs        # reqwest POST to {server}/{topic}, Title/Tags/Priority headers
    │   └── email.rs       # lettre SMTP; message building split out for testability
    └── web/
        ├── mod.rs         # router, AppState { pool, checker_handle }, serve()
        ├── handlers.rs    # one handler per page + override form POST + run-now POST
        └── templates/     # askama: base.html, index.html, upcoming.html, new.html, unmatched.html
```

Key traits:

```rust
/// A remote book-metadata service that can resolve a series and list its books.
/// Implementations must be swappable; the checker only sees this trait.
#[async_trait]
pub trait MetadataSource: Send + Sync {
    /// Fuzzy-search series by name; returns candidates for matching.rs to score.
    async fn find_series(&self, name: &str) -> Result<Vec<SeriesCandidate>, MetadataError>;
    /// All books in a series with position and release_date.
    async fn series_books(&self, source_series_id: &str) -> Result<Vec<RemoteBook>, MetadataError>;
}

#[async_trait]
pub trait Notifier: Send + Sync {
    fn name(&self) -> &'static str;
    async fn notify(&self, event: &ReleaseEvent) -> Result<(), NotifyError>;
}
```

Normalized types in metadata/mod.rs: SeriesCandidate { source_id, name, is_completed }, RemoteBook { source_book_id, title, position: Option<f64>, release_date: Option<NaiveDate>, asin: Option<String> }. Audnexus is NOT a MetadataSource (it can't discover series); it's a small enrichment client whose output (narrator, runtime, summary) decorates notification text when an ASIN exists. Enrichment failure never fails a check.

## 3. Config format

config.toml, path via --config (default ./config.toml).

```toml
[audiobookshelf]
base_url = "https://audiobook.pi4.us"
api_key = ""                    # or env BRT_ABS_API_KEY
library_ids = []                 # empty = all libraries

[hardcover]
token = ""                       # or env BRT_HARDCOVER_TOKEN
min_request_interval_ms = 1100   # client-side throttle
max_retries = 3                  # 429/5xx exponential backoff

[audnexus]
enabled = true
region = "us"

[check]
interval_minutes = 360           # daemon mode only
db_path = "book-tracker.db"

[dashboard]
enabled = true                   # daemon mode only
bind = "127.0.0.1:8091"

[notify.ntfy]
enabled = false
server = "https://ntfy.sh"
topic = ""
token = ""                       # optional, env BRT_NTFY_TOKEN

[notify.email]
enabled = false
smtp_host = ""
smtp_port = 587
username = ""
password = ""                    # or env BRT_SMTP_PASSWORD
from = ""
to = ""
```

config::load reads TOML, applies the four env overrides, then validates: base_url parses as URL, api_key/token non-empty, each enabled notifier has its required fields. Validation failures list every problem at once and exit non-zero. Ship config.example.toml with comments.

## 4. SQLite schema (migrations/0001_init.sql)

```sql
-- One row per ABS series we track
CREATE TABLE series (
  id INTEGER PRIMARY KEY,
  abs_series_id TEXT NOT NULL UNIQUE,
  abs_name TEXT NOT NULL,
  hardcover_series_id TEXT,          -- NULL until matched
  hardcover_name TEXT,
  is_completed INTEGER NOT NULL DEFAULT 0,
  match_status TEXT NOT NULL DEFAULT 'unmatched',  -- unmatched|matched|overridden|ignored
  last_checked_at TEXT
);

-- Books the user owns in ABS, per series
CREATE TABLE owned_books (
  id INTEGER PRIMARY KEY,
  series_id INTEGER NOT NULL REFERENCES series(id) ON DELETE CASCADE,
  abs_item_id TEXT NOT NULL UNIQUE,
  title TEXT NOT NULL,
  sequence REAL,
  asin TEXT,
  isbn TEXT
);

-- Every remote book we've seen from Hardcover; dedupe + notification state
CREATE TABLE known_releases (
  id INTEGER PRIMARY KEY,
  series_id INTEGER NOT NULL REFERENCES series(id) ON DELETE CASCADE,
  source_book_id TEXT NOT NULL,      -- Hardcover book id
  title TEXT NOT NULL,
  position REAL,
  release_date TEXT,                 -- ISO date, NULL = unknown
  status TEXT NOT NULL,              -- upcoming|released|owned
  asin TEXT,
  first_seen_at TEXT NOT NULL,
  upcoming_notified_at TEXT,         -- set once when announced as upcoming
  released_notified_at TEXT,         -- set once when announced as released
  UNIQUE(series_id, source_book_id)
);

-- Manual ABS-name -> Hardcover mapping for mismatches (edited via dashboard)
CREATE TABLE series_overrides (
  abs_name_normalized TEXT PRIMARY KEY,
  hardcover_series_id TEXT,          -- NULL + ignore=1 means "stop trying"
  ignore INTEGER NOT NULL DEFAULT 0
);

-- Run history for dashboard status page
CREATE TABLE check_runs (
  id INTEGER PRIMARY KEY,
  started_at TEXT NOT NULL,
  finished_at TEXT,
  status TEXT NOT NULL,              -- running|ok|partial|failed
  series_checked INTEGER NOT NULL DEFAULT 0,
  events_found INTEGER NOT NULL DEFAULT 0,
  errors TEXT                        -- JSON array of per-series error strings
);
```

Dedupe rule: a book alerts at most twice, once as upcoming (upcoming_notified_at NULL and release_date future) and once as released (released_notified_at NULL, release_date <= today, not owned). A book first seen already-released alerts only once. Timestamps are set only after at least one notifier succeeds, so a total notification outage retries next run; per-notifier partial failure is logged, not retried.

## 5. Core check flow (checker.rs)

One run_check(pool, abs, metadata, enricher, notifiers) -> CheckReport used by both modes:

1. Insert check_runs row (status running).
2. Fetch ABS: libraries (filtered by config), series per library, items per series sorted by sequence. Upsert into series / owned_books. ABS down = fatal for this run: mark run failed, exit cleanly.
3. Match each series not yet matched/ignored, refresh matched ones:
   - Check series_overrides by normalized name first (override wins; ignore=1 skips).
   - Else metadata.find_series(abs_name) (Hardcover _ilike with % wrapping). Score candidates in pure matching.rs: exact normalized equality wins; if exactly one candidate survives normalization, take it; zero or ambiguous -> match_status='unmatched' for dashboard review. Never guess between multiple candidates.
   - Normalization: lowercase, Unicode NFKC, strip punctuation, collapse whitespace, strip leading article (the/a/an), strip trailing "series". Pure function, heavily unit-tested.
4. Diff + classify per matched series (diff.rs, pure): fetch series_books, compare against owned_books (match by position, fall back to normalized title) and known_releases:
   - owned -> status owned, no event
   - release_date <= today, not owned, released_notified_at NULL -> ReleaseEvent::NewRelease
   - release_date > today, upcoming_notified_at NULL -> ReleaseEvent::Upcoming
   - no release_date -> store as upcoming with NULL date, no notification until a date appears
5. Enrich: for events with an ASIN and audnexus enabled, GET Audnexus; merge narrator/runtime/summary into the event. Failures logged at warn, event proceeds unenriched.
6. Notify + persist: for each event, call every enabled notifier; if >=1 succeeded, stamp the matching *_notified_at and update known_releases.
7. Finalize check_runs: ok, partial (some series errored), or failed; error list as JSON.

Error isolation: steps 3-6 wrap each series in its own Result; errors go to the report and the loop continues. Only ABS fetch and DB open are run-fatal. check mode exit codes: 0 ok, 1 partial, 2 failed (cron-friendly).

Hardcover rate limiting: client enforces min_request_interval_ms between requests (Mutex<Instant> gate) and retries 429/5xx with exponential backoff + jitter (1s, 2s, 4s, capped by max_retries), honoring Retry-After if present. GraphQL-level errors parsed and surfaced as MetadataError::Api(String), never silently swallowed.

## 6. Notifiers and dashboard

Notifiers (notify/): build_notifiers(&config) returns Vec<Box<dyn Notifier>> from enabled blocks. Shared format_event(&ReleaseEvent) -> (title, body) so all channels say the same thing.

- ntfy.rs: POST {server}/{topic}, body = message, headers Title, Tags: books, optional Authorization: Bearer, Priority default (upcoming) / high (released). Non-2xx is an error.
- email.rs: lettre async SMTP with STARTTLS; build_message(event) -> Message is a separate fn so tests assert content without a transport.
- Dashboard is NOT a notifier; it reads the DB.

Dashboard (web/, daemon mode only, server-rendered HTML, no JS build):
- GET / : last check status + history (check_runs), counts, next scheduled run
- GET /upcoming : known_releases status upcoming, sorted by release_date (NULL last)
- GET /new : recent released rows, newest first
- GET /unmatched : series with match_status='unmatched' + inline form per row
- POST /overrides : body abs_name, hardcover_series_id (or ignore=1); writes series_overrides, resets series so next check picks it up; redirect back
- POST /check : trigger an immediate run via mpsc channel to the scheduler loop (skip if already running)

Daemon mode: tokio::select! over three tasks: interval loop (tokio::time::interval + run-now channel), axum server, tokio::signal::ctrl_c() for graceful shutdown. Runs one check immediately on startup.

## 7. Testing strategy (all offline, cargo test)

- Pure logic: matching.rs normalization table tests; diff.rs classification matrix (owned/new/upcoming/no-date/already-notified/position-vs-title fallback).
- HTTP clients: wiremock per client. Every client takes base_url: Url in its constructor, so tests point at the mock. Cover: happy-path deserialization from fixtures in tests/fixtures/, auth header assertions, 401/500 error mapping, Hardcover 429-then-success retry, GraphQL error payload -> MetadataError::Api.
- DB: sqlx against sqlite::memory: (tempfile for the migration test). Cover migrations apply cleanly, upsert idempotency, UNIQUE dedupe constraint, notified-timestamp queries.
- Integration (tests/check_flow.rs): wiremock ABS + Hardcover + Audnexus, temp DB, a RecordingNotifier test impl capturing events; run run_check twice and assert the second run produces zero notifications (dedupe proof), plus a partial-failure scenario.
- Config: parse example file, env override, validation error aggregation.

No test touches the network or needs real keys. #[tokio::test] throughout.

## 8. Build / verify

Every milestone ends with cargo build, cargo test, cargo clippy -- -D warnings, all green. cargo fmt before finishing each milestone.

## 9. Ordered milestones

1. Scaffold + config: cargo init --name book-release-tracker (NO git init), Cargo.toml full dep set, main.rs/cli.rs/config.rs/error.rs, tracing setup, config.example.toml. Both subcommands print "not implemented". Tests: config parse/override/validation.
2. DB layer: migration file, db/ module (pool init, models, all query fns per schema). Tests: in-memory sqlx suite. Depends on 1.
3. ABS client: abs/ with three endpoint calls and DTOs. Tests: wiremock + fixtures. Depends on 1 (parallel with 2).
4. Metadata layer: metadata/ trait + Hardcover GraphQL client with throttle/backoff; Audnexus client. Tests: wiremock incl. retry. Depends on 1 (parallel with 2, 3).
5. Matching + diff: matching.rs, diff.rs, ReleaseEvent type. Pure, extensive unit tests. Depends on 4's types only.
6. Checker (check mode complete): checker.rs wiring 2-5, check subcommand functional end-to-end with exit codes; Notifier trait + a LogNotifier (tracing-based, always on). Integration test with dedupe-on-second-run assertion. Depends on 2-5. Convergence point.
7. ntfy + email notifiers: notify/ntfy.rs, notify/email.rs, build_notifiers, shared formatting. Tests: wiremock for ntfy, message-content assertions for email. Depends on 6.
8. Daemon + dashboard: web/ module, askama templates, interval scheduler, run-now channel, graceful shutdown. Tests: axum handler tests via tower::ServiceExt::oneshot against a seeded temp DB. Depends on 6.
9. README + polish: README.md (setup incl. getting a Hardcover token and ABS API key, config reference, cron/launchd example, daemon usage, build/test), final clippy/fmt sweep, confirm config.example.toml matches config.rs exactly.

Open item (not blocking): whether Hardcover's book_series query should filter compilations/omnibus editions. Hardcover positions can be non-integer (1.5 for novellas); diff logic treats position as REAL so novellas work; omnibus false-positives get handled via the dashboard ignore flow initially.
