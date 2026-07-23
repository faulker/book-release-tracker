//! book-release-tracker: watches Audiobookshelf series and alerts on new or
//! upcoming releases.
//!
//! All application modules live in this library crate so both the binary
//! (`main.rs`) and the integration tests under `tests/` can link against
//! them; Cargo integration tests cannot import from a bin-only crate.

pub mod abs;
pub mod checker;
pub mod cli;
pub mod config;
pub mod db;
pub mod diff;
pub mod error;
pub mod matching;
pub mod metadata;
pub mod notify;
pub mod web;
