//! Audiobookshelf HTTP client: libraries, series, and series-items listing.
//!
//! The constructor takes an explicit `base_url` so tests can point it at a
//! wiremock server instead of a real ABS instance.

mod types;

pub use types::{ItemMetadata, ItemSeries, Library, LibraryItem, Series};

use reqwest::{Client, StatusCode, Url};
use serde::de::DeserializeOwned;
use std::time::Duration;
use thiserror::Error;
use types::{LibrariesResponse, PaginatedResponse};

/// Number of results requested per page on paginated list endpoints.
const PAGE_LIMIT: u64 = 100;

/// Errors returned by `AbsClient` methods.
#[derive(Debug, Error)]
pub enum AbsError {
    /// ABS rejected the API key (401).
    #[error("audiobookshelf rejected the API key (401 unauthorized)")]
    Auth,
    /// Any other non-2xx response, with a truncated body for context.
    #[error("audiobookshelf returned {status}: {body}")]
    Status { status: StatusCode, body: String },
    /// Request never completed (DNS, connection, timeout, decode, ...).
    #[error("audiobookshelf request failed: {0}")]
    Transport(#[from] reqwest::Error),
}

/// Audiobookshelf API client. Every request sends `Authorization: Bearer <api_key>`.
pub struct AbsClient {
    base_url: Url,
    api_key: String,
    client: Client,
}

impl AbsClient {
    /// Builds a client pointed at `base_url`, authenticating with `api_key`.
    pub fn new(base_url: Url, api_key: impl Into<String>) -> Self {
        Self {
            base_url,
            api_key: api_key.into(),
            client: Client::new(),
        }
    }

    /// Health probe against ABS's unauthenticated `/ping` endpoint. Succeeds
    /// on any 2xx; the auth header is sent but ignored by this endpoint.
    pub async fn ping(&self) -> Result<(), AbsError> {
        let url = self
            .base_url
            .join("/ping")
            .expect("static ABS ping path is always a valid relative reference");
        self.get(url).await?;
        Ok(())
    }

    /// Polls `ping()` every `poll` until ABS responds or `timeout` elapses.
    /// Meant for daemon startup when ABS lives in the same container and comes
    /// up after us. Returns the last ping error once the deadline passes.
    pub async fn wait_until_ready(
        &self,
        timeout: Duration,
        poll: Duration,
    ) -> Result<(), AbsError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match self.ping().await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    // Only sleep if another attempt still fits in the budget.
                    if tokio::time::Instant::now() + poll > deadline {
                        return Err(e);
                    }
                    tracing::warn!(error = %e, "audiobookshelf not ready yet; retrying");
                    tokio::time::sleep(poll).await;
                }
            }
        }
    }

    /// Lists book-type libraries (id + name), ignoring podcast/other libraries.
    pub async fn libraries(&self) -> Result<Vec<Library>, AbsError> {
        let url = self
            .base_url
            .join("/api/libraries")
            .expect("static ABS API path is always a valid relative reference");
        let resp = self.get(url).await?;
        let parsed: LibrariesResponse = resp.json().await?;
        Ok(parsed
            .libraries
            .into_iter()
            .filter(|l| l.media_type == "book")
            .collect())
    }

    /// Lists all series in `library_id`, paging through results until exhausted.
    pub async fn series(&self, library_id: &str) -> Result<Vec<Series>, AbsError> {
        let path = format!("/api/libraries/{library_id}/series");
        self.paginated(&path, &[]).await
    }

    /// Lists all library items in `series_id` (within `library_id`), sorted by
    /// series sequence. ABS expects the series filter value as
    /// `series.<base64(series_id)>`.
    pub async fn series_items(
        &self,
        library_id: &str,
        series_id: &str,
    ) -> Result<Vec<LibraryItem>, AbsError> {
        let path = format!("/api/libraries/{library_id}/items");
        let filter = format!("series.{}", base64_encode(series_id.as_bytes()));
        self.paginated(
            &path,
            &[
                ("filter", filter),
                ("sort", "media.metadata.series.sequence".to_string()),
            ],
        )
        .await
    }

    /// Fetches every page of a paginated ABS list endpoint at `path`, merging
    /// `extra_query` into each request alongside `limit`/`page`, until the
    /// accumulated results reach the server-reported `total`.
    async fn paginated<T: DeserializeOwned>(
        &self,
        path: &str,
        extra_query: &[(&str, String)],
    ) -> Result<Vec<T>, AbsError> {
        let mut page = 0u64;
        let mut all = Vec::new();
        loop {
            let url = self
                .base_url
                .join(path)
                .expect("ABS API path is always a valid relative reference");
            let mut query: Vec<(String, String)> = vec![
                ("limit".into(), PAGE_LIMIT.to_string()),
                ("page".into(), page.to_string()),
            ];
            query.extend(extra_query.iter().map(|(k, v)| (k.to_string(), v.clone())));

            let resp = self.get_with_query(url, &query).await?;
            let page_resp: PaginatedResponse<T> = resp.json().await?;
            let got = page_resp.results.len() as u64;
            all.extend(page_resp.results);

            if all.len() as u64 >= page_resp.total || got < PAGE_LIMIT {
                break;
            }
            page += 1;
        }
        Ok(all)
    }

    /// Sends an authenticated GET with no query params and maps the response status.
    async fn get(&self, url: Url) -> Result<reqwest::Response, AbsError> {
        let resp = self
            .client
            .get(url)
            .bearer_auth(&self.api_key)
            .send()
            .await?;
        Self::check_status(resp).await
    }

    /// Sends an authenticated GET with `query` params appended to `url`, and
    /// maps the response status. Query pairs are appended directly on the
    /// `Url` (rather than via `RequestBuilder::query`, which needs reqwest's
    /// `query` feature) since we only depend on reqwest's default features.
    async fn get_with_query(
        &self,
        mut url: Url,
        query: &[(String, String)],
    ) -> Result<reqwest::Response, AbsError> {
        url.query_pairs_mut()
            .extend_pairs(query.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        let resp = self
            .client
            .get(url)
            .bearer_auth(&self.api_key)
            .send()
            .await?;
        Self::check_status(resp).await
    }

    /// Maps a response's HTTP status to `AbsError`, or passes it through on success.
    async fn check_status(resp: reqwest::Response) -> Result<reqwest::Response, AbsError> {
        match resp.status() {
            StatusCode::UNAUTHORIZED => Err(AbsError::Auth),
            status if status.is_success() => Ok(resp),
            status => {
                let body = resp.text().await.unwrap_or_default();
                let snippet: String = body.chars().take(200).collect();
                Err(AbsError::Status {
                    status,
                    body: snippet,
                })
            }
        }
    }
}

