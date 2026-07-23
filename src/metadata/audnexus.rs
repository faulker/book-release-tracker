//! Audnexus enrichment client. Not a `MetadataSource`: it can only decorate a
//! book already identified by ASIN (narrator, runtime, summary), not discover
//! series or list a series' books.

use chrono::NaiveDate;
use reqwest::{Client, StatusCode, Url};
use serde::Deserialize;

use super::MetadataError;

/// Audnexus's production API base URL.
pub const DEFAULT_BASE_URL: &str = "https://api.audnex.us";

/// Narrator/runtime/summary/release-date data for a single audiobook ASIN.
#[derive(Debug, Clone, PartialEq)]
pub struct BookEnrichment {
    pub narrators: Vec<String>,
    pub runtime_minutes: Option<u32>,
    pub summary: Option<String>,
    pub release_date: Option<NaiveDate>,
}

/// Small Audnexus HTTP client. `base_url` is explicit (rather than hardcoded) so
/// tests can point it at a `wiremock` server; use `DEFAULT_BASE_URL` for
/// production wiring.
pub struct AudnexusClient {
    base_url: Url,
    region: String,
    client: Client,
}

impl AudnexusClient {
    /// Builds a client for the given `base_url` and marketplace `region` (e.g. "us").
    pub fn new(base_url: Url, region: String) -> Self {
        Self {
            base_url,
            region,
            client: Client::new(),
        }
    }

    /// Fetches enrichment data for `asin` via `GET /books/{asin}?region=..`.
    /// Returns `Ok(None)` if Audnexus has no record for the ASIN (404); any
    /// other non-success status or transport failure is returned as an error,
    /// never silently treated as "no data".
    pub async fn book(&self, asin: &str) -> Result<Option<BookEnrichment>, MetadataError> {
        let mut url = self
            .base_url
            .join(&format!("books/{asin}"))
            .map_err(|e| MetadataError::Transport(e.to_string()))?;
        url.query_pairs_mut().append_pair("region", &self.region);

        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| MetadataError::Transport(e.to_string()))?;

        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(MetadataError::Transport(format!(
                "unexpected HTTP status {}",
                response.status()
            )));
        }

        let body: AudnexusBook = response
            .json()
            .await
            .map_err(|e| MetadataError::Decode(e.to_string()))?;

        Ok(Some(BookEnrichment {
            narrators: body.narrators.into_iter().map(|n| n.name).collect(),
            runtime_minutes: body.runtime_length_min,
            summary: body.summary,
            release_date: body.release_date,
        }))
    }
}

#[derive(Debug, Deserialize)]
struct AudnexusBook {
    #[serde(default)]
    narrators: Vec<AudnexusNarrator>,
    #[serde(default, rename = "runtimeLengthMin")]
    runtime_length_min: Option<u32>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(
        default,
        rename = "releaseDate",
        deserialize_with = "deserialize_flexible_date"
    )]
    release_date: Option<NaiveDate>,
}

#[derive(Debug, Deserialize)]
struct AudnexusNarrator {
    name: String,
}

/// Audnexus has been observed returning `releaseDate` as either a bare date or
/// a full ISO-8601 datetime; this accepts either and keeps only the date part.
fn deserialize_flexible_date<'de, D>(deserializer: D) -> Result<Option<NaiveDate>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Option<String> = Option::deserialize(deserializer)?;
    match raw {
        None => Ok(None),
        Some(s) => {
            let date_part = s.split('T').next().unwrap_or(&s);
            NaiveDate::parse_from_str(date_part, "%Y-%m-%d")
                .map(Some)
                .map_err(serde::de::Error::custom)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/audnexus/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap_or_else(|e| panic!("read fixture {name}: {e}"))
    }

    #[tokio::test]
    async fn book_happy_path_parses_enrichment() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/books/B0071VXKAG"))
            .and(query_param("region", "us"))
            .respond_with(ResponseTemplate::new(200).set_body_string(fixture("book.json")))
            .expect(1)
            .mount(&server)
            .await;

        let client = AudnexusClient::new(
            Url::parse(&server.uri()).expect("mock server uri"),
            "us".to_string(),
        );
        let enrichment = client
            .book("B0071VXKAG")
            .await
            .expect("book lookup")
            .expect("enrichment present");

        assert_eq!(enrichment.narrators, vec!["Jefferson Mays".to_string()]);
        assert_eq!(enrichment.runtime_minutes, Some(1177));
        assert!(enrichment.summary.is_some());
        assert_eq!(
            enrichment.release_date,
            Some(NaiveDate::from_ymd_opt(2011, 6, 15).unwrap())
        );
    }

    #[tokio::test]
    async fn book_not_found_returns_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/books/UNKNOWNASIN"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;

        let client = AudnexusClient::new(
            Url::parse(&server.uri()).expect("mock server uri"),
            "us".to_string(),
        );
        let enrichment = client.book("UNKNOWNASIN").await.expect("book lookup");

        assert!(enrichment.is_none());
    }
}
