//! Hardcover GraphQL client: series search and per-series book listing, with
//! client-side request throttling and retry/backoff on 429/5xx.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use reqwest::{Client, StatusCode, Url};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;

use super::{MetadataError, MetadataSource, RemoteBook, SeriesCandidate};

/// Hardcover's production GraphQL endpoint.
pub const DEFAULT_ENDPOINT: &str = "https://api.hardcover.app/v1/graphql";

/// Hardcover GraphQL client. `endpoint` is explicit (rather than hardcoded) so
/// tests can point it at a `wiremock` server; use `default_endpoint()` for
/// production wiring.
pub struct HardcoverClient {
    endpoint: Url,
    token: String,
    client: Client,
    min_request_interval_ms: u64,
    max_retries: u32,
    /// ISO 639-1 code (e.g. "en"). When set, `series_books` drops books whose
    /// editions are all in other languages (translations that Hardcover lists
    /// as separate series entries). `None` = no filtering.
    language: Option<String>,
    last_request: Mutex<Option<Instant>>,
}

impl HardcoverClient {
    /// Builds a client. `min_request_interval_ms` is the client-side throttle gap
    /// enforced between outgoing requests (including retries); `max_retries`
    /// bounds exponential-backoff retries on 429/5xx responses; `language` is
    /// the optional edition-language filter applied by `series_books`.
    pub fn new(
        endpoint: Url,
        token: String,
        min_request_interval_ms: u64,
        max_retries: u32,
        language: Option<String>,
    ) -> Self {
        // Hardcover's site shows the token with a "Bearer " prefix; strip it if
        // present since `bearer_auth` adds its own, which would double it.
        let token = token
            .trim()
            .strip_prefix("Bearer ")
            .map(str::to_string)
            .unwrap_or(token);
        // Normalize once so per-book comparisons are a plain equality check.
        let language = language
            .map(|l| l.trim().to_ascii_lowercase())
            .filter(|l| !l.is_empty());
        Self {
            endpoint,
            token,
            client: Client::new(),
            min_request_interval_ms,
            max_retries,
            language,
            last_request: Mutex::new(None),
        }
    }

    /// Hardcover's production GraphQL endpoint, parsed from `DEFAULT_ENDPOINT`.
    pub fn default_endpoint() -> Url {
        Url::parse(DEFAULT_ENDPOINT).expect("DEFAULT_ENDPOINT is a valid URL")
    }

    /// Blocks until at least `min_request_interval_ms` has elapsed since the last
    /// request started, then records the new start time. The mutex both guards
    /// the timestamp and serializes concurrent callers through the gate.
    async fn throttle(&self) {
        let mut guard = self.last_request.lock().await;
        let min_interval = Duration::from_millis(self.min_request_interval_ms);
        if let Some(last) = *guard {
            let elapsed = last.elapsed();
            if elapsed < min_interval {
                tokio::time::sleep(min_interval - elapsed).await;
            }
        }
        *guard = Some(Instant::now());
    }

