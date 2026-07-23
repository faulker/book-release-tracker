//! Response DTOs for the subset of the Audiobookshelf API we consume. Only the
//! fields we actually read are modeled; ABS metadata is often incomplete, so
//! optional fields fall back to sensible defaults via `#[serde(default)]`.
//! Shapes follow https://api.audiobookshelf.org/.

use serde::{Deserialize, Deserializer};

/// A single Audiobookshelf library, from `GET /api/libraries`.
#[derive(Debug, Clone, Deserialize)]
pub struct Library {
    pub id: String,
    pub name: String,
    #[serde(rename = "mediaType")]
    pub media_type: String,
}

/// Response envelope for `GET /api/libraries` (not paginated).
#[derive(Debug, Clone, Deserialize)]
pub struct LibrariesResponse {
    pub libraries: Vec<Library>,
}

/// A series entry from `GET /api/libraries/{id}/series`. We only need the
/// id/name for matching and a book count for display; the full book list is
/// fetched separately via `series_items`.
#[derive(Debug, Clone, Deserialize)]
pub struct Series {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub books: Vec<serde_json::Value>,
}

impl Series {
    /// Number of books ABS reports for this series.
    pub fn book_count(&self) -> usize {
        self.books.len()
    }
}

/// Paginated response envelope shared by the `series` and `items` list
/// endpoints. ABS also sends `limit`/`page`, but only `results` and `total`
/// are needed to drive pagination, so the rest is left undeserialized.
#[derive(Debug, Clone, Deserialize)]
pub struct PaginatedResponse<T> {
    pub results: Vec<T>,
    pub total: u64,
}

/// A series/sequence pairing attached to a library item's metadata. ABS
/// usually sends `sequence` as a string (e.g. "1", "2.5"), but some versions
/// send a bare number, and it's `null` when the item has no position, so it's
/// normalized through [`deserialize_sequence`].
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ItemSeries {
    pub id: String,
    pub name: String,
    #[serde(default, deserialize_with = "deserialize_sequence")]
    pub sequence: Option<String>,
}

/// Deserializes `sequence` from a string, a bare number, or `null`, since ABS
/// is inconsistent about which one it sends.
fn deserialize_sequence<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrNumber {
        String(String),
        Number(f64),
    }

    Ok(
        Option::<StringOrNumber>::deserialize(deserializer)?.map(|v| match v {
            StringOrNumber::String(s) => s,
            StringOrNumber::Number(n) => n.to_string(),
        }),
    )
}

/// The subset of a library item's `media.metadata` we read.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ItemMetadata {
    #[serde(default)]
    pub title: String,
    #[serde(default, rename = "authorName")]
    pub author_name: String,
    #[serde(default, deserialize_with = "deserialize_series")]
    pub series: Vec<ItemSeries>,
    #[serde(default)]
    pub asin: Option<String>,
    #[serde(default)]
    pub isbn: Option<String>,
}

/// Deserializes `series`, which ABS sends as an array of series objects in
/// the full item shape but as a single object (or `null`) in the "minified"
/// shape returned when the items listing is filtered by series. Normalizes
/// both forms into a `Vec`.
fn deserialize_series<'de, D>(deserializer: D) -> Result<Vec<ItemSeries>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(ItemSeries),
        Many(Vec<ItemSeries>),
    }

    Ok(match Option::<OneOrMany>::deserialize(deserializer)? {
        Some(OneOrMany::One(s)) => vec![s],
        Some(OneOrMany::Many(s)) => s,
        None => Vec::new(),
    })
}

/// The subset of a library item's `media` we read.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ItemMedia {
    #[serde(default)]
    pub metadata: ItemMetadata,
}

/// A library item from `GET /api/libraries/{id}/items`.
#[derive(Debug, Clone, Deserialize)]
pub struct LibraryItem {
    pub id: String,
    #[serde(default)]
    pub media: ItemMedia,
}
