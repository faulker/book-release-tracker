# book-release-tracker

Watches your [Audiobookshelf](https://www.audiobookshelf.org/) series, matches them against [Hardcover](https://hardcover.app/), and alerts you when a new book in a series you're following gets released or announced. Alerts go out via log, [ntfy](https://ntfy.sh/), and/or email, and a web dashboard shows what's upcoming, what's new, and which series still need a manual match.

[Project page on sleepymagpie.com](https://sleepymagpie.com/tools/book-release-tracker.html)

## Requirements

- Rust (stable toolchain)
- An Audiobookshelf API key: log into ABS as an admin, go to **Settings -> Users**, open your user, and generate an API key.
- A Hardcover API token: log into [hardcover.app](https://hardcover.app/), go to **Account Settings -> API** (https://hardcover.app/account/api), and copy your token.

## Setup

Copy the example config and fill in your values:

```sh
cp config.example.toml config.toml
```

At minimum, set `audiobookshelf.base_url`, `audiobookshelf.api_key`, and `hardcover.token`. Secrets can also be supplied via environment variables instead of the config file, which take precedence over whatever is in `config.toml`:

- `BRT_ABS_API_KEY` overrides `audiobookshelf.api_key`
- `BRT_HARDCOVER_TOKEN` overrides `hardcover.token`
- `BRT_NTFY_TOKEN` overrides `notify.ntfy.token`
- `BRT_SMTP_PASSWORD` overrides `notify.email.password`

Config is validated on load; every problem (missing/invalid fields) is reported at once rather than one at a time.

## Config reference

```toml
[audiobookshelf]
base_url = "https://audiobook.pi4.us"  # required, must be a valid URL
api_key = ""                            # required; or env BRT_ABS_API_KEY
library_ids = []                        # optional; empty = check all libraries
startup_timeout_secs = 300              # daemon mode: wait up to this long for ABS
                                        # to answer /ping before the first check
                                        # (covers ABS starting after the daemon in
                                        # the same container). 0 = no wait.

[hardcover]
token = ""                       # required; or env BRT_HARDCOVER_TOKEN
min_request_interval_ms = 1100   # client-side throttle between requests
max_retries = 3                  # retries on 429/5xx with exponential backoff
language = "en"                  # ISO 639-1; drops series books whose editions are
                                 # all in other languages (translations listed as
                                 # separate books). "" disables the filter. Books
                                 # with no language data on Hardcover are kept.

[audnexus]
enabled = true      # optional enrichment (narrator/runtime/summary) via Audnexus
region = "us"        # Audnexus storefront region

[check]
interval_minutes = 360        # daemon mode only: time between checks
db_path = "book-tracker.db"   # SQLite file (or ":memory:")
run_timeout_mins = 120        # cancel a check that runs longer than this and record
                              # it as timed_out on the dashboard. 0 = no limit.
scan_cooldown_days = 14       # skip scheduled scans while a completed scan is
                              # younger than this; manual runs always scan. 0 disables.

[dashboard]
enabled = true               # daemon mode only
bind = "127.0.0.1:8091"

[notify.ntfy]
enabled = false
server = "https://ntfy.sh"
topic = ""                # required if enabled
token = ""                # optional; or env BRT_NTFY_TOKEN

[notify.email]
enabled = false
smtp_host = ""     # required if enabled
smtp_port = 587
username = ""      # required if enabled
password = ""      # required if enabled; or env BRT_SMTP_PASSWORD
from = ""          # required if enabled
to = ""            # required if enabled
```

A log notifier is always active regardless of the `[notify.*]` settings, so every release event shows up in the process log even with ntfy and email both disabled.

## Usage

### `check`: one-shot mode

Runs a single check and exits. Exit codes are cron-friendly: `0` all series checked cleanly, `1` some series had errors (partial), `2` the run failed outright (e.g. Audiobookshelf was unreachable).

```sh
book-release-tracker --config /path/to/config.toml check
book-release-tracker --config /path/to/config.toml check --partial
```

`--partial` skips series that already have a book scheduled for a future release date; there's nothing new to learn about them until that date passes. On an empty database nothing is scheduled yet, so a partial check naturally covers everything.

Crontab example (every 6 hours):

```cron
0 */6 * * * cd /opt/book-release-tracker && ./book-release-tracker check >> check.log 2>&1
```

macOS `launchd` example (`~/Library/LaunchAgents/com.example.book-release-tracker.plist`):

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.example.book-release-tracker</string>
    <key>ProgramArguments</key>
    <array>
        <string>/opt/book-release-tracker/book-release-tracker</string>
        <string>--config</string>
        <string>/opt/book-release-tracker/config.toml</string>
        <string>check</string>
    </array>
    <key>StartInterval</key>
    <integer>21600</integer>
    <key>StandardOutPath</key>
    <string>/opt/book-release-tracker/check.log</string>
    <key>StandardErrorPath</key>
    <string>/opt/book-release-tracker/check.log</string>
</dict>
</plist>
```

Load it with `launchctl load ~/Library/LaunchAgents/com.example.book-release-tracker.plist`.

### `daemon`: continuous mode

Runs a full check immediately, then a partial check every `check.interval_minutes` (series with a book already scheduled for a future date are skipped until that date passes), and serves the web dashboard (if `dashboard.enabled`) at `dashboard.bind` (default `http://127.0.0.1:8091`) until it receives ctrl-c, at which point it finishes any in-flight check before exiting.

Scheduled scans additionally honor a cooldown: while a completed scan is younger than `check.scan_cooldown_days` (default 14), interval ticks skip the scan and only process watch-list reminders. Manual runs (the dashboard's Run buttons or the `check` subcommand) always scan.

```sh
book-release-tracker --config /path/to/config.toml daemon
```

On startup it prints the check interval and the dashboard URL. Logs default to `info` level; set `RUST_LOG` (e.g. `RUST_LOG=debug`) to change verbosity.

Dashboard pages:

- `/` : redirects to `/watchlist`, the default page
- `/status` : recent check-run history, headline counts, and "Run check" (partial) / "Run full scan" buttons (skipped if a check is already running or queued)
- `/series` : every tracked series sorted by next release date (soonest first, unscheduled last) with author, match status, the next scheduled release, and (when nothing is scheduled) its last release date; each series name links to its detail page, each row has an ignore/unignore button, a search box filters by series or author, a toggle hides series without a scheduled release, and a status filter shows all, ignored, not-ignored, or archived series (ignored and archived ones are shaded and hidden by default)
- `/upcoming` : releases with a future date, soonest first, filterable by series or author; each row links the series to its Hardcover page, the author to an Audible author search, and (when an ASIN is known) the title to its Audible page, and has a Watch button that opens a popup asking how many days before or after release to remind you; watched rows are tinted, and a filter narrows the list to watched or unwatched releases
- `/series/{id}` : one series' upcoming and released books (owned ones marked), with Watch buttons on upcoming books and a form to change its Hardcover match by pasting a series URL or id
- `/new` : recently released books, filterable by series or author, with the same series/author/title links as `/upcoming`
- `/watchlist` : releases you're watching, with their reminder window (editable until the reminder is sent) and a remove button; a reminder fires once per watch, through every enabled notification channel, when `release_date` plus/minus the configured day offset arrives (e.g. 1 day before, or 7 days after for audiobooks that lag the print date)
- `/unmatched` : series the matcher couldn't resolve, plus series you matched by hand (so a wrong id can be corrected), with a form that takes a Hardcover series id or a pasted `https://hardcover.app/series/{slug}` URL (resolved to its id on save), or tick "ignore" to stop trying; saving an id syncs that series with Hardcover right away (dropping books from a previous match), and new books are announced on the next check
- `/settings` : edit every `config.toml` field from the browser; saving validates and rewrites the file atomically (comments are not preserved), and changes take effect the next time the daemon starts. Secret fields never display their stored value; leave one blank to keep it, or type a new value to replace it. Env var overrides (`BRT_*`) still win at startup

Form errors (a bad id, a failed Hardcover lookup or sync) return you to the same page with a dismissable popover rather than an error page.

Ignoring a series (from `/series` or `/unmatched`) stores an override keyed by normalized name, so it sticks across library re-syncs; ignored series are never matched or checked again until unignored, and their books are hidden from `/upcoming`, `/new`, and `/unmatched`.

When a check's ABS sync no longer lists a series, it's archived: its owned books, known releases, and watches are deleted, but the series row and its Hardcover match are kept. Archived series only show on `/series` under the Archived filter. If the series shows up in ABS again, the next check un-archives it and pulls its books back in. A sync that returns no series at all archives nothing, so an empty or misconfigured ABS can't wipe the database. Narrowing `library_ids` counts as removal, so series from libraries you drop get archived too.

## Build & test

```sh
cargo build            # debug build
cargo build --release   # release build
cargo test              # full test suite (offline; no network or real API keys needed)
cargo clippy -- -D warnings
```

### Building for Linux (from macOS)

The dependency tree is fully static-friendly (rustls for TLS, bundled SQLite), so you can cross-compile a self-contained musl binary that runs on any Linux distro with no runtime dependencies:

```sh
brew install zig
cargo install cargo-zigbuild

scripts/build-linux.sh            # x86_64
scripts/build-linux.sh aarch64    # arm64 (Raspberry Pi, Graviton, etc.)
```

The script wraps `cargo zigbuild`, which you can also run directly:

```sh
cargo zigbuild --release --target x86_64-unknown-linux-musl   # static binary (what the script builds)
cargo zigbuild --release --target x86_64-unknown-linux-gnu    # glibc binary
```

Don't use a plain `cargo build --target ...-linux-...` on macOS. It links with Apple's `ld`, which fails with `ld: unknown options: --as-needed -Bstatic ...`.

The binary lands in `target/<arch>-unknown-linux-musl/release/book-release-tracker` (or `-linux-gnu` for the glibc build). Copy it to the target machine along with your `config.toml` (the `migrations/` directory and web templates are embedded in the binary at compile time). Building natively on a Linux box also works with a plain `cargo build --release`.

## How it works

Each check syncs libraries/series/items from Audiobookshelf, matches each series against Hardcover by normalized name (falling back to a manual override if one exists), diffs Hardcover's book list against what you own and what's already been seen to classify each book as owned/new/upcoming, enriches events that have an ASIN via Audnexus (narrator, runtime, summary), and notifies every enabled channel. A book is only notified once per state (once as upcoming, once as released), and a notification is only recorded as sent once at least one channel delivered it, so a total outage retries on the next run. Series that can't be confidently matched are left for review on the `/unmatched` dashboard page rather than guessed at.

## Notes / limitations

- Hardcover's API is in beta and its rate limits aren't documented publicly. The client throttles requests client-side (`hardcover.min_request_interval_ms`) and retries 429/5xx responses with exponential backoff, but you may still need to tune these if you hit limits.
- Series matching is by normalized name (case/punctuation/whitespace-insensitive, articles and a trailing "series" stripped). Hardcover's catalog contains duplicate same-name series entries, so among normalized-equal candidates the one with a uniquely highest reader count wins; a genuine tie or missing match is never guessed and instead lands on `/unmatched` for a manual Hardcover series id override.
- `audnexus.region` controls which Audnexus storefront is queried for enrichment; enrichment is best-effort and never blocks a notification if it fails.