    /// POSTs a GraphQL query/variables pair, throttling and retrying (429/5xx)
    /// as configured. GraphQL-level `errors` are surfaced as `MetadataError::Api`;
    /// they are never silently dropped even on an HTTP 200.
    async fn post_graphql<T: for<'de> Deserialize<'de>>(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<T, MetadataError> {
        let body = json!({ "query": query, "variables": variables });
        let mut attempt = 0u32;

        loop {
            self.throttle().await;

            let response = self
                .client
                .post(self.endpoint.clone())
                .bearer_auth(&self.token)
                .json(&body)
                .send()
                .await
                .map_err(|e| MetadataError::Transport(e.to_string()))?;

            let status = response.status();

            if status == StatusCode::UNAUTHORIZED {
                return Err(MetadataError::Auth);
            }

            // Hardcover uses 403 for policy rejections (e.g. disallowed query
            // operators) with an explanatory JSON body, not for bad tokens
            // (those get 401) — surface the body instead of claiming an auth
            // failure.
            if status == StatusCode::FORBIDDEN {
                let body = response.text().await.unwrap_or_default();
                return Err(MetadataError::Api(format!(
                    "HTTP 403 Forbidden: {}",
                    body.trim()
                )));
            }

            if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                if attempt >= self.max_retries {
                    return Err(MetadataError::RateLimited);
                }
                let wait = retry_after(&response).unwrap_or_else(|| backoff(attempt));
                attempt += 1;
                tokio::time::sleep(wait).await;
                continue;
            }

            if !status.is_success() {
                return Err(MetadataError::Transport(format!(
                    "unexpected HTTP status {status}"
                )));
            }

            let parsed: GraphQlResponse<T> = response
                .json()
                .await
                .map_err(|e| MetadataError::Decode(error_chain(&e)))?;

            if let Some(errors) = parsed.errors
                && !errors.is_empty()
            {
                let message = errors
                    .into_iter()
                    .map(|e| e.message)
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(MetadataError::Api(message));
            }

            return parsed
                .data
                .ok_or_else(|| MetadataError::Decode("GraphQL response missing data".into()));
        }
    }

    /// Whether a book passes the configured language filter: no filter set,
    /// any edition in the wanted language, or no language data at all (never
    /// drop a book Hardcover hasn't tagged yet).
    fn book_matches_language(&self, book: &BookRow) -> bool {
        let Some(wanted) = &self.language else {
            return true;
        };
        let codes: Vec<&str> = book
            .language_editions
            .iter()
            .filter_map(|e| e.language.as_ref())
            .filter_map(|l| l.code2.as_deref())
            .collect();
        codes.is_empty() || codes.iter().any(|code| code.eq_ignore_ascii_case(wanted))
    }
}

#[async_trait]
impl MetadataSource for HardcoverClient {
    async fn find_series(&self, name: &str) -> Result<Vec<SeriesCandidate>, MetadataError> {
        // Hardcover rejects `_ilike` filters with HTTP 403 ("ilike and related
        // operations are not permitted on this server"), so series search goes
        // through the Typesense-backed `search` field instead. Its documents
        // don't carry `is_completed`, so a by-id `series` lookup fills that in.
        let query = r#"
            query FindSeries($q: String!) {
                search(query: $q, query_type: "Series", per_page: 10) {
                    results
                }
            }
        "#;
        let data: SearchData = self.post_graphql(query, json!({ "q": name })).await?;

        // Keep readers_count from the search document; the `series` table
        // lookup below doesn't expose it, and matching.rs needs it to break
        // ties between Hardcover's duplicate same-name series entries.
        let hits: Vec<(i64, u64)> = data
            .search
            .results
            .hits
            .iter()
            .filter_map(|h| Some((h.document.id.parse().ok()?, h.document.readers_count)))
            .collect();
        if hits.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<i64> = hits.iter().map(|(id, _)| *id).collect();

        let query = r#"
            query SeriesByIds($ids: [Int!]!) {
                series(where: { id: { _in: $ids } }) {
                    id
                    name
                    slug
                    is_completed
                }
            }
        "#;
        let details: FindSeriesData = self.post_graphql(query, json!({ "ids": ids })).await?;
        let by_id: HashMap<i64, SeriesRow> =
            details.series.into_iter().map(|s| (s.id, s)).collect();

        // Emit candidates in the search endpoint's relevance order, not the
        // arbitrary order the by-id lookup returns.
        Ok(hits
            .iter()
            .filter_map(|(id, readers)| Some((by_id.get(id)?, *readers)))
            .map(|(s, readers_count)| SeriesCandidate {
                source_id: s.id.to_string(),
                name: s.name.clone(),
                slug: s.slug.clone(),
                is_completed: s.is_completed.unwrap_or(false),
                readers_count,
            })
            .collect())
    }

