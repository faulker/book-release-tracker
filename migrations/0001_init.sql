-- One row per ABS series we track
CREATE TABLE series (
  id INTEGER PRIMARY KEY,
  abs_series_id TEXT NOT NULL UNIQUE,
  abs_name TEXT NOT NULL,
  hardcover_series_id TEXT,          -- NULL until matched
  hardcover_name TEXT,
  is_completed INTEGER NOT NULL DEFAULT 0,
  match_status TEXT NOT NULL DEFAULT 'unmatched',  -- unmatched|matched|overridden|ignored
  last_checked_at TEXT
);

-- Books the user owns in ABS, per series
CREATE TABLE owned_books (
  id INTEGER PRIMARY KEY,
  series_id INTEGER NOT NULL REFERENCES series(id) ON DELETE CASCADE,
  abs_item_id TEXT NOT NULL UNIQUE,
  title TEXT NOT NULL,
  sequence REAL,
  asin TEXT,
  isbn TEXT
);

-- Every remote book we've seen from Hardcover; dedupe + notification state
CREATE TABLE known_releases (
  id INTEGER PRIMARY KEY,
  series_id INTEGER NOT NULL REFERENCES series(id) ON DELETE CASCADE,
  source_book_id TEXT NOT NULL,      -- Hardcover book id
  title TEXT NOT NULL,
  position REAL,
  release_date TEXT,                 -- ISO date, NULL = unknown
  status TEXT NOT NULL,              -- upcoming|released|owned
  asin TEXT,
  first_seen_at TEXT NOT NULL,
  upcoming_notified_at TEXT,         -- set once when announced as upcoming
  released_notified_at TEXT,         -- set once when announced as released
  UNIQUE(series_id, source_book_id)
);

-- Manual ABS-name -> Hardcover mapping for mismatches (edited via dashboard)
CREATE TABLE series_overrides (
  abs_name_normalized TEXT PRIMARY KEY,
  hardcover_series_id TEXT,          -- NULL + ignore=1 means "stop trying"
  ignore INTEGER NOT NULL DEFAULT 0
);

-- Run history for dashboard status page
CREATE TABLE check_runs (
  id INTEGER PRIMARY KEY,
  started_at TEXT NOT NULL,
  finished_at TEXT,
  status TEXT NOT NULL,              -- running|ok|partial|failed
  series_checked INTEGER NOT NULL DEFAULT 0,
  events_found INTEGER NOT NULL DEFAULT 0,
  errors TEXT                        -- JSON array of per-series error strings
);
