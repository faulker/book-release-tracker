//! Entry point: parse CLI args, load config, init tracing, dispatch to check/daemon.
//! All application logic lives in the library crate (`lib.rs`); this binary
//! only wires config into clients and maps the check report to an exit code.

use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use reqwest::Url;
use sqlx::SqlitePool;
use tokio::sync::{mpsc, watch};

use book_release_tracker::abs::AbsClient;
use book_release_tracker::cli::{Cli, Command};
use book_release_tracker::metadata::audnexus::{self, AudnexusClient};
use book_release_tracker::metadata::hardcover::HardcoverClient;
use book_release_tracker::notify::{Notifier, build_notifiers};
use book_release_tracker::{checker, config, db, web};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Default to info-level logs so the app is visibly doing something out of
    // the box; RUST_LOG still overrides (an unset RUST_LOG would otherwise
    // yield an empty filter, which only shows errors).
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let config = config::load(&cli.config)?;

    match cli.command {
        Command::Check { partial } => run_check(&config, partial).await,
        Command::Daemon => run_daemon(&config, cli.config.clone()).await,
    }
}

/// Builds the ABS, Hardcover, and optional Audnexus clients from config.
/// Shared by check and daemon modes so both wire clients identically.
fn build_clients(
    config: &config::Config,
) -> anyhow::Result<(AbsClient, HardcoverClient, Option<AudnexusClient>)> {
    let abs_base = Url::parse(&config.audiobookshelf.base_url)
        .context("audiobookshelf.base_url is not a valid URL")?;
    let abs = AbsClient::new(abs_base, config.audiobookshelf.api_key.clone());

    let hardcover = HardcoverClient::new(
        HardcoverClient::default_endpoint(),
        config.hardcover.token.clone(),
        config.hardcover.min_request_interval_ms,
        config.hardcover.max_retries,
        Some(config.hardcover.language.clone()).filter(|l| !l.trim().is_empty()),
    );

    let enricher = if config.audnexus.enabled {
        let base = Url::parse(audnexus::DEFAULT_BASE_URL)
            .expect("audnexus DEFAULT_BASE_URL is a valid URL");
        Some(AudnexusClient::new(base, config.audnexus.region.clone()))
    } else {
        None
    };

    Ok((abs, hardcover, enricher))
}

/// Builds the clients from config, runs one check, and exits with the
/// cron-friendly code from the report: 0 ok, 1 partial, 2 failed. Notifiers
/// are whatever `build_notifiers` assembles from `[notify.*]` config (always
/// includes the log notifier).
async fn run_check(config: &config::Config, partial: bool) -> anyhow::Result<()> {
    let pool = db::init(&config.check.db_path)
        .await
        .context("could not open the tracker database")?;

    let (abs, hardcover, enricher) = build_clients(config)?;

    let notifiers = build_notifiers(config);

    let mode = if partial {
        checker::ScanMode::Partial
    } else {
        checker::ScanMode::Full
    };
    let report = checker::run_check(
        &pool,
        &abs,
        &hardcover,
        enricher.as_ref(),
        &notifiers,
        &config.audiobookshelf.library_ids,
        chrono::Utc::now().date_naive(),
        mode,
    )
    .await
    .context("check run aborted")?;

    // Close cleanly before process::exit, which skips destructors.
    pool.close().await;
    std::process::exit(report.exit_code());
}

