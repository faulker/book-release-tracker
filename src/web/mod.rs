//! Web dashboard (daemon mode only): server-rendered HTML over the tracker
//! database, plus form posts for series overrides, run-now, and config-file
//! edits (the settings page). The dashboard never talks to ABS; it reads what
//! the checker wrote, nudges the scheduler through `run_tx`, and rewrites
//! `config.toml` for the next daemon start. Its Hardcover calls are
//! resolving a pasted series URL to an id and syncing a series right after
//! its override id is saved.

pub mod handlers;

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::{get, post};
use sqlx::SqlitePool;
use tokio::sync::mpsc;

use crate::checker::ScanMode;
use crate::metadata::MetadataSource;

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
    /// Metadata source for resolving pasted series page URLs to ids on the
    /// override form. `None` disables URL input (ids still work).
    pub metadata: Option<Arc<dyn MetadataSource>>,
}

/// Builds the dashboard router. Separate from [`serve`] so tests can drive
/// it directly with `tower::ServiceExt::oneshot`.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route(
            "/",
            get(|| async { axum::response::Redirect::to("/watchlist") }),
        )
        .route("/favicon.svg", get(favicon))
        .route("/status", get(handlers::index))
        .route("/series", get(handlers::series))
        .route("/series/{id}", get(handlers::series_detail))
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

/// App icon, compiled into the binary so the dashboard needs no static dir.
const ICON_SVG: &str = include_str!("static/icon.svg");

/// Serves the app icon used as the browser favicon.
async fn favicon() -> impl axum::response::IntoResponse {
    (
        [
            (axum::http::header::CONTENT_TYPE, "image/svg+xml"),
            (axum::http::header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        ICON_SVG,
    )
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
