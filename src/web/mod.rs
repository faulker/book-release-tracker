//! Web dashboard (daemon mode only): server-rendered HTML over the tracker
//! database, plus form posts for series overrides, run-now, and config-file
//! edits (the settings page). The dashboard never talks to ABS or Hardcover
//! itself; it reads what the checker wrote, nudges the scheduler through
//! `run_tx`, and rewrites `config.toml` for the next daemon start.

pub mod handlers;

use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use axum::Router;
use axum::routing::{get, post};
use sqlx::SqlitePool;
use tokio::sync::mpsc;

use crate::checker::ScanMode;

/// Shared state for all dashboard handlers.
#[derive(Clone)]
pub struct AppState {
    /// Tracker database; every page renders from it.
    pub pool: SqlitePool,
    /// Run-now signal into the scheduler loop, carrying the requested scan
    /// mode. Bounded at 1 so a second press while a check is running/queued
    /// fails fast ("already running").
    pub run_tx: mpsc::Sender<ScanMode>,
    /// Configured check interval, shown on the status page.
    pub interval: Duration,
    /// Path to `config.toml`; the settings page reads and rewrites it.
    /// Changes take effect on the next daemon start, not live.
    pub config_path: PathBuf,
}

/// Builds the dashboard router. Separate from [`serve`] so tests can drive
/// it directly with `tower::ServiceExt::oneshot`.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(handlers::index))
        .route("/series", get(handlers::series))
        .route("/series/ignore", post(handlers::set_ignore))
        .route("/upcoming", get(handlers::upcoming))
        .route("/new", get(handlers::new_releases))
        .route("/watchlist", get(handlers::watchlist))
        .route("/watch", post(handlers::add_watch))
        .route("/watch/remove", post(handlers::remove_watch))
        .route("/unmatched", get(handlers::unmatched))
        .route(
            "/settings",
            get(handlers::settings).post(handlers::save_settings),
        )
        .route("/settings/test-email", post(handlers::test_email))
        .route("/overrides", post(handlers::save_override))
        .route("/check", post(handlers::run_now))
        .with_state(state)
}

/// Serves the dashboard on `listener` until `shutdown` resolves, then stops
/// accepting connections and drains in-flight requests. The caller binds the
/// listener so a bad `dashboard.bind` fails at startup, not inside a task.
pub async fn serve(
    listener: tokio::net::TcpListener,
    state: AppState,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await
}