    async fn series_slug(&self, source_series_id: &str) -> Result<Option<String>, MetadataError> {
        let series_id: i64 = source_series_id.parse().map_err(|_| {
            MetadataError::Api(format!("invalid Hardcover series id: {source_series_id:?}"))
        })?;

        let query = r#"
            query SeriesSlug($id: Int!) {
                series(where: { id: { _eq: $id } }) {
                    id
                    name
                    slug
                    is_completed
                }
            }
        "#;
        let data: FindSeriesData = self.post_graphql(query, json!({ "id": series_id })).await?;
        Ok(data.series.into_iter().next().and_then(|s| s.slug))
    }

    async fn series_books(&self, source_series_id: &str) -> Result<Vec<RemoteBook>, MetadataError> {
        let series_id: i64 = source_series_id.parse().map_err(|_| {
            MetadataError::Api(format!("invalid Hardcover series id: {source_series_id:?}"))
        })?;

        // reading_format_id 2 is Hardcover's audiobook format; the nested
        // `editions` selection picks the first audiobook edition (if any) that
        // carries an ASIN, which is the only edition shape we need for Audnexus
        // enrichment. `language_editions` collects the distinct edition
        // languages per book so translations can be filtered out client-side.
        let query = r#"
            query SeriesBooks($seriesId: Int!) {
                book_series(where: { series_id: { _eq: $seriesId } }, order_by: { position: asc }) {
                    position
                    book {
                        id
                        title
                        release_date
                        editions(where: { reading_format_id: { _eq: 2 }, asin: { _is_null: false } }, limit: 1) {
                            asin
                        }
                        language_editions: editions(distinct_on: language_id, order_by: { language_id: asc }) {
                            language {
                                code2
                            }
                        }
                    }
                }
            }
        "#;
        let data: SeriesBooksData = self
            .post_graphql(query, json!({ "seriesId": series_id }))
            .await?;

        Ok(data
            .book_series
            .into_iter()
            .filter(|row| self.book_matches_language(&row.book))
            .map(|row| RemoteBook {
                source_book_id: row.book.id.to_string(),
                title: row.book.title,
                position: row.position,
                release_date: row.book.release_date,
                asin: row.book.editions.into_iter().find_map(|e| e.asin),
            })
            .collect())
    }
}

/// Hardcover serves Postgres dates verbatim, so ancient works come back as BC
/// dates (e.g. Plato's dialogues carry `"0376-01-01 BC"`), which chrono's
/// `NaiveDate` deserializer rejects and which would otherwise fail the whole
/// series. Anything that isn't a plain `YYYY-MM-DD` date is treated as "no
/// known release date".
fn deserialize_lenient_date<'de, D>(deserializer: D) -> Result<Option<chrono::NaiveDate>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Option<String> = Option::deserialize(deserializer)?;
    Ok(raw.and_then(|s| chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok()))
}

/// reqwest's `Display` for a body-decode failure is just "error decoding
/// response body for url (...)"; the serde detail that says *what* didn't fit
/// lives in the source chain, so flatten the whole chain into the message.
fn error_chain(err: &dyn std::error::Error) -> String {
    let mut parts = vec![err.to_string()];
    let mut source = err.source();
    while let Some(e) = source {
        parts.push(e.to_string());
        source = e.source();
    }
    parts.join(": ")
}

/// Exponential backoff with a little jitter: ~1s, ~2s, ~4s, ... for `attempt` 0, 1, 2, ...
fn backoff(attempt: u32) -> Duration {
    let base_ms = 1000u64.saturating_mul(1u64 << attempt.min(10));
    Duration::from_millis(base_ms + jitter_ms(250))
}

/// A little jitter derived from the current time, to spread out retries without
/// pulling in a `rand` dependency for something this small.
fn jitter_ms(max_jitter_ms: u64) -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64;
    nanos % (max_jitter_ms + 1)
}

