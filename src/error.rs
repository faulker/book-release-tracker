//! Top-level application error type. Module-specific errors (e.g. `MetadataError`,
//! `NotifyError`) live alongside the modules that produce them and get wrapped here
//! as the call stack unwinds toward `main`.
//!
//! Not yet constructed: main.rs currently propagates `ConfigError` via `anyhow`
//! directly. `AppError` will be used once later milestones add more failure modes
//! that need distinguishing at the top level.

use thiserror::Error;

/// The error type returned by binary-level operations (config loading, dispatch, etc).
#[derive(Debug, Error)]
pub enum AppError {
    /// Config file could not be read, parsed, or failed validation.
    #[error("config error: {0}")]
    Config(#[from] crate::config::ConfigError),
}