/// Minimal standard base64 encoder (RFC 4648, with padding). ABS's series
/// filter needs `base64(series_id)`; pulling in a whole crate for one encode
/// felt like overkill.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        let n = ((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32);
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client_for(server: &MockServer) -> AbsClient {
        AbsClient::new(Url::parse(&server.uri()).unwrap(), "test-api-key")
    }

    #[tokio::test]
    async fn libraries_filters_to_book_type_and_sends_auth_header() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries"))
            .and(header("Authorization", "Bearer test-api-key"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                include_str!("../../tests/fixtures/abs/libraries.json"),
                "application/json",
            ))
            .mount(&server)
            .await;

        let libs = client_for(&server)
            .libraries()
            .await
            .expect("libraries should succeed");

        assert_eq!(libs.len(), 1);
        assert_eq!(libs[0].id, "lib_c1u2c");
        assert_eq!(libs[0].name, "Audiobooks");
    }

    #[tokio::test]
    async fn series_deserializes_happy_path() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries/lib_1/series"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                include_str!("../../tests/fixtures/abs/series.json"),
                "application/json",
            ))
            .mount(&server)
            .await;

        let series = client_for(&server)
            .series("lib_1")
            .await
            .expect("series should succeed");

        assert_eq!(series.len(), 1);
        assert_eq!(series[0].id, "ser_1");
        assert_eq!(series[0].name, "The Expanse");
        assert_eq!(series[0].book_count(), 3);
    }

    #[tokio::test]
    async fn series_items_sorts_by_sequence_and_defaults_missing_fields() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries/lib_1/items"))
            .and(query_param("sort", "media.metadata.series.sequence"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                include_str!("../../tests/fixtures/abs/items.json"),
                "application/json",
            ))
            .mount(&server)
            .await;

        let items = client_for(&server)
            .series_items("lib_1", "ser_1")
            .await
            .expect("series_items should succeed");

        assert_eq!(items.len(), 3);
        assert_eq!(items[0].media.metadata.title, "Leviathan Wakes");
        assert_eq!(items[0].media.metadata.author_name, "James S. A. Corey");
        assert_eq!(
            items[0].media.metadata.series[0].sequence.as_deref(),
            Some("1")
        );
        assert_eq!(items[0].media.metadata.asin.as_deref(), Some("B0064CGH1W"));

        // Third item omits authorName/series/asin/isbn entirely; defaults apply.
        assert_eq!(items[2].media.metadata.author_name, "");
        assert!(items[2].media.metadata.series.is_empty());
        assert_eq!(items[2].media.metadata.asin, None);
        assert_eq!(items[2].media.metadata.isbn, None);
    }

    #[tokio::test]
    async fn series_items_deserializes_minified_object_form_series() {
        // ABS returns `media.metadata.series` as a single object (not an
        // array), with `sequence` as a null or a bare number, in the
        // "minified" shape used when filtering the items listing by series.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries/lib_1/items"))
            .and(query_param("sort", "media.metadata.series.sequence"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                include_str!("../../tests/fixtures/abs/items_minified.json"),
                "application/json",
            ))
            .mount(&server)
            .await;

        let items = client_for(&server)
            .series_items("lib_1", "ser_9")
            .await
            .expect("series_items should succeed");

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].media.metadata.series.len(), 1);
        assert_eq!(items[0].media.metadata.series[0].id, "ser_9");
        assert_eq!(items[0].media.metadata.series[0].sequence, None);
        assert_eq!(items[0].media.metadata.asin, None);
        assert_eq!(items[0].media.metadata.isbn, None);

        // Bare number sequence normalizes to a string like the string form.
        assert_eq!(
            items[1].media.metadata.series[0].sequence.as_deref(),
            Some("2")
        );
    }

    #[tokio::test]
    async fn series_paginates_until_total_is_reached() {
        let server = MockServer::start().await;
        let page0 = serde_json::json!({
            "results": (0..PAGE_LIMIT)
                .map(|i| serde_json::json!({"id": format!("ser_{i}"), "name": format!("Series {i}")}))
                .collect::<Vec<_>>(),
            "total": PAGE_LIMIT + 1,
            "limit": PAGE_LIMIT,
            "page": 0,
        });
        let page1 = serde_json::json!({
            "results": [{"id": "ser_last", "name": "Series Last"}],
            "total": PAGE_LIMIT + 1,
            "limit": PAGE_LIMIT,
            "page": 1,
        });

        Mock::given(method("GET"))
            .and(path("/api/libraries/lib_1/series"))
            .and(query_param("page", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page0))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/libraries/lib_1/series"))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page1))
            .mount(&server)
            .await;

        let series = client_for(&server)
            .series("lib_1")
            .await
            .expect("series should succeed");

        assert_eq!(series.len(), (PAGE_LIMIT + 1) as usize);
        assert_eq!(series.last().unwrap().name, "Series Last");
    }

    #[tokio::test]
    async fn ping_succeeds_on_200() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ping"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"success":true}"#))
            .mount(&server)
            .await;

        client_for(&server)
            .ping()
            .await
            .expect("ping should succeed");
    }

    #[tokio::test]
    async fn wait_until_ready_retries_until_abs_is_up() {
        let server = MockServer::start().await;
        // First two probes fail with 503, then ABS is "up".
        Mock::given(method("GET"))
            .and(path("/ping"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(2)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ping"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"success":true}"#))
            .mount(&server)
            .await;

        client_for(&server)
            .wait_until_ready(Duration::from_secs(5), Duration::from_millis(10))
            .await
            .expect("should succeed after retries");
    }

    #[tokio::test]
    async fn wait_until_ready_returns_last_error_on_timeout() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ping"))
            .respond_with(ResponseTemplate::new(503).set_body_string("starting up"))
            .mount(&server)
            .await;

        let err = client_for(&server)
            .wait_until_ready(Duration::from_millis(50), Duration::from_millis(20))
            .await
            .expect_err("expected timeout with last error");

        match err {
            AbsError::Status { status, .. } => {
                assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE);
            }
            other => panic!("expected Status error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unauthorized_maps_to_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let err = client_for(&server)
            .libraries()
            .await
            .expect_err("expected auth error");

        assert!(matches!(err, AbsError::Auth));
    }

    #[tokio::test]
    async fn server_error_maps_to_status_error_with_body_snippet() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal boom"))
            .mount(&server)
            .await;

        let err = client_for(&server)
            .libraries()
            .await
            .expect_err("expected status error");

        match err {
            AbsError::Status { status, body } => {
                assert_eq!(status, reqwest::StatusCode::INTERNAL_SERVER_ERROR);
                assert!(body.contains("internal boom"));
            }
            other => panic!("expected Status error, got {other:?}"),
        }
    }
}