/// Reads a numeric `Retry-After` header (seconds), if present.
fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// Generic GraphQL envelope: `data` present on success, `errors` present (and
/// possibly alongside partial `data`) when the server reports a GraphQL-level
/// failure.
// The default `#[serde(bound)]` inference would require `T: Default` here (a
// known serde-derive quirk for generic `Option<T>` fields with `#[default]`);
// override it since `Option<T>` is `Default` regardless of `T`.
#[derive(Debug, Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
struct GraphQlResponse<T> {
    #[serde(default)]
    data: Option<T>,
    #[serde(default)]
    errors: Option<Vec<GraphQlErrorEntry>>,
}

#[derive(Debug, Deserialize)]
struct GraphQlErrorEntry {
    message: String,
}

/// Envelope for the `search` field. `results` is a GraphQL `json` scalar
/// holding the raw Typesense response; only `hits[].document` is used.
#[derive(Debug, Deserialize)]
struct SearchData {
    search: SearchResults,
}

#[derive(Debug, Deserialize)]
struct SearchResults {
    results: SearchResultsBody,
}

#[derive(Debug, Deserialize)]
struct SearchResultsBody {
    #[serde(default)]
    hits: Vec<SearchHit>,
}

#[derive(Debug, Deserialize)]
struct SearchHit {
    document: SearchDocument,
}

/// Search documents carry ids as strings, unlike the `series` table's ints.
/// `readers_count` feeds matching.rs's duplicate tie-break; missing means 0.
#[derive(Debug, Deserialize)]
struct SearchDocument {
    id: String,
    #[serde(default)]
    readers_count: u64,
}

#[derive(Debug, Deserialize)]
struct FindSeriesData {
    series: Vec<SeriesRow>,
}

// `is_completed` is nullable on Hardcover's side (live rows return null);
// treat null as "not completed".
#[derive(Debug, Deserialize)]
struct SeriesRow {
    id: i64,
    name: String,
    #[serde(default)]
    slug: Option<String>,
    is_completed: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct SeriesBooksData {
    book_series: Vec<BookSeriesRow>,
}

#[derive(Debug, Deserialize)]
struct BookSeriesRow {
    position: Option<f64>,
    book: BookRow,
}

#[derive(Debug, Deserialize)]
struct BookRow {
    id: i64,
    title: String,
    #[serde(default, deserialize_with = "deserialize_lenient_date")]
    release_date: Option<chrono::NaiveDate>,
    #[serde(default)]
    editions: Vec<EditionRow>,
    /// Distinct edition languages, from the aliased `language_editions`
    /// selection; feeds the client-side translation filter.
    #[serde(default)]
    language_editions: Vec<LanguageEditionRow>,
}

#[derive(Debug, Deserialize)]
struct EditionRow {
    asin: Option<String>,
}

/// One distinct edition language; both levels are nullable on Hardcover's side.
#[derive(Debug, Deserialize)]
struct LanguageEditionRow {
    #[serde(default)]
    language: Option<LanguageRow>,
}

#[derive(Debug, Deserialize)]
struct LanguageRow {
    #[serde(default)]
    code2: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/hardcover/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap_or_else(|e| panic!("read fixture {name}: {e}"))
    }

    fn client_for(server: &MockServer, token: &str) -> HardcoverClient {
        client_with_language(server, token, None)
    }

    fn client_with_language(
        server: &MockServer,
        token: &str,
        language: Option<&str>,
    ) -> HardcoverClient {
        HardcoverClient::new(
            Url::parse(&server.uri()).expect("mock server uri"),
            token.to_string(),
            0,
            3,
            language.map(str::to_string),
        )
    }