/// Daemon mode: runs a check immediately and then every
/// `check.interval_days`, serves the dashboard when enabled, and shuts
/// down on ctrl-c after letting any in-flight check finish.
async fn run_daemon(
    config: &config::Config,
    config_path: std::path::PathBuf,
) -> anyhow::Result<()> {
    let pool = db::init(&config.check.db_path)
        .await
        .context("could not open the tracker database")?;

    let (abs, hardcover, enricher) = build_clients(config)?;
    let notifiers = build_notifiers(config);

    // ABS may live in the same container and start after us; block until it
    // answers /ping (or the configured timeout passes) before the first check.
    let startup_timeout = config.audiobookshelf.startup_timeout_secs;
    if startup_timeout > 0 {
        tracing::info!(
            timeout_secs = startup_timeout,
            "waiting for audiobookshelf to become reachable"
        );
        abs.wait_until_ready(Duration::from_secs(startup_timeout), Duration::from_secs(5))
            .await
            .with_context(|| {
                format!(
                    "audiobookshelf did not become reachable within {startup_timeout}s \
                 (audiobookshelf.startup_timeout_secs)"
                )
            })?;
        tracing::info!("audiobookshelf is up");
    }

    // tokio::time::interval panics on a zero period, so clamp to 1 day.
    let interval_days = config.check.interval_days.max(1);
    let every = Duration::from_secs(interval_days * 86_400);
    tracing::info!(
        interval_days,
        "daemon started; running a check now, then on the interval (ctrl-c to stop)"
    );

    // run_tx: dashboard -> scheduler "run now" signal carrying the requested
    // scan mode, bounded at 1 so a second press while a run is in
    // flight/queued reports "already running". The original sender stays
    // alive here even with the dashboard disabled, so run_rx.recv() never
    // sees a closed channel while the daemon runs.
    let (run_tx, run_rx) = mpsc::channel::<checker::ScanMode>(1);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // The scheduler is spawned (not select!-inline) so ctrl-c never cancels a
    // check mid-flight; shutdown is only observed between runs.
    let scheduler = tokio::spawn(scheduler_loop(
        pool.clone(),
        abs,
        hardcover,
        enricher,
        notifiers,
        config.audiobookshelf.library_ids.clone(),
        every,
        run_rx,
        shutdown_rx.clone(),
    ));

    // Bind up front so a bad `dashboard.bind` fails at startup.
    let listener = if config.dashboard.enabled {
        let listener = tokio::net::TcpListener::bind(&config.dashboard.bind)
            .await
            .with_context(|| format!("could not bind dashboard to {}", config.dashboard.bind))?;
        // local_addr resolves the actual port when config binds to port 0.
        let addr = listener
            .local_addr()
            .context("dashboard listener has no local address")?;
        let host = if addr.ip().is_unspecified() {
            "localhost".to_string()
        } else {
            addr.ip().to_string()
        };
        tracing::info!("dashboard available at http://{host}:{}", addr.port());
        Some(listener)
    } else {
        tracing::info!("dashboard disabled; running scheduler only");
        None
    };
    let dashboard_enabled = listener.is_some();

    let state = web::AppState {
        pool: pool.clone(),
        run_tx: run_tx.clone(),
        interval: every,
        config_path,
    };
    let mut server_shutdown = shutdown_rx.clone();
    let server = async move {
        match listener {
            Some(listener) => {
                web::serve(listener, state, async move {
                    let _ = server_shutdown.changed().await;
                })
                .await
            }
            None => std::future::pending::<std::io::Result<()>>().await,
        }
    };
    tokio::pin!(server);

    tokio::select! {
        signal = tokio::signal::ctrl_c() => {
            match signal {
                Ok(()) => tracing::info!("ctrl-c received; finishing any in-flight check before exit"),
                Err(e) => tracing::warn!(error = %e, "ctrl-c handler failed; shutting down"),
            }
        }
        result = &mut server => {
            // The server only returns before shutdown on an error; stop the
            // scheduler and surface it.
            let _ = shutdown_tx.send(true);
            scheduler.await.context("scheduler task panicked")?;
            pool.close().await;
            result.context("dashboard server failed")?;
            anyhow::bail!("dashboard server exited unexpectedly");
        }
    }

    let _ = shutdown_tx.send(true);
    scheduler.await.context("scheduler task panicked")?;
    if dashboard_enabled {
        server
            .await
            .context("dashboard server failed during shutdown")?;
    }
    pool.close().await;
    tracing::info!("daemon stopped");
    Ok(())
}

/// The daemon's check loop: one full run on startup, then a partial run per
/// `every` tick (series with a book already scheduled for a future date are
/// skipped), plus dashboard run-now signals carrying their own mode (which
/// restart the countdown). Each run also delivers due watch-list reminders
/// (`run_check` does this first). Runs are strictly serial within this loop,
/// so two checks can never overlap; run-now presses that arrive mid-run are
/// drained afterwards because the run that just finished already covers
/// them. A check failure (e.g. the database briefly unavailable) is logged
/// and the loop continues.
#[allow(clippy::too_many_arguments)] // one arg per collaborator, mirroring checker::run_check
async fn scheduler_loop(
    pool: SqlitePool,
    abs: AbsClient,
    hardcover: HardcoverClient,
    enricher: Option<AudnexusClient>,
    notifiers: Vec<Box<dyn Notifier>>,
    library_ids: Vec<String>,
    every: Duration,
    mut run_rx: mpsc::Receiver<checker::ScanMode>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut first_run = true;

    loop {
        // The first tick completes immediately, giving the startup check.
        let mode;
        tokio::select! {
            _ = ticker.tick() => {
                // Initial scan covers everything; interval scans are partial.
                mode = if first_run { checker::ScanMode::Full } else { checker::ScanMode::Partial };
            }
            signal = run_rx.recv() => {
                match signal {
                    Some(requested) => {
                        tracing::info!(mode = ?requested, "run-now signal received from dashboard");
                        ticker.reset();
                        mode = requested;
                    }
                    // All senders dropped; the daemon is tearing down.
                    None => break,
                }
            }
            _ = shutdown_rx.changed() => break,
        }
        first_run = false;

        let today = chrono::Utc::now().date_naive();

        match checker::run_check(
            &pool,
            &abs,
            &hardcover,
            enricher.as_ref(),
            &notifiers,
            &library_ids,
            today,
            mode,
        )
        .await
        {
            Ok(report) => tracing::info!(
                status = ?report.status,
                events_found = report.events_found,
                "scheduled check finished"
            ),
            Err(e) => tracing::error!(error = %e, "check run aborted; daemon continues"),
        }

        // Coalesce run-now presses that arrived while the check was running.
        while run_rx.try_recv().is_ok() {}
    }
    tracing::info!("scheduler stopped");
}
