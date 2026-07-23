//! Command-line interface definition. See `main.rs` for dispatch.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// book-release-tracker: watches Audiobookshelf series and alerts on new or
/// upcoming releases via dashboard, ntfy, and email.
#[derive(Debug, Parser)]
#[command(name = "book-release-tracker", version, about)]
pub struct Cli {
    /// Path to the config file.
    #[arg(long, global = true, default_value = "./config.toml")]
    pub config: PathBuf,

    #[command(subcommand)]
    pub command: Command,
}

/// Top-level subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run a single check and exit (cron-friendly: 0 ok, 1 partial, 2 failed).
    Check {
        /// Only check series without a book scheduled for a future release
        /// date; series whose next release is already known are skipped.
        #[arg(long)]
        partial: bool,
    },
    /// Run continuously: periodic checks plus the web dashboard.
    Daemon,
}