    /// Mounts the two mocks `find_series` needs: the `search` query and the
    /// follow-up by-id `series` lookup, both asserting the given auth header.
    async fn mount_find_series_mocks(server: &MockServer, auth: &str) {
        Mock::given(method("POST"))
            .and(path("/"))
            .and(header("Authorization", auth))
            .and(body_string_contains("search("))
            .respond_with(ResponseTemplate::new(200).set_body_string(fixture("search_series.json")))
            .expect(1)
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/"))
            .and(header("Authorization", auth))
            .and(body_string_contains("SeriesByIds"))
            .respond_with(ResponseTemplate::new(200).set_body_string(fixture("find_series.json")))
            .expect(1)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn find_series_happy_path_asserts_bearer_header() {
        let server = MockServer::start().await;
        mount_find_series_mocks(&server, "Bearer test-token").await;

        let client = client_for(&server, "test-token");
        let candidates = client.find_series("Expanse").await.expect("find_series");

        // Search-rank order (101 first) wins over the by-id lookup's order
        // (202 first in the fixture); null is_completed maps to false.
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].source_id, "101");
        assert_eq!(candidates[0].name, "The Expanse");
        assert_eq!(candidates[0].slug.as_deref(), Some("the-expanse"));
        assert!(candidates[0].is_completed);
        assert_eq!(candidates[1].slug, None);
        assert_eq!(candidates[0].readers_count, 12345);
        assert_eq!(candidates[1].source_id, "202");
        assert!(!candidates[1].is_completed);
        // Missing readers_count in the search document defaults to 0.
        assert_eq!(candidates[1].readers_count, 0);
    }

    #[tokio::test]
    async fn find_series_no_hits_returns_empty_without_lookup() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(fixture("search_empty.json")))
            .expect(1)
            .mount(&server)
            .await;

        let client = client_for(&server, "test-token");
        let candidates = client
            .find_series("No Such Series")
            .await
            .expect("find_series");
        assert!(candidates.is_empty());
    }

    #[tokio::test]
    async fn token_with_bearer_prefix_is_not_double_prefixed() {
        let server = MockServer::start().await;
        mount_find_series_mocks(&server, "Bearer test-token").await;

        let client = client_for(&server, "Bearer test-token");
        client.find_series("Expanse").await.expect("find_series");
    }

    #[tokio::test]
    async fn http_401_maps_to_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;

        let client = client_for(&server, "test-token");
        let err = client
            .find_series("Expanse")
            .await
            .expect_err("expected Auth");
        assert!(matches!(err, MetadataError::Auth));
    }

    #[tokio::test]
    async fn http_403_maps_to_api_error_with_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(403).set_body_string(
                r#"{"error":"ilike and related operations are not permitted on this server."}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let client = client_for(&server, "test-token");
        let err = client
            .find_series("Expanse")
            .await
            .expect_err("expected Api");
        assert!(matches!(err, MetadataError::Api(_)));
        assert!(err.to_string().contains("ilike"));
    }

    #[tokio::test]
    async fn series_slug_fetches_slug_by_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_string_contains("SeriesSlug"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"data":{"series":[{"id":101,"name":"The Expanse","slug":"the-expanse","is_completed":true}]}}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let client = client_for(&server, "test-token");
        let slug = client.series_slug("101").await.expect("series_slug");
        assert_eq!(slug.as_deref(), Some("the-expanse"));
    }

    #[tokio::test]
    async fn series_slug_missing_series_returns_none() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":{"series":[]}}"#))
            .expect(1)
            .mount(&server)
            .await;

        let client = client_for(&server, "test-token");
        let slug = client.series_slug("999").await.expect("series_slug");
        assert_eq!(slug, None);
    }

    #[tokio::test]
    async fn series_books_happy_path_parses_position_and_release_date() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(fixture("series_books.json")))
            .expect(1)
            .mount(&server)
            .await;

        let client = client_for(&server, "test-token");
        let books = client.series_books("101").await.expect("series_books");

        assert_eq!(books.len(), 3);
        assert_eq!(books[0].source_book_id, "1001");
        assert_eq!(books[0].title, "Leviathan Wakes");
        assert_eq!(books[0].position, Some(1.0));
        assert_eq!(
            books[0].release_date,
            Some(chrono::NaiveDate::from_ymd_opt(2011, 6, 15).unwrap())
        );
        assert_eq!(books[0].asin.as_deref(), Some("B005QJTNQC"));
        assert!(books[1].asin.is_none());
        assert_eq!(books[2].position, Some(8.5));
        assert!(books[2].release_date.is_none());
        assert!(books[2].asin.is_none());
    }

    #[tokio::test]
    async fn series_books_treats_bc_dates_as_unknown_release_date() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(fixture("series_books_bc_dates.json")),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = client_for(&server, "test-token");
        let books = client.series_books("10595").await.expect("series_books");

        // The BC date decodes to None instead of failing the whole series, and
        // the ordinary date alongside it still parses.
        assert_eq!(books.len(), 2);
        assert_eq!(books[0].source_book_id, "759804");
        assert!(books[0].release_date.is_none());
        assert_eq!(
            books[1].release_date,
            Some(chrono::NaiveDate::from_ymd_opt(2012, 9, 27).unwrap())
        );
    }

    #[tokio::test]
    async fn decode_failure_message_includes_serde_detail() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"data":{"book_series":[{"position":1,"book":{"id":"not-an-int","title":"X"}}]}}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let client = client_for(&server, "test-token");
        let err = client
            .series_books("101")
            .await
            .expect_err("expected Decode");

        assert!(matches!(err, MetadataError::Decode(_)));
        // The reqwest wrapper alone says only "error decoding response body";
        // the flattened chain must carry the serde reason too.
        assert!(
            err.to_string().contains("invalid type"),
            "expected serde detail in {err}"
        );
    }

    #[tokio::test]
    async fn series_books_filters_translations_when_language_set() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(fixture("series_books_translations.json")),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = client_with_language(&server, "test-token", Some("en"));
        let books = client.series_books("101").await.expect("series_books");

        // The German-only translation (1099) is dropped; the book with an
        // English edition (1001) and the book with no language data (1002)
        // both survive.
        let ids: Vec<&str> = books.iter().map(|b| b.source_book_id.as_str()).collect();
        assert_eq!(ids, vec!["1001", "1002"]);
    }

    #[tokio::test]
    async fn series_books_keeps_all_books_without_language_filter() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(fixture("series_books_translations.json")),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = client_for(&server, "test-token");
        let books = client.series_books("101").await.expect("series_books");

        assert_eq!(books.len(), 3);
    }

    #[tokio::test]
    async fn graphql_errors_payload_maps_to_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(fixture("errors.json")))
            .expect(1)
            .mount(&server)
            .await;

        let client = client_for(&server, "test-token");
        let err = client
            .find_series("Expanse")
            .await
            .expect_err("expected Api error");

        assert!(matches!(err, MetadataError::Api(_)));
        assert!(err.to_string().contains("query_root"));
    }

    #[tokio::test]
    async fn retries_after_429_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(429))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(fixture("search_empty.json")))
            .expect(1)
            .mount(&server)
            .await;

        // Zero-length throttle keeps this test fast; the retry backoff (~1s) is
        // the only real delay, which is acceptable for a single test.
        let client = client_for(&server, "test-token");
        let candidates = client.find_series("Expanse").await.expect("find_series");

        assert!(candidates.is_empty());
        let received = server.received_requests().await.expect("recording enabled");
        assert_eq!(received.len(), 2);
    }

    #[tokio::test]
    async fn throttle_spaces_out_consecutive_requests() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(fixture("search_empty.json")))
            .mount(&server)
            .await;

        let client = HardcoverClient::new(
            Url::parse(&server.uri()).expect("mock server uri"),
            "test-token".to_string(),
            50,
            3,
            None,
        );

        let start = Instant::now();
        client.find_series("Expanse").await.expect("first call");
        client.find_series("Expanse").await.expect("second call");
        let elapsed = start.elapsed();

        assert!(
            elapsed >= Duration::from_millis(50),
            "expected throttled calls to take at least 50ms, took {elapsed:?}"
        );
    }
}
