//! `MetadataSource` trait and normalized domain types shared by remote
//! book-metadata backends (currently Hardcover). Audnexus lives here too as a
//! plain enrichment client, but it does not implement `MetadataSource` since it
//! can't discover series, only decorate a known ASIN.

pub mod audnexus;
pub mod hardcover;

use async_trait::async_trait;
use chrono::NaiveDate;
use thiserror::Error;

/// A remote book-metadata service that can resolve a series and list its books.
/// Implementations must be swappable; the checker only sees this trait.
#[async_trait]
pub trait MetadataSource: Send + Sync {
    /// Fuzzy-search series by name; returns candidates for matching.rs to score.
    async fn find_series(&self, name: &str) -> Result<Vec<SeriesCandidate>, MetadataError>;
    /// All books in a series with position and release_date.
    async fn series_books(&self, source_series_id: &str) -> Result<Vec<RemoteBook>, MetadataError>;
    /// The series' URL slug on the source's website, by source id. Used to
    /// backfill slugs for series matched (or overridden) before the slug was
    /// stored; `None` when the source has no slug for it.
    async fn series_slug(&self, source_series_id: &str) -> Result<Option<String>, MetadataError>;
}

/// A candidate series match returned by `find_series`, before scoring in matching.rs.
#[derive(Debug, Clone, PartialEq)]
pub struct SeriesCandidate {
    pub source_id: String,
    pub name: String,
    /// URL slug on the source's website (e.g. hardcover.app/series/{slug});
    /// stored so the dashboard can link to the series page.
    pub slug: Option<String>,
    pub is_completed: bool,
    /// Reader count reported by the source's search index; used by matching.rs
    /// to break ties between duplicate series entries with the same name.
    pub readers_count: u64,
}

/// A single book within a series, as reported by the metadata source.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteBook {
    pub source_book_id: String,
    pub title: String,
    pub position: Option<f64>,
    pub release_date: Option<NaiveDate>,
    pub asin: Option<String>,
}

/// Errors produced by `MetadataSource` implementations and enrichment clients.
#[derive(Debug, Error)]
pub enum MetadataError {
    /// The remote API's GraphQL `errors` array was non-empty; surfaced explicitly
    /// rather than being silently ignored.
    #[error("metadata API returned errors: {0}")]
    Api(String),
    /// Retries against repeated 429/5xx responses were exhausted.
    #[error("rate limited after exhausting retries")]
    RateLimited,
    /// The request was rejected for authentication/authorization reasons (401/403).
    #[error("authentication failed")]
    Auth,
    /// A network-level failure or an unexpected (non-retryable) HTTP status.
    #[error("transport error: {0}")]
    Transport(String),
    /// The response body could not be decoded into the expected shape.
    #[error("could not decode response: {0}")]
    Decode(String),
}
